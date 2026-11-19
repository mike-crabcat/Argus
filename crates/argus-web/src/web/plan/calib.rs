//! Calibrating a camera in the plan editor: its picture beside the plan,
//! click a spot on the ground in the picture and then the same spot on the
//! plan, four times or more. The fit (the same code the server uses) runs
//! as you go: each pair's error in mm, pairs set aside as mis-clicks, and
//! the plan's metre grid drawn over the picture to show how well it fits.
//! Save makes it a `set_calibration` command, undoable like any edit.

use argus_plan::calib::{self, CalPair, Calibration};
use argus_plan::{Command, Id, Point, pt};
use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, pos2, vec2};

use super::{Msg, PlanView, btn, heading, hint, sp};
use crate::web::theme::{self, ACCENT, ALERT, LINE, TEXT, TEXT_2, TEXT_3};
use crate::web::{fetch_image, send};

/// Colours for pair numbers, so a pair reads as one across both panes.
const PAIR: Color32 = Color32::from_rgb(0x5B, 0xC8, 0xF2);

pub(super) enum Snap {
    Loading,
    /// Texture key in `bgs`, and where it came from.
    Ready(String, &'static str),
    Failed(String),
}

pub(super) struct Calib {
    pub level: Id,
    pub camera: Id,
    pub name: String,
    pub pairs: Vec<CalPair>,
    /// A picture point waiting for its plan point.
    pub pending: Option<[f64; 2]>,
    pub fit: Option<Result<Calibration, String>>,
    pub snap: Snap,
    /// Where the picture was drawn last frame (for scripts).
    pub image_rect: Option<Rect>,
}

impl Calib {
    fn refit(&mut self) {
        self.fit =
            (self.pairs.len() >= 4).then(|| calib::fit(&self.pairs).map_err(|e| e.to_string()));
    }
}

/// Where an image of `px` pixels fits in `r`, keeping its shape.
fn fit_rect(r: Rect, px: [u32; 2]) -> Rect {
    let k = (r.width() / px[0] as f32).min(r.height() / px[1] as f32);
    Rect::from_center_size(r.center(), vec2(px[0] as f32 * k, px[1] as f32 * k))
}

impl PlanView {
    /// Starts calibrating `camera` (a placement on `level`).
    pub(super) fn start_calibration(&mut self, ctx: &egui::Context, level: Id, camera: Id) {
        let Some(c) = self
            .doc
            .as_ref()
            .and_then(|d| d.plan.levels.iter().find(|l| l.id == level))
            .and_then(|l| l.cameras.iter().find(|c| c.id == camera))
        else {
            return;
        };
        let pairs = c
            .calibration
            .as_ref()
            .map(|k| k.pairs.clone())
            .unwrap_or_default();
        let mut k = Calib {
            level,
            camera,
            name: c.camera.clone(),
            pairs,
            pending: None,
            fit: None,
            snap: Snap::Loading,
            image_rect: None,
        };
        k.refit();
        self.calib = Some(k);
        self.edit = None;
        self.fetch_snapshot(ctx);
    }

    /// The camera's picture: a snapshot now, or else its latest event still.
    pub(super) fn fetch_snapshot(&mut self, ctx: &egui::Context) {
        let Some(k) = &mut self.calib else { return };
        k.snap = Snap::Loading;
        let name = k.name.clone();
        let (inbox, token, ctx) = (self.inbox.clone(), self.token.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let key = format!("calib-{name}-{}", super::now_ms() as u64);
            let snap = format!("/api/cameras/{name}/snapshot");
            let mut got = fetch_image(&snap, token.as_deref(), 2048)
                .await
                .map(|r| (r, "live snapshot"));
            if got.is_err() {
                let from = crate::web::now_us() / 1000 - 7 * 86_400_000;
                let list = format!("/api/events?from={from}&camera={name}");
                if let Ok((200, body)) = send("GET", &list, None, None, token.as_deref()).await
                    && let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
                    && let Some(still) = v["events"].as_array().and_then(|es| {
                        es.iter()
                            .rev()
                            .find_map(|e| e["stills"][0].as_str().map(str::to_owned))
                    })
                {
                    got = fetch_image(&format!("/api/media/{still}"), token.as_deref(), 2048)
                        .await
                        .map(|r| (r, "latest event still"));
                }
            }
            let msg = match got {
                Ok(((size, orig, rgba), from)) => Msg::Snapshot {
                    key,
                    from,
                    size,
                    orig,
                    rgba,
                },
                Err(e) => Msg::SnapshotFailed(format!("No picture from {name}: {e}")),
            };
            inbox.borrow_mut().push(msg);
            ctx.request_repaint();
        });
    }

