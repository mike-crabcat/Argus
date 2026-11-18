//! The plan: levels holding walls, openings, rooms, dimensions and cameras.
//! All lengths are millimetres; see [`crate::geom`] for axes and angles.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::geom::Point;

/// A stable entity id, unique across the whole plan and never reused.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct Id(pub u64);

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Plan {
    pub levels: Vec<Level>,
    /// The next id to hand out. Ids only ever increase.
    pub next_id: u64,
    /// While a batch is applied: ids it gives explicitly, which implicit
    /// numbering (wall ends, split walls) skips. Empty otherwise.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) reserved: Vec<u64>,
}

impl Default for Plan {
    /// One empty level, "Ground".
    fn default() -> Self {
        let mut p = Plan {
            levels: Vec::new(),
            next_id: 1,
            reserved: Vec::new(),
        };
        let id = p.alloc();
        p.levels.push(Level::new(id, "Ground"));
        p
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Level {
    pub id: Id,
    pub name: String,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub walls: Vec<Wall>,
    #[serde(default)]
    pub openings: Vec<Opening>,
    #[serde(default)]
    pub rooms: Vec<Room>,
    #[serde(default)]
    pub dimensions: Vec<Dimension>,
    #[serde(default)]
    pub cameras: Vec<CameraPlacement>,
    #[serde(default)]
    pub background: Option<Background>,
}

/// A wall end. Walls meeting at a node are joined there (mitred corners).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Node {
    pub id: Id,
    pub at: Point,
}

/// A straight wall along the centreline from node `a` to node `b`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Wall {
    pub id: Id,
    pub a: Id,
    pub b: Id,
    /// Thickness, mm.
    pub thickness: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpeningKind {
    Door,
    Window,
}

/// Which side of a wall, looking from its node `a` towards `b`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Left,
    Right,
}

/// Which end of the opening the door is hinged at, along the wall.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Hinge {
    /// The end nearer the wall's node `a`.
    Start,
    /// The end nearer the wall's node `b`.
    End,
}

/// How a door swings: the hinge end and the side it opens towards.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Swing {
    pub hinge: Hinge,
    pub side: Side,
}

/// A door or window in a wall.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Opening {
    pub id: Id,
    pub wall: Id,
    pub kind: OpeningKind,
    /// Distance from the wall's node `a` to the opening's centre, mm.
    pub offset: f64,
    /// Width along the wall, mm.
    pub width: f64,
    /// Doors only; windows have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swing: Option<Swing>,
}

/// A named room. Its outline is the enclosed area of walls around `at`,
/// unless `polygon` gives one explicitly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Room {
    pub id: Id,
    pub name: String,
    /// A point inside the room; also where the label is drawn.
    pub at: Point,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polygon: Option<Vec<Point>>,
}

/// A measured line between two points, drawn `offset` mm to its left.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Dimension {
    pub id: Id,
    pub a: Point,
    pub b: Point,
    #[serde(default)]
    pub offset: f64,
}

/// A camera from the Argus config, placed on the plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CameraPlacement {
    pub id: Id,
    /// The camera's `name` in argus.toml.
    pub camera: String,
    pub at: Point,
    /// Where it points, degrees CCW from +x (east).
    pub heading: f64,
    /// Horizontal field of view, degrees.
    pub fov: f64,
    /// How far it usefully sees, mm (the drawn cone's length).
    pub range: f64,
    /// Height above the floor, mm.
    pub mount_height: f64,
    /// Which floor point each point of its picture shows, once calibrated
    /// (`set_calibration`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<crate::calib::Calibration>,
}

/// An image under a level (a scanned plan or a site photo), placed by its
/// scale, offset and rotation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Background {
    /// The uploaded image's id (`/api/plan/assets/{asset}`).
    pub asset: String,
    /// Millimetres per image pixel.
    pub mm_per_px: f64,
    /// Where the image's top-left corner sits on the plan, mm.
    pub offset: Point,
    /// Rotation about the top-left corner, degrees CCW.
    #[serde(default)]
    pub rotation: f64,
    /// 0 (invisible) to 1.
    pub opacity: f64,
}

/// Any entity, for lookups and restores.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entity {
    Node(Node),
    Wall(Wall),
    Opening(Opening),
    Room(Room),
    Dimension(Dimension),
    Camera(CameraPlacement),
}

