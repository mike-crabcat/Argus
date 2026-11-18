//! The editor's document: the plan as last saved, the local copy with
//! unsaved commands, undo and redo, plus hit testing and the view
//! transform. Plain Rust, tested natively; the egui side is in
//! `web/plan.rs`.

use argus_plan::{Command, Entity, Id, Level, Plan, PlanError, Point, Versioned, geom, pt};

/// The plan being edited.
#[derive(Clone, Debug)]
pub struct Doc {
    /// The server's version the local plan builds on.
    pub version: u64,
    /// The plan with every local command applied.
    pub plan: Plan,
    undo: Vec<Command>,
    redo: Vec<Command>,
    /// Commands not yet sent.
    pending: Vec<Command>,
    /// Commands sent and awaiting the server's answer.
    in_flight: Vec<Command>,
    /// An agent's draft being reviewed: how things were before it.
    draft: Option<DraftMark>,
}

#[derive(Clone, Debug)]
struct DraftMark {
    plan: Plan,
    undo: usize,
    pending: usize,
}

impl Doc {
    pub fn new(v: Versioned) -> Self {
        Doc {
            version: v.version,
            plan: v.plan,
            undo: Vec::new(),
            redo: Vec::new(),
            pending: Vec::new(),
            in_flight: Vec::new(),
            draft: None,
        }
    }

    /// Applies an agent's batch as a draft: one undoable step, not saved
    /// until accepted. Edits made meanwhile belong to the draft.
    pub fn begin_draft(&mut self, commands: Vec<Command>) -> Result<(), PlanError> {
        if self.draft.is_some() {
            return Err(PlanError::Invalid("finish the current draft first".into()));
        }
        let mark = DraftMark {
            plan: self.plan.clone(),
            undo: self.undo.len(),
            pending: self.pending.len(),
        };
        self.exec(Command::Batch { commands })?;
        self.draft = Some(mark);
        Ok(())
    }

    pub fn has_draft(&self) -> bool {
        self.draft.is_some()
    }

    /// Ids the draft added (to highlight them).
    pub fn draft_ids(&self) -> Vec<Id> {
        let Some(m) = &self.draft else {
            return Vec::new();
        };
        let mut ids = Vec::new();
        for l in &self.plan.levels {
            let all = l
                .nodes
                .iter()
                .map(|e| e.id)
                .chain(l.walls.iter().map(|e| e.id))
                .chain(l.openings.iter().map(|e| e.id))
                .chain(l.rooms.iter().map(|e| e.id))
                .chain(l.dimensions.iter().map(|e| e.id))
                .chain(l.cameras.iter().map(|e| e.id));
            ids.extend(all.filter(|id| !m.plan.uses(*id)));
        }
        ids
    }

    /// Keeps the draft (and any tweaks): it saves like any edit.
    pub fn accept_draft(&mut self) {
        self.draft = None;
    }

    /// Drops the draft and everything done since it arrived.
    pub fn discard_draft(&mut self) {
        if let Some(m) = self.draft.take() {
            self.plan = m.plan;
            self.undo.truncate(m.undo);
            self.pending.truncate(m.pending);
            self.redo.clear();
        }
    }

    /// Applies a command locally and queues it for saving.
    pub fn exec(&mut self, cmd: Command) -> Result<(), PlanError> {
        let inv = self.plan.apply(&cmd)?;
        self.undo.push(inv);
        self.redo.clear();
        self.pending.push(cmd);
        Ok(())
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Undoes the last edit (itself a command that gets saved).
    pub fn undo(&mut self) -> bool {
        self.step(true)
    }

    pub fn redo(&mut self) -> bool {
        self.step(false)
    }

    fn step(&mut self, back: bool) -> bool {
        let (from, to) = if back {
            (&mut self.undo, &mut self.redo)
        } else {
            (&mut self.redo, &mut self.undo)
        };
        let Some(cmd) = from.pop() else {
            return false;
        };
        match self.plan.apply(&cmd) {
            Ok(inv) => {
                to.push(inv);
                self.pending.push(cmd);
                true
            }
            Err(_) => {
                // Can't happen for our own inverses; drop the history.
                self.undo.clear();
                self.redo.clear();
                false
            }
        }
    }

    /// Whether there are changes the server hasn't confirmed.
    pub fn dirty(&self) -> bool {
        !self.pending.is_empty() || !self.in_flight.is_empty()
    }

    pub fn saving(&self) -> bool {
        !self.in_flight.is_empty()
    }

    /// The commands to send now (with the version they build on), if any.
    pub fn begin_save(&mut self) -> Option<(u64, Vec<Command>)> {
        if self.saving() || self.pending.is_empty() || self.draft.is_some() {
            return None;
        }
        self.in_flight = std::mem::take(&mut self.pending);
        Some((self.version, self.in_flight.clone()))
    }

    /// The server applied the batch.
    pub fn saved(&mut self, version: u64) {
        self.version = version;
        self.in_flight.clear();
    }

    /// The save failed (network): put the commands back in front.
    pub fn save_failed(&mut self) {
        let mut cmds = std::mem::take(&mut self.in_flight);
        cmds.append(&mut self.pending);
        self.pending = cmds;
    }

    /// Replaces everything with the server's plan (after a conflict).
    pub fn reload(&mut self, v: Versioned) {
        *self = Doc::new(v);
    }
}

/// World (mm, y up) to screen (points, y down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    /// Screen position of the world origin.
    pub origin: (f32, f32),
    /// Screen points per millimetre.
    pub scale: f32,
}

