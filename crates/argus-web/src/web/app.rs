//! The app: top bar, a grid of camera tiles, live or replay; in replay a
//! timeline with play/pause and seek. Double-click a live tile for its main
//! stream full window.

use std::cell::RefCell;
use std::rc::Rc;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2,
};
use eframe::egui_wgpu::RenderState;

use super::replay::Replay;
use super::theme::{self, ACCENT, ALERT, BG, LINE, SURFACE, TEXT, TEXT_2, TEXT_3};
use super::tile::{Status, Tile};
use super::{get_text, now_ms, now_us, page_token};
use crate::{Camera, CameraList, layout};

const GAP: f32 = 8.0;
const RECONNECT_MS: f64 = 2000.0;
/// How much history the replay timeline shows.
const TIMELINE_MS: f64 = 3_600_000.0;
const TIMELINE_H: f32 = 64.0;

enum Cameras {
    Loading,
    Loaded(Vec<Camera>),
    Failed(String),
}

/// Recorded spans for the timeline (union over cameras), ms since epoch.
#[derive(Default)]
struct Spans {
    spans: Vec<(f64, f64)>,
    fetched_ms: f64,
}

/// What one tile shows, whatever the source.
struct TileInfo {
    camera: String,
    texture: Option<(egui::TextureId, (u32, u32))>,
    message: Option<String>,
    dot: Option<Color32>,
    stats: Option<String>,
}