impl Entity {
    pub fn id(&self) -> Id {
        match self {
            Entity::Node(e) => e.id,
            Entity::Wall(e) => e.id,
            Entity::Opening(e) => e.id,
            Entity::Room(e) => e.id,
            Entity::Dimension(e) => e.id,
            Entity::Camera(e) => e.id,
        }
    }
}

impl Plan {
    /// A plan with no levels, handing out ids from `next_id`.
    pub fn empty(next_id: u64) -> Self {
        Plan {
            levels: Vec::new(),
            next_id,
            reserved: Vec::new(),
        }
    }

    pub(crate) fn alloc(&mut self) -> Id {
        // Skip ids the batch being applied gives explicitly, so a batch
        // can name its walls 2, 3, 4... although each wall's ends are
        // numbered as it is added.
        while self.reserved.contains(&self.next_id) {
            self.next_id += 1;
        }
        let id = Id(self.next_id);
        self.next_id += 1;
        id
    }

    pub fn level(&self, id: Id) -> Option<&Level> {
        self.levels.iter().find(|l| l.id == id)
    }

    pub fn level_mut(&mut self, id: Id) -> Option<&mut Level> {
        self.levels.iter_mut().find(|l| l.id == id)
    }

    /// Whether any level or entity has this id.
    pub fn uses(&self, id: Id) -> bool {
        self.levels
            .iter()
            .any(|l| l.id == id || l.entity(id).is_some())
    }

    /// The level holding entity `id`.
    pub fn level_of(&self, id: Id) -> Option<&Level> {
        self.levels.iter().find(|l| l.entity(id).is_some())
    }
}

impl Level {
    pub fn new(id: Id, name: &str) -> Self {
        Level {
            id,
            name: name.to_owned(),
            nodes: Vec::new(),
            walls: Vec::new(),
            openings: Vec::new(),
            rooms: Vec::new(),
            dimensions: Vec::new(),
            cameras: Vec::new(),
            background: None,
        }
    }

    pub fn node(&self, id: Id) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }
    pub fn wall(&self, id: Id) -> Option<&Wall> {
        self.walls.iter().find(|w| w.id == id)
    }
    pub fn opening(&self, id: Id) -> Option<&Opening> {
        self.openings.iter().find(|o| o.id == id)
    }
    pub fn room(&self, id: Id) -> Option<&Room> {
        self.rooms.iter().find(|r| r.id == id)
    }
    pub fn dimension(&self, id: Id) -> Option<&Dimension> {
        self.dimensions.iter().find(|d| d.id == id)
    }
    pub fn camera(&self, id: Id) -> Option<&CameraPlacement> {
        self.cameras.iter().find(|c| c.id == id)
    }

    pub fn entity(&self, id: Id) -> Option<Entity> {
        self.node(id)
            .cloned()
            .map(Entity::Node)
            .or_else(|| self.wall(id).cloned().map(Entity::Wall))
            .or_else(|| self.opening(id).cloned().map(Entity::Opening))
            .or_else(|| self.room(id).cloned().map(Entity::Room))
            .or_else(|| self.dimension(id).cloned().map(Entity::Dimension))
            .or_else(|| self.camera(id).cloned().map(Entity::Camera))
    }

    /// A wall's end points.
    pub fn wall_ends(&self, w: &Wall) -> Option<(Point, Point)> {
        Some((self.node(w.a)?.at, self.node(w.b)?.at))
    }

    pub fn wall_len(&self, w: &Wall) -> f64 {
        self.wall_ends(w).map_or(0.0, |(a, b)| a.dist(b))
    }

    /// Walls with an end at node `id`.
    pub fn walls_at(&self, id: Id) -> impl Iterator<Item = &Wall> {
        self.walls.iter().filter(move |w| w.a == id || w.b == id)
    }

    /// The node at `p` (within `tol` mm), if any.
    pub fn node_near(&self, p: Point, tol: f64) -> Option<&Node> {
        self.nodes
            .iter()
            .filter(|n| n.at.dist(p) <= tol)
            .min_by(|a, b| a.at.dist(p).total_cmp(&b.at.dist(p)))
    }
}