impl View {
    pub fn to_screen(&self, p: Point) -> (f32, f32) {
        (
            self.origin.0 + p.x as f32 * self.scale,
            self.origin.1 - p.y as f32 * self.scale,
        )
    }
    pub fn to_world(&self, s: (f32, f32)) -> Point {
        pt(
            ((s.0 - self.origin.0) / self.scale) as f64,
            ((self.origin.1 - s.1) / self.scale) as f64,
        )
    }
    /// Zooms by `factor` keeping screen point `at` fixed.
    pub fn zoom_at(&mut self, at: (f32, f32), factor: f32) {
        let w = self.to_world(at);
        self.scale = (self.scale * factor).clamp(0.002, 2.0);
        self.origin = (
            at.0 - w.x as f32 * self.scale,
            at.1 + w.y as f32 * self.scale,
        );
    }
    /// Fits world box (min, max) into a screen rect (x, y, w, h) with margin.
    pub fn fit(min: Point, max: Point, rect: (f32, f32, f32, f32), margin: f32) -> View {
        let (w, h) = (
            ((max.x - min.x) as f32).max(1000.0),
            ((max.y - min.y) as f32).max(1000.0),
        );
        let scale = ((rect.2 - 2.0 * margin) / w)
            .min((rect.3 - 2.0 * margin) / h)
            .max(0.002);
        let cx = (min.x + max.x) as f32 / 2.0;
        let cy = (min.y + max.y) as f32 / 2.0;
        View {
            origin: (
                rect.0 + rect.2 / 2.0 - cx * scale,
                rect.1 + rect.3 / 2.0 + cy * scale,
            ),
            scale,
        }
    }
}

/// The extent of a level's contents (walls, cameras with their range, rooms
/// and dimensions), or None if it's empty.
pub fn bounds(l: &Level, with_cones: bool) -> Option<(Point, Point)> {
    let mut pts: Vec<Point> = l.nodes.iter().map(|n| n.at).collect();
    for c in &l.cameras {
        pts.push(c.at);
        if with_cones {
            pts.extend(argus_plan::outline::camera_cone(c, 8));
        }
    }
    pts.extend(l.rooms.iter().map(|r| r.at));
    pts.extend(l.dimensions.iter().flat_map(|d| [d.a, d.b]));
    let first = *pts.first()?;
    Some(pts.iter().fold((first, first), |(lo, hi), p| {
        (
            pt(lo.x.min(p.x), lo.y.min(p.y)),
            pt(hi.x.max(p.x), hi.y.max(p.y)),
        )
    }))
}

