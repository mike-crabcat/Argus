//! The floor plan: drawn from the shared model, viewable anywhere, and
//! editable on a wide screen with the main token. Every edit is a
//! [`Command`] applied locally (with undo) and saved to the server in
//! batches through `/api/plan/commands`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use argus_plan::outline::{camera_cone, opening_frame};
use argus_plan::snap::{SnapKind, SnapOptions, snap};
use argus_plan::{
    Background, Command, End, Entity, Hinge, Id, Level, OpeningKind, Plan, Point, Side, Swing,
    Versioned, pt,
};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Mesh, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, pos2,
    vec2,
};

use super::theme::{self, ACCENT, ALERT, BG, LINE, SURFACE, TEXT, TEXT_2, TEXT_3};
use super::{fetch_image, now_ms, pick_image, send};
use crate::plandoc::{self, Doc, EventList, MapEvent, View, fmt_len};

/// What a tap on the map asks the app to do.
#[derive(Clone, Debug, PartialEq)]
pub enum MapAction {
    /// Show this camera live, full window.
    Live(String),
    /// Replay from this time (ms), with this camera full window.
    Replay { camera: String, t_ms: f64 },
}

/// How far back the map shows events.
const WINDOWS: [(&str, f64); 3] = [
    ("15 min", 900_000.0),
    ("1 h", 3_600_000.0),
    ("6 h", 21_600_000.0),
];
const EVENTS_EVERY_MS: f64 = 15_000.0;

/// Marker colour per event label.
fn label_color(label: &str) -> Color32 {
    match label {
        "person" => ALERT,
        "vehicle" => Color32::from_rgb(0xF2, 0xB8, 0x5B),
        "animal" => Color32::from_rgb(0x7F, 0xD1, 0x8B),
        _ => TEXT_2,
    }
}

thread_local! {
    /// Toolbar buttons drawn this frame, (label, [x, y, w, h]), for scripts.
    static BUTTONS: RefCell<Vec<(String, [f32; 4])>> = const { RefCell::new(Vec::new()) };
}

/// Below this width the plan is view-only.
pub const EDIT_MIN_WIDTH: f32 = 900.0;
const PROPS_W: f32 = 280.0;
const BAR_H: f32 = 44.0;
/// Unsaved edits are sent this long after the last one.
const AUTOSAVE_MS: f64 = 1200.0;

const WALL: Color32 = Color32::from_rgb(0xB4, 0xB4, 0xBC);
const ROOM: Color32 = Color32::from_rgb(0x14, 0x14, 0x17);

enum Msg {
    Loaded(Versioned),
    LoadFailed(String),
    Saved(u64),
    Conflict,
    SaveFailed(String),
    Uploaded {
        id: String,
        w: u32,
        h: u32,
    },
    Failed(String),
    Events(Vec<MapEvent>),
    Image {
        id: String,
        size: [usize; 2],
        orig: [u32; 2],
        rgba: Vec<u8>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Select,
    Wall,
    Door,
    Window,
    Room,
    Dimension,
    Camera,
    Pan,
    Scale,
}

impl Tool {
    const BAR: [(Tool, &'static str, egui::Key); 8] = [
        (Tool::Select, "Select", egui::Key::V),
        (Tool::Wall, "Wall", egui::Key::W),
        (Tool::Door, "Door", egui::Key::D),
        (Tool::Window, "Window", egui::Key::N),
        (Tool::Room, "Room", egui::Key::R),
        (Tool::Dimension, "Dimension", egui::Key::M),
        (Tool::Camera, "Camera", egui::Key::C),
        (Tool::Pan, "Pan", egui::Key::H),
    ];
    fn name(self) -> &'static str {
        match self {
            Tool::Scale => "Scale",
            t => Tool::BAR.iter().find(|x| x.0 == t).map_or("", |x| x.1),
        }
    }
}

struct Bg {
    tex: egui::TextureHandle,
    /// The image's real size in pixels (the texture may be smaller).
    px: [u32; 2],
}

enum Drag {
    Pan,
    Box {
        start: Pos2,
    },
    Move {
        from: Point,
        ids: Vec<Id>,
        node: Option<Id>,
    },
    Heading {
        camera: Id,
    },
    PlaceCamera {
        at: Point,
    },
}

/// What the pointer would do in a draw tool, for the preview.
enum Hint {
    Snap(argus_plan::snap::Snapped),
    Opening { wall: Id, offset: f64, side: Side },
}

pub struct PlanView {
    token: Option<String>,
    inbox: Rc<RefCell<Vec<Msg>>>,
    doc: Option<Doc>,
    error: Option<String>,
    note: Option<(String, f64)>,
    pub editing: bool,
    tool: Tool,
    level: Option<Id>,
    view: Option<View>,
    sel: Vec<Id>,
    chain: Option<(Point, Point)>, // (first, last) of a wall chain
    first_click: Option<Point>,
    typed: String,
    drag: Option<Drag>,
    preview: Option<Plan>,
    wall_thickness: f64,
    door_width: f64,
    window_width: f64,
    camera_pick: String,
    bgs: HashMap<String, Bg>,
    bg_loading: HashSet<String>,
    last_edit_ms: f64,
    focus_name: bool,
    edit: Option<Entity>,
    wall_len: Option<(Id, f64)>,
    scale_input: String,
    /// The scale tool's second point.
    scale_to: Option<Point>,
    canvas: Rect,
    hover_world: Option<Point>,
    events: Vec<MapEvent>,
    events_ms: f64,
    window: usize,
    /// Event markers drawn last frame: (screen position, index in events).
    markers: Vec<(Pos2, usize)>,
    action: Option<MapAction>,
}

fn sp(v: &View, p: Point) -> Pos2 {
    let (x, y) = v.to_screen(p);
    pos2(x, y)
}

fn wp(v: &View, p: Pos2) -> Point {
    v.to_world((p.x, p.y))
}

impl PlanView {
    pub fn new(token: Option<String>, ctx: &egui::Context) -> Self {
        let mut v = PlanView {
            token,
            inbox: Rc::default(),
            doc: None,
            error: None,
            note: None,
            editing: false,
            tool: Tool::Select,
            level: None,
            view: None,
            sel: Vec::new(),
            chain: None,
            first_click: None,
            typed: String::new(),
            drag: None,
            preview: None,
            wall_thickness: 230.0,
            door_width: 820.0,
            window_width: 1200.0,
            camera_pick: String::new(),
            bgs: HashMap::new(),
            bg_loading: HashSet::new(),
            last_edit_ms: 0.0,
            focus_name: false,
            edit: None,
            wall_len: None,
            scale_input: String::new(),
            scale_to: None,
            canvas: Rect::NOTHING,
            hover_world: None,
            events: Vec::new(),
            events_ms: f64::NEG_INFINITY,
            window: 1,
            markers: Vec::new(),
            action: None,
        };
        v.load(ctx);
        v
    }

    fn load(&mut self, ctx: &egui::Context) {
        let (inbox, token, ctx) = (self.inbox.clone(), self.token.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let msg = match send("GET", "/api/plan", None, None, token.as_deref()).await {
                Ok((200, body)) => match serde_json::from_str::<Versioned>(&body) {
                    Ok(v) => Msg::Loaded(v),
                    Err(e) => Msg::LoadFailed(format!("the plan didn't parse: {e}")),
                },
                Ok((code, _)) => Msg::LoadFailed(format!("loading the plan: HTTP {code}")),
                Err(e) => Msg::LoadFailed(e),
            };
            inbox.borrow_mut().push(msg);
            ctx.request_repaint();
        });
    }