    /// The picture pane: click a ground spot; pairs and the grid drawn on it.
    pub(super) fn calib_image_ui(&mut self, ui: &mut egui::Ui, area: Rect) {
        let p = ui.painter_at(area);
        p.rect_filled(
            area,
            CornerRadius::ZERO,
            Color32::from_rgb(0x0C, 0x0C, 0x0E),
        );
        p.vline(area.right() - 0.5, area.y_range(), Stroke::new(1.0, LINE));
        let Some(k) = &self.calib else { return };
        let (key, from) = match &k.snap {
            Snap::Ready(key, from) => (key.clone(), *from),
            Snap::Loading => {
                p.text(
                    area.center(),
                    Align2::CENTER_CENTER,
                    "Getting the camera's picture…",
                    theme::regular(13.0),
                    TEXT_2,
                );
                return;
            }
            Snap::Failed(e) => {
                p.text(
                    area.center(),
                    Align2::CENTER_CENTER,
                    e,
                    theme::regular(13.0),
                    TEXT_2,
                );
                return;
            }
        };
        let Some(bg) = self.bgs.get(&key) else { return };
        let img = fit_rect(area.shrink(12.0), bg.px);
        if let Some(k) = &mut self.calib {
            k.image_rect = Some(img);
        }
        let Some(k) = &self.calib else { return };
        p.image(
            bg.tex.id(),
            img,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        let to_screen = |f: [f64; 2]| {
            pos2(
                img.left() + f[0] as f32 * img.width(),
                img.top() + f[1] as f32 * img.height(),
            )
        };
        p.text(
            pos2(area.left() + 14.0, area.top() + 12.0),
            Align2::LEFT_TOP,
            format!("{} · {from}", k.name),
            theme::regular(12.0),
            TEXT_2,
        );
        // Where the fit puts the picture behind the camera (beyond its
        // horizon): shaded, since positions there are refused.
        if let Some(Ok(fit)) = &k.fit
            && fit.folded > 0.0
        {
            let (nx, ny) = (48, 36);
            let (cw, ch) = (img.width() / nx as f32, img.height() / ny as f32);
            for j in 0..ny {
                for i in 0..nx {
                    let f = [(i as f64 + 0.5) / nx as f64, (j as f64 + 0.5) / ny as f64];
                    if calib::beyond_horizon(fit, f) {
                        let at = pos2(img.left() + i as f32 * cw, img.top() + j as f32 * ch);
                        p.rect_filled(
                            Rect::from_min_size(at, vec2(cw + 0.5, ch + 0.5)),
                            CornerRadius::ZERO,
                            ALERT.gamma_multiply(0.28),
                        );
                    }
                }
            }
        }
        // The plan's metre grid on the floor, through the fit.
        if let Some(Ok(fit)) = &k.fit
            && let Some(inv) = calib::picture_map(&fit.homography)
        {
            let centre = calib::project(&fit.homography, [0.5, 0.8]);
            if let Some(c) = centre {
                let (cx, cy) = (
                    (c.x / 1000.0).round() * 1000.0,
                    (c.y / 1000.0).round() * 1000.0,
                );
                let span = 12_000.0;
                let stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.55));
                let line = |a: Point, b: Point| {
                    let mut prev: Option<Pos2> = None;
                    for i in 0..=40 {
                        let t = i as f64 / 40.0;
                        let q = pt(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t);
                        let s = calib::to_picture(&inv, q)
                            .filter(|f| {
                                (-0.2..=1.2).contains(&f[0]) && (-0.2..=1.2).contains(&f[1])
                            })
                            .map(to_screen);
                        if let (Some(a), Some(b)) = (prev, s)
                            && img.expand(4.0).contains(a)
                            && img.expand(4.0).contains(b)
                        {
                            p.line_segment([a, b], stroke);
                        }
                        prev = s;
                    }
                };
                let mut v = -span;
                while v <= span {
                    line(pt(cx + v, cy - span), pt(cx + v, cy + span));
                    line(pt(cx - span, cy + v), pt(cx + span, cy + v));
                    v += 1000.0;
                }
            }
        }
        // Pairs: numbered, red if set aside.
        let outliers = match &k.fit {
            Some(Ok(f)) => f.outliers.clone(),
            _ => Vec::new(),
        };
        for (i, pr) in k.pairs.iter().enumerate() {
            let col = if outliers.contains(&i) { ALERT } else { PAIR };
            marker(&p, to_screen(pr.image), i + 1, col);
        }
        if let Some(f) = k.pending {
            marker(&p, to_screen(f), k.pairs.len() + 1, ACCENT);
        }
        // Clicks.
        let resp = ui.interact(img, ui.scope_id().with("calib-image"), Sense::click());
        if resp.clicked()
            && let Some(at) = resp.interact_pointer_pos()
        {
            let f = [
                ((at.x - img.left()) / img.width()).clamp(0.0, 1.0) as f64,
                ((at.y - img.top()) / img.height()).clamp(0.0, 1.0) as f64,
            ];
            if let Some(k) = &mut self.calib {
                k.pending = Some(f);
            }
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
    }