pub struct App {
    token: Option<String>,
    cameras: Rc<RefCell<Cameras>>,
    tiles: Vec<Tile>,
    replay: Option<Replay>,
    spans: Rc<RefCell<Spans>>,
    /// Where each tile was drawn last frame, in CSS pixels (for stats).
    tile_rects: Vec<(usize, Rect)>,
    /// Index of the camera shown full window (on its main stream).
    focus: Option<usize>,
    show_stats: bool,
    backend: String,
    last_stats_ms: f64,
    /// Pointer position over the timeline, as a time (ms), for the hint.
    hover_ms: Option<f64>,
    /// The timeline bar's rect and time range, [x, y, w, h, t0, t1], for
    /// scripts that want to click a time.
    timeline_rect_cache: Option<[f64; 6]>,
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
            replay: None,
            spans: Rc::new(RefCell::new(Spans::default())),
            tile_rects: Vec::new(),
            focus: None,
            show_stats: true,
            backend,
            last_stats_ms: 0.0,
            hover_ms: None,
            timeline_rect_cache: None,
        }
    }

    fn camera_names(&self) -> Vec<String> {
        match &*self.cameras.borrow() {
            Cameras::Loaded(c) => c.iter().map(|c| c.name.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// Opens live tiles once the camera list is in, and reopens failed ones.
    fn connect(&mut self, ctx: &egui::Context, rs: &RenderState) {
        if self.replay.is_some() {
            return;
        }
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

    /// Shows camera `f` full window on its main stream (live or replay), or
    /// goes back to the grid.
    fn set_focus(&mut self, f: Option<usize>) {
        if let Some(r) = &mut self.replay {
            if let Some(old) = self.focus.filter(|o| Some(*o) != f) {
                r.set_stream(old, "sub");
            }
            if let Some(new) = f {
                r.set_stream(new, "main");
            }
        }
        self.focus = f;
    }

    fn start_replay(&mut self, ctx: &egui::Context, rs: &RenderState, t_ms: f64) {
        for t in self.tiles.iter_mut() {
            t.release(rs);
        }
        self.tiles.clear();
        self.focus = None;
        let names = self.camera_names();
        self.replay = Some(Replay::open(
            ctx,
            &names,
            "sub",
            t_ms as i64,
            self.token.as_deref(),
        ));
        self.fetch_spans(ctx);
    }

    fn stop_replay(&mut self, rs: &RenderState) {
        if let Some(mut r) = self.replay.take() {
            r.release(rs);
        }
    }

    fn fetch_spans(&self, ctx: &egui::Context) {
        let now = now_us() as f64 / 1000.0;
        let from = (now - TIMELINE_MS) as i64;
        let path = format!("/api/recordings?stream=sub&from={from}");
        let (spans, token, ctx) = (self.spans.clone(), self.token.clone(), ctx.clone());
        spans.borrow_mut().fetched_ms = now_ms();
        wasm_bindgen_futures::spawn_local(async move {
            let Ok(body) = get_text(&path, token.as_deref()).await else {
                return;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
                return;
            };
            let mut all: Vec<(f64, f64)> = v["cameras"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|c| c["spans"].as_array().cloned().unwrap_or_default())
                .filter_map(|s| Some((s[0].as_f64()?, s[1].as_f64()?)))
                .collect();
            all.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            let mut merged: Vec<(f64, f64)> = Vec::new();
            for (a, b) in all {
                match merged.last_mut() {
                    Some(last) if a <= last.1 + 2000.0 => last.1 = last.1.max(b),
                    _ => merged.push((a, b)),
                }
            }
            spans.borrow_mut().spans = merged;
            ctx.request_repaint();
        });
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
        let rect_of = |i: usize| {
            rects
                .iter()
                .find(|(j, _)| *j == i)
                .map(|(_, r)| [r.min.x, r.min.y, r.width(), r.height()])
        };
        let tiles: Vec<_> = self
            .tiles
            .iter_mut()
            .enumerate()
            .map(|(i, t)| {
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
                    "rect": rect_of(i),
                })
            })
            .collect();
        let focus = self
            .focus
            .and_then(|i| self.tiles.get(i))
            .map(|t| t.camera.clone());
        let replay = self.replay.as_ref().map(|r| {
            let mut s = r.stats();
            let n = s["cameras"].as_array().map_or(0, |c| c.len());
            s["rects"] = (0..n).map(rect_of).collect::<Vec<_>>().into();
            s
        });
        let json = serde_json::json!({
            "backend": self.backend,
            "mode": if self.replay.is_some() { "replay" } else { "live" },
            "focus": focus,
            "tiles": tiles,
            "replay": replay,
            "timeline": self.timeline_rect_cache,
        })
        .to_string();
        if let Some(w) = web_sys::window() {
            let _ = js_sys::Reflect::set(&w, &"argusStats".into(), &json.into());
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, rs: &RenderState) {
        let n = self.camera_names().len();
        let live = self
            .tiles
            .iter_mut()
            .map(|t| t.view().live)
            .filter(|l| *l)
            .count();
        let bar = ui.max_rect();
        let p = ui.painter().clone();
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

        // Right side, right to left: Live | Replay switch, Stats, backend.
        let pill = |x_right: f32, w: f32| {
            Rect::from_center_size(pos2(x_right - w / 2.0, cy), vec2(w, 26.0))
        };
        let seg = pill(bar.right() - 16.0, 148.0);
        let (live_r, replay_r) = seg.split_left_right_at_fraction(0.5);
        let in_replay = self.replay.is_some();
        p.rect_filled(seg, CornerRadius::same(theme::PILL_RADIUS), SURFACE);
        p.rect_stroke(
            seg,
            CornerRadius::same(theme::PILL_RADIUS),
            Stroke::new(1.0, LINE),
            StrokeKind::Inside,
        );
        let active = if in_replay { replay_r } else { live_r };
        p.rect_stroke(
            active.shrink(2.0),
            CornerRadius::same(theme::PILL_RADIUS),
            Stroke::new(1.0, ACCENT),
            StrokeKind::Inside,
        );
        let on = live > 0 && !in_replay;
        p.circle_filled(
            pos2(live_r.left() + 17.0, cy),
            3.5,
            if on { ALERT } else { TEXT_3 },
        );
        p.text(
            pos2(live_r.left() + 27.0, cy),
            Align2::LEFT_CENTER,
            "LIVE",
            theme::mono(11.0),
            if in_replay { TEXT_2 } else { TEXT },
        );
        p.text(
            replay_r.center(),
            Align2::CENTER_CENTER,
            "REPLAY",
            theme::mono(11.0),
            if in_replay { TEXT } else { TEXT_2 },
        );
        let id = ui.scope_id();
        if ui
            .interact(live_r, id.with("live"), Sense::click())
            .clicked()
            && in_replay
        {
            self.stop_replay(rs);
        }
        if ui
            .interact(replay_r, id.with("replay"), Sense::click())
            .clicked()
            && !in_replay
        {
            let t = now_us() as f64 / 1000.0 - 60_000.0;
            self.start_replay(ui.ctx(), rs, t);
        }

        let stats_r = pill(seg.left() - 8.0, 64.0);
        let resp = ui.interact(stats_r, id.with("stats"), Sense::click());
        if resp.clicked() {
            self.show_stats = !self.show_stats;
        }
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

    fn tile_info(&mut self, i: usize) -> TileInfo {
        let show_stats = self.show_stats;
        if let Some(r) = &self.replay {
            let v = r.view(i);
            let message = if v.texture.is_some() {
                None
            } else if let Some(e) = r.error() {
                Some(e)
            } else {
                Some("Loading…".into())
            };
            let stats = (show_stats && !v.waiting)
                .then(|| v.behind_ms.map(|b| format!("{:+.0} ms", -b)))
                .flatten();
            return TileInfo {
                camera: v.camera,
                texture: v.texture,
                message,
                dot: None,
                stats,
            };
        }
        let focused = self.focus == Some(i);
        let t = &mut self.tiles[i];
        let v = t.view();
        let message = match (&v.texture, &v.status) {
            (Some(_), _) => None,
            (None, Status::Error(e)) => Some(e.clone()),
            (None, _) => Some("Connecting…".into()),
        };
        let stats = show_stats.then(|| {
            let lat = v.latency_ms.map_or("–".into(), |l| format!("{l:.0} ms"));
            let s = format!("{:.0} fps · {lat}", v.fps);
            match (focused, v.texture) {
                (true, Some((_, (w, h)))) => format!("{} {w}×{h} · {s}", t.stream),
                _ => s,
            }
        });
        TileInfo {
            camera: t.camera.clone(),
            texture: v.texture,
            message,
            dot: Some(if v.live { ALERT } else { TEXT_3 }),
            stats,
        }
    }

    fn draw_tile(&mut self, ui: &mut egui::Ui, i: usize, rect: Rect) {
        let hovered = ui.rect_contains_pointer(rect);
        let info = self.tile_info(i);
        let p = ui.painter_at(rect);
        let radius = CornerRadius::same(theme::TILE_RADIUS);
        p.rect_filled(rect, radius, SURFACE);
        if let Some((tex, (w, h))) = info.texture {
            let (x, y, fw, fh) = layout::fit(rect.width(), rect.height(), w as f32 / h as f32);
            let r = Rect::from_min_size(rect.min + vec2(x, y), vec2(fw, fh));
            egui::Image::new((tex, r.size()))
                .corner_radius(radius)
                .paint_at(ui, r);
        }
        if let Some(msg) = info.message {
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                msg,
                theme::regular(12.5),
                TEXT_3,
            );
        }
        if hovered && self.replay.is_none() {
            p.rect_stroke(rect, radius, Stroke::new(1.5, ACCENT), StrokeKind::Inside);
        }
        // Name chip, top left.
        let name_galley = p.layout_no_wrap(info.camera, theme::semibold(13.5), TEXT);
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
        let right = if let Some(dot) = info.dot {
            p.circle_filled(pos2(rect.right() - 16.0, rect.top() + 16.0), 4.0, dot);
            rect.right() - 30.0
        } else {
            rect.right() - 10.0
        };
        // Stats, top right (cameras print their own text at the bottom).
        if let Some(s) = info.stats {
            let g = p.layout_no_wrap(s, theme::mono(11.0), TEXT_2);
            let size = g.size() + vec2(12.0, 6.0);
            let r =
                Rect::from_min_size(pos2(right - size.x, rect.top() + 16.0 - size.y / 2.0), size);
            p.rect_filled(
                r,
                CornerRadius::same(theme::PILL_RADIUS),
                BG.gamma_multiply(0.85),
            );
            p.galley(r.min + vec2(6.0, 3.0), g, TEXT_2);
        }
    }

    /// Play/pause, ±10 s, the time, and a bar of the last hour with the
    /// recorded spans and the playhead. Click the bar to seek.
    fn timeline(&mut self, ui: &mut egui::Ui) {
        let Some((pos, playing)) = self.replay.as_ref().map(|r| (r.position_ms(), r.playing()))
        else {
            return;
        };
        let area = ui.max_rect();
        let p = ui.painter().clone();
        p.hline(area.x_range(), area.top() + 0.5, Stroke::new(1.0, LINE));
        let cy = area.center().y;
        let id = ui.scope_id();

        // Play / pause.
        let play = Rect::from_center_size(pos2(area.left() + 32.0, cy), vec2(32.0, 32.0));
        let resp = ui.interact(play, id.with("play"), Sense::click());
        p.circle_filled(play.center(), 16.0, SURFACE);
        p.circle_stroke(
            play.center(),
            16.0,
            Stroke::new(1.0, if resp.hovered() { ACCENT } else { LINE }),
        );
        if playing {
            for dx in [-4.0, 4.0] {
                p.rect_filled(
                    Rect::from_center_size(play.center() + vec2(dx, 0.0), vec2(3.0, 12.0)),
                    CornerRadius::same(1),
                    TEXT,
                );
            }
        } else {
            let c = play.center();
            p.add(egui::Shape::convex_polygon(
                vec![
                    c + vec2(-4.0, -7.0),
                    c + vec2(7.0, 0.0),
                    c + vec2(-4.0, 7.0),
                ],
                TEXT,
                Stroke::NONE,
            ));
        }
        let toggle = resp.clicked() || ui.input(|i| i.key_pressed(egui::Key::Space));

        // ±10 s.
        let mut seek_to = None;
        let mut x = play.right() + 12.0;
        for (label, d) in [("−10 s", -10_000.0), ("+10 s", 10_000.0)] {
            let b = Rect::from_min_size(pos2(x, cy - 13.0), vec2(56.0, 26.0));
            let resp = ui.interact(b, id.with(label), Sense::click());
            p.rect_filled(b, CornerRadius::same(theme::PILL_RADIUS), SURFACE);
            p.rect_stroke(
                b,
                CornerRadius::same(theme::PILL_RADIUS),
                Stroke::new(1.0, if resp.hovered() { ACCENT } else { LINE }),
                StrokeKind::Inside,
            );
            p.text(
                b.center(),
                Align2::CENTER_CENTER,
                label,
                theme::mono(11.0),
                TEXT_2,
            );
            if resp.clicked() {
                seek_to = Some(pos + d);
            }
            x = b.right() + 6.0;
        }
        ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowLeft) {
                seek_to = Some(pos - 10_000.0);
            }
            if i.key_pressed(egui::Key::ArrowRight) {
                seek_to = Some(pos + 10_000.0);
            }
        });

        // Time of the playhead, local.
        let clock = local_time(pos);
        let g = p.text(
            pos2(x + 10.0, cy - 7.0),
            Align2::LEFT_CENTER,
            clock.0,
            theme::mono(13.5),
            TEXT,
        );
        p.text(
            pos2(x + 10.0, cy + 9.0),
            Align2::LEFT_CENTER,
            clock.1,
            theme::mono(11.0),
            TEXT_3,
        );

        // The bar.
        let now = now_us() as f64 / 1000.0;
        let (t0, t1) = (now - TIMELINE_MS, now);
        let bar = Rect::from_min_max(
            pos2(g.right().max(x + 90.0) + 20.0, cy - 14.0),
            pos2(area.right() - 20.0, cy + 14.0),
        );
        self.timeline_rect_cache = Some([
            f64::from(bar.min.x),
            f64::from(bar.min.y),
            f64::from(bar.width()),
            f64::from(bar.height()),
            t0,
            t1,
        ]);
        let to_x =
            |t: f64| bar.left() + ((t - t0) / (t1 - t0)).clamp(0.0, 1.0) as f32 * bar.width();
        let track = Rect::from_min_max(pos2(bar.left(), cy - 3.0), pos2(bar.right(), cy + 3.0));
        p.rect_filled(track, CornerRadius::same(3), SURFACE);
        if now_ms() - self.spans.borrow().fetched_ms > 30_000.0 {
            self.fetch_spans(ui.ctx());
        }
        for &(a, b) in &self.spans.borrow().spans {
            if b < t0 || a > t1 {
                continue;
            }
            let s = Rect::from_min_max(
                pos2(to_x(a), cy - 3.0),
                pos2(to_x(b).max(to_x(a) + 1.0), cy + 3.0),
            );
            p.rect_filled(s, CornerRadius::same(3), TEXT_3);
        }
        // Hour labels at the ends.
        p.text(
            pos2(bar.left(), bar.bottom() + 2.0),
            Align2::LEFT_TOP,
            local_time(t0).0,
            theme::mono(11.0),
            TEXT_3,
        );
        p.text(
            pos2(bar.right(), bar.bottom() + 2.0),
            Align2::RIGHT_TOP,
            "now",
            theme::mono(11.0),
            TEXT_3,
        );
        // Playhead.
        let px = to_x(pos);
        p.vline(px, bar.y_range(), Stroke::new(1.5, ACCENT));
        p.circle_filled(pos2(px, cy), 4.0, ACCENT);
        // Hover hint and click to seek.
        let resp = ui.interact(bar, id.with("bar"), Sense::click());
        self.hover_ms = resp
            .hover_pos()
            .map(|h| t0 + f64::from((h.x - bar.left()) / bar.width()) * (t1 - t0));
        if let Some(h) = self.hover_ms {
            let hx = to_x(h);
            p.vline(hx, bar.y_range(), Stroke::new(1.0, TEXT_2));
            p.text(
                pos2(hx, bar.top() - 2.0),
                Align2::CENTER_BOTTOM,
                local_time(h).0,
                theme::mono(11.0),
                TEXT_2,
            );
            if resp.clicked() {
                seek_to = Some(h);
            }
        }
        if let Some(r) = &mut self.replay {
            if toggle {
                r.set_playing(!playing);
            }
            if let Some(t) = seek_to {
                r.seek(t.min(now));
            }
        }
    }
}