    fn fetch_events(&mut self, ctx: &egui::Context) {
        if now_ms() - self.events_ms < EVENTS_EVERY_MS {
            return;
        }
        self.events_ms = now_ms();
        let from = super::now_us() / 1000 - WINDOWS[self.window].1 as i64;
        let path = format!("/api/events?from={from}");
        let (inbox, token, ctx) = (self.inbox.clone(), self.token.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok((200, body)) = send("GET", &path, None, None, token.as_deref()).await
                && let Ok(list) = serde_json::from_str::<EventList>(&body)
            {
                inbox.borrow_mut().push(Msg::Events(list.events));
                ctx.request_repaint();
            }
        });
    }

    fn note(&mut self, msg: impl Into<String>) {
        self.note = Some((msg.into(), now_ms() + 4000.0));
    }

    /// Handles async results.
    fn poll(&mut self, ctx: &egui::Context) {
        let msgs: Vec<Msg> = self.inbox.borrow_mut().drain(..).collect();
        for m in msgs {
            match m {
                Msg::Loaded(v) => {
                    match &mut self.doc {
                        Some(d) => d.reload(v),
                        None => self.doc = Some(Doc::new(v)),
                    }
                    self.error = None;
                    let levels = &self.doc.as_ref().unwrap().plan.levels;
                    if self.level.is_none_or(|l| !levels.iter().any(|x| x.id == l)) {
                        self.level = levels.first().map(|l| l.id);
                    }
                    self.sel.clear();
                }
                Msg::LoadFailed(e) => self.error = Some(e),
                Msg::Saved(v) => {
                    if let Some(d) = &mut self.doc {
                        d.saved(v);
                    }
                }
                Msg::Conflict => {
                    self.note("The plan was changed elsewhere; reloaded it. Your last edits were not saved.");
                    self.load(ctx);
                }
                Msg::SaveFailed(e) => {
                    if let Some(d) = &mut self.doc {
                        d.save_failed();
                    }
                    self.note(format!("Couldn't save: {e}"));
                    self.last_edit_ms = now_ms() + 5000.0;
                }
                Msg::Uploaded { id, w, h } => self.place_background(id, w, h),
                Msg::Failed(e) => self.note(e),
                Msg::Events(e) => self.events = e,
                Msg::Image {
                    id,
                    size,
                    orig,
                    rgba,
                } => {
                    let img = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                    let tex = ctx.load_texture(format!("plan-bg-{id}"), img, Default::default());
                    self.bgs.insert(id, Bg { tex, px: orig });
                }
            }
        }
    }

    fn save(&mut self, ctx: &egui::Context) {
        let Some((version, commands)) = self.doc.as_mut().and_then(|d| d.begin_save()) else {
            return;
        };
        let body = serde_json::json!({ "version": version, "commands": commands }).to_string();
        let (inbox, token, ctx) = (self.inbox.clone(), self.token.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let r = send(
                "POST",
                "/api/plan/commands",
                Some(&wasm_bindgen::JsValue::from_str(&body)),
                Some("application/json"),
                token.as_deref(),
            )
            .await;
            let msg = match r {
                Ok((200, b)) => serde_json::from_str::<serde_json::Value>(&b)
                    .ok()
                    .and_then(|v| v["version"].as_u64())
                    .map_or(Msg::SaveFailed("odd answer".into()), Msg::Saved),
                Ok((409, _)) => Msg::Conflict,
                Ok((code, b)) => Msg::SaveFailed(
                    serde_json::from_str::<serde_json::Value>(&b)
                        .ok()
                        .and_then(|v| v["error"].as_str().map(String::from))
                        .unwrap_or_else(|| format!("HTTP {code}")),
                ),
                Err(e) => Msg::SaveFailed(e),
            };
            inbox.borrow_mut().push(msg);
            ctx.request_repaint();
        });
    }

    /// Applies an edit; shows why if it's refused.
    fn exec(&mut self, cmd: Command) -> bool {
        let Some(d) = &mut self.doc else { return false };
        match d.exec(cmd) {
            Ok(()) => {
                self.last_edit_ms = now_ms();
                true
            }
            Err(e) => {
                self.note(e.to_string());
                false
            }
        }
    }

    fn level(&self) -> Option<&Level> {
        let d = self.doc.as_ref()?;
        let plan = self.preview.as_ref().unwrap_or(&d.plan);
        self.level.and_then(|id| plan.level(id))
    }

    fn level_id(&self) -> Id {
        self.level.unwrap_or(Id(0))
    }

    fn upload(&mut self, ctx: &egui::Context) {
        let (inbox, token, ctx) = (self.inbox.clone(), self.token.clone(), ctx.clone());
        pick_image(move |file| {
            let (inbox, token, ctx) = (inbox.clone(), token.clone(), ctx.clone());
            wasm_bindgen_futures::spawn_local(async move {
                let ct = file.type_();
                let r = send(
                    "POST",
                    "/api/plan/assets",
                    Some(&file.into()),
                    Some(if ct.is_empty() {
                        "application/octet-stream"
                    } else {
                        &ct
                    }),
                    token.as_deref(),
                )
                .await;
                let msg = match r {
                    Ok((201, b)) => {
                        let v: serde_json::Value = serde_json::from_str(&b).unwrap_or_default();
                        match (v["id"].as_str(), v["width"].as_u64(), v["height"].as_u64()) {
                            (Some(id), Some(w), Some(h)) => Msg::Uploaded {
                                id: id.into(),
                                w: w as u32,
                                h: h as u32,
                            },
                            _ => Msg::Failed("odd answer to the upload".into()),
                        }
                    }
                    Ok((_, b)) => Msg::Failed(
                        serde_json::from_str::<serde_json::Value>(&b)
                            .ok()
                            .and_then(|v| v["error"].as_str().map(String::from))
                            .unwrap_or_else(|| "the upload failed".into()),
                    ),
                    Err(e) => Msg::Failed(e),
                };
                inbox.borrow_mut().push(msg);
                ctx.request_repaint();
            });
        });
    }

    /// A new background: about 20 m wide over the plan, half see-through,
    /// then the scale tool to set it properly.
    fn place_background(&mut self, asset: String, w: u32, _h: u32) {
        let Some(l) = self.level() else { return };
        let (lo, hi) = plandoc::bounds(l, false).unwrap_or((pt(0.0, 0.0), pt(20000.0, 15000.0)));
        let span = (hi.x - lo.x).max(20000.0);
        let mm_per_px = span / w.max(1) as f64;
        let cmd = Command::SetBackground {
            level: self.level_id(),
            background: Some(Background {
                asset,
                mm_per_px,
                offset: pt(lo.x, hi.y.max(lo.y + 1000.0)),
                rotation: 0.0,
                opacity: 0.5,
            }),
        };
        if self.exec(cmd) {
            self.tool = Tool::Scale;
            self.first_click = None;
            self.scale_input.clear();
            self.note("Image added. Now click two points a known distance apart.");
        }
    }

    fn ensure_bg(&mut self, ctx: &egui::Context) {
        let Some(bg) = self.level().and_then(|l| l.background.clone()) else {
            return;
        };
        if self.bgs.contains_key(&bg.asset) || !self.bg_loading.insert(bg.asset.clone()) {
            return;
        }
        let (inbox, token, ctx) = (self.inbox.clone(), self.token.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let path = format!("/api/plan/assets/{}", bg.asset);
            let msg = match fetch_image(&path, token.as_deref(), 4096).await {
                Ok((size, orig, rgba)) => Msg::Image {
                    id: bg.asset,
                    size,
                    orig,
                    rgba,
                },
                Err(e) => Msg::Failed(format!("background image: {e}")),
            };
            inbox.borrow_mut().push(msg);
            ctx.request_repaint();
        });
    }

    /// For scripts and the stats readout.
    pub fn stats(&self) -> serde_json::Value {
        let d = self.doc.as_ref();
        let l = self.level();
        serde_json::json!({
            "loaded": d.is_some(),
            "error": self.error,
            "note": self.note.as_ref().map(|n| n.0.clone()),
            "editing": self.editing,
            "tool": self.tool.name(),
            "level": self.level.map(|l| l.0),
            "levels": d.map(|d| d.plan.levels.iter().map(|l| l.name.clone()).collect::<Vec<_>>()),
            "version": d.map(|d| d.version),
            "dirty": d.is_some_and(|d| d.dirty()),
            "saving": d.is_some_and(|d| d.saving()),
            "can_undo": d.is_some_and(|d| d.can_undo()),
            "selection": self.sel.iter().map(|i| i.0).collect::<Vec<_>>(),
            "counts": l.map(|l| serde_json::json!({
                "walls": l.walls.len(), "openings": l.openings.len(), "rooms": l.rooms.len(),
                "dimensions": l.dimensions.len(), "cameras": l.cameras.len(),
                "background": l.background.is_some(),
            })),
            "background": l.and_then(|l| l.background.clone()),
            "view": self.view.map(|v| [v.origin.0, v.origin.1, v.scale]),
            "canvas": [self.canvas.min.x, self.canvas.min.y, self.canvas.width(), self.canvas.height()],
            "chain": self.chain.is_some(),
            "typed": self.typed,
            "images": { "loaded": self.bgs.len(), "loading": self.bg_loading.len() },
            "buttons": BUTTONS.with(|b| b.borrow().clone()),
            "events": self.events.len(),
            "markers": self.markers.iter().map(|(p, i)| serde_json::json!({
                "x": p.x, "y": p.y, "camera": self.events[*i].camera, "label": self.events[*i].label,
                "start_ms": self.events[*i].start_ms,
            })).collect::<Vec<_>>(),
            "cameras_on_screen": self.level().zip(self.view).map(|(l, v)| l.cameras.iter().map(|c| {
                let p = sp(&v, c.at);
                serde_json::json!({ "camera": c.camera, "x": p.x, "y": p.y })
            }).collect::<Vec<_>>()),
            "window": WINDOWS[self.window].0,
            "headings": l.map(|l| l.cameras.iter().map(|c| (c.camera.clone(), c.heading)).collect::<std::collections::BTreeMap<_, _>>()),
        })
    }

    /// Draws the plan in `rect`. `cameras` are the configured camera names;
    /// `admin` says whether editing is allowed.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        cameras: &[String],
        admin: bool,
    ) -> Option<MapAction> {
        let ctx = ui.ctx().clone();
        BUTTONS.with(|b| b.borrow_mut().clear());
        self.poll(&ctx);
        let wide = rect.width() >= EDIT_MIN_WIDTH;
        if self.editing && (!wide || !admin) {
            self.set_editing(false);
        }
        if let Some(d) = &self.doc
            && d.dirty()
            && !d.saving()
            && now_ms() - self.last_edit_ms > AUTOSAVE_MS
        {
            self.save(&ctx);
        }
        if self.camera_pick.is_empty()
            || (!cameras.contains(&self.camera_pick) && !cameras.is_empty())
        {
            self.camera_pick = cameras.first().cloned().unwrap_or_default();
        }
        self.ensure_bg(&ctx);

        let p = ui.painter_at(rect);
        p.rect_filled(rect, CornerRadius::ZERO, BG);
        let bar = Rect::from_min_size(rect.min, vec2(rect.width(), BAR_H));
        let mut canvas = Rect::from_min_max(pos2(rect.left(), bar.bottom()), rect.max);
        let props = self.editing.then(|| {
            let r = Rect::from_min_max(pos2(rect.right() - PROPS_W, bar.bottom()), rect.max);
            canvas.max.x = r.left();
            r
        });
        self.canvas = canvas;

        if self.doc.is_none() {
            let msg = self
                .error
                .clone()
                .unwrap_or_else(|| "loading the plan…".into());
            p.text(
                canvas.center(),
                Align2::CENTER_CENTER,
                msg,
                theme::regular(13.5),
                TEXT_2,
            );
            return None;
        }
        if !self.editing {
            self.fetch_events(&ctx);
        }
        if self.view.is_none() {
            self.fit();
        }
        self.canvas_ui(ui, canvas, cameras);
        self.bar_ui(ui, bar, admin && wide);
        if let Some(r) = props {
            self.props_ui(ui, r, cameras);
        }
        if let Some((msg, until)) = self.note.clone() {
            if now_ms() > until {
                self.note = None;
            } else {
                let g = p.layout_no_wrap(msg, theme::regular(12.5), TEXT);
                let r = Rect::from_center_size(
                    pos2(canvas.center().x, canvas.bottom() - 28.0),
                    g.size() + vec2(28.0, 14.0),
                );
                p.rect_filled(r, CornerRadius::same(theme::PILL_RADIUS), SURFACE);
                p.rect_stroke(
                    r,
                    CornerRadius::same(theme::PILL_RADIUS),
                    Stroke::new(1.0, LINE),
                    StrokeKind::Inside,
                );
                p.galley(r.min + vec2(14.0, 7.0), g, TEXT);
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
            }
        }
        self.action.take()
    }

    fn set_editing(&mut self, on: bool) {
        self.editing = on;
        self.tool = Tool::Select;
        self.chain = None;
        self.first_click = None;
        self.typed.clear();
        self.drag = None;
        self.preview = None;
        self.edit = None;
    }

    fn fit(&mut self) {
        let r = self.canvas;
        if r.width() < 10.0 {
            return;
        }
        let (lo, hi) = self
            .level()
            .and_then(|l| plandoc::bounds(l, false))
            .unwrap_or((pt(-2000.0, -2000.0), pt(18000.0, 13000.0)));
        let margin = (r.width().min(r.height()) * 0.08).max(if self.editing { 40.0 } else { 90.0 });
        self.view = Some(View::fit(
            lo,
            hi,
            (r.min.x, r.min.y, r.width(), r.height()),
            margin,
        ));
    }

    fn bar_ui(&mut self, ui: &mut egui::Ui, bar: Rect, can_edit: bool) {
        let p = ui.painter().clone();
        p.hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, LINE));
        let cy = bar.center().y;
        let id = ui.scope_id().with("planbar");
        let mut x = bar.left() + 12.0;
        // Levels.
        let levels: Vec<(Id, String)> = self
            .doc
            .as_ref()
            .map(|d| {
                d.plan
                    .levels
                    .iter()
                    .map(|l| (l.id, l.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (lid, name) in &levels {
            let on = Some(*lid) == self.level;
            if pill(ui, &p, &mut x, cy, name, on, id.with(lid.0)).clicked() && !on {
                self.level = Some(*lid);
                self.sel.clear();
                self.chain = None;
                self.view = None;
            }
        }
        if self.editing && pill(ui, &p, &mut x, cy, "+ Level", false, id.with("addlevel")).clicked()
        {
            let n = levels.len() + 1;
            let lid = self.doc.as_ref().map_or(Id(0), |d| Id(d.plan.next_id));
            if self.exec(Command::AddLevel {
                id: Some(lid),
                name: format!("Level {n}"),
            }) {
                self.level = Some(lid);
                self.sel.clear();
            }
        }
        x += 12.0;
        if !self.editing {
            p.vline(x, (cy - 10.0)..=(cy + 10.0), Stroke::new(1.0, LINE));
            x += 12.0;
            for (i, (name, _)) in WINDOWS.iter().enumerate() {
                if pill(
                    ui,
                    &p,
                    &mut x,
                    cy,
                    name,
                    self.window == i,
                    id.with(("win", i)),
                )
                .clicked()
                    && self.window != i
                {
                    self.window = i;
                    self.events_ms = f64::NEG_INFINITY;
                    self.events.clear();
                }
            }
        }
        if self.editing {
            p.vline(x, (cy - 10.0)..=(cy + 10.0), Stroke::new(1.0, LINE));
            x += 12.0;
            for (tool, name, _) in Tool::BAR {
                if pill(ui, &p, &mut x, cy, name, self.tool == tool, id.with(name)).clicked() {
                    self.set_tool(tool);
                }
            }
        }
        // Right side, right to left.
        let mut rx = bar.right() - 12.0;
        let right_pill = |ui: &mut egui::Ui, rx: &mut f32, label: &str, on: bool, salt: &str| {
            let g = p.layout_no_wrap(label.into(), theme::regular(12.5), TEXT);
            let r = Rect::from_center_size(
                pos2(*rx - (g.size().x + 24.0) / 2.0, cy),
                vec2(g.size().x + 24.0, 26.0),
            );
            *rx = r.left() - 6.0;
            draw_pill(ui, &p, r, label, on, id.with(salt))
        };
        if self.editing {
            if right_pill(ui, &mut rx, "Done", true, "done").clicked() {
                if let Some(d) = &self.doc
                    && d.dirty()
                {
                    self.save(ui.ctx());
                }
                self.set_editing(false);
            }
            let (dirty, saving, undo, redo) =
                self.doc.as_ref().map_or((false, false, false, false), |d| {
                    (d.dirty(), d.saving(), d.can_undo(), d.can_redo())
                });
            if right_pill(ui, &mut rx, "Save", dirty, "save").clicked() {
                self.save(ui.ctx());
            }
            let (txt, col) = if saving {
                ("Saving…", TEXT_2)
            } else if dirty {
                ("Unsaved", ALERT)
            } else {
                ("Saved", TEXT_3)
            };
            let g = p.layout_no_wrap(txt.into(), theme::regular(12.0), col);
            rx -= g.size().x;
            p.galley(pos2(rx, cy - g.size().y / 2.0), g, col);
            if dirty {
                p.circle_filled(pos2(rx - 8.0, cy), 3.0, ALERT);
                rx -= 8.0;
            }
            rx -= 14.0;
            if right_pill(ui, &mut rx, "Redo", false, "redo").clicked() && redo {
                self.redo();
            }
            if right_pill(ui, &mut rx, "Undo", false, "undo").clicked() && undo {
                self.undo();
            }
        } else if can_edit && right_pill(ui, &mut rx, "Edit plan", false, "edit").clicked() {
            self.set_editing(true);
        }
        if right_pill(ui, &mut rx, "Fit", false, "fit").clicked() {
            self.fit();
        }
    }

    fn set_tool(&mut self, t: Tool) {
        self.tool = t;
        self.chain = None;
        self.first_click = None;
        self.scale_to = None;
        self.typed.clear();
    }

    fn undo(&mut self) {
        if let Some(d) = &mut self.doc
            && d.undo()
        {
            self.last_edit_ms = now_ms();
            self.after_history();
        }
    }

    fn redo(&mut self) {
        if let Some(d) = &mut self.doc
            && d.redo()
        {
            self.last_edit_ms = now_ms();
            self.after_history();
        }
    }

    fn after_history(&mut self) {
        let d = self.doc.as_ref().unwrap();
        self.sel
            .retain(|id| d.plan.levels.iter().any(|l| l.entity(*id).is_some()));
        if self.level.is_none_or(|l| d.plan.level(l).is_none()) {
            self.level = d.plan.levels.first().map(|l| l.id);
        }
        self.edit = None;
        self.chain = None;
    }

    fn keys(&mut self, ctx: &egui::Context) {
        if ctx.text_edit_focused() {
            return;
        }
        let (undo, redo, esc, del, enter) = ctx.input_mut(|i| {
            let redo = i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::Z,
            ) || i.consume_key(egui::Modifiers::COMMAND, egui::Key::Y);
            let undo = i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z);
            (
                undo,
                redo,
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
                i.key_pressed(egui::Key::Enter),
            )
        });
        if !self.editing {
            return;
        }
        if undo {
            self.undo();
        }
        if redo {
            self.redo();
        }
        // Typed lengths while drawing a wall: "3600", "3.6m", "3600<90".
        if self.chain.is_some() {
            let text: String = ctx.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::Text(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect()
            });
            for ch in text.chars() {
                if ch.is_ascii_digit()
                    || ".<mc-".contains(ch)
                    || (ch == 'm' && !self.typed.is_empty())
                {
                    self.typed.push(ch);
                }
            }
            if !self.typed.is_empty() {
                if del {
                    self.typed.pop();
                    return;
                }
                if enter {
                    self.apply_typed();
                    return;
                }
            }
        }
        if esc {
            if !self.typed.is_empty() {
                self.typed.clear();
            } else if self.chain.is_some() || self.first_click.is_some() || self.drag.is_some() {
                self.chain = None;
                self.first_click = None;
                self.drag = None;
                self.preview = None;
            } else if !self.sel.is_empty() {
                self.sel.clear();
            } else {
                self.set_tool(Tool::Select);
            }
        }
        if del && self.typed.is_empty() && !self.sel.is_empty() {
            self.delete_selection();
        }
        if self.chain.is_none() && self.typed.is_empty() {
            let mods = ctx.input(|i| i.modifiers);
            if !mods.command && !mods.alt {
                for (tool, _, key) in Tool::BAR {
                    if ctx.input(|i| i.key_pressed(key)) {
                        self.set_tool(tool);
                    }
                }
            }
        }
    }

    fn delete_selection(&mut self) {
        let level = self.level_id();
        let commands: Vec<Command> = self
            .sel
            .iter()
            .map(|&id| Command::DeleteEntity { level, id })
            .collect();
        // Deleting a wall may take a node selected with it: delete what's
        // still there, one by one, as a batch.
        let mut ok = Vec::new();
        let mut probe = self.doc.as_ref().unwrap().plan.clone();
        for c in commands {
            if probe.apply(&c).is_ok() {
                ok.push(c);
            }
        }
        if !ok.is_empty() && self.exec(Command::Batch { commands: ok }) {
            self.sel.clear();
        }
    }

    fn apply_typed(&mut self) {
        let Some((_, last)) = self.chain else { return };
        let Some((len, ang)) = plandoc::parse_typed(&self.typed) else {
            self.note("Type a length (3600, 3.6m) or length<angle (3600<90).");
            return;
        };
        let ang = ang.unwrap_or_else(|| {
            let to = self.hover_world.unwrap_or(last + pt(1.0, 0.0));
            let a = last.angle_to(to);
            (a / 15.0).round() * 15.0
        });
        let end = argus_plan::snap::polar(last, len, ang);
        self.typed.clear();
        self.wall_to(end, false);
    }

    fn wall_to(&mut self, end: Point, closes: bool) {
        let Some((first, last)) = self.chain else {
            self.chain = Some((end, end));
            return;
        };
        let cmd = Command::AddWall {
            level: self.level_id(),
            id: None,
            a: End::At(last),
            b: End::At(end),
            thickness: self.wall_thickness,
        };
        if self.exec(cmd) {
            self.chain = if closes || end.dist(first) < 1.0 {
                None
            } else {
                Some((first, end))
            };
        }
    }

    fn snap_opts(&self) -> SnapOptions {
        let scale = self.view.map_or(0.05, |v| v.scale) as f64;
        SnapOptions {
            grid: if scale > 0.08 { 50.0 } else { 100.0 },
            radius: 10.0 / scale,
            ..Default::default()
        }
    }

    fn canvas_ui(&mut self, ui: &mut egui::Ui, canvas: Rect, cameras: &[String]) {
        let ctx = ui.ctx().clone();
        let resp = ui.interact(
            canvas,
            ui.scope_id().with("plan-canvas"),
            Sense::click_and_drag(),
        );
        let mut view = self.view.unwrap();
        // Zoom with the wheel or a pinch, about the pointer.
        if resp.hovered() || resp.contains_pointer() {
            let (scroll, zoom, pos) = ctx.input(|i| {
                (
                    i.smooth_scroll_delta.y,
                    i.zoom_delta(),
                    i.pointer.hover_pos(),
                )
            });
            let f = zoom * (scroll * 0.0015).exp();
            if let Some(pos) = pos
                && (f - 1.0).abs() > 1e-4
            {
                view.zoom_at((pos.x, pos.y), f);
            }
            // Two fingers pan (and pinch zooms, above).
            if let Some(m) = ctx.input(|i| i.multi_touch()) {
                view.origin.0 += m.translation_delta.x;
                view.origin.1 += m.translation_delta.y;
            }
        }
        self.keys(&ctx);
        let pointer = resp.hover_pos().or(resp.interact_pointer_pos());
        let world = pointer.map(|p| wp(&view, p));
        self.hover_world = world;
        let opts = self.snap_opts();
        let tol = 8.0 / view.scale as f64;

        // Pan: middle or right drag always; primary with the Pan tool or
        // when only viewing.
        let primary_pans = !self.editing || self.tool == Tool::Pan;
        let panning = resp.dragged_by(egui::PointerButton::Middle)
            || resp.dragged_by(egui::PointerButton::Secondary)
            || (primary_pans && resp.dragged_by(egui::PointerButton::Primary));
        if panning {
            let d = resp.drag_delta();
            view.origin.0 += d.x;
            view.origin.1 += d.y;
            self.drag = Some(Drag::Pan);
        }
        self.view = Some(view);

        // What a draw tool would do here.
        let mut hint: Option<Hint> = None;
        if self.editing && !panning {
            if let (Some(w), Some(l)) = (world, self.level()) {
                hint = match self.tool {
                    Tool::Wall | Tool::Dimension | Tool::Room | Tool::Camera | Tool::Scale => {
                        let from = match self.tool {
                            Tool::Wall => self.chain.map(|c| c.1),
                            Tool::Dimension | Tool::Scale => self.first_click,
                            _ => None,
                        };
                        let mut o = opts;
                        if self.tool == Tool::Scale {
                            o.grid = 0.0;
                            o.angle_step = 0.0;
                        }
                        Some(Hint::Snap(snap(l, w, from, &o)))
                    }
                    Tool::Door | Tool::Window => self.opening_hint(l, w, tol),
                    _ => None,
                };
            }
        }

        if self.editing && !panning {
            self.pointer_actions(&resp, world, &hint, tol, cameras);
        }
        if !resp.dragged() && matches!(self.drag, Some(Drag::Pan)) {
            self.drag = None;
        }

        // Draw.
        let view = self.view.unwrap();
        let p = ui.painter_at(canvas);
        if self.editing {
            draw_grid(&p, &view, canvas);
        }
        let level = self.level().cloned();
        if let Some(l) = &level {
            if let Some(bg) = &l.background
                && let Some(t) = self.bgs.get(&bg.asset)
            {
                draw_background(&p, &view, bg, t);
            }
            let hover = if self.editing && self.tool == Tool::Select && self.drag.is_none() {
                world.and_then(|w| plandoc::hit(l, w, tol))
            } else {
                None
            };
            draw_level(&p, &view, l, &self.sel, hover, self.editing);
        }
        if !self.editing
            && let Some(l) = &level
        {
            self.draw_events(&p, &view, l, canvas, pointer);
            if resp.clicked()
                && let Some(at) = resp.interact_pointer_pos()
            {
                self.map_tap(&view, l, at);
            }
        }
        if let Some(Drag::Box { start }) = &self.drag
            && let Some(cur) = pointer
        {
            let r = Rect::from_two_pos(*start, cur);
            p.rect_filled(r, CornerRadius::ZERO, ACCENT.gamma_multiply(0.08));
            p.rect_stroke(
                r,
                CornerRadius::ZERO,
                Stroke::new(1.0, ACCENT),
                StrokeKind::Inside,
            );
        }
        self.draw_hint(&p, &view, &hint, pointer);
        if self.editing {
            ctx.set_cursor_icon(match self.tool {
                Tool::Pan => egui::CursorIcon::Grab,
                Tool::Select => egui::CursorIcon::Default,
                _ => egui::CursorIcon::Crosshair,
            });
        }
    }

    /// Event markers around each camera, newest nearest, tinted by label
    /// and fading with age; a legend; a hint under the pointer.
    fn draw_events(
        &mut self,
        p: &egui::Painter,
        view: &View,
        l: &Level,
        canvas: Rect,
        pointer: Option<Pos2>,
    ) {
        self.markers.clear();
        let now = super::now_us() as f64 / 1000.0;
        let window = WINDOWS[self.window].1;
        let mut counts: Vec<(String, usize)> = Vec::new();
        for c in &l.cameras {
            let mut idx: Vec<usize> = (0..self.events.len())
                .filter(|&i| self.events[i].camera == c.camera)
                .collect();
            idx.sort_by_key(|&i| std::cmp::Reverse(self.events[i].start_ms));
            let at = sp(view, c.at);
            const MAX: usize = 24;
            for (k, &i) in idx.iter().take(MAX).enumerate() {
                let e = &self.events[i];
                let (dx, dy) = plandoc::marker_offset(k, c.heading, c.fov);
                let m = at + vec2(dx, dy);
                let a = plandoc::marker_alpha(now, e.start_ms as f64, window);
                p.circle_filled(m, 5.5, BG);
                p.circle_filled(m, 4.5, label_color(&e.label).gamma_multiply(a));
                self.markers.push((m, i));
            }
            if idx.len() > MAX {
                let (dx, dy) = plandoc::marker_offset(MAX, c.heading, c.fov);
                p.text(
                    at + vec2(dx, dy),
                    Align2::CENTER_CENTER,
                    format!("+{}", idx.len() - MAX),
                    theme::mono(10.0),
                    TEXT_2,
                );
            }
            for &i in &idx {
                let label = &self.events[i].label;
                match counts.iter_mut().find(|(l, _)| l == label) {
                    Some(c) => c.1 += 1,
                    None => counts.push((label.clone(), 1)),
                }
            }
        }
        // Legend, bottom left.
        counts.sort();
        let mut y = canvas.bottom() - 18.0;
        let x = canvas.left() + 16.0;
        let total: usize = counts.iter().map(|c| c.1).sum();
        let head = if total == 0 {
            format!("No events in the last {}", WINDOWS[self.window].0)
        } else {
            format!("{total} events · last {}", WINDOWS[self.window].0)
        };
        for (label, n) in counts.iter().rev() {
            p.circle_filled(pos2(x + 4.0, y), 4.0, label_color(label));
            p.text(
                pos2(x + 14.0, y),
                Align2::LEFT_CENTER,
                format!("{label} {n}"),
                theme::regular(12.0),
                TEXT_2,
            );
            y -= 18.0;
        }
        p.text(
            pos2(x, y),
            Align2::LEFT_CENTER,
            head,
            theme::regular(12.0),
            TEXT_3,
        );
        // Hint under the pointer.
        let Some(ptr) = pointer else { return };
        let hit = self
            .markers
            .iter()
            .filter(|(m, _)| m.distance(ptr) < 8.0)
            .min_by(|a, b| a.0.distance(ptr).total_cmp(&b.0.distance(ptr)));
        let text = if let Some((_, i)) = hit {
            let e = &self.events[*i];
            let (hms, _) = local_hms(e.start_ms as f64);
            Some(format!(
                "{} · {} · {hms} ({}) · tap to replay",
                e.label,
                e.camera,
                crate::ago(now, e.start_ms as f64)
            ))
        } else {
            l.cameras
                .iter()
                .find(|c| sp(view, c.at).distance(ptr) < 14.0)
                .map(|c| format!("{} · tap for live view", c.camera))
        };
        if let Some(t) = text {
            p.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            let g = p.layout_no_wrap(t, theme::regular(12.0), TEXT);
            let mut r = Rect::from_min_size(ptr + vec2(14.0, 12.0), g.size() + vec2(16.0, 10.0));
            if r.right() > canvas.right() - 4.0 {
                r = r.translate(vec2(-(r.width() + 28.0), 0.0));
            }
            p.rect_filled(r, CornerRadius::same(6), SURFACE);
            p.rect_stroke(
                r,
                CornerRadius::same(6),
                Stroke::new(1.0, LINE),
                StrokeKind::Inside,
            );
            p.galley(r.min + vec2(8.0, 5.0), g, TEXT);
        }
    }

    /// A tap on the map: an event marker opens replay there, a camera its
    /// live view.
    fn map_tap(&mut self, view: &View, l: &Level, at: Pos2) {
        // Fingers are blunt: a little more slack than the mouse hint.
        let marker = self
            .markers
            .iter()
            .filter(|(m, _)| m.distance(at) < 11.0)
            .min_by(|a, b| a.0.distance(at).total_cmp(&b.0.distance(at)));
        if let Some((_, i)) = marker {
            let e = &self.events[*i];
            self.action = Some(MapAction::Replay {
                camera: e.camera.clone(),
                t_ms: e.start_ms as f64,
            });
            return;
        }
        if let Some(c) = l
            .cameras
            .iter()
            .filter(|c| sp(view, c.at).distance(at) < 18.0)
            .min_by(|a, b| {
                sp(view, a.at)
                    .distance(at)
                    .total_cmp(&sp(view, b.at).distance(at))
            })
        {
            self.action = Some(MapAction::Live(c.camera.clone()));
        }
    }

    fn opening_hint(&self, l: &Level, w: Point, tol: f64) -> Option<Hint> {
        let width = if self.tool == Tool::Door {
            self.door_width
        } else {
            self.window_width
        };
        let (wall, a, b, _) = l
            .walls
            .iter()
            .filter_map(|x| {
                let (a, b) = l.wall_ends(x)?;
                let d = argus_plan::geom::project(w, a, b).2;
                (d <= x.thickness / 2.0 + tol * 2.0).then_some((x.id, a, b, d))
            })
            .min_by(|x, y| x.3.total_cmp(&y.3))?;
        let len = a.dist(b);
        if len < width {
            return None;
        }
        let (t, _, _) = argus_plan::geom::project(w, a, b);
        let offset = ((t * len / 50.0).round() * 50.0).clamp(width / 2.0, len - width / 2.0);
        let u = b - a;
        let cross = u.x * (w.y - a.y) - u.y * (w.x - a.x);
        Some(Hint::Opening {
            wall,
            offset,
            side: if cross >= 0.0 {
                Side::Left
            } else {
                Side::Right
            },
        })
    }

    fn pointer_actions(
        &mut self,
        resp: &egui::Response,
        world: Option<Point>,
        hint: &Option<Hint>,
        tol: f64,
        cameras: &[String],
    ) {
        let ctx = resp.ctx.clone();
        let shift = ctx.input(|i| i.modifiers.shift);
        let snapped = match hint {
            Some(Hint::Snap(s)) => Some(*s),
            _ => None,
        };
        let level = self.level_id();
        match self.tool {
            Tool::Wall => {
                if resp.secondary_clicked() || resp.double_clicked() {
                    self.chain = None;
                } else if resp.clicked()
                    && let Some(s) = snapped
                {
                    let closes = self.chain.is_some_and(|(first, _)| {
                        matches!(s.kind, SnapKind::Node { .. }) && s.at.dist(first) < 1.0
                    });
                    self.wall_to(s.at, closes);
                }
            }
            Tool::Door | Tool::Window => {
                if resp.clicked()
                    && let Some(Hint::Opening { wall, offset, side }) = hint
                {
                    let door = self.tool == Tool::Door;
                    self.exec(Command::AddOpening {
                        level,
                        id: None,
                        wall: *wall,
                        kind: if door {
                            OpeningKind::Door
                        } else {
                            OpeningKind::Window
                        },
                        offset: *offset,
                        width: if door {
                            self.door_width
                        } else {
                            self.window_width
                        },
                        swing: door.then_some(Swing {
                            hinge: Hinge::Start,
                            side: *side,
                        }),
                    });
                }
            }
            Tool::Room => {
                if resp.clicked()
                    && let Some(s) = snapped
                {
                    let id = Id(self.doc.as_ref().unwrap().plan.next_id);
                    let n = self.level().map_or(0, |l| l.rooms.len()) + 1;
                    if self.exec(Command::AddRoom {
                        level,
                        id: Some(id),
                        name: format!("Room {n}"),
                        at: s.at,
                        polygon: None,
                    }) {
                        self.sel = vec![id];
                        self.focus_name = true;
                        self.set_tool(Tool::Select);
                    }
                }
            }
            Tool::Dimension => {
                if resp.clicked()
                    && let Some(s) = snapped
                {
                    match self.first_click.take() {
                        None => self.first_click = Some(s.at),
                        Some(a) if a.dist(s.at) > 1.0 => {
                            self.exec(Command::AddDimension {
                                level,
                                id: None,
                                a,
                                b: s.at,
                                offset: 600.0,
                            });
                        }
                        _ => {}
                    }
                }
            }
            Tool::Scale => {
                if resp.clicked()
                    && let Some(s) = snapped
                {
                    match (self.first_click, self.scale_to) {
                        (Some(a), None) if a.dist(s.at) > 1.0 => {
                            self.scale_to = Some(s.at);
                            self.scale_input.clear();
                            self.focus_name = true;
                        }
                        _ => {
                            self.first_click = Some(s.at);
                            self.scale_to = None;
                        }
                    }
                }
            }
            Tool::Camera => {
                if resp.drag_started_by(egui::PointerButton::Primary)
                    && let Some(s) = snapped
                {
                    // Where the button went down, not where the drag was noticed.
                    let at = ctx
                        .input(|i| i.pointer.press_origin())
                        .zip(self.view)
                        .zip(self.level())
                        .map(|((o, v), l)| snap(l, wp(&v, o), None, &self.snap_opts()).at)
                        .unwrap_or(s.at);
                    self.drag = Some(Drag::PlaceCamera { at });
                }
                let click_place = resp
                    .clicked()
                    .then_some(snapped)
                    .flatten()
                    .map(|s| (s.at, 90.0));
                let drag_place = match (&self.drag, resp.drag_stopped()) {
                    (Some(Drag::PlaceCamera { at }), true) => {
                        let to = world.unwrap_or(*at + pt(0.0, 1.0));
                        let h = if to.dist(*at) > 300.0 {
                            (at.angle_to(to) / 5.0).round() * 5.0
                        } else {
                            90.0
                        };
                        Some((*at, argus_plan::geom::norm_deg(h)))
                    }
                    _ => None,
                };
                if let Some((at, heading)) = click_place.or(drag_place) {
                    self.drag = None;
                    let name = if self.camera_pick.is_empty() {
                        "camera".to_string()
                    } else {
                        self.camera_pick.clone()
                    };
                    let id = Id(self.doc.as_ref().unwrap().plan.next_id);
                    if self.exec(Command::PlaceCamera {
                        level,
                        id: Some(id),
                        camera: name,
                        at,
                        heading,
                        fov: 90.0,
                        range: 10000.0,
                        mount_height: 2400.0,
                    }) {
                        self.sel = vec![id];
                        // Next one not yet on the plan.
                        let placed: Vec<String> = self
                            .doc
                            .as_ref()
                            .unwrap()
                            .plan
                            .levels
                            .iter()
                            .flat_map(|l| l.cameras.iter().map(|c| c.camera.clone()))
                            .collect();
                        if let Some(next) = cameras.iter().find(|c| !placed.contains(c)) {
                            self.camera_pick = next.clone();
                        }
                    }
                }
            }
            Tool::Select => self.select_actions(resp, world, tol, shift),
            Tool::Pan => {}
        }
    }

    fn select_actions(
        &mut self,
        resp: &egui::Response,
        world: Option<Point>,
        tol: f64,
        shift: bool,
    ) {
        let Some(w) = world else { return };
        let level = self.level_id();
        let Some(l) = self.doc.as_ref().and_then(|d| d.plan.level(level)).cloned() else {
            return;
        };
        let view = self.view.unwrap();
        if resp.clicked() {
            match plandoc::hit(&l, w, tol) {
                Some(id) if shift => {
                    if let Some(i) = self.sel.iter().position(|x| *x == id) {
                        self.sel.remove(i);
                    } else {
                        self.sel.push(id);
                    }
                }
                Some(id) => self.sel = vec![id],
                None if !shift => self.sel.clear(),
                None => {}
            }
            self.edit = None;
        }
        if resp.drag_started_by(egui::PointerButton::Primary) {
            let origin = resp
                .ctx
                .input(|i| i.pointer.press_origin())
                .or(resp.interact_pointer_pos());
            let press = origin.map(|p| wp(&view, p)).unwrap_or(w);
            // The heading handle of a selected camera.
            let handle = self.sel.iter().find_map(|id| {
                let c = l.camera(*id)?;
                let hp = sp(&view, c.at) + heading_vec(c.heading) * 34.0;
                (hp.distance(sp(&view, press)) < 10.0).then_some(c.id)
            });
            self.drag = Some(if let Some(camera) = handle {
                Drag::Heading { camera }
            } else {
                match plandoc::hit(&l, press, tol) {
                    Some(id) => {
                        if !self.sel.contains(&id) {
                            self.sel = vec![id];
                        }
                        let node = (self.sel.len() == 1 && l.node(id).is_some()).then_some(id);
                        Drag::Move {
                            from: press,
                            ids: self.sel.clone(),
                            node,
                        }
                    }
                    None => Drag::Box {
                        start: origin.unwrap_or_default(),
                    },
                }
            });
        }
        let plan = &self.doc.as_ref().unwrap().plan;
        // Live preview while dragging, committed on release.
        let pending: Option<Command> = match &self.drag {
            Some(Drag::Move { from, ids, node }) => match node {
                Some(n) => {
                    // Snap, ignoring the node being moved.
                    let mut tmp = l.clone();
                    tmp.walls.retain(|x| x.a != *n && x.b != *n);
                    tmp.nodes.retain(|x| x.id != *n);
                    let s = snap(&tmp, w, None, &self.snap_opts());
                    Some(Command::MoveNode {
                        level,
                        node: *n,
                        to: s.at,
                    })
                }
                None => {
                    let g = self.snap_opts().grid;
                    let d = w - *from;
                    let by = pt((d.x / g).round() * g, (d.y / g).round() * g);
                    (by.len() > 0.0).then(|| Command::MoveEntities {
                        level,
                        ids: ids.clone(),
                        by,
                    })
                }
            },
            Some(Drag::Heading { camera }) => l.camera(*camera).map(|c| {
                let mut c = c.clone();
                c.heading = argus_plan::geom::norm_deg((c.at.angle_to(w) / 5.0).round() * 5.0);
                Command::UpdateEntity {
                    level,
                    entity: Entity::Camera(c),
                }
            }),
            _ => None,
        };
        self.preview = pending.as_ref().and_then(|c| {
            let mut p = plan.clone();
            p.apply(c).ok().map(|_| p)
        });
        if resp.drag_stopped() {
            match self.drag.take() {
                Some(Drag::Box { start }) => {
                    if let Some(end) = resp.interact_pointer_pos().or(resp.hover_pos()) {
                        let (a, b) = (wp(&view, start), wp(&view, end));
                        let lo = pt(a.x.min(b.x), a.y.min(b.y));
                        let hi = pt(a.x.max(b.x), a.y.max(b.y));
                        let ids = plandoc::in_box(&l, lo, hi);
                        if shift {
                            for id in ids {
                                if !self.sel.contains(&id) {
                                    self.sel.push(id);
                                }
                            }
                        } else {
                            self.sel = ids;
                        }
                    }
                }
                Some(_) => {
                    if let Some(c) = pending {
                        self.exec(c);
                    }
                }
                None => {}
            }
            self.preview = None;
            self.edit = None;
        }
    }

    fn draw_hint(
        &self,
        p: &egui::Painter,
        view: &View,
        hint: &Option<Hint>,
        pointer: Option<Pos2>,
    ) {
        let Some(l) = self.level() else { return };
        match hint {
            Some(Hint::Snap(s)) => {
                let at = sp(view, s.at);
                let from = match self.tool {
                    Tool::Wall => self.chain.map(|c| c.1),
                    Tool::Dimension | Tool::Scale => self.first_click,
                    _ => None,
                };
                if let Some(f) = from {
                    let fs = sp(view, f);
                    let typed_end = (!self.typed.is_empty())
                        .then(|| plandoc::parse_typed(&self.typed))
                        .flatten()
                        .map(|(len, ang)| {
                            let a = ang.unwrap_or_else(|| (f.angle_to(s.at) / 15.0).round() * 15.0);
                            argus_plan::snap::polar(f, len, a)
                        });
                    let end = typed_end.unwrap_or(s.at);
                    let es = sp(view, end);
                    if self.tool == Tool::Wall {
                        let t = (self.wall_thickness as f32 * view.scale).max(2.0);
                        p.line_segment([fs, es], Stroke::new(t, WALL.gamma_multiply(0.5)));
                    }
                    p.line_segment([fs, es], Stroke::new(1.0, ACCENT));
                    let len = f.dist(end);
                    let ang = argus_plan::geom::norm_deg(f.angle_to(end));
                    let label = if self.typed.is_empty() {
                        format!("{} · {ang:.0}°", fmt_len(len))
                    } else {
                        format!("{}▏", self.typed)
                    };
                    let g = p.layout_no_wrap(label, theme::mono(11.5), TEXT);
                    let r = Rect::from_min_size(es + vec2(14.0, 10.0), g.size() + vec2(12.0, 6.0));
                    p.rect_filled(r, CornerRadius::same(6), SURFACE);
                    p.rect_stroke(
                        r,
                        CornerRadius::same(6),
                        Stroke::new(1.0, LINE),
                        StrokeKind::Inside,
                    );
                    p.galley(r.min + vec2(6.0, 3.0), g, TEXT);
                }
                match s.kind {
                    SnapKind::Node { .. } => {
                        p.rect_stroke(
                            Rect::from_center_size(at, vec2(11.0, 11.0)),
                            CornerRadius::ZERO,
                            Stroke::new(1.5, ACCENT),
                            StrokeKind::Middle,
                        );
                    }
                    SnapKind::Midpoint { .. } => {
                        p.add(Shape::convex_polygon(
                            vec![
                                at + vec2(0.0, -7.0),
                                at + vec2(6.0, 4.5),
                                at + vec2(-6.0, 4.5),
                            ],
                            Color32::TRANSPARENT,
                            Stroke::new(1.5, ACCENT),
                        ));
                    }
                    SnapKind::OnWall { .. } => {
                        let k = 5.0;
                        p.line_segment(
                            [at + vec2(-k, -k), at + vec2(k, k)],
                            Stroke::new(1.5, ACCENT),
                        );
                        p.line_segment(
                            [at + vec2(-k, k), at + vec2(k, -k)],
                            Stroke::new(1.5, ACCENT),
                        );
                    }
                    _ => {
                        p.circle_stroke(at, 4.0, Stroke::new(1.5, ACCENT));
                    }
                }
                if self.tool == Tool::Camera
                    && let Some(Drag::PlaceCamera { at: c }) = &self.drag
                    && let Some(ptr) = pointer
                {
                    p.line_segment([sp(view, *c), ptr], Stroke::new(1.0, ACCENT));
                }
            }
            Some(Hint::Opening { wall, offset, side }) => {
                let door = self.tool == Tool::Door;
                let o = argus_plan::Opening {
                    id: Id(0),
                    wall: *wall,
                    kind: if door {
                        OpeningKind::Door
                    } else {
                        OpeningKind::Window
                    },
                    offset: *offset,
                    width: if door {
                        self.door_width
                    } else {
                        self.window_width
                    },
                    swing: door.then_some(Swing {
                        hinge: Hinge::Start,
                        side: *side,
                    }),
                };
                draw_opening(p, view, l, &o, ACCENT);
            }
            None => {}
        }
    }

    fn props_ui(&mut self, ui: &mut egui::Ui, r: Rect, cameras: &[String]) {
        let p = ui.painter().clone();
        p.rect_filled(r, CornerRadius::ZERO, BG);
        p.vline(r.left() + 0.5, r.y_range(), Stroke::new(1.0, LINE));
        let inner = r.shrink2(vec2(16.0, 14.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            ui.style_mut().spacing.interact_size.y = 26.0;
            match (self.sel.len(), self.tool) {
                (0, Tool::Scale) => self.scale_props(ui),
                (0, Tool::Wall) => {
                    heading(ui, "Wall");
                    hint(ui, "Click to start, click for each corner. Type a length (3600, 3.6m or 3600<90) and press Enter. Esc or right-click ends.");
                    num(ui, "Thickness", &mut self.wall_thickness, 10.0..=2000.0, " mm", 5.0);
                    ui.horizontal(|ui| {
                        if btn(ui, "Outside 230").clicked() {
                            self.wall_thickness = 230.0;
                        }
                        if btn(ui, "Inside 90").clicked() {
                            self.wall_thickness = 90.0;
                        }
                    });
                }
                (0, Tool::Door) => {
                    heading(ui, "Door");
                    hint(ui, "Click on a wall. It swings to the side you click on; change it after.");
                    num(ui, "Width", &mut self.door_width, 300.0..=6000.0, " mm", 10.0);
                }
                (0, Tool::Window) => {
                    heading(ui, "Window");
                    hint(ui, "Click on a wall.");
                    num(ui, "Width", &mut self.window_width, 300.0..=6000.0, " mm", 10.0);
                }
                (0, Tool::Camera) => {
                    heading(ui, "Camera");
                    hint(ui, "Pick a camera, then click where it is, or press and drag towards where it looks.");
                    egui::ComboBox::from_id_salt("camera-pick")
                        .selected_text(self.camera_pick.clone())
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for c in cameras {
                                ui.selectable_value(&mut self.camera_pick, c.clone(), c);
                            }
                        });
                }
                (0, Tool::Dimension) => {
                    heading(ui, "Dimension");
                    hint(ui, "Click two points.");
                }
                (0, Tool::Room) => {
                    heading(ui, "Room");
                    hint(ui, "Click inside a room to name it. Its outline comes from the walls around it.");
                }
                (0, _) => self.level_props(ui),
                (1, _) => self.entity_props(ui, cameras),
                (n, _) => {
                    heading(ui, &format!("{n} selected"));
                    hint(ui, "Drag to move them together. Delete removes them.");
                    if btn(ui, "Delete").clicked() {
                        self.delete_selection();
                    }
                }
            }
        });
    }

    fn scale_props(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Set the image's scale");
        let to = self.scale_to;
        if self.first_click.is_none() || to.is_none() {
            hint(
                ui,
                "Click two points on the image a known distance apart (e.g. the ends of a dimensioned wall).",
            );
            if btn(ui, "Cancel").clicked() {
                self.set_tool(Tool::Select);
            }
            return;
        }
        hint(ui, "The real distance between the two points:");
        let edit = ui.add(
            egui::TextEdit::singleline(&mut self.scale_input)
                .hint_text("e.g. 3.6m")
                .desired_width(ui.available_width()),
        );
        if std::mem::take(&mut self.focus_name) {
            edit.request_focus();
        }
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (btn(ui, "Apply").clicked() || enter)
            && let (Some(a), Some(b)) = (self.first_click, to)
        {
            match argus_plan::snap::parse_length(&self.scale_input) {
                Some(real) => self.apply_scale(a, b, real),
                None => self.note("Type a distance such as 3600 or 3.6m."),
            }
        }
    }

    fn apply_scale(&mut self, a: Point, b: Point, real: f64) {
        let Some(bg) = self.level().and_then(|l| l.background.clone()) else {
            return;
        };
        let measured = a.dist(b);
        if measured < 1.0 {
            return;
        }
        let k = real / measured;
        // Scale about point a, so it stays put.
        let off = a + (bg.offset - a).scale(k);
        let new = Background {
            mm_per_px: bg.mm_per_px * k,
            offset: off,
            ..bg
        };
        if self.exec(Command::SetBackground {
            level: self.level_id(),
            background: Some(new),
        }) {
            self.set_tool(Tool::Select);
            self.note(format!(
                "Scale set: those points are {} apart.",
                fmt_len(real)
            ));
            self.fit();
        }
    }

    fn level_props(&mut self, ui: &mut egui::Ui) {
        let Some(l) = self.level().cloned() else {
            return;
        };
        heading(ui, "Level");
        let mut name = l.name.clone();
        let r = ui.add(egui::TextEdit::singleline(&mut name).desired_width(ui.available_width()));
        if r.changed() && !name.trim().is_empty() {
            self.exec(Command::RenameLevel { level: l.id, name });
        }
        let n_levels = self.doc.as_ref().map_or(1, |d| d.plan.levels.len());
        if n_levels > 1 && btn(ui, "Delete level").clicked() {
            self.exec(Command::DeleteLevel { level: l.id });
            self.after_history();
            self.view = None;
        }
        ui.add_space(8.0);
        heading(ui, "Background image");
        match l.background.clone() {
            None => {
                hint(ui, "A scanned plan or a sketch to trace over. PNG or JPEG.");
                if btn(ui, "Upload image…").clicked() {
                    self.upload(ui.ctx());
                }
            }
            Some(bg) => {
                let mut b = bg.clone();
                ui.add(egui::Slider::new(&mut b.opacity, 0.0..=1.0).text("Opacity"));
                num(
                    ui,
                    "Scale",
                    &mut b.mm_per_px,
                    0.001..=1000.0,
                    " mm/px",
                    0.01,
                );
                num(ui, "Left", &mut b.offset.x, -1e6..=1e6, " mm", 10.0);
                num(ui, "Top", &mut b.offset.y, -1e6..=1e6, " mm", 10.0);
                num(ui, "Rotation", &mut b.rotation, -360.0..=360.0, "°", 0.1);
                let busy = ui.ctx().dragged_id().is_some() || ui.ctx().text_edit_focused();
                if b != bg && !busy {
                    self.exec(Command::SetBackground {
                        level: l.id,
                        background: Some(b),
                    });
                } else if b != bg {
                    // Show it live while dragging; commit on release.
                    let mut p = self.doc.as_ref().unwrap().plan.clone();
                    let _ = p.apply(&Command::SetBackground {
                        level: l.id,
                        background: Some(b),
                    });
                    self.preview = Some(p);
                }
                ui.horizontal(|ui| {
                    if btn(ui, "Set scale").clicked() {
                        self.set_tool(Tool::Scale);
                    }
                    if btn(ui, "Replace…").clicked() {
                        self.upload(ui.ctx());
                    }
                    if btn(ui, "Remove").clicked() {
                        self.exec(Command::SetBackground {
                            level: l.id,
                            background: None,
                        });
                    }
                });
            }
        }
        ui.add_space(8.0);
        hint(
            ui,
            "Wheel to zoom, right-drag to pan. Ctrl+Z undoes, Ctrl+Shift+Z redoes. Keys: V W D N R M C H pick tools.",
        );
    }

    fn entity_props(&mut self, ui: &mut egui::Ui, cameras: &[String]) {
        let id = self.sel[0];
        let Some(l) = self
            .doc
            .as_ref()
            .and_then(|d| self.level.and_then(|lv| d.plan.level(lv)))
            .cloned()
        else {
            return;
        };
        let Some(cur) = l.entity(id) else {
            self.sel.clear();
            return;
        };
        if self.edit.as_ref().is_none_or(|e| e.id() != id) {
            self.edit = Some(cur.clone());
        }
        let mut e = self.edit.clone().unwrap();
        let level = l.id;
        match &mut e {
            Entity::Wall(w) => {
                heading(ui, "Wall");
                num(ui, "Thickness", &mut w.thickness, 10.0..=2000.0, " mm", 5.0);
                let len_now = l.wall_len(w);
                let mut len = match self.wall_len {
                    Some((wid, v)) if wid == id => v,
                    _ => len_now,
                };
                num(ui, "Length", &mut len, 10.0..=1e6, " mm", 10.0);
                self.wall_len = Some((id, len));
                let busy = ui.ctx().dragged_id().is_some() || ui.ctx().text_edit_focused();
                if !busy && (len - len_now).abs() > 0.5 {
                    // Keep node a, move node b along the wall.
                    if let Some((a, b)) = l.wall_ends(w) {
                        let to = argus_plan::snap::polar(a, len, a.angle_to(b));
                        self.exec(Command::MoveNode {
                            level,
                            node: w.b,
                            to,
                        });
                    }
                    self.wall_len = None;
                }
                ui.label(
                    egui::RichText::new(format!(
                        "{} openings",
                        l.openings.iter().filter(|o| o.wall == id).count()
                    ))
                    .color(TEXT_3),
                );
                if btn(ui, "Split in the middle").clicked() {
                    self.exec(Command::SplitWall {
                        level,
                        wall: id,
                        offset: len_now / 2.0,
                        node: None,
                        new_wall: None,
                    });
                }
            }
            Entity::Opening(o) => {
                heading(
                    ui,
                    if o.kind == OpeningKind::Door {
                        "Door"
                    } else {
                        "Window"
                    },
                );
                ui.horizontal(|ui| {
                    let mut door = o.kind == OpeningKind::Door;
                    ui.selectable_value(&mut door, true, "Door");
                    ui.selectable_value(&mut door, false, "Window");
                    let kind = if door {
                        OpeningKind::Door
                    } else {
                        OpeningKind::Window
                    };
                    if kind != o.kind {
                        o.kind = kind;
                        o.swing = door.then_some(Swing {
                            hinge: Hinge::Start,
                            side: Side::Left,
                        });
                    }
                });
                num(ui, "Width", &mut o.width, 100.0..=10000.0, " mm", 10.0);
                num(ui, "From wall start", &mut o.offset, 0.0..=1e6, " mm", 10.0);
                if o.kind == OpeningKind::Door {
                    let mut swings = o.swing.is_some();
                    ui.checkbox(&mut swings, "Swings (off: sliding or roller)");
                    if swings != o.swing.is_some() {
                        o.swing = swings.then_some(Swing {
                            hinge: Hinge::Start,
                            side: Side::Left,
                        });
                    }
                    if let Some(s) = &mut o.swing {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Hinge").color(TEXT_2));
                            ui.selectable_value(&mut s.hinge, Hinge::Start, "Start");
                            ui.selectable_value(&mut s.hinge, Hinge::End, "End");
                        });
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Opens").color(TEXT_2));
                            ui.selectable_value(&mut s.side, Side::Left, "Left");
                            ui.selectable_value(&mut s.side, Side::Right, "Right");
                        });
                    }
                }
            }
            Entity::Room(r) => {
                heading(ui, "Room");
                let ed = ui.add(
                    egui::TextEdit::singleline(&mut r.name).desired_width(ui.available_width()),
                );
                if std::mem::take(&mut self.focus_name) {
                    ed.request_focus();
                }
                match argus_plan::rooms::room_polygon(&l, r) {
                    Some(poly) => hint(ui, &format!("{:.1} m²", argus_plan::rooms::area_m2(&poly))),
                    None => hint(ui, "Not enclosed by walls yet."),
                }
            }
            Entity::Dimension(d) => {
                heading(ui, "Dimension");
                hint(ui, &fmt_len(d.a.dist(d.b)));
                num(ui, "Offset", &mut d.offset, -1e5..=1e5, " mm", 10.0);
            }
            Entity::Camera(c) => {
                heading(ui, "Camera");
                egui::ComboBox::from_id_salt("camera-name")
                    .selected_text(c.camera.clone())
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for n in cameras {
                            ui.selectable_value(&mut c.camera, n.clone(), n);
                        }
                    });
                num(ui, "Heading", &mut c.heading, -720.0..=720.0, "°", 1.0);
                num(ui, "Field of view", &mut c.fov, 1.0..=360.0, "°", 1.0);
                let mut range_m = c.range / 1000.0;
                num(ui, "Range", &mut range_m, 0.5..=200.0, " m", 0.1);
                c.range = range_m * 1000.0;
                num(
                    ui,
                    "Mount height",
                    &mut c.mount_height,
                    0.0..=20000.0,
                    " mm",
                    10.0,
                );
                hint(ui, "Drag the handle on the camera to aim it.");
            }
            Entity::Node(n) => {
                heading(ui, "Joint");
                num(ui, "x", &mut n.at.x, -1e6..=1e6, " mm", 10.0);
                num(ui, "y", &mut n.at.y, -1e6..=1e6, " mm", 10.0);
            }
        }
        // Commit once nothing is being dragged or typed.
        let busy = ui.ctx().dragged_id().is_some() || ui.ctx().text_edit_focused();
        if e != cur && !busy {
            if self.exec(Command::UpdateEntity {
                level,
                entity: e.clone(),
            }) {
                self.edit = None;
            } else {
                self.edit = Some(cur);
            }
        } else {
            self.edit = Some(e);
        }
        ui.add_space(6.0);
        if btn(ui, "Delete").clicked() {
            self.delete_selection();
        }
    }
}

