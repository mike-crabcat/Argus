//! Event search: a query box and a results panel with each event's still,
//! summary, camera and time. Clicking a result replays it. With a model set
//! up, "Ask" lets it read the question (times, places) first.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use eframe::egui::{
    self, Align2, CornerRadius, Rect, Sense, Stroke, StrokeKind, TextureHandle, pos2, vec2,
};

use super::theme::{self, ALERT, BG, LINE, SURFACE, TEXT, TEXT_2, TEXT_3};
use super::{fetch_image, send};

thread_local! {
    /// Result rows drawn this frame, (event id, [x, y, w, h]), for scripts.
    static ROWS: RefCell<Vec<(i64, [f32; 4])>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct Hit {
    pub id: i64,
    pub camera: String,
    pub start_ms: i64,
    pub label: String,
    pub summary: Option<String>,
    #[serde(default)]
    pub notable: Option<bool>,
    #[serde(default)]
    pub stills: Vec<String>,
}

#[derive(Default)]
struct Shared {
    results: Vec<Hit>,
    /// How the server read the query, when the model rewrote it.
    reading: Option<String>,
    busy: bool,
    error: Option<String>,
    thumbs: HashMap<String, TextureHandle>,
    agent: Option<bool>,
}

pub enum SearchAction {
    Replay { camera: String, t_ms: f64 },
}

#[derive(Default)]
pub struct Search {
    pub open: bool,
    pub query: String,
    pub smart: bool,
    shared: Rc<RefCell<Shared>>,
    loading: HashSet<String>,
    focus: bool,
    asked_agent: bool,
}

/// ("13:05", "Tue 18 Nov") in local time.
fn when(ms: i64) -> (String, String) {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    (
        format!("{:02}:{:02}", d.get_hours(), d.get_minutes()),
        format!(
            "{} {} {}",
            DAYS[d.get_day() as usize % 7],
            d.get_date(),
            MONTHS[d.get_month() as usize % 12]
        ),
    )
}

impl Search {
    /// Opens the panel with the cursor in its box.
    pub fn show(&mut self) {
        self.open = true;
        self.focus = true;
    }

    pub fn rows() -> Vec<(i64, [f32; 4])> {
        ROWS.with(|r| r.borrow().clone())
    }

    pub fn stats(&self) -> serde_json::Value {
        let s = self.shared.borrow();
        serde_json::json!({
            "open": self.open,
            "query": self.query,
            "busy": s.busy,
            "error": s.error,
            "reading": s.reading,
            "results": s.results.len(),
            "thumbs": s.thumbs.len(),
            "first": s.results.first().map(|h| serde_json::json!({
                "id": h.id, "camera": h.camera, "summary": h.summary, "stills": h.stills.len(),
            })),
            "rows": Self::rows(),
        })
    }

    pub fn run(&mut self, token: Option<String>, ctx: &egui::Context) {
        let q = self.query.trim().to_owned();
        self.open = true;
        {
            let mut s = self.shared.borrow_mut();
            s.busy = true;
            s.error = None;
        }
        let smart = self.smart && self.shared.borrow().agent == Some(true);
        let path = format!(
            "/api/events/search?limit=60&q={}{}",
            js_sys::encode_uri_component(&q),
            if smart { "&smart=1" } else { "" }
        );
        let (shared, ctx) = (self.shared.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let r = send("GET", &path, None, None, token.as_deref()).await;
            let mut s = shared.borrow_mut();
            s.busy = false;
            match r {
                Ok((200, body)) => {
                    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                    s.results = serde_json::from_value(v["results"].clone()).unwrap_or_default();
                    s.reading = (v["query"]["rewritten"] == true).then(|| {
                        let mut parts =
                            vec![format!("\"{}\"", v["query"]["text"].as_str().unwrap_or(""))];
                        if let Some(c) = v["query"]["cameras"].as_array().filter(|c| !c.is_empty())
                        {
                            parts.push(
                                c.iter()
                                    .filter_map(|x| x.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            );
                        }
                        if let Some(f) = v["query"]["from_ms"].as_i64() {
                            let (t, d) = when(f);
                            parts.push(format!("from {d} {t}"));
                        }
                        format!("Searched {}", parts.join(" · "))
                    });
                }
                Ok((code, _)) => s.error = Some(format!("Search failed (HTTP {code}).")),
                Err(e) => s.error = Some(e),
            }
            ctx.request_repaint();
        });
    }

    fn check_agent(&mut self, token: Option<String>, ctx: &egui::Context) {
        if self.asked_agent {
            return;
        }
        self.asked_agent = true;
        let (shared, ctx) = (self.shared.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok((200, body)) =
                send("GET", "/api/agent/status", None, None, token.as_deref()).await
            {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                shared.borrow_mut().agent = Some(v["configured"] == true);
                ctx.request_repaint();
            }
        });
    }

    fn thumb(
        &mut self,
        key: &str,
        token: Option<String>,
        ctx: &egui::Context,
    ) -> Option<TextureHandle> {
        if let Some(t) = self.shared.borrow().thumbs.get(key) {
            return Some(t.clone());
        }
        if self.loading.insert(key.to_owned()) {
            let (shared, ctx, key) = (self.shared.clone(), ctx.clone(), key.to_owned());
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok((size, _, rgba)) =
                    fetch_image(&format!("/api/media/{key}"), token.as_deref(), 360).await
                {
                    let img = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                    let tex = ctx.load_texture(format!("still-{key}"), img, Default::default());
                    shared.borrow_mut().thumbs.insert(key, tex);
                    ctx.request_repaint();
                }
            });
        }
        None
    }

    /// The results panel, over the right of `area`.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        area: Rect,
        token: Option<String>,
    ) -> Option<SearchAction> {
        ROWS.with(|r| r.borrow_mut().clear());
        if !self.open {
            return None;
        }
        let ctx = ui.ctx().clone();
        self.check_agent(token.clone(), &ctx);
        let w = area.width().min(460.0);
        let panel = Rect::from_min_max(pos2(area.right() - w, area.top()), area.max);
        let p = ui.painter().clone();
        p.rect_filled(panel, CornerRadius::ZERO, BG);
        p.vline(panel.left() + 0.5, panel.y_range(), Stroke::new(1.0, LINE));
        // Clicks inside the panel stay in it.
        let _ = ui.interact(panel, ui.scope_id().with("search-bg"), Sense::click());
        let mut action = None;
        let inner = panel.shrink2(vec2(14.0, 12.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            ui.horizontal(|ui| {
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Search events: parcel, van, back gate…")
                        .desired_width(ui.available_width() - 64.0)
                        .margin(vec2(12.0, 7.0)),
                );
                if std::mem::take(&mut self.focus) {
                    edit.request_focus();
                }
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.run(token.clone(), &ctx);
                }
                if ui.button("Close").clicked() {
                    self.open = false;
                }
            });
            if self.shared.borrow().agent == Some(true) {
                ui.checkbox(
                    &mut self.smart,
                    "Let the agent read the question (times, places)",
                );
            }
            let (busy, error, reading, n) = {
                let s = self.shared.borrow();
                (s.busy, s.error.clone(), s.reading.clone(), s.results.len())
            };
            if busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new("Searching…").color(TEXT_2));
                });
            } else if let Some(e) = error {
                ui.label(egui::RichText::new(e).color(ALERT));
            } else {
                let mut line = format!("{n} event{}", if n == 1 { "" } else { "s" });
                if let Some(r) = reading {
                    line = format!("{line} · {r}");
                }
                ui.label(
                    egui::RichText::new(line)
                        .font(theme::regular(12.0))
                        .color(TEXT_3),
                );
            }
            let results = self.shared.borrow().results.clone();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for h in &results {
                        let (row, resp) = ui
                            .allocate_exact_size(vec2(ui.available_width(), 84.0), Sense::click());
                        ROWS.with(|r| {
                            r.borrow_mut()
                                .push((h.id, [row.min.x, row.min.y, row.width(), row.height()]))
                        });
                        let p = ui.painter();
                        if resp.hovered() {
                            p.rect_filled(row, CornerRadius::same(8), SURFACE);
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        let thumb =
                            Rect::from_min_size(row.min + vec2(6.0, 6.0), vec2(128.0, 72.0));
                        p.rect_filled(thumb, CornerRadius::same(6), SURFACE);
                        if let Some(key) = h.stills.first() {
                            if let Some(t) = self.thumb(key, token.clone(), &ctx) {
                                let [tw, th] = t.size();
                                let (fx, fy, fw, fh) = crate::layout::fit(
                                    thumb.width(),
                                    thumb.height(),
                                    tw as f32 / th as f32,
                                );
                                let r = Rect::from_min_size(thumb.min + vec2(fx, fy), vec2(fw, fh));
                                egui::Image::new((t.id(), r.size()))
                                    .corner_radius(CornerRadius::same(6))
                                    .paint_at(ui, r);
                            }
                        } else {
                            p.text(
                                thumb.center(),
                                Align2::CENTER_CENTER,
                                "no still",
                                theme::regular(11.0),
                                TEXT_3,
                            );
                        }
                        let x = thumb.right() + 12.0;
                        let (t, d) = when(h.start_ms);
                        let head = format!("{} · {d} {t} · {}", h.camera, h.label);
                        p.text(
                            pos2(x, row.top() + 14.0),
                            Align2::LEFT_CENTER,
                            head,
                            theme::mono(11.0),
                            TEXT_2,
                        );
                        if h.notable == Some(true) {
                            let tag = Rect::from_min_size(
                                pos2(row.right() - 68.0, row.top() + 6.0),
                                vec2(62.0, 18.0),
                            );
                            p.rect_stroke(
                                tag,
                                CornerRadius::same(9),
                                Stroke::new(1.0, ALERT),
                                StrokeKind::Inside,
                            );
                            p.text(
                                tag.center(),
                                Align2::CENTER_CENTER,
                                "notable",
                                theme::regular(11.0),
                                ALERT,
                            );
                        }
                        let text = h
                            .summary
                            .clone()
                            .unwrap_or_else(|| format!("{} detected (no summary)", h.label));
                        let col = if h.summary.is_some() { TEXT } else { TEXT_3 };
                        let g = p.layout(text, theme::regular(13.0), col, row.right() - x - 8.0);
                        p.galley(pos2(x, row.top() + 26.0), g, col);
                        if resp.clicked() {
                            action = Some(SearchAction::Replay {
                                camera: h.camera.clone(),
                                t_ms: h.start_ms as f64,
                            });
                        }
                    }
                });
        });
        action
    }
}