    /// A click on the plan while calibrating: the pending pair's plan spot.
    pub(super) fn calib_plan_click(&mut self, at: Point) {
        let Some(k) = &mut self.calib else { return };
        let Some(image) = k.pending.take() else {
            self.note("First click the spot in the camera's picture, then the same spot here.");
            return;
        };
        k.pairs.push(CalPair { image, plan: at });
        k.refit();
    }

    /// Pairs on the plan, and where each picture point lands through the
    /// fit (a line shows the error).
    pub(super) fn draw_calib_plan(&self, p: &egui::Painter, view: &crate::plandoc::View) {
        let Some(k) = &self.calib else { return };
        let fit = match &k.fit {
            Some(Ok(f)) => Some(f),
            _ => None,
        };
        for (i, pr) in k.pairs.iter().enumerate() {
            let at = sp(view, pr.plan);
            let out = fit.is_some_and(|f| f.outliers.contains(&i));
            if let Some(f) = fit
                && let Some(q) = calib::project(&f.homography, pr.image)
            {
                p.line_segment(
                    [at, sp(view, q)],
                    Stroke::new(1.5, if out { ALERT } else { TEXT_3 }),
                );
                p.circle_stroke(sp(view, q), 3.0, Stroke::new(1.0, TEXT_2));
            }
            marker(p, at, i + 1, if out { ALERT } else { PAIR });
        }
    }

    /// The side panel while calibrating.
    pub(super) fn calib_props(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(k) = &mut self.calib else { return };
        heading(ui, &format!("Calibrate {}", k.name));
        hint(
            ui,
            "Click a spot on the ground in the picture, then the same spot on the plan: corners of paving, the foot of a post, a door threshold. Use six or more, spread across the whole view: left and right, near the bottom of the picture and further away. Four is the minimum, but four always fit exactly, so mistakes can't show.",
        );
        if k.pending.is_some() {
            ui.label(
                egui::RichText::new(format!(
                    "Pair {}: now click it on the plan.",
                    k.pairs.len() + 1
                ))
                .color(ACCENT),
            );
        }
        ui.add_space(4.0);
        match &k.fit {
            None => {
                hint(ui, &format!("{} of 4 pairs.", k.pairs.len()));
            }
            Some(Err(e)) => {
                ui.label(egui::RichText::new(e).color(ALERT));
            }
            Some(Ok(f)) => {
                let quality = if f.folded > 0.01 {
                    "poor"
                } else if f.rms_mm < 150.0 && !f.warnings.is_empty() {
                    "fair"
                } else if f.rms_mm < 150.0 {
                    "good"
                } else if f.rms_mm < 400.0 {
                    "fair"
                } else {
                    "poor"
                };
                ui.label(
                    egui::RichText::new(format!("Fit: {:.0} mm RMS ({quality})", f.rms_mm))
                        .font(theme::semibold(13.0))
                        .color(TEXT),
                );
                for (i, e) in f.errors.iter().enumerate() {
                    let out = f.outliers.contains(&i);
                    let txt = if out {
                        format!("{}  {:.0} mm · set aside", i + 1, e)
                    } else {
                        format!("{}  {:.0} mm", i + 1, e)
                    };
                    ui.label(egui::RichText::new(txt).color(if out { ALERT } else { TEXT_2 }));
                }
                if !f.outliers.is_empty() {
                    hint(
                        ui,
                        "Pairs set aside disagree with the rest: likely a mis-click. Remove them or click again.",
                    );
                }
                for w in &f.warnings {
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new(w).color(ALERT));
                }
            }
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if btn(ui, "Undo point").clicked() {
                if k.pending.take().is_none() {
                    k.pairs.pop();
                }
                k.refit();
            }
            if btn(ui, "Clear").clicked() {
                k.pairs.clear();
                k.pending = None;
                k.refit();
            }
        });
        if btn(ui, "New picture").clicked() {
            self.fetch_snapshot(&ctx);
            return;
        }
        let Some(k) = &mut self.calib else { return };
        let ok = matches!(k.fit, Some(Ok(_)));
        let (level, camera, pairs) = (k.level, k.camera, k.pairs.clone());
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ok {
                if btn(ui, "Save calibration").clicked()
                    && self.exec(Command::SetCalibration {
                        level,
                        camera,
                        pairs: Some(pairs),
                    })
                {
                    self.note("Calibration saved.");
                    self.calib = None;
                }
            } else {
                ui.add_enabled(false, egui::Button::new("Save calibration"));
            }
            if btn(ui, "Cancel").clicked() {
                self.calib = None;
            }
        });
    }
}

fn marker(p: &egui::Painter, at: Pos2, n: usize, col: Color32) {
    p.circle_filled(at, 9.0, col);
    p.circle_stroke(at, 9.0, Stroke::new(1.5, Color32::from_black_alpha(160)));
    p.text(
        at,
        Align2::CENTER_CENTER,
        n.to_string(),
        theme::semibold(11.0),
        Color32::BLACK,
    );
}
