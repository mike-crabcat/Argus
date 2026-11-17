//! The app: top bar and a grid of live tiles; double-click for full window.

use std::cell::RefCell;
use std::rc::Rc;

use eframe::egui::{self, Align2, CornerRadius, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};
use eframe::egui_wgpu::RenderState;

use super::theme::{self, ACCENT, ALERT, BG, LINE, SURFACE, TEXT, TEXT_2, TEXT_3};
use super::tile::{Status, Tile};
use super::{get_text, now_ms, page_token};
use crate::{Camera, CameraList, layout};

const GAP: f32 = 8.0;
const RECONNECT_MS: f64 = 2000.0;

enum Cameras {
    Loading,
    Loaded(Vec<Camera>),
    Failed(String),
}

pub struct App {
    token: Option<String>,
    cameras: Rc<RefCell<Cameras>>,
    tiles: Vec<Tile>,
    /// Where each tile was drawn last frame, in CSS pixels (for stats).
    tile_rects: Vec<(usize, Rect)>,
    /// Index of the camera shown full window (on its main stream).
    focus: Option<usize>,
    show_stats: bool,
    backend: String,
    last_stats_ms: f64,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        super::set_status("");
        let token = page_token();
        let cameras = Rc::new(RefCell::new(Cameras::Loading));
        let (c, t, ctx) = (cameras.clone(), token.clone(), cc.egui_ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text("/api/cameras", t.as_deref())
                .await
                .and_then(|body| {
                    serde_json::from_str::<CameraList>(&body).map_err(|e| e.to_string())
                });
            *c.borrow_mut() = match result {
                Ok(list) => Cameras::Loaded(list.cameras),
                Err(e) => Cameras::Failed(e),
            };
            ctx.request_repaint();
        });
        let backend = cc
            .wgpu_render_state
            .as_ref()
            .map(|rs| match rs.adapter.get_info().backend {
                eframe::wgpu::Backend::BrowserWebGpu => "WebGPU".to_string(),
                eframe::wgpu::Backend::Gl => "WebGL2".to_string(),
                b => format!("{b:?}"),
            })
            .unwrap_or_else(|| "no GPU".into());
        Self {
            token,
            cameras,
            tiles: Vec::new(),
            tile_rects: Vec::new(),
            focus: None,
            show_stats: true,
            backend,
            last_stats_ms: 0.0,
        }
    }

    /// Opens tiles once the camera list is in, and reopens failed ones.
    fn connect(&mut self, ctx: &egui::Context, rs: &RenderState) {
        let cams = match &*self.cameras.borrow() {
            Cameras::Loaded(c) => c.clone(),
            _ => return,
        };
        if self.tiles.is_empty() {
            for cam in &cams {
                let stream = cam.grid_stream().unwrap_or("sub");
                self.tiles
                    .push(Tile::open(ctx, &cam.name, stream, self.token.as_deref()));
            }
        }
        let now = now_ms();
        for (i, cam) in cams.iter().enumerate() {
            let want = if self.focus == Some(i) {
                cam.focus_stream()
            } else {
                cam.grid_stream()
            }
            .unwrap_or("sub");
            let t = &mut self.tiles[i];
            let stale = t.stream != want || (t.failed() && now - t.opened_ms > RECONNECT_MS);
            if stale {
                t.release(rs);
                *t = Tile::open(ctx, &cam.name, want, self.token.as_deref());
            }
        }
    }

    /// Publishes per-tile numbers on `window.argusStats` (for tests and
    /// curious users) twice a second.
    fn publish_stats(&mut self) {
        let now = now_ms();
        if now - self.last_stats_ms < 500.0 {
            return;
        }
        self.last_stats_ms = now;
        let rects = self.tile_rects.clone();
        let tiles: Vec<_> = self
            .tiles
            .iter_mut()
            .enumerate()
            .map(|(i, t)| {
                let rect = rects
                    .iter()
                    .find(|(j, _)| *j == i)
                    .map(|(_, r)| [r.min.x, r.min.y, r.width(), r.height()]);
                let (decoded, received) = (t.decoded_fps(), t.received_fps());
                let v = t.view();
                serde_json::json!({
                    "camera": t.camera,
                    "stream": t.stream,
                    "status": format!("{:?}", v.status),
                    "codec": v.codec,
                    "size": v.texture.map(|(_, s)| [s.0, s.1]),
                    "received_fps": received,
                    "decoded_fps": decoded,
                    "shown_fps": v.fps,
                    "latency_ms": v.latency_ms,
                    "age_ms": v.age_ms,
                    "dropped": v.dropped,
                    "rect": rect,
                })
            })
            .collect();
        let focus = self
            .focus
            .and_then(|i| self.tiles.get(i))
            .map(|t| t.camera.clone());
        let json = serde_json::json!({
            "backend": self.backend, "focus": focus, "tiles": tiles,
        })
        .to_string();
        if let Some(w) = web_sys::window() {
            let _ = js_sys::Reflect::set(&w, &"argusStats".into(), &json.into());
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let n = self.tiles.len();
        let live = self
            .tiles
            .iter_mut()
            .map(|t| t.view().live)
            .filter(|l| *l)
            .count();
        let bar = ui.max_rect();
        let p = ui.painter();
        p.hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, LINE));
        let cy = bar.center().y;
        let title = p.text(
            pos2(bar.left() + 16.0, cy),
            Align2::LEFT_CENTER,
            "Argus",
            theme::semibold(17.0),
            TEXT,
        );
        let what = match &*self.cameras.borrow() {
            Cameras::Loading => "loading cameras…".to_string(),
            Cameras::Loaded(_) => format!("{n} cameras"),
            Cameras::Failed(_) => "no cameras".to_string(),
        };
        p.text(
            pos2(title.right() + 14.0, cy),
            Align2::LEFT_CENTER,
            what,
            theme::regular(13.5),
            TEXT_2,
        );

        // Right side, right to left: LIVE pill, Stats toggle, GPU backend.
        let pill = |x_right: f32, w: f32| {
            Rect::from_center_size(pos2(x_right - w / 2.0, cy), vec2(w, 26.0))
        };
        let live_r = pill(bar.right() - 16.0, 68.0);
        p.rect_stroke(
            live_r,
            CornerRadius::same(theme::PILL_RADIUS),
            Stroke::new(1.0, LINE),
            StrokeKind::Inside,
        );
        let on = live > 0;
        p.circle_filled(
            pos2(live_r.left() + 15.0, cy),
            3.5,
            if on { ALERT } else { TEXT_3 },
        );
        p.text(
            pos2(live_r.left() + 25.0, cy),
            Align2::LEFT_CENTER,
            "LIVE",
            theme::mono(11.0),
            if on { TEXT } else { TEXT_3 },
        );

        let stats_r = pill(live_r.left() - 8.0, 64.0);
        let resp = ui.interact(stats_r, ui.scope_id().with("stats"), Sense::click());
        if resp.clicked() {
            self.show_stats = !self.show_stats;
        }
        let p = ui.painter();
        p.rect_filled(stats_r, CornerRadius::same(theme::PILL_RADIUS), SURFACE);
        let border = if self.show_stats || resp.hovered() {
            ACCENT
        } else {
            LINE
        };
        p.rect_stroke(
            stats_r,
            CornerRadius::same(theme::PILL_RADIUS),
            Stroke::new(1.0, border),
            StrokeKind::Inside,
        );
        p.text(
            stats_r.center(),
            Align2::CENTER_CENTER,
            "Stats",
            theme::regular(12.5),
            if self.show_stats { TEXT } else { TEXT_2 },
        );

        p.text(
            pos2(stats_r.left() - 12.0, cy),
            Align2::RIGHT_CENTER,
            &self.backend,
            theme::mono(11.0),
            TEXT_3,
        );
    }

    fn draw_tile(&mut self, ui: &mut egui::Ui, i: usize, rect: Rect) {
        let hovered = ui.rect_contains_pointer(rect);
        let focused = self.focus == Some(i);
        let show_stats = self.show_stats;
        let t = &mut self.tiles[i];
        let v = t.view();
        let p = ui.painter_at(rect);
        let radius = CornerRadius::same(theme::TILE_RADIUS);
        p.rect_filled(rect, radius, SURFACE);
        if let Some((tex, (w, h))) = v.texture {
            let (x, y, fw, fh) = layout::fit(rect.width(), rect.height(), w as f32 / h as f32);
            let r = Rect::from_min_size(rect.min + vec2(x, y), vec2(fw, fh));
            egui::Image::new((tex, r.size()))
                .corner_radius(radius)
                .paint_at(ui, r);
        } else {
            let msg = match &v.status {
                Status::Connecting | Status::Live => "Connecting…".to_string(),
                Status::Error(e) => e.clone(),
            };
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                msg,
                theme::regular(12.5),
                TEXT_3,
            );
        }
        if hovered {
            p.rect_stroke(rect, radius, Stroke::new(1.5, ACCENT), StrokeKind::Inside);
        }
        // Name chip, top left.
        let name_galley = p.layout_no_wrap(t.camera.clone(), theme::semibold(13.5), TEXT);
        let chip = Rect::from_min_size(
            rect.min + vec2(8.0, 8.0),
            name_galley.size() + vec2(16.0, 8.0),
        );
        p.rect_filled(
            chip,
            CornerRadius::same(theme::PILL_RADIUS),
            BG.gamma_multiply(0.85),
        );
        p.galley(chip.min + vec2(8.0, 4.0), name_galley, TEXT);
        // Live dot, top right.
        let dot = if v.live { ALERT } else { TEXT_3 };
        p.circle_filled(pos2(rect.right() - 16.0, rect.top() + 16.0), 4.0, dot);
        // Stats, top right beside the live dot (cameras print their own
        // text at the bottom).
        if show_stats {
            let lat = v.latency_ms.map_or("–".into(), |l| format!("{l:.0} ms"));
            let mut s = format!("{:.0} fps · {lat}", v.fps);
            if focused {
                if let Some((_, (w, h))) = v.texture {
                    s = format!("{} {w}×{h} · {s}", t.stream);
                }
            }
            let g = p.layout_no_wrap(s, theme::mono(11.0), TEXT_2);
            let size = g.size() + vec2(12.0, 6.0);
            let r = Rect::from_min_size(
                pos2(
                    rect.right() - 30.0 - size.x,
                    rect.top() + 16.0 - size.y / 2.0,
                ),
                size,
            );
            p.rect_filled(
                r,
                CornerRadius::same(theme::PILL_RADIUS),
                BG.gamma_multiply(0.85),
            );
            p.galley(r.min + vec2(6.0, 3.0), g, TEXT_2);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        let Some(rs) = frame.wgpu_render_state().cloned() else {
            return;
        };
        self.connect(ctx, &rs);
        for t in &mut self.tiles {
            t.update(&rs);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.focus = None;
        }
        self.publish_stats();

        egui::Panel::top("bar")
            .exact_size(44.0)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(BG))
            .show(ui, |ui| self.top_bar(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(BG).inner_margin(GAP))
            .show(ui, |ui| {
                let area = ui.available_rect_before_wrap();
                if let Cameras::Failed(e) = &*self.cameras.borrow() {
                    ui.painter().text(
                        area.center(),
                        Align2::CENTER_CENTER,
                        e,
                        theme::regular(13.5),
                        TEXT_2,
                    );
                    return;
                }
                let mut rects = Vec::new();
                match self.focus.filter(|&i| i < self.tiles.len()) {
                    Some(i) => {
                        self.draw_tile(ui, i, area);
                        rects.push((i, area));
                    }
                    None => {
                        let n = self.tiles.len();
                        let aspect = self
                            .tiles
                            .iter_mut()
                            .find_map(|t| t.view().texture.map(|(_, (w, h))| w as f32 / h as f32))
                            .unwrap_or(4.0 / 3.0);
                        let (cols, rows, w, h) =
                            layout::grid(n, area.width(), area.height(), GAP, aspect);
                        let total = vec2(
                            cols as f32 * w + (cols as f32 - 1.0) * GAP,
                            rows as f32 * h + (rows as f32 - 1.0) * GAP,
                        );
                        let origin = area.min + (area.size() - total) / 2.0;
                        for i in 0..n {
                            let (c, r) = (i % cols.max(1), i / cols.max(1));
                            let min = origin + vec2(c as f32 * (w + GAP), r as f32 * (h + GAP));
                            let rect = Rect::from_min_size(min, Vec2::new(w, h));
                            self.draw_tile(ui, i, rect);
                            rects.push((i, rect));
                        }
                    }
                }
                // Double-click a tile: its main stream, full window; again
                // (or Escape) to go back to the grid.
                let dbl = ui.input(|i| {
                    i.pointer
                        .button_double_clicked(egui::PointerButton::Primary)
                        .then(|| i.pointer.latest_pos())
                        .flatten()
                });
                if let Some(pos) = dbl {
                    if let Some(&(i, _)) = rects.iter().find(|(_, r)| r.contains(pos)) {
                        self.focus = if self.focus == Some(i) { None } else { Some(i) };
                    }
                }
                self.tile_rects = rects;
            });
        // Keep the stats readout fresh even without new frames.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}
