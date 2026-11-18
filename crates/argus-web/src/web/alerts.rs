//! The Alerts panel: this device's notifications, the daily brief, alert
//! rules (the main token adds, edits, tests and deletes them; phones see
//! them) and recent alerts.

use std::cell::RefCell;
use std::rc::Rc;

use eframe::egui::{self, CornerRadius, Rect, Sense, Stroke, pos2, vec2};
use serde_json::{Value, json};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use super::send;
use super::theme::{self, ACCENT, ALERT, BG, LINE, TEXT, TEXT_2, TEXT_3};

thread_local! {
    /// Buttons drawn this frame, for scripts.
    static BUTTONS: RefCell<Vec<(String, [f32; 4])>> = const { RefCell::new(Vec::new()) };
}

fn btn(ui: &mut egui::Ui, label: &str, key: &str) -> egui::Response {
    let r = ui.button(label);
    BUTTONS.with(|b| {
        b.borrow_mut().push((
            key.to_owned(),
            [r.rect.min.x, r.rect.min.y, r.rect.width(), r.rect.height()],
        ))
    });
    r
}

#[derive(Default)]
struct Shared {
    rules: Vec<Value>,
    log: Vec<Value>,
    brief: Value,
    push: Option<String>,
    busy: Option<String>,
    error: Option<String>,
    /// Rule id -> its test result.
    tests: std::collections::HashMap<String, Value>,
}

pub enum AlertAction {
    Replay { camera: String, t_ms: f64 },
}

/// A rule's filter being edited, as text fields.
#[derive(Clone, Default)]
struct Edit {
    id: String,
    cameras: String,
    labels: String,
    from: String,
    to: String,
    days: String,
    check: String,
    cooldown_min: f64,
}

#[derive(Default)]
pub struct Alerts {
    pub open: bool,
    shared: Rc<RefCell<Shared>>,
    new_rule: String,
    edit: Option<Edit>,
    loaded: bool,
}

/// A request from the panel.
struct Req {
    method: &'static str,
    path: String,
    body: Option<Value>,
    busy: &'static str,
    /// Store the answer as this rule's test result.
    then_test: Option<String>,
}

impl Req {
    fn new(
        method: &'static str,
        path: impl Into<String>,
        body: Option<Value>,
        busy: &'static str,
    ) -> Self {
        Req {
            method,
            path: path.into(),
            body,
            busy,
            then_test: None,
        }
    }
}

fn list(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|x| x.to_string().trim_matches('"').to_owned())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn split(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_owned())
        .filter(|x| !x.is_empty())
        .collect()
}

/// Calls `window.argusPush[method](token)`; returns its string result.
async fn push_call(method: &str, token: Option<String>) -> Result<String, String> {
    let w = web_sys::window().ok_or("no window")?;
    let p = js_sys::Reflect::get(&w, &"argusPush".into()).map_err(|_| "push.js didn't load")?;
    let f: js_sys::Function = js_sys::Reflect::get(&p, &method.into())
        .map_err(|_| "no such push call")?
        .dyn_into()
        .map_err(|_| "not a function")?;
    let promise: js_sys::Promise = f
        .call1(&p, &token.unwrap_or_default().into())
        .map_err(|e| format!("{e:?}"))?
        .dyn_into()
        .map_err(|_| "not a promise")?;
    let v = JsFuture::from(promise)
        .await
        .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
    Ok(v.as_string().unwrap_or_default())
}

fn when(ms: f64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms));
    format!("{:02}:{:02}", d.get_hours(), d.get_minutes())
}

impl Alerts {
    /// Closed, or open (from the brief's notification).
    pub fn opened(open: bool) -> Self {
        Alerts {
            open,
            ..Default::default()
        }
    }

    pub fn buttons() -> Vec<(String, [f32; 4])> {
        BUTTONS.with(|b| b.borrow().clone())
    }

