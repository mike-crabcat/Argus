//! Phones: the admin adds a phone by name and gets a QR code holding a
//! sign-in link with that phone's own token; the list shows each phone with
//! a Revoke button. Only the main (admin) token sees any of this.
//!
//! The new token lives only in the QR shown here. Done (or closing the
//! panel) drops it; the server keeps only its hash.

use std::cell::RefCell;
use std::rc::Rc;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2,
};

use super::theme::{self, ACCENT, ALERT, BG, LINE, SURFACE, TEXT, TEXT_2, TEXT_3};
use super::{UNAUTHORIZED, get_text, origin, request};
use crate::{Device, DeviceList, ServerInfo, ago, is_local_url, pairing_url, qr_modules};

const WIDTH: f32 = 400.0;
const QR_SIZE: f32 = 288.0;

thread_local! {
    /// Buttons drawn this frame, (label, [x, y, w, h]), for scripts.
    static BUTTONS: RefCell<Vec<(String, [f32; 4])>> = const { RefCell::new(Vec::new()) };
}

/// The panel's buttons as drawn last frame.
pub fn buttons() -> Vec<(String, [f32; 4])> {
    BUTTONS.with(|b| b.borrow().clone())
}

/// A QR code on screen: who it's for and the link it holds.
struct Pairing {
    name: String,
    url: String,
    modules: (usize, Vec<bool>),
}

#[derive(Default)]
struct State {
    info: Option<ServerInfo>,
    devices: Vec<Device>,
    pairing: Option<Pairing>,
    error: Option<String>,
    busy: bool,
}

#[derive(Default)]
pub struct Phones {
    state: Rc<RefCell<State>>,
    pub open: bool,
    name_input: String,
    /// Where the QR code was drawn last frame (CSS pixels), for scripts.
    pub qr_rect: Option<Rect>,
    /// Put the cursor in the name field on the next frame.
    focus_name: bool,
}