/// The entity under world point `p` (within `tol` mm), most specific
/// first: cameras, nodes, openings, room labels, dimensions, walls.
pub fn hit(l: &Level, p: Point, tol: f64) -> Option<Id> {
    if let Some(c) = l
        .cameras
        .iter()
        .filter(|c| c.at.dist(p) <= tol * 1.5)
        .min_by(|a, b| a.at.dist(p).total_cmp(&b.at.dist(p)))
    {
        return Some(c.id);
    }
    if let Some(n) = l.node_near(p, tol) {
        return Some(n.id);
    }
    for o in &l.openings {
        if let Some((c, u, t)) = argus_plan::outline::opening_frame(l, o) {
            let d = p - c;
            let along = d.x * u.x + d.y * u.y;
            let across = -d.x * u.y + d.y * u.x;
            if along.abs() <= o.width / 2.0 && across.abs() <= t / 2.0 + tol {
                return Some(o.id);
            }
        }
    }
    if let Some(r) = l.rooms.iter().find(|r| r.at.dist(p) <= tol * 3.0) {
        return Some(r.id);
    }
    for d in &l.dimensions {
        let (a, b) = offset_line(d.a, d.b, d.offset);
        if geom::project(p, a, b).2 <= tol {
            return Some(d.id);
        }
    }
    l.walls
        .iter()
        .filter_map(|w| {
            let (a, b) = l.wall_ends(w)?;
            let d = geom::project(p, a, b).2;
            (d <= w.thickness / 2.0 + tol).then_some((d, w.id))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|x| x.1)
}

/// Entities whose anchor lies inside the world box (box select).
pub fn in_box(l: &Level, lo: Point, hi: Point) -> Vec<Id> {
    let inside = |p: Point| p.x >= lo.x && p.x <= hi.x && p.y >= lo.y && p.y <= hi.y;
    let mut ids = Vec::new();
    for w in &l.walls {
        if let Some((a, b)) = l.wall_ends(w)
            && inside(a)
            && inside(b)
        {
            ids.push(w.id);
        }
    }
    ids.extend(l.cameras.iter().filter(|c| inside(c.at)).map(|c| c.id));
    ids.extend(l.rooms.iter().filter(|r| inside(r.at)).map(|r| r.id));
    ids.extend(
        l.dimensions
            .iter()
            .filter(|d| inside(d.a) && inside(d.b))
            .map(|d| d.id),
    );
    ids
}

/// A dimension's drawn line: a-b moved `offset` mm to its left.
pub fn offset_line(a: Point, b: Point, offset: f64) -> (Point, Point) {
    let u = b - a;
    let l = u.len().max(1e-9);
    let n = pt(-u.y / l, u.x / l).scale(offset);
    (a + n, b + n)
}

/// A typed entry while drawing: "3600" (a length, mm), "3.6m", or
/// "3600<90" (length and angle in degrees).
pub fn parse_typed(s: &str) -> Option<(f64, Option<f64>)> {
    let (len, ang) = match s.split_once('<') {
        Some((l, a)) => (l, Some(a.trim().parse::<f64>().ok()?)),
        None => (s, None),
    };
    Some((argus_plan::snap::parse_length(len)?, ang))
}

/// "4.25 m" for lengths, to the centimetre.
pub fn fmt_len(mm: f64) -> String {
    if mm.abs() >= 1000.0 {
        format!("{:.2} m", mm / 1000.0)
    } else {
        format!("{mm:.0} mm")
    }
}

/// The entity, by id, on any level: (level, entity).
pub fn find(p: &Plan, id: Id) -> Option<(Id, Entity)> {
    p.levels
        .iter()
        .find_map(|l| l.entity(id).map(|e| (l.id, e)))
}

/// An event as `/api/events` lists it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct MapEvent {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub summary: Option<String>,
    pub camera: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub label: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct EventList {
    pub events: Vec<MapEvent>,
}

/// Where the `i`th newest of a camera's event markers goes, in screen
/// points from the camera icon: rows fanning out along its heading, the
/// newest nearest. `heading` and `fov` in degrees; y points down.
pub fn marker_offset(i: usize, heading: f64, fov: f64) -> (f32, f32) {
    // Row r holds 3 + 2r markers.
    let (mut row, mut first) = (0usize, 0usize);
    while i >= first + 3 + 2 * row {
        first += 3 + 2 * row;
        row += 1;
    }
    let n = 3 + 2 * row;
    let k = i - first;
    let spread = fov.clamp(30.0, 110.0);
    // Middle first, then alternating outwards, so few markers sit centred.
    let order = {
        let mid = (n - 1) as f64 / 2.0;
        let step = (k as f64 / 2.0).ceil();
        let pos = if k % 2 == 1 { mid - step } else { mid + step };
        pos / (n - 1).max(1) as f64
    };
    let a = (heading + (order - 0.5) * spread).to_radians();
    let r = 30.0 + 14.0 * row as f64;
    ((r * a.cos()) as f32, (-r * a.sin()) as f32)
}

/// How opaque an event marker is: 1 when new, fading to 0.2 at the end of
/// the window.
pub fn marker_alpha(now_ms: f64, start_ms: f64, window_ms: f64) -> f32 {
    let age = ((now_ms - start_ms) / window_ms).clamp(0.0, 1.0);
    (1.0 - 0.8 * age) as f32
}

/// Triangles (vertex indices) filling a simple polygon, by ear clipping.
/// Works for concave outlines such as L-shaped rooms.
pub fn triangulate(poly: &[Point]) -> Vec<[usize; 3]> {
    let n = poly.len();
    if n < 3 {
        return Vec::new();
    }
    let mut idx: Vec<usize> = (0..n).collect();
    if geom::signed_area(poly) < 0.0 {
        idx.reverse();
    }
    let cross =
        |a: Point, b: Point, c: Point| (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
    let mut out = Vec::new();
    let mut guard = 0;
    while idx.len() > 3 && guard < n * n {
        guard += 1;
        let m = idx.len();
        let ear = (0..m).find(|&i| {
            let (a, b, c) = (idx[(i + m - 1) % m], idx[i], idx[(i + 1) % m]);
            if cross(poly[a], poly[b], poly[c]) <= 1e-9 {
                return false;
            }
            !idx.iter().any(|&j| {
                j != a && j != b && j != c && geom::contains(&[poly[a], poly[b], poly[c]], poly[j])
            })
        });
        match ear {
            Some(i) => {
                out.push([idx[(i + m - 1) % m], idx[i], idx[(i + 1) % m]]);
                idx.remove(i);
            }
            None => break,
        }
    }
    if idx.len() == 3 {
        out.push([idx[0], idx[1], idx[2]]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_plan::fixture::demo_house;

    fn room(level: Id, name: &str) -> Command {
        Command::AddRoom {
            level,
            id: None,
            name: name.into(),
            at: pt(0.0, 0.0),
            polygon: None,
        }
    }

    #[test]
    fn undo_redo_and_saving() {
        let mut d = Doc::new(Versioned {
            version: 3,
            plan: Plan::default(),
        });
        let lv = d.plan.levels[0].id;
        d.exec(room(lv, "A")).unwrap();
        d.exec(room(lv, "B")).unwrap();
        assert_eq!(d.plan.levels[0].rooms.len(), 2);
        assert!(d.undo());
        assert_eq!(d.plan.levels[0].rooms.len(), 1);
        assert!(d.redo());
        assert_eq!(d.plan.levels[0].rooms.len(), 2);
        assert!(d.undo() && d.undo() && !d.undo());
        assert!(d.plan.levels[0].rooms.is_empty());
        assert!(d.dirty());
        // Everything (edits and their undos) goes to the server in order,
        // so it ends up with the same plan.
        let (base, cmds) = d.begin_save().unwrap();
        assert_eq!(base, 3);
        assert!(d.begin_save().is_none(), "one save at a time");
        let mut server = Plan::default();
        server.apply_all(&cmds).unwrap();
        assert_eq!(server, d.plan);
        // A failed save keeps the commands, ahead of newer ones.
        d.exec(room(lv, "C")).unwrap();
        d.save_failed();
        let (_, again) = d.begin_save().unwrap();
        assert_eq!(again.len(), cmds.len() + 1);
        d.saved(4);
        assert!(!d.dirty());
        assert_eq!(d.version, 4);
        // A new edit clears redo.
        d.undo();
        d.exec(room(lv, "D")).unwrap();
        assert!(!d.can_redo());
    }

    #[test]
    fn drafts_accept_or_discard_as_one_step() {
        let mut d = Doc::new(Versioned {
            version: 0,
            plan: Plan::default(),
        });
        let lv = d.plan.levels[0].id;
        d.exec(room(lv, "Mine")).unwrap();
        let (_, first) = d.begin_save().unwrap();
        d.saved(1);
        let house = argus_plan::fixture::demo_house_commands(lv, 1000);
        d.begin_draft(house.clone()).unwrap();
        assert!(d.has_draft() && d.dirty());
        assert!(d.begin_save().is_none(), "drafts aren't saved");
        assert!(d.draft_ids().len() > 50);
        // A tweak during review, then discard: back to before the draft.
        d.exec(room(lv, "Tweak")).unwrap();
        d.discard_draft();
        assert_eq!(d.plan.levels[0].rooms.len(), 1);
        assert!(!d.dirty() && !d.can_redo());
        // Accept: one undo step, then saved with the tweak.
        d.begin_draft(house).unwrap();
        d.exec(room(lv, "Tweak")).unwrap();
        d.accept_draft();
        let (_, cmds) = d.begin_save().unwrap();
        assert_eq!(cmds.len(), 2, "the batch and the tweak");
        let mut server = Plan::default();
        server.apply_all(&first).unwrap();
        server.apply_all(&cmds).unwrap();
        assert_eq!(server, d.plan);
        d.undo();
        d.undo();
        assert_eq!(
            d.plan.levels[0].rooms.len(),
            1,
            "two undos remove tweak and draft"
        );
    }

    #[test]
    fn view_round_trips_and_zooms_about_a_point() {
        let mut v = View {
            origin: (100.0, 500.0),
            scale: 0.05,
        };
        let p = pt(4000.0, 3000.0);
        let s = v.to_screen(p);
        assert_eq!(s, (300.0, 350.0));
        assert!(v.to_world(s).dist(p) < 1e-3);
        v.zoom_at((300.0, 350.0), 2.0);
        let s2 = v.to_screen(p);
        assert!((s2.0 - 300.0).abs() < 1e-3 && (s2.1 - 350.0).abs() < 1e-3);
        let f = View::fit(
            pt(0.0, 0.0),
            pt(16000.0, 11000.0),
            (0.0, 0.0, 1600.0, 1100.0),
            50.0,
        );
        let (a, b) = (f.to_screen(pt(0.0, 11000.0)), f.to_screen(pt(16000.0, 0.0)));
        assert!(a.0 >= 49.0 && a.1 >= 49.0 && b.0 <= 1551.0 && b.1 <= 1051.0);
    }

    #[test]
    fn hits_and_boxes() {
        let p = demo_house();
        let l = &p.levels[0];
        let cam = l.cameras.iter().find(|c| c.camera == "garage").unwrap();
        assert_eq!(hit(l, cam.at + pt(50.0, 0.0), 100.0), Some(cam.id));
        // The front door's centre.
        let door = hit(l, pt(5000.0, 0.0), 50.0).unwrap();
        assert!(l.opening(door).is_some());
        // Mid-wall: the wall.
        let w = hit(l, pt(16000.0, 3000.0), 50.0).unwrap();
        assert!(l.wall(w).is_some());
        assert_eq!(hit(l, pt(30000.0, 30000.0), 50.0), None);
        let all = in_box(l, pt(-5000.0, -5000.0), pt(25000.0, 25000.0));
        assert_eq!(
            all.len(),
            l.walls.len() + 5 + l.rooms.len() + l.dimensions.len()
        );
        let (lo, hi) = bounds(l, false).unwrap();
        assert!(lo.x <= 0.0 && hi.x >= 16000.0);
    }

    #[test]
    fn triangulates_an_l_shape() {
        let l = [
            pt(0.0, 0.0),
            pt(2.0, 0.0),
            pt(2.0, 1.0),
            pt(1.0, 1.0),
            pt(1.0, 2.0),
            pt(0.0, 2.0),
        ];
        let t = triangulate(&l);
        assert_eq!(t.len(), 4);
        let area: f64 = t
            .iter()
            .map(|t| geom::signed_area(&[l[t[0]], l[t[1]], l[t[2]]]).abs())
            .sum();
        assert!((area - 3.0).abs() < 1e-9);
        // Every room of the demo house fills to its full area.
        let p = demo_house();
        let lv = &p.levels[0];
        for r in &lv.rooms {
            let poly = argus_plan::rooms::room_polygon(lv, r).unwrap();
            let a: f64 = triangulate(&poly)
                .iter()
                .map(|t| geom::signed_area(&[poly[t[0]], poly[t[1]], poly[t[2]]]).abs())
                .sum();
            assert!(
                (a - geom::signed_area(&poly).abs()).abs() < 1.0,
                "{}",
                r.name
            );
        }
    }

    #[test]
    fn markers_fan_out_without_overlapping() {
        let pts: Vec<(f32, f32)> = (0..40).map(|i| marker_offset(i, 90.0, 90.0)).collect();
        // The first sits straight ahead (up, since y points down).
        assert!(pts[0].0.abs() < 1e-3 && pts[0].1 < -29.0, "{:?}", pts[0]);
        for (i, a) in pts.iter().enumerate() {
            for b in &pts[i + 1..] {
                let d = ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
                assert!(d > 9.0, "{a:?} {b:?}");
            }
        }
        assert_eq!(marker_alpha(1000.0, 1000.0, 3_600_000.0), 1.0);
        assert!((marker_alpha(3_601_000.0, 1000.0, 3_600_000.0) - 0.2).abs() < 1e-6);
        let list: EventList = serde_json::from_str(
            r#"{"events":[{"camera":"a","start_ms":1,"end_ms":2,"source":"reolink","label":"person"}]}"#,
        )
        .unwrap();
        assert_eq!(list.events[0].label, "person");
    }

    #[test]
    fn typed_entries() {
        assert_eq!(parse_typed("3600"), Some((3600.0, None)));
        assert_eq!(parse_typed("3.6m<90"), Some((3600.0, Some(90.0))));
        assert_eq!(parse_typed("<90"), None);
        assert_eq!(fmt_len(4250.0), "4.25 m");
        assert_eq!(fmt_len(90.0), "90 mm");
    }
}