    pub fn stats(&self) -> Value {
        let s = self.shared.borrow();
        json!({
            "open": self.open,
            "push": s.push,
            "busy": s.busy,
            "error": s.error,
            "rules": s.rules.iter().map(|r| json!({"id": r["id"], "text": r["text"], "describe": r["describe"], "enabled": r["enabled"]})).collect::<Vec<_>>(),
            "log": s.log.len(),
            "brief": s.brief["latest"]["text"],
            "tests": s.tests,
            "buttons": Self::buttons(),
        })
    }

    pub fn show(&mut self, token: Option<String>, ctx: &egui::Context) {
        self.open = true;
        self.refresh(token, ctx);
    }

    fn refresh(&mut self, token: Option<String>, ctx: &egui::Context) {
        self.loaded = true;
        let (sh, ctx) = (self.shared.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let get = |p: &'static str| {
                let t = token.clone();
                async move {
                    match send("GET", p, None, None, t.as_deref()).await {
                        Ok((200, b)) => serde_json::from_str::<Value>(&b).unwrap_or_default(),
                        _ => Value::Null,
                    }
                }
            };
            let rules = get("/api/alerts/rules").await;
            let log = get("/api/alerts/log").await;
            let brief = get("/api/brief").await;
            let push = push_call("state", None).await.ok();
            let mut s = sh.borrow_mut();
            s.rules = rules["rules"].as_array().cloned().unwrap_or_default();
            s.log = log["log"].as_array().cloned().unwrap_or_default();
            s.brief = brief;
            s.push = push;
            ctx.request_repaint();
        });
    }

    fn call(&self, token: Option<String>, ctx: &egui::Context, req: Req) {
        let Req {
            method,
            path,
            body,
            busy,
            then_test,
        } = req;
        self.shared.borrow_mut().busy = Some(busy.into());
        let (sh, ctx) = (self.shared.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let b = body.map(|b| wasm_bindgen::JsValue::from_str(&b.to_string()));
            let r = send(
                method,
                &path,
                b.as_ref(),
                Some("application/json"),
                token.as_deref(),
            )
            .await;
            {
                let mut s = sh.borrow_mut();
                s.busy = None;
                match r {
                    Ok((code, text)) if (200..300).contains(&code) => {
                        s.error = None;
                        if let Some(id) = then_test {
                            s.tests
                                .insert(id, serde_json::from_str(&text).unwrap_or_default());
                        }
                    }
                    Ok((_, text)) => {
                        s.error = Some(
                            serde_json::from_str::<Value>(&text)
                                .ok()
                                .and_then(|v| v["error"].as_str().map(String::from))
                                .unwrap_or_else(|| "That didn't work.".into()),
                        )
                    }
                    Err(e) => s.error = Some(e),
                }
            }
            // Reload the lists.
            let get = |p: &'static str| {
                let t = token.clone();
                async move {
                    match send("GET", p, None, None, t.as_deref()).await {
                        Ok((200, b)) => serde_json::from_str::<Value>(&b).unwrap_or_default(),
                        _ => Value::Null,
                    }
                }
            };
            let rules = get("/api/alerts/rules").await;
            let brief = get("/api/brief").await;
            let mut s = sh.borrow_mut();
            s.rules = rules["rules"].as_array().cloned().unwrap_or_default();
            s.brief = brief;
            ctx.request_repaint();
        });
    }

    fn push(&self, method: &'static str, token: Option<String>, ctx: &egui::Context) {
        self.shared.borrow_mut().busy = Some("Setting up notifications…".into());
        let (sh, ctx) = (self.shared.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let r = push_call(method, token).await;
            let mut s = sh.borrow_mut();
            s.busy = None;
            match r {
                Ok(state) if method != "test" => s.push = Some(state),
                Ok(_) => {}
                Err(e) => s.error = Some(format!("Notifications: {e}")),
            }
            ctx.request_repaint();
        });
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        area: Rect,
        token: Option<String>,
        admin: bool,
    ) -> Option<AlertAction> {
        BUTTONS.with(|b| b.borrow_mut().clear());
        if !self.open {
            return None;
        }
        let ctx = ui.ctx().clone();
        if !self.loaded {
            self.refresh(token.clone(), &ctx);
        }
        let w = area.width().min(480.0);
        let panel = Rect::from_min_max(pos2(area.right() - w, area.top()), area.max);
        let p = ui.painter().clone();
        p.rect_filled(panel, CornerRadius::ZERO, BG);
        p.vline(panel.left() + 0.5, panel.y_range(), Stroke::new(1.0, LINE));
        let _ = ui.interact(panel, ui.scope_id().with("alerts-bg"), Sense::click());
        let mut action = None;
        let inner = panel.shrink2(vec2(16.0, 12.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Alerts").font(theme::semibold(17.0)).color(TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if btn(ui, "Close", "close").clicked() {
                            self.open = false;
                        }
                    });
                });
                let (busy, error) = {
                    let s = self.shared.borrow();
                    (s.busy.clone(), s.error.clone())
                };
                if let Some(b) = busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(egui::RichText::new(b).color(TEXT_2));
                    });
                }
                if let Some(e) = error {
                    ui.label(egui::RichText::new(e).color(ALERT));
                }

                // This device's notifications.
                section(ui, "Notifications on this device");
                let state = self.shared.borrow().push.clone().unwrap_or_default();
                let (msg, can_on, can_off) = match state.as_str() {
                    "on" => ("On.", false, true),
                    "off" => ("Off.", true, false),
                    "denied" => ("Blocked in this browser's settings for this site.", false, false),
                    "ios-needs-home-screen" => (
                        "On iPhone, add Argus to the Home Screen first: tap Share, then \"Add to Home Screen\", open Argus from there and enable notifications.",
                        false,
                        false,
                    ),
                    "unsupported" => ("This browser can't receive push notifications.", false, false),
                    _ => ("Checking…", false, false),
                };
                ui.label(egui::RichText::new(msg).font(theme::regular(12.5)).color(TEXT_2));
                ui.horizontal(|ui| {
                    if can_on && btn(ui, "Enable notifications", "enable").clicked() {
                        self.push("enable", token.clone(), &ctx);
                    }
                    if can_off {
                        if btn(ui, "Send a test", "test-push").clicked() {
                            self.push("test", token.clone(), &ctx);
                        }
                        if btn(ui, "Turn off", "disable").clicked() {
                            self.push("disable", token.clone(), &ctx);
                        }
                    }
                });

                // The daily brief.
                section(ui, "Daily brief");
                let brief = self.shared.borrow().brief.clone();
                match brief["latest"]["text"].as_str() {
                    Some(t) => {
                        ui.label(
                            egui::RichText::new(format!("{} · {} events", when(brief["latest"]["at_ms"].as_f64().unwrap_or(0.0)), brief["latest"]["events"]))
                                .font(theme::mono(11.0))
                                .color(TEXT_3),
                        );
                        ui.label(egui::RichText::new(t).font(theme::regular(13.0)).color(TEXT));
                    }
                    None => {
                        ui.label(egui::RichText::new("No brief yet.").font(theme::regular(12.5)).color(TEXT_2));
                    }
                }
                if brief["configured"] == false {
                    ui.label(egui::RichText::new("Set up a model in argus.toml to get a daily brief.").font(theme::regular(12.5)).color(ALERT));
                } else if let Some(n) = brief["next"].as_str() {
                    ui.label(egui::RichText::new(format!("Next: {n}")).font(theme::regular(12.0)).color(TEXT_3));
                }
                if admin && brief["configured"] == true && btn(ui, "Write one now", "brief-now").clicked() {
                    self.call(token.clone(), &ctx, Req::new("POST", "/api/brief/run", None, "Writing the brief…"));
                }

                // Rules.
                section(ui, "Rules");
                if admin {
                    let ed = ui.add(
                        egui::TextEdit::multiline(&mut self.new_rule)
                            .hint_text("Describe an alert, e.g. \"a person at the front door after 10pm\"")
                            .desired_rows(2)
                            .desired_width(ui.available_width()),
                    );
                    BUTTONS.with(|b| b.borrow_mut().push(("rule-box".into(), [ed.rect.min.x, ed.rect.min.y, ed.rect.width(), ed.rect.height()])));
                    if btn(ui, "Add rule", "add-rule").clicked() && !self.new_rule.trim().is_empty() {
                        let text = std::mem::take(&mut self.new_rule);
                        self.call(token.clone(), &ctx, Req::new("POST", "/api/alerts/rules", Some(json!({"text": text})), "The agent is reading the rule…"));
                    }
                }
                let rules = self.shared.borrow().rules.clone();
                if rules.is_empty() {
                    ui.label(egui::RichText::new("No rules yet.").color(TEXT_3));
                }
                for (i, r) in rules.iter().enumerate() {
                    let id = r["id"].as_str().unwrap_or("").to_owned();
                    ui.separator();
                    let on = r["enabled"] == true;
                    ui.label(egui::RichText::new(r["text"].as_str().unwrap_or("")).font(theme::semibold(13.5)).color(if on { TEXT } else { TEXT_3 }));
                    ui.label(egui::RichText::new(r["describe"].as_str().unwrap_or("")).font(theme::mono(11.0)).color(ACCENT));
                    if let Some(n) = r["note"].as_str() {
                        ui.label(egui::RichText::new(format!("Note: {n}")).font(theme::regular(12.0)).color(TEXT_2));
                    }
                    let cooldown = r["cooldown_s"].as_u64().unwrap_or(600);
                    ui.label(egui::RichText::new(format!("At most once every {} min{}", cooldown / 60, r["last_fired_ms"].as_f64().map(|t| format!(" · last fired {}", when(t))).unwrap_or_default())).font(theme::regular(12.0)).color(TEXT_3));
                    if let Some(t) = self.shared.borrow().tests.get(&id) {
                        let mut line = format!(
                            "Last 24 h: would have fired {}× ({} matched of {} events)",
                            t["would_fire"], t["matched"], t["events"]
                        );
                        if t["asks_model"] == true {
                            line.push_str(", each after the model's check");
                        }
                        ui.label(egui::RichText::new(line).font(theme::regular(12.5)).color(TEXT));
                        let times: Vec<String> = t["results"]
                            .as_array()
                            .map(|a| a.iter().filter(|x| x["fires"] == true).take(8).map(|x| format!("{} {}", when(x["start_ms"].as_f64().unwrap_or(0.0)), x["camera"].as_str().unwrap_or(""))).collect())
                            .unwrap_or_default();
                        if !times.is_empty() {
                            ui.label(egui::RichText::new(times.join(" · ")).font(theme::mono(11.0)).color(TEXT_2));
                        }
                    }
                    if admin {
                        ui.horizontal(|ui| {
                            let mut en = on;
                            let c = ui.checkbox(&mut en, "On");
                            BUTTONS.with(|b| b.borrow_mut().push((format!("on-{i}"), [c.rect.min.x, c.rect.min.y, c.rect.width(), c.rect.height()])));
                            if c.changed() {
                                self.call(token.clone(), &ctx, Req::new("PUT", format!("/api/alerts/rules/{id}"), Some(json!({"enabled": en})), "Saving…"));
                            }
                            if btn(ui, "Test on last 24 h", &format!("test-{i}")).clicked() {
                                self.call(token.clone(), &ctx, Req { then_test: Some(id.clone()), ..Req::new("POST", format!("/api/alerts/rules/{id}/test"), None, "Trying the rule…") });
                            }
                            if btn(ui, "Edit", &format!("edit-{i}")).clicked() {
                                let f = &r["filter"];
                                self.edit = Some(Edit {
                                    id: id.clone(),
                                    cameras: list(&f["cameras"]),
                                    labels: list(&f["labels"]),
                                    from: f["from"].as_str().unwrap_or("").into(),
                                    to: f["to"].as_str().unwrap_or("").into(),
                                    days: list(&f["days"]),
                                    check: f["check"].as_str().unwrap_or("").into(),
                                    cooldown_min: cooldown as f64 / 60.0,
                                });
                            }
                            if btn(ui, "Delete", &format!("delete-{i}")).clicked() {
                                self.call(token.clone(), &ctx, Req::new("DELETE", format!("/api/alerts/rules/{id}"), None, "Deleting…"));
                            }
                        });
                    }
                    if self.edit.as_ref().is_some_and(|e| e.id == id) {
                        let mut e = self.edit.clone().unwrap();
                        let mut save = false;
                        egui::Grid::new(("edit", i)).num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                            for (label, v) in [("Cameras", &mut e.cameras), ("Labels", &mut e.labels), ("From", &mut e.from), ("To", &mut e.to), ("Days (1=Mon)", &mut e.days), ("Ask the model", &mut e.check)] {
                                ui.label(egui::RichText::new(label).color(TEXT_2));
                                ui.add(egui::TextEdit::singleline(v).desired_width(260.0));
                                ui.end_row();
                            }
                            ui.label(egui::RichText::new("Cooldown").color(TEXT_2));
                            ui.add(egui::DragValue::new(&mut e.cooldown_min).range(0.0..=1440.0).suffix(" min"));
                            ui.end_row();
                        });
                        ui.horizontal(|ui| {
                            save = btn(ui, "Save", "save-edit").clicked();
                            if btn(ui, "Cancel", "cancel-edit").clicked() {
                                self.edit = None;
                            }
                        });
                        if save {
                            let days: Vec<u8> = split(&e.days).iter().filter_map(|d| d.parse().ok()).collect();
                            let opt = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_owned());
                            let filter = json!({
                                "cameras": split(&e.cameras), "labels": split(&e.labels),
                                "from": opt(&e.from), "to": opt(&e.to), "days": days, "check": opt(&e.check),
                            });
                            self.call(token.clone(), &ctx, Req::new("PUT", format!("/api/alerts/rules/{}", e.id), Some(json!({"filter": filter, "cooldown_s": (e.cooldown_min * 60.0) as u64})), "Saving…"));
                            self.edit = None;
                        } else if self.edit.is_some() {
                            self.edit = Some(e);
                        }
                    }
                }

                // Recent alerts.
                section(ui, "Recent alerts");
                let log = self.shared.borrow().log.clone();
                if log.is_empty() {
                    ui.label(egui::RichText::new("None yet.").color(TEXT_3));
                }
                for (i, f) in log.iter().take(12).enumerate() {
                    let r = ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "{} · {} — {}",
                                when(f["at_ms"].as_f64().unwrap_or(0.0)),
                                f["title"].as_str().unwrap_or(""),
                                f["body"].as_str().unwrap_or("")
                            ))
                            .font(theme::regular(12.5))
                            .color(TEXT),
                        )
                        .sense(Sense::click()),
                    );
                    BUTTONS.with(|b| b.borrow_mut().push((format!("alert-{i}"), [r.rect.min.x, r.rect.min.y, r.rect.width(), r.rect.height()])));
                    if r.clicked() {
                        // The event's time is in the log entry's url-less
                        // data: replay from when it fired.
                        action = Some(AlertAction::Replay {
                            camera: f["camera"].as_str().unwrap_or("").into(),
                            t_ms: f["at_ms"].as_f64().unwrap_or(0.0) - 10_000.0,
                        });
                    }
                }
            });
        });
        action
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(title)
            .font(theme::semibold(14.0))
            .color(TEXT),
    );
}