impl Phones {
    /// Asks the server who we are; call after signing in.
    pub fn load(&mut self, token: Option<String>, ctx: &egui::Context) {
        *self.state.borrow_mut() = State::default();
        self.open = false;
        let (st, ctx) = (self.state.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(body) = get_text("/api/info", token.as_deref()).await {
                st.borrow_mut().info = serde_json::from_str(&body).ok();
                ctx.request_repaint();
            }
        });
    }

    pub fn forget(&mut self) {
        *self.state.borrow_mut() = State::default();
        self.open = false;
        self.name_input.clear();
    }

    pub fn is_admin(&self) -> bool {
        self.state
            .borrow()
            .info
            .as_ref()
            .is_some_and(|i| i.role == "admin")
    }

    pub fn count(&self) -> usize {
        self.state.borrow().devices.len()
    }

    pub fn showing_qr(&self) -> bool {
        self.state.borrow().pairing.is_some()
    }

    pub fn role(&self) -> Option<String> {
        self.state.borrow().info.as_ref().map(|i| i.role.clone())
    }

    /// The address phones will open, and whether it only works here.
    fn base(&self) -> (String, bool) {
        let public = self
            .state
            .borrow()
            .info
            .as_ref()
            .and_then(|i| i.public_url.clone());
        let base = public.unwrap_or_else(origin);
        let local = is_local_url(&base);
        (base, local)
    }

    pub fn show(&mut self, token: Option<String>, ctx: &egui::Context) {
        self.open = true;
        self.focus_name = true;
        self.refresh(token, ctx);
    }

    fn refresh(&self, token: Option<String>, ctx: &egui::Context) {
        let (st, ctx) = (self.state.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let r = get_text("/api/devices", token.as_deref()).await;
            let mut s = st.borrow_mut();
            match r.and_then(|b| serde_json::from_str::<DeviceList>(&b).map_err(|e| e.to_string()))
            {
                Ok(list) => s.devices = list.devices,
                Err(e) => s.error = Some(describe(e)),
            }
            ctx.request_repaint();
        });
    }

    fn create(&mut self, token: Option<String>, ctx: &egui::Context) {
        let name = self.name_input.trim().to_owned();
        if name.is_empty() || self.state.borrow().busy {
            return;
        }
        {
            let mut s = self.state.borrow_mut();
            s.busy = true;
            s.error = None;
        }
        let (base, _) = self.base();
        let (st, ctx2) = (self.state.clone(), ctx.clone());
        let body = serde_json::json!({ "name": name }).to_string();
        let this_token = token.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let r = request("POST", "/api/devices", Some(&body), this_token.as_deref()).await;
            let mut s = st.borrow_mut();
            s.busy = false;
            match r.and_then(|b| {
                serde_json::from_str::<serde_json::Value>(&b).map_err(|e| e.to_string())
            }) {
                Ok(v) => {
                    let device: Option<Device> = serde_json::from_value(v["device"].clone()).ok();
                    let secret = v["token"].as_str().unwrap_or_default();
                    let url = pairing_url(Some(&base), "", secret);
                    match (device, qr_modules(&url)) {
                        (Some(d), Some(modules)) => {
                            s.pairing = Some(Pairing {
                                name: d.name.clone(),
                                url,
                                modules,
                            });
                            s.devices.push(d);
                        }
                        _ => s.error = Some("The server's answer made no sense.".into()),
                    }
                }
                Err(e) => s.error = Some(describe(e)),
            }
            ctx2.request_repaint();
        });
        self.name_input.clear();
    }

    fn revoke(&self, id: String, token: Option<String>, ctx: &egui::Context) {
        let (st, ctx) = (self.state.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let path = format!("/api/devices/{id}");
            let r = request("DELETE", &path, None, token.as_deref()).await;
            let mut s = st.borrow_mut();
            match r {
                Ok(_) => s.devices.retain(|d| d.id != id),
                Err(e) => s.error = Some(describe(e)),
            }
            ctx.request_repaint();
        });
    }

    /// Hides the QR code for good: the token is gone from this page.
    fn done(&mut self) {
        self.state.borrow_mut().pairing = None;
    }

    /// The panel, over everything else, while open.
    pub fn ui(&mut self, ctx: &egui::Context, token: Option<String>, now_ms: f64) {
        self.qr_rect = None;
        BUTTONS.with(|b| b.borrow_mut().clear());
        if !self.open {
            return;
        }
        let modal = egui::Modal::new(egui::Id::unique("argus-phones"))
            .backdrop_color(Color32::from_black_alpha(170))
            .frame(
                egui::Frame::NONE
                    .fill(SURFACE)
                    .stroke(Stroke::new(1.0, LINE))
                    .corner_radius(CornerRadius::same(theme::TILE_RADIUS))
                    .inner_margin(24.0),
            )
            .show(ctx, |ui| {
                ui.set_width(WIDTH.min(ctx.content_rect().width() - 80.0));
                ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                if self.state.borrow().pairing.is_some() {
                    self.qr_view(ui);
                } else {
                    self.list_view(ui, token, now_ms);
                }
            });
        if modal.should_close() {
            self.done();
            self.open = false;
        }
    }

    fn qr_view(&mut self, ui: &mut egui::Ui) {
        let (name, url, (w, modules)) = {
            let s = self.state.borrow();
            let p = s.pairing.as_ref().unwrap();
            (p.name.clone(), p.url.clone(), p.modules.clone())
        };
        heading(ui, "Scan with your phone camera");
        ui.label(
            egui::RichText::new(format!("Opens Argus on {name}, signed in."))
                .font(theme::regular(12.5))
                .color(TEXT_2),
        );
        ui.add_space(8.0);
        let full = ui.available_width();
        let (row, _) = ui.allocate_exact_size(vec2(full, QR_SIZE.min(full)), Sense::hover());
        let rect = Rect::from_center_size(row.center(), Vec2::splat(row.height()));
        draw_qr(ui, rect, w, &modules);
        self.qr_rect = Some(rect);
        ui.add_space(4.0);
        let host = url
            .split_once("://")
            .map_or(url.as_str(), |(_, r)| r)
            .split(['/', '?'])
            .next()
            .unwrap_or("")
            .to_owned();
        let local = is_local_url(&url);
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new(host)
                    .font(theme::mono(11.5))
                    .color(TEXT_3),
            );
        });
        if local {
            warning(ui);
        }
        ui.add_space(8.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if pill(ui, "Done", Kind::Primary, true).clicked() {
                self.done();
            }
        });
    }

    fn list_view(&mut self, ui: &mut egui::Ui, token: Option<String>, now_ms: f64) {
        ui.horizontal(|ui| {
            heading(ui, "Phones");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if pill(ui, "Close", Kind::Plain, true).clicked() {
                    self.open = false;
                }
            });
        });
        ui.label(
            egui::RichText::new(
                "Each phone signs in with its own token. Revoke one to sign it out.",
            )
            .font(theme::regular(12.5))
            .color(TEXT_2),
        );
        ui.add_space(6.0);
        let devices = self.state.borrow().devices.clone();
        if devices.is_empty() {
            ui.label(
                egui::RichText::new("No phones yet.")
                    .font(theme::regular(13.5))
                    .color(TEXT_3),
            );
        }
        for d in &devices {
            separator(ui);
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        egui::RichText::new(&d.name)
                            .font(theme::regular(14.0))
                            .color(TEXT),
                    );
                    let used = match d.last_used_ms {
                        Some(t) => format!("used {}", ago(now_ms, t as f64)),
                        None => "not used yet".into(),
                    };
                    ui.label(
                        egui::RichText::new(format!(
                            "added {} · {used}",
                            ago(now_ms, d.created_ms as f64)
                        ))
                        .font(theme::regular(12.0))
                        .color(TEXT_3),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if pill(ui, "Revoke", Kind::Danger, true).clicked() {
                        self.revoke(d.id.clone(), token.clone(), ui.ctx());
                    }
                });
            });
        }
        separator(ui);
        ui.add_space(4.0);
        let busy = self.state.borrow().busy;
        let mut add = false;
        ui.horizontal(|ui| {
            let button_w = 104.0;
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.name_input)
                    .hint_text("Phone name, e.g. Mike's iPhone")
                    .char_limit(60)
                    .font(theme::regular(13.5))
                    .margin(vec2(14.0, 8.0))
                    .desired_width(ui.available_width() - button_w - 8.0),
            );
            if std::mem::take(&mut self.focus_name) {
                edit.request_focus();
            }
            let ready = !self.name_input.trim().is_empty() && !busy;
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if pill(ui, "Add phone", Kind::Primary, ready).clicked() || (ready && enter) {
                add = true;
            }
        });
        if add {
            self.create(token, ui.ctx());
        }
        if self.base().1 {
            warning(ui);
        }
        if let Some(e) = self.state.borrow().error.clone() {
            ui.label(
                egui::RichText::new(e)
                    .font(theme::regular(12.5))
                    .color(ALERT),
            );
        }
    }
}

