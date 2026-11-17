//! Every change to a plan is a [`Command`]: the editor, the server and
//! (later) an import agent all use the same ones. [`Plan::apply`] checks a
//! command, applies it all or not at all, and returns its inverse, which
//! undoes it exactly; applying that returns the inverse's inverse (redo).
//!
//! Commands are JSON (`{"op": "add_wall", ...}`); see
//! `docs/plan-commands.md` and [`command_schema`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::geom::{Point, project};
use crate::model::*;

/// How close (mm) a point must be to a node or wall to join it.
pub const JOIN_TOL: f64 = 1.0;
/// Walls thinner or thicker than this are refused, mm.
pub const THICKNESS_RANGE: (f64, f64) = (10.0, 2000.0);

/// A wall end: an existing node, or a point (joined to a node or wall
/// within 1 mm, else a new node).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum End {
    Node(Id),
    At(Point),
}

/// One edit. Fields named `id` are optional: give one to choose the new
/// entity's id (it must be unused), or leave it out to get the next free one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    /// Adds a level (storey or area such as "Yard") at the end.
    AddLevel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
        name: String,
    },
    RenameLevel {
        level: Id,
        name: String,
    },
    /// Deletes a level and everything on it. The last level can't go.
    DeleteLevel {
        level: Id,
    },
    /// Puts a deleted level back at `index` (the inverse of DeleteLevel).
    RestoreLevel {
        index: usize,
        level: Level,
    },
    /// Replaces a level's whole contents (same id). Inverse of the
    /// structural edits; also handy for bulk import.
    ReplaceLevel {
        level: Level,
    },
    /// A straight wall from `a` to `b` (centreline). Ends given as points
    /// join an existing node or wall there, so chains and T-junctions
    /// connect.
    AddWall {
        level: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
        a: End,
        b: End,
        thickness: f64,
    },
    /// Moves a wall node (and so every wall ending there).
    MoveNode {
        level: Id,
        node: Id,
        to: Point,
    },
    /// Splits a wall `offset` mm from its node `a` with a new node; the
    /// part beyond becomes a new wall. Openings follow their part.
    SplitWall {
        level: Id,
        wall: Id,
        offset: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_wall: Option<Id>,
    },
    /// A door or window in a wall. `offset` is from the wall's node `a` to
    /// the opening's centre.
    AddOpening {
        level: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
        wall: Id,
        kind: OpeningKind,
        offset: f64,
        width: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        swing: Option<Swing>,
    },
    /// A named room around point `at` (or with an explicit outline).
    AddRoom {
        level: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
        name: String,
        at: Point,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        polygon: Option<Vec<Point>>,
    },
    SetRoomName {
        level: Id,
        room: Id,
        name: String,
    },
    AddDimension {
        level: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
        a: Point,
        b: Point,
        #[serde(default)]
        offset: f64,
    },
    /// Places a camera from the config (by name) on the plan.
    PlaceCamera {
        level: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
        camera: String,
        at: Point,
        heading: f64,
        fov: f64,
        range: f64,
        mount_height: f64,
    },
    /// Replaces an entity with new values (same id and type): any field of
    /// a wall, opening, room, dimension or camera, or a node's position.
    UpdateEntity {
        level: Id,
        entity: Entity,
    },
    /// Deletes an entity. A node takes its walls with it; a wall takes its
    /// openings; nodes left without walls go too.
    DeleteEntity {
        level: Id,
        id: Id,
    },
    /// Moves entities by `by`: nodes, walls (their nodes), rooms,
    /// dimensions and cameras. Openings stay in their walls.
    MoveEntities {
        level: Id,
        ids: Vec<Id>,
        by: Point,
    },
    SetBackground {
        level: Id,
        background: Option<Background>,
    },
    /// Several commands applied together: all or none.
    Batch {
        commands: Vec<Command>,
    },
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PlanError {
    #[error("no level {0}")]
    NoLevel(Id),
    #[error("no entity {0} on that level")]
    NoEntity(Id),
    #[error("{0} is not a {1}")]
    WrongType(Id, &'static str),
    #[error("id {0} is already used")]
    DuplicateId(Id),
    #[error("{0}")]
    Invalid(String),
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, PlanError> {
    Err(PlanError::Invalid(msg.into()))
}

fn finite(v: f64, what: &str) -> Result<(), PlanError> {
    if v.is_finite() {
        Ok(())
    } else {
        invalid(format!("{what} must be a number"))
    }
}

fn point(p: Point, what: &str) -> Result<(), PlanError> {
    if p.is_finite() {
        Ok(())
    } else {
        invalid(format!("{what} must be a finite point"))
    }
}

fn thickness(t: f64) -> Result<(), PlanError> {
    finite(t, "thickness")?;
    if !(THICKNESS_RANGE.0..=THICKNESS_RANGE.1).contains(&t) {
        return invalid(format!(
            "wall thickness must be {} to {} mm",
            THICKNESS_RANGE.0, THICKNESS_RANGE.1
        ));
    }
    Ok(())
}

fn camera_fields(
    fov: f64,
    range: f64,
    heading: f64,
    mount: f64,
    at: Point,
) -> Result<(), PlanError> {
    point(at, "camera position")?;
    finite(heading, "heading")?;
    finite(mount, "mount height")?;
    if !(fov > 0.0 && fov <= 360.0) {
        return invalid("field of view must be over 0 and at most 360 degrees");
    }
    if !(range > 0.0 && range.is_finite()) {
        return invalid("range must be positive");
    }
    Ok(())
}

/// Whether an opening of `width` centred at `offset` fits on `wall`
/// without leaving it or overlapping another (other than `except`).
fn opening_fits(
    l: &Level,
    wall: Id,
    offset: f64,
    width: f64,
    except: Option<Id>,
) -> Result<(), PlanError> {
    finite(offset, "offset")?;
    finite(width, "width")?;
    let w = l.wall(wall).ok_or(PlanError::NoEntity(wall))?;
    if width <= 0.0 {
        return invalid("an opening needs a width");
    }
    let len = l.wall_len(w);
    if offset - width / 2.0 < -1e-6 || offset + width / 2.0 > len + 1e-6 {
        return invalid(format!(
            "the opening ({width:.0} mm at {offset:.0} mm) doesn't fit in the {len:.0} mm wall"
        ));
    }
    for o in l
        .openings
        .iter()
        .filter(|o| o.wall == wall && Some(o.id) != except)
    {
        if (o.offset - offset).abs() < (o.width + width) / 2.0 - 1e-6 {
            return invalid(format!("the opening would overlap opening {}", o.id));
        }
    }
    Ok(())
}

impl Plan {
    /// A fresh id: the one asked for if it's free, else the next one.
    fn fresh(&mut self, want: Option<Id>) -> Result<Id, PlanError> {
        match want {
            Some(id) if id.0 == 0 || self.uses(id) => Err(PlanError::DuplicateId(id)),
            Some(id) => {
                self.next_id = self.next_id.max(id.0 + 1);
                Ok(id)
            }
            None => Ok(self.alloc()),
        }
    }

    fn lvl(&self, id: Id) -> Result<&Level, PlanError> {
        self.level(id).ok_or(PlanError::NoLevel(id))
    }

    fn lvl_mut(&mut self, id: Id) -> Result<&mut Level, PlanError> {
        self.level_mut(id).ok_or(PlanError::NoLevel(id))
    }

    /// Applies `cmd` (all of it or, on error, none) and returns the command
    /// that undoes it.
    pub fn apply(&mut self, cmd: &Command) -> Result<Command, PlanError> {
        let backup = self.clone();
        let r = self.apply_inner(cmd);
        if r.is_err() {
            *self = backup;
        }
        r
    }

    /// Applies several commands as one; returns the inverse batch.
    pub fn apply_all(&mut self, cmds: &[Command]) -> Result<Command, PlanError> {
        self.apply(&Command::Batch {
            commands: cmds.to_vec(),
        })
    }

    fn snapshot(&self, level: Id) -> Result<Command, PlanError> {
        Ok(Command::ReplaceLevel {
            level: self.lvl(level)?.clone(),
        })
    }

    fn apply_inner(&mut self, cmd: &Command) -> Result<Command, PlanError> {
        use Command as C;
        match cmd {
            C::Batch { commands } => {
                let mut inv = Vec::with_capacity(commands.len());
                for c in commands {
                    inv.push(self.apply_inner(c)?);
                }
                inv.reverse();
                Ok(C::Batch { commands: inv })
            }
            C::AddLevel { id, name } => {
                let id = self.fresh(*id)?;
                self.levels.push(Level::new(id, name.trim()));
                Ok(C::DeleteLevel { level: id })
            }
            C::RenameLevel { level, name } => {
                let l = self.lvl_mut(*level)?;
                let old = std::mem::replace(&mut l.name, name.trim().to_owned());
                Ok(C::RenameLevel {
                    level: *level,
                    name: old,
                })
            }
            C::DeleteLevel { level } => {
                let index = self
                    .levels
                    .iter()
                    .position(|l| l.id == *level)
                    .ok_or(PlanError::NoLevel(*level))?;
                if self.levels.len() == 1 {
                    return invalid("a plan keeps at least one level");
                }
                let level = self.levels.remove(index);
                Ok(C::RestoreLevel { index, level })
            }
            C::RestoreLevel { index, level } => {
                if self.uses(level.id) {
                    return Err(PlanError::DuplicateId(level.id));
                }
                self.check_level(level, None)?;
                let index = (*index).min(self.levels.len());
                self.levels.insert(index, level.clone());
                Ok(C::DeleteLevel { level: level.id })
            }
            C::ReplaceLevel { level } => {
                let inv = self.snapshot(level.id)?;
                self.check_level(level, Some(level.id))?;
                *self.lvl_mut(level.id)? = level.clone();
                Ok(inv)
            }
            C::AddWall {
                level,
                id,
                a,
                b,
                thickness: t,
            } => {
                thickness(*t)?;
                let inv = self.snapshot(*level)?;
                let id = self.fresh(*id)?;
                let na = self.resolve_end(*level, a)?;
                let nb = self.resolve_end(*level, b)?;
                if na == nb {
                    return invalid("a wall needs two different ends");
                }
                let l = self.lvl(*level)?;
                let (pa, pb) = (l.node(na).unwrap().at, l.node(nb).unwrap().at);
                if pa.dist(pb) < JOIN_TOL {
                    return invalid("a wall needs some length");
                }
                // Join every wall it crosses, and pass through nodes on its
                // way: one wall per stretch between joints.
                let crossings: Vec<Point> = l
                    .walls
                    .iter()
                    .filter_map(|w| {
                        let (c, d) = l.wall_ends(w)?;
                        crate::geom::segments_cross(pa, pb, c, d)
                            .then(|| crate::geom::intersection(pa, pb, c, d))
                            .flatten()
                    })
                    .collect();
                for x in crossings {
                    self.resolve_end(*level, &End::At(x))?;
                }
                let l = self.lvl_mut(*level)?;
                let len = pa.dist(pb);
                let mut stops: Vec<(f64, Id)> = l
                    .nodes
                    .iter()
                    .filter_map(|n| {
                        let (t, _, d) = project(n.at, pa, pb);
                        (n.id == na
                            || n.id == nb
                            || (d <= JOIN_TOL && t * len > JOIN_TOL && (1.0 - t) * len > JOIN_TOL))
                            .then_some((t, n.id))
                    })
                    .collect();
                stops.sort_by(|x, y| x.0.total_cmp(&y.0));
                if stops.first().map(|s| s.1) != Some(na) {
                    stops.reverse();
                }
                let pairs: Vec<(Id, Id)> = stops
                    .windows(2)
                    .map(|p| (p[0].1, p[1].1))
                    .filter(|&(u, v)| {
                        !l.walls
                            .iter()
                            .any(|w| (w.a == u && w.b == v) || (w.a == v && w.b == u))
                    })
                    .collect();
                let added = pairs.len();
                for (i, (u, v)) in pairs.into_iter().enumerate() {
                    let wid = if i == 0 { id } else { self.alloc() };
                    self.lvl_mut(*level)?.walls.push(Wall {
                        id: wid,
                        a: u,
                        b: v,
                        thickness: *t,
                    });
                }
                if added == 0 {
                    return invalid("there is already a wall there");
                }
                Ok(inv)
            }
            C::MoveNode { level, node, to } => {
                point(*to, "position")?;
                let l = self.lvl_mut(*level)?;
                let n = l
                    .nodes
                    .iter_mut()
                    .find(|n| n.id == *node)
                    .ok_or(PlanError::NoEntity(*node))?;
                let old = std::mem::replace(&mut n.at, *to);
                Ok(C::MoveNode {
                    level: *level,
                    node: *node,
                    to: old,
                })
            }
            C::SplitWall {
                level,
                wall,
                offset,
                node,
                new_wall,
            } => {
                finite(*offset, "offset")?;
                let inv = self.snapshot(*level)?;
                let node = self.fresh(*node)?;
                let new_wall = self.fresh(*new_wall)?;
                self.split(*level, *wall, *offset, node, new_wall)?;
                Ok(inv)
            }
            C::AddOpening {
                level,
                id,
                wall,
                kind,
                offset,
                width,
                swing,
            } => {
                opening_fits(self.lvl(*level)?, *wall, *offset, *width, None)?;
                if *kind == OpeningKind::Window && swing.is_some() {
                    return invalid("windows don't swing");
                }
                let id = self.fresh(*id)?;
                self.lvl_mut(*level)?.openings.push(Opening {
                    id,
                    wall: *wall,
                    kind: *kind,
                    offset: *offset,
                    width: *width,
                    swing: *swing,
                });
                Ok(C::DeleteEntity { level: *level, id })
            }
            C::AddRoom {
                level,
                id,
                name,
                at,
                polygon,
            } => {
                point(*at, "room position")?;
                if let Some(p) = polygon {
                    check_polygon(p)?;
                }
                self.lvl(*level)?;
                let id = self.fresh(*id)?;
                self.lvl_mut(*level)?.rooms.push(Room {
                    id,
                    name: name.trim().to_owned(),
                    at: *at,
                    polygon: polygon.clone(),
                });
                Ok(C::DeleteEntity { level: *level, id })
            }
            C::SetRoomName { level, room, name } => {
                let l = self.lvl_mut(*level)?;
                let r = l
                    .rooms
                    .iter_mut()
                    .find(|r| r.id == *room)
                    .ok_or(PlanError::NoEntity(*room))?;
                let old = std::mem::replace(&mut r.name, name.trim().to_owned());
                Ok(C::SetRoomName {
                    level: *level,
                    room: *room,
                    name: old,
                })
            }
            C::AddDimension {
                level,
                id,
                a,
                b,
                offset,
            } => {
                point(*a, "a")?;
                point(*b, "b")?;
                finite(*offset, "offset")?;
                self.lvl(*level)?;
                let id = self.fresh(*id)?;
                self.lvl_mut(*level)?.dimensions.push(Dimension {
                    id,
                    a: *a,
                    b: *b,
                    offset: *offset,
                });
                Ok(C::DeleteEntity { level: *level, id })
            }
            C::PlaceCamera {
                level,
                id,
                camera,
                at,
                heading,
                fov,
                range,
                mount_height,
            } => {
                camera_fields(*fov, *range, *heading, *mount_height, *at)?;
                if camera.trim().is_empty() {
                    return invalid("which camera? (its name in argus.toml)");
                }
                self.lvl(*level)?;
                let id = self.fresh(*id)?;
                self.lvl_mut(*level)?.cameras.push(CameraPlacement {
                    id,
                    camera: camera.trim().to_owned(),
                    at: *at,
                    heading: *heading,
                    fov: *fov,
                    range: *range,
                    mount_height: *mount_height,
                });
                Ok(C::DeleteEntity { level: *level, id })
            }
            C::UpdateEntity { level, entity } => self.update(*level, entity),
            C::DeleteEntity { level, id } => {
                let inv = self.snapshot(*level)?;
                self.delete(*level, *id)?;
                Ok(inv)
            }
            C::MoveEntities { level, ids, by } => {
                point(*by, "by")?;
                let inv = self.snapshot(*level)?;
                let l = self.lvl_mut(*level)?;
                let mut nodes = Vec::new();
                for id in ids {
                    match l.entity(*id).ok_or(PlanError::NoEntity(*id))? {
                        Entity::Node(n) => nodes.push(n.id),
                        Entity::Wall(w) => nodes.extend([w.a, w.b]),
                        Entity::Opening(_) => {}
                        Entity::Room(_) => {
                            let r = l.rooms.iter_mut().find(|r| r.id == *id).unwrap();
                            r.at = r.at + *by;
                            if let Some(p) = &mut r.polygon {
                                p.iter_mut().for_each(|q| *q = *q + *by);
                            }
                        }
                        Entity::Dimension(_) => {
                            let d = l.dimensions.iter_mut().find(|d| d.id == *id).unwrap();
                            d.a = d.a + *by;
                            d.b = d.b + *by;
                        }
                        Entity::Camera(_) => {
                            let c = l.cameras.iter_mut().find(|c| c.id == *id).unwrap();
                            c.at = c.at + *by;
                        }
                    }
                }
                nodes.sort();
                nodes.dedup();
                for n in l.nodes.iter_mut().filter(|n| nodes.contains(&n.id)) {
                    n.at = n.at + *by;
                }
                Ok(inv)
            }
            C::SetBackground { level, background } => {
                if let Some(b) = background {
                    point(b.offset, "offset")?;
                    finite(b.rotation, "rotation")?;
                    if !(b.mm_per_px > 0.0 && b.mm_per_px.is_finite()) {
                        return invalid("the scale (mm per pixel) must be positive");
                    }
                    if !(0.0..=1.0).contains(&b.opacity) {
                        return invalid("opacity is 0 to 1");
                    }
                    if b.asset.is_empty()
                        || !b
                            .asset
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '.')
                    {
                        return invalid("that isn't an uploaded image id");
                    }
                }
                let l = self.lvl_mut(*level)?;
                let old = std::mem::replace(&mut l.background, background.clone());
                Ok(C::SetBackground {
                    level: *level,
                    background: old,
                })
            }
        }
    }

    /// The node for a wall end, creating or splitting as needed.
    fn resolve_end(&mut self, level: Id, end: &End) -> Result<Id, PlanError> {
        match end {
            End::Node(id) => {
                self.lvl(level)?.node(*id).ok_or(PlanError::NoEntity(*id))?;
                Ok(*id)
            }
            End::At(p) => {
                point(*p, "wall end")?;
                let l = self.lvl(level)?;
                if let Some(n) = l.node_near(*p, JOIN_TOL) {
                    return Ok(n.id);
                }
                // On a wall: split it there (a T-junction).
                let on_wall = l.walls.iter().find_map(|w| {
                    let (a, b) = l.wall_ends(w)?;
                    let (t, _, d) = project(*p, a, b);
                    let len = a.dist(b);
                    (d <= JOIN_TOL && t * len > JOIN_TOL && (1.0 - t) * len > JOIN_TOL)
                        .then_some((w.id, t * len))
                });
                let node = self.alloc();
                match on_wall {
                    Some((wall, offset)) => {
                        let new_wall = self.alloc();
                        self.split(level, wall, offset, node, new_wall)?;
                        // Put the node exactly where asked.
                        let l = self.lvl_mut(level)?;
                        l.nodes.iter_mut().find(|n| n.id == node).unwrap().at = *p;
                    }
                    None => self.lvl_mut(level)?.nodes.push(Node { id: node, at: *p }),
                }
                Ok(node)
            }
        }
    }

    fn split(
        &mut self,
        level: Id,
        wall: Id,
        offset: f64,
        node: Id,
        new_wall: Id,
    ) -> Result<(), PlanError> {
        let l = self.lvl_mut(level)?;
        let w = l.wall(wall).ok_or(PlanError::NoEntity(wall))?.clone();
        let (a, b) = l.wall_ends(&w).ok_or(PlanError::NoEntity(wall))?;
        let len = a.dist(b);
        if offset < JOIN_TOL || offset > len - JOIN_TOL {
            return invalid("split the wall somewhere between its ends");
        }
        for o in l.openings.iter().filter(|o| o.wall == wall) {
            if (o.offset - offset).abs() < o.width / 2.0 {
                return invalid(format!("opening {} is in the way of the split", o.id));
            }
        }
        l.nodes.push(Node {
            id: node,
            at: a.lerp(b, offset / len),
        });
        l.walls.iter_mut().find(|x| x.id == wall).unwrap().b = node;
        l.walls.push(Wall {
            id: new_wall,
            a: node,
            b: w.b,
            thickness: w.thickness,
        });
        for o in l
            .openings
            .iter_mut()
            .filter(|o| o.wall == wall && o.offset > offset)
        {
            o.wall = new_wall;
            o.offset -= offset;
        }
        Ok(())
    }

    fn update(&mut self, level: Id, e: &Entity) -> Result<Command, PlanError> {
        let l = self.lvl(level)?;
        let old = l.entity(e.id()).ok_or(PlanError::NoEntity(e.id()))?;
        if std::mem::discriminant(&old) != std::mem::discriminant(e) {
            return Err(PlanError::WrongType(e.id(), "the same kind of entity"));
        }
        match e {
            Entity::Node(n) => point(n.at, "position")?,
            Entity::Wall(w) => {
                thickness(w.thickness)?;
                let (Some(_), Some(_)) = (l.node(w.a), l.node(w.b)) else {
                    return invalid("a wall's ends must be nodes on its level");
                };
                if w.a == w.b {
                    return invalid("a wall needs two different ends");
                }
            }
            Entity::Opening(o) => {
                opening_fits(l, o.wall, o.offset, o.width, Some(o.id))?;
                if o.kind == OpeningKind::Window && o.swing.is_some() {
                    return invalid("windows don't swing");
                }
            }
            Entity::Room(r) => {
                point(r.at, "room position")?;
                if let Some(p) = &r.polygon {
                    check_polygon(p)?;
                }
            }
            Entity::Dimension(d) => {
                point(d.a, "a")?;
                point(d.b, "b")?;
                finite(d.offset, "offset")?;
            }
            Entity::Camera(c) => {
                camera_fields(c.fov, c.range, c.heading, c.mount_height, c.at)?;
            }
        }
        let l = self.lvl_mut(level)?;
        fn put<T: Clone>(v: &mut [T], id: Id, f: impl Fn(&T) -> Id, new: &T) {
            if let Some(x) = v.iter_mut().find(|x| f(x) == id) {
                *x = new.clone();
            }
        }
        match e {
            Entity::Node(n) => put(&mut l.nodes, n.id, |x| x.id, n),
            Entity::Wall(w) => put(&mut l.walls, w.id, |x| x.id, w),
            Entity::Opening(o) => put(&mut l.openings, o.id, |x| x.id, o),
            Entity::Room(r) => put(&mut l.rooms, r.id, |x| x.id, r),
            Entity::Dimension(d) => put(&mut l.dimensions, d.id, |x| x.id, d),
            Entity::Camera(c) => put(&mut l.cameras, c.id, |x| x.id, c),
        }
        Ok(Command::UpdateEntity { level, entity: old })
    }

    fn delete(&mut self, level: Id, id: Id) -> Result<(), PlanError> {
        let l = self.lvl_mut(level)?;
        let e = l.entity(id).ok_or(PlanError::NoEntity(id))?;
        let mut walls: Vec<Id> = Vec::new();
        let is_node = matches!(e, Entity::Node(_));
        match e {
            Entity::Node(n) => walls.extend(l.walls_at(n.id).map(|w| w.id)),
            Entity::Wall(w) => walls.push(w.id),
            Entity::Opening(_) => l.openings.retain(|o| o.id != id),
            Entity::Room(_) => l.rooms.retain(|r| r.id != id),
            Entity::Dimension(_) => l.dimensions.retain(|d| d.id != id),
            Entity::Camera(_) => l.cameras.retain(|c| c.id != id),
        }
        if !walls.is_empty() || is_node {
            l.walls.retain(|w| !walls.contains(&w.id));
            l.openings.retain(|o| !walls.contains(&o.wall));
            let used: Vec<Id> = l.walls.iter().flat_map(|w| [w.a, w.b]).collect();
            l.nodes.retain(|n| used.contains(&n.id));
        }
        Ok(())
    }

    /// Checks a whole plan (e.g. one uploaded with PUT): at least one
    /// level, ids unique, references intact. Raises `next_id` past every id.
    pub fn check(&mut self) -> Result<(), PlanError> {
        if self.levels.is_empty() {
            return invalid("a plan needs at least one level");
        }
        let mut seen = Plan {
            levels: Vec::new(),
            next_id: self.next_id,
        };
        for l in &self.levels {
            seen.check_level(l, None)?;
            seen.levels.push(l.clone());
        }
        self.next_id = seen.next_id;
        Ok(())
    }

    /// Checks a level given whole (restore or replace): ids unique and
    /// unused elsewhere, references intact. `replacing` is the level it
    /// replaces, whose ids may be reused.
    fn check_level(&mut self, level: &Level, replacing: Option<Id>) -> Result<(), PlanError> {
        let mut ids: Vec<Id> = vec![level.id];
        ids.extend(level.nodes.iter().map(|e| e.id));
        ids.extend(level.walls.iter().map(|e| e.id));
        ids.extend(level.openings.iter().map(|e| e.id));
        ids.extend(level.rooms.iter().map(|e| e.id));
        ids.extend(level.dimensions.iter().map(|e| e.id));
        ids.extend(level.cameras.iter().map(|e| e.id));
        let mut sorted = ids.clone();
        sorted.sort();
        if let Some(w) = sorted.windows(2).find(|w| w[0] == w[1]) {
            return Err(PlanError::DuplicateId(w[0]));
        }
        for id in &ids {
            if id.0 == 0 {
                return Err(PlanError::DuplicateId(*id));
            }
            let clash = self
                .levels
                .iter()
                .filter(|l| Some(l.id) != replacing)
                .any(|l| l.id == *id || l.entity(*id).is_some());
            if clash {
                return Err(PlanError::DuplicateId(*id));
            }
        }
        for w in &level.walls {
            if level.node(w.a).is_none() || level.node(w.b).is_none() || w.a == w.b {
                return invalid(format!("wall {} must join two nodes on its level", w.id));
            }
            thickness(w.thickness)?;
        }
        for o in &level.openings {
            if level.wall(o.wall).is_none() {
                return invalid(format!("opening {} is in no wall", o.id));
            }
        }
        if let Some(max) = ids.iter().map(|i| i.0).max() {
            self.next_id = self.next_id.max(max + 1);
        }
        Ok(())
    }
}

fn check_polygon(p: &[Point]) -> Result<(), PlanError> {
    if p.len() < 3 || !p.iter().all(|q| q.is_finite()) {
        return invalid("a room outline needs at least three points");
    }
    Ok(())
}

/// The JSON Schema of [`Command`], for tools that write commands.
pub fn command_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Command)).expect("schema is JSON")
}