/// ("HH:MM:SS", "YYYY-MM-DD") in the viewer's local time zone.
fn local_time(ms: f64) -> (String, String) {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms));
    (
        format!(
            "{:02}:{:02}:{:02}",
            d.get_hours(),
            d.get_minutes(),
            d.get_seconds()
        ),
        format!(
            "{:04}-{:02}-{:02}",
            d.get_full_year(),
            d.get_month() + 1,
            d.get_date()
        ),
    )
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
        if let Some(r) = &mut self.replay {
            r.update(&rs);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.set_focus(None);
        }
        self.publish_stats();

        egui::Panel::top("bar")
            .exact_size(44.0)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(BG))
            .show(ui, |ui| self.top_bar(ui, &rs));
        if self.replay.is_some() {
            egui::Panel::bottom("timeline")
                .exact_size(TIMELINE_H)
                .show_separator_line(false)
                .frame(egui::Frame::NONE.fill(BG))
                .show(ui, |ui| self.timeline(ui));
        }
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
                let n = if self.replay.is_some() {
                    self.camera_names().len()
                } else {
                    self.tiles.len()
                };
                let mut rects = Vec::new();
                match self.focus.filter(|&i| i < n) {
                    Some(i) => {
                        self.draw_tile(ui, i, area);
                        rects.push((i, area));
                    }
                    None => {
                        let aspect = (0..n)
                            .find_map(|i| {
                                self.tile_info(i)
                                    .texture
                                    .map(|(_, (w, h))| w as f32 / h as f32)
                            })
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
                // Live: double-click a tile for its main stream, full
                // window; again (or Escape) to go back to the grid.
                let dbl = ui.input(|i| {
                    i.pointer
                        .button_double_clicked(egui::PointerButton::Primary)
                        .then(|| i.pointer.latest_pos())
                        .flatten()
                });
                if let Some(pos) = dbl {
                    if let Some(&(i, _)) = rects.iter().find(|(_, r)| r.contains(pos)) {
                        self.set_focus(if self.focus == Some(i) { None } else { Some(i) });
                    }
                }
                self.tile_rects = rects;
            });
        // Keep the stats readout fresh even without new frames.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}