fn describe(e: String) -> String {
    if e == UNAUTHORIZED {
        "The server refused this token.".into()
    } else {
        e
    }
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .font(theme::semibold(17.0))
            .color(TEXT),
    );
}

fn separator(ui: &mut egui::Ui) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter()
        .hline(r.x_range(), r.center().y, Stroke::new(1.0, LINE));
}

fn warning(ui: &mut egui::Ui) {
    ui.label(
        egui::RichText::new(
            "This page is open at a local address, so the QR code only works on this \
             computer. Set [http] public_url, or open Argus at its network address.",
        )
        .font(theme::regular(12.5))
        .color(ALERT),
    );
}

#[derive(PartialEq)]
enum Kind {
    Primary,
    Plain,
    Danger,
}

/// A pill button in the app's style.
fn pill(ui: &mut egui::Ui, label: &str, kind: Kind, enabled: bool) -> egui::Response {
    let g = ui
        .painter()
        .layout_no_wrap(label.into(), theme::regular(13.0), TEXT);
    let size = vec2(g.size().x + 32.0, 32.0);
    let (rect, resp) = ui.allocate_exact_size(
        size,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    BUTTONS.with(|b| {
        b.borrow_mut().push((
            label.to_owned(),
            [rect.min.x, rect.min.y, rect.width(), rect.height()],
        ))
    });
    let hot = enabled && resp.hovered();
    let (fill, border, text) = match (kind, enabled, hot) {
        (_, false, _) => (SURFACE, LINE, TEXT_3),
        (Kind::Primary, true, true) => (ACCENT, ACCENT, BG),
        (Kind::Primary, true, false) => (SURFACE, ACCENT, TEXT),
        (Kind::Danger, true, true) => (SURFACE, ALERT, ALERT),
        (Kind::Danger, true, false) => (SURFACE, LINE, TEXT_2),
        (Kind::Plain, true, h) => (SURFACE, LINE, if h { TEXT } else { TEXT_2 }),
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(theme::PILL_RADIUS), fill);
    p.rect_stroke(
        rect,
        CornerRadius::same(theme::PILL_RADIUS),
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        theme::regular(13.0),
        text,
    );
    if hot {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// Black modules on white with a four-module quiet zone, snapped to whole
/// physical pixels so cameras see crisp edges.
fn draw_qr(ui: &egui::Ui, rect: Rect, w: usize, modules: &[bool]) {
    let ppp = ui.ctx().pixels_per_point();
    let cells = (w + 8) as f32;
    let module_px = ((rect.width() * ppp) / cells).floor().max(1.0);
    let side = module_px * cells / ppp;
    let min = pos2(
        ((rect.center().x - side / 2.0) * ppp).round() / ppp,
        ((rect.center().y - side / 2.0) * ppp).round() / ppp,
    );
    let m = module_px / ppp;
    let p = ui.painter();
    p.rect_filled(
        Rect::from_min_size(min, Vec2::splat(side)),
        CornerRadius::same(6),
        Color32::WHITE,
    );
    for y in 0..w {
        let mut x = 0;
        while x < w {
            if !modules[y * w + x] {
                x += 1;
                continue;
            }
            let start = x;
            while x < w && modules[y * w + x] {
                x += 1;
            }
            p.rect_filled(
                Rect::from_min_size(
                    pos2(min.x + (start + 4) as f32 * m, min.y + (y + 4) as f32 * m),
                    vec2((x - start) as f32 * m, m),
                ),
                CornerRadius::ZERO,
                Color32::BLACK,
            );
        }
    }
}