/// ("HH:MM:SS", "YYYY-MM-DD") in the viewer's local time.
fn local_hms(ms: f64) -> (String, String) {
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

fn heading_vec(deg: f64) -> egui::Vec2 {
    let r = deg.to_radians();
    vec2(r.cos() as f32, -(r.sin() as f32))
}

/// A side-panel button, recorded for scripts like the toolbar's.
fn btn(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let r = ui.button(label);
    BUTTONS.with(|b| {
        b.borrow_mut().push((
            label.to_owned(),
            [r.rect.min.x, r.rect.min.y, r.rect.width(), r.rect.height()],
        ))
    });
    r
}

fn heading(ui: &mut egui::Ui, s: &str) {
    ui.label(
        egui::RichText::new(s)
            .font(theme::semibold(15.0))
            .color(TEXT),
    );
}

fn hint(ui: &mut egui::Ui, s: &str) {
    ui.label(
        egui::RichText::new(s)
            .font(theme::regular(12.5))
            .color(TEXT_2),
    );
}

fn num(
    ui: &mut egui::Ui,
    label: &str,
    v: &mut f64,
    range: std::ops::RangeInclusive<f64>,
    suffix: &str,
    speed: f64,
) {
    ui.horizontal(|ui| {
        ui.add_sized(
            vec2(96.0, 24.0),
            egui::Label::new(
                egui::RichText::new(label)
                    .font(theme::regular(12.5))
                    .color(TEXT_2),
            ),
        );
        ui.add(
            egui::DragValue::new(v)
                .range(range)
                .suffix(suffix)
                .speed(speed)
                .max_decimals(2),
        );
    });
}

/// A pill in a toolbar row, advancing `x`.
fn pill(
    ui: &mut egui::Ui,
    p: &egui::Painter,
    x: &mut f32,
    cy: f32,
    label: &str,
    on: bool,
    id: egui::Id,
) -> egui::Response {
    let g = p.layout_no_wrap(label.into(), theme::regular(12.5), TEXT);
    let r = Rect::from_min_size(pos2(*x, cy - 13.0), vec2(g.size().x + 22.0, 26.0));
    *x = r.right() + 6.0;
    draw_pill(ui, p, r, label, on, id)
}

fn draw_pill(
    ui: &mut egui::Ui,
    p: &egui::Painter,
    r: Rect,
    label: &str,
    on: bool,
    id: egui::Id,
) -> egui::Response {
    BUTTONS.with(|b| {
        b.borrow_mut()
            .push((label.to_owned(), [r.min.x, r.min.y, r.width(), r.height()]))
    });
    let resp = ui.interact(r, id, Sense::click());
    p.rect_filled(r, CornerRadius::same(theme::PILL_RADIUS), SURFACE);
    p.rect_stroke(
        r,
        CornerRadius::same(theme::PILL_RADIUS),
        Stroke::new(1.0, if on || resp.hovered() { ACCENT } else { LINE }),
        StrokeKind::Inside,
    );
    p.text(
        r.center(),
        Align2::CENTER_CENTER,
        label,
        theme::regular(12.5),
        if on || resp.hovered() { TEXT } else { TEXT_2 },
    );
    resp
}

fn draw_grid(p: &egui::Painter, v: &View, r: Rect) {
    let step = [100.0f32, 500.0, 1000.0, 5000.0, 10000.0]
        .into_iter()
        .find(|s| s * v.scale >= 22.0)
        .unwrap_or(10000.0);
    let lo = wp(v, r.left_bottom());
    let hi = wp(v, r.right_top());
    let major = |w: f64| (w / (step as f64 * 5.0)).round() * step as f64 * 5.0 == w;
    let mut x = (lo.x / step as f64).floor() * step as f64;
    while x <= hi.x {
        let sx = sp(v, pt(x, 0.0)).x;
        let c = if major(x) {
            LINE
        } else {
            LINE.gamma_multiply(0.45)
        };
        p.vline(sx, r.y_range(), Stroke::new(1.0, c));
        x += step as f64;
    }
    let mut y = (lo.y / step as f64).floor() * step as f64;
    while y <= hi.y {
        let sy = sp(v, pt(0.0, y)).y;
        let c = if major(y) {
            LINE
        } else {
            LINE.gamma_multiply(0.45)
        };
        p.hline(r.x_range(), sy, Stroke::new(1.0, c));
        y += step as f64;
    }
}

fn draw_background(p: &egui::Painter, v: &View, b: &Background, t: &Bg) {
    let (w, h) = (t.px[0] as f64 * b.mm_per_px, t.px[1] as f64 * b.mm_per_px);
    let (s, c) = b.rotation.to_radians().sin_cos();
    let rot = |x: f64, y: f64| b.offset + pt(x * c - y * s, x * s + y * c);
    let corners = [rot(0.0, 0.0), rot(w, 0.0), rot(w, -h), rot(0.0, -h)];
    let uvs = [
        pos2(0.0, 0.0),
        pos2(1.0, 0.0),
        pos2(1.0, 1.0),
        pos2(0.0, 1.0),
    ];
    let tint = Color32::WHITE.gamma_multiply(b.opacity as f32);
    let mut m = Mesh::with_texture(t.tex.id());
    for (q, uv) in corners.iter().zip(uvs) {
        m.vertices.push(egui::epaint::Vertex {
            pos: sp(v, *q),
            uv,
            color: tint,
        });
    }
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(Shape::mesh(m));
}

fn fill_poly(p: &egui::Painter, v: &View, poly: &[Point], color: Color32) {
    let mut m = Mesh::default();
    for q in poly {
        m.colored_vertex(sp(v, *q), color);
    }
    for t in plandoc::triangulate(poly) {
        m.add_triangle(t[0] as u32, t[1] as u32, t[2] as u32);
    }
    p.add(Shape::mesh(m));
}

/// Draws a level: rooms, walls, openings, dimensions, cameras.
pub fn draw_level(
    p: &egui::Painter,
    v: &View,
    l: &Level,
    sel: &[Id],
    hover: Option<Id>,
    editing: bool,
) {
    let hot = |id: Id| sel.contains(&id) || hover == Some(id);
    for r in &l.rooms {
        if let Some(poly) = argus_plan::rooms::room_polygon(l, r) {
            let c = if hot(r.id) {
                ACCENT.gamma_multiply(0.10)
            } else {
                ROOM
            };
            fill_poly(p, v, &poly, c);
        }
    }
    // Camera cones under the walls.
    for c in &l.cameras {
        let cone = camera_cone(c, 24);
        let col = if hot(c.id) {
            ACCENT.gamma_multiply(0.20)
        } else {
            ACCENT.gamma_multiply(0.09)
        };
        let pts: Vec<Pos2> = cone.iter().map(|q| sp(v, *q)).collect();
        let mut m = Mesh::default();
        for q in &pts {
            m.colored_vertex(*q, col);
        }
        for i in 1..pts.len() - 1 {
            m.add_triangle(0, i as u32, i as u32 + 1);
        }
        p.add(Shape::mesh(m));
        let edge = Stroke::new(1.0, ACCENT.gamma_multiply(0.35));
        p.line_segment([pts[0], pts[1]], edge);
        p.line_segment([pts[0], *pts.last().unwrap()], edge);
        p.add(Shape::line(
            pts[1..].to_vec(),
            Stroke::new(1.0, ACCENT.gamma_multiply(0.18)),
        ));
    }
    for w in &l.walls {
        let c = if hot(w.id) { ACCENT } else { WALL };
        for piece in argus_plan::outline::wall_pieces(l, w) {
            fill_poly(p, v, &piece, c);
        }
    }
    for o in &l.openings {
        let c = if hot(o.id) { ACCENT } else { TEXT_2 };
        draw_opening(p, v, l, o, c);
    }
    if editing {
        for n in &l.nodes {
            if hot(n.id) {
                p.circle_filled(sp(v, n.at), 4.0, ACCENT);
            }
        }
    }
    for d in &l.dimensions {
        let col = if hot(d.id) { ACCENT } else { TEXT_3 };
        let (a, b) = plandoc::offset_line(d.a, d.b, d.offset);
        let (sa, sb) = (sp(v, a), sp(v, b));
        let st = Stroke::new(1.0, col);
        p.line_segment([sp(v, d.a), sa], Stroke::new(1.0, col.gamma_multiply(0.5)));
        p.line_segment([sp(v, d.b), sb], Stroke::new(1.0, col.gamma_multiply(0.5)));
        p.line_segment([sa, sb], st);
        for e in [sa, sb] {
            p.line_segment([e + vec2(-4.0, 4.0), e + vec2(4.0, -4.0)], st);
        }
        let mid = sa + (sb - sa) / 2.0;
        let g = p.layout_no_wrap(fmt_len(d.a.dist(d.b)), theme::mono(11.0), col);
        let r = Rect::from_center_size(mid, g.size() + vec2(8.0, 2.0));
        p.rect_filled(r, CornerRadius::same(3), BG);
        p.galley(r.min + vec2(4.0, 1.0), g, col);
    }
    for r in &l.rooms {
        let at = sp(v, r.at);
        let col = if hot(r.id) { ACCENT } else { TEXT_2 };
        p.text(
            at,
            Align2::CENTER_BOTTOM,
            &r.name,
            theme::semibold(13.0),
            col,
        );
        if let Some(poly) = argus_plan::rooms::room_polygon(l, r) {
            p.text(
                at + vec2(0.0, 3.0),
                Align2::CENTER_TOP,
                format!("{:.1} m²", argus_plan::rooms::area_m2(&poly)),
                theme::mono(10.5),
                TEXT_3,
            );
        }
    }
    for c in &l.cameras {
        let at = sp(v, c.at);
        let col = if hot(c.id) {
            ACCENT
        } else {
            ACCENT.gamma_multiply(0.85)
        };
        let hv = heading_vec(c.heading);
        p.line_segment([at, at + hv * 14.0], Stroke::new(2.0, col));
        p.circle_filled(at, 7.0, SURFACE);
        p.circle_stroke(at, 7.0, Stroke::new(1.5, col));
        p.circle_filled(at, 2.5, col);
        // The name goes behind the camera, away from its cone and markers.
        let side = |v: f32| {
            if v > 0.3 {
                egui::Align::Max
            } else if v < -0.3 {
                egui::Align::Min
            } else {
                egui::Align::Center
            }
        };
        p.text(
            at - hv * 13.0,
            Align2([side(hv.x), side(hv.y)]),
            &c.camera,
            theme::mono(11.0),
            TEXT,
        );
        if editing && sel.contains(&c.id) {
            let h = at + hv * 34.0;
            p.line_segment([at, h], Stroke::new(1.0, ACCENT));
            p.circle_filled(h, 5.0, ACCENT);
        }
    }
}

fn draw_opening(p: &egui::Painter, v: &View, l: &Level, o: &argus_plan::Opening, col: Color32) {
    let Some((c, u, t)) = opening_frame(l, o) else {
        return;
    };
    let n = pt(-u.y, u.x);
    let hw = o.width / 2.0;
    let st = Stroke::new(1.2, col);
    let (j1, j2) = (c - u.scale(hw), c + u.scale(hw));
    // Jambs.
    for j in [j1, j2] {
        p.line_segment(
            [sp(v, j + n.scale(t / 2.0)), sp(v, j - n.scale(t / 2.0))],
            st,
        );
    }
    match o.kind {
        OpeningKind::Window => {
            let quad = [
                j1 + n.scale(t * 0.18),
                j2 + n.scale(t * 0.18),
                j2 - n.scale(t * 0.18),
                j1 - n.scale(t * 0.18),
            ];
            fill_poly(p, v, &quad, col.gamma_multiply(0.25));
            for k in [-0.18, 0.18] {
                p.line_segment([sp(v, j1 + n.scale(t * k)), sp(v, j2 + n.scale(t * k))], st);
            }
        }
        OpeningKind::Door => match o.swing {
            None => {
                p.line_segment(
                    [sp(v, j1), sp(v, j2)],
                    Stroke::new(1.0, col.gamma_multiply(0.6)),
                );
            }
            Some(s) => {
                let (hinge, other) = match s.hinge {
                    Hinge::Start => (j1, j2),
                    Hinge::End => (j2, j1),
                };
                let side = match s.side {
                    Side::Left => n,
                    Side::Right => n.scale(-1.0),
                };
                let leaf_end = hinge + side.scale(o.width);
                p.line_segment([sp(v, hinge), sp(v, leaf_end)], Stroke::new(1.6, col));
                let a0 = (leaf_end - hinge).y.atan2((leaf_end - hinge).x);
                let a1 = (other - hinge).y.atan2((other - hinge).x);
                let mut d = a1 - a0;
                while d > std::f64::consts::PI {
                    d -= std::f64::consts::TAU;
                }
                while d < -std::f64::consts::PI {
                    d += std::f64::consts::TAU;
                }
                let arc: Vec<Pos2> = (0..=16)
                    .map(|i| {
                        let a = a0 + d * i as f64 / 16.0;
                        sp(v, hinge + pt(a.cos(), a.sin()).scale(o.width))
                    })
                    .collect();
                p.add(Shape::dashed_line(
                    &arc,
                    Stroke::new(1.0, col.gamma_multiply(0.7)),
                    4.0,
                    3.0,
                ));
            }
        },
    }
}
