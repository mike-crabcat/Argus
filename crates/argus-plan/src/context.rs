//! Where a camera is, in words a model can use: the level and room it is
//! in, which way it faces, and which rooms and doors its view covers.

use serde::Serialize;

use crate::geom::{Point, contains};
use crate::model::{OpeningKind, Plan};
use crate::outline::{camera_cone, opening_frame};
use crate::rooms::room_polygon;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CameraContext {
    pub level: String,
    /// The room it is in, or None when outside every room.
    pub room: Option<String>,
    /// "north-east" and so on (north is up on the plan).
    pub facing: String,
    pub fov: f64,
    pub range_m: f64,
    pub mount_height_m: f64,
    /// Rooms its view reaches.
    pub sees_rooms: Vec<String>,
    /// Doors in view, as "door between Entry and outside".
    pub sees_doors: Vec<String>,
}

impl CameraContext {
    /// One paragraph for a prompt.
    pub fn describe(&self, camera: &str) -> String {
        let place = match &self.room {
            Some(r) => format!("inside, in the {r}"),
            None => "outside the building".into(),
        };
        let mut s = format!(
            "Camera \"{camera}\" is on the {} level, {place}, mounted {:.1} m up, facing {} with a {:.0}° view reaching about {:.0} m.",
            self.level, self.mount_height_m, self.facing, self.fov, self.range_m
        );
        if !self.sees_rooms.is_empty() {
            s.push_str(&format!(
                " Its view covers: {}.",
                self.sees_rooms.join(", ")
            ));
        }
        if !self.sees_doors.is_empty() {
            s.push_str(&format!(" Doors in view: {}.", self.sees_doors.join("; ")));
        }
        s
    }
}

/// Compass words for a heading (degrees counter-clockwise from east).
pub fn compass(heading: f64) -> &'static str {
    // Clockwise from north, in 45 degree sectors.
    let bearing = (90.0 - heading).rem_euclid(360.0);
    const NAMES: [&str; 8] = [
        "north",
        "north-east",
        "east",
        "south-east",
        "south",
        "south-west",
        "west",
        "north-west",
    ];
    NAMES[(((bearing + 22.5) / 45.0) as usize) % 8]
}

fn room_at(l: &crate::model::Level, p: Point) -> Option<String> {
    l.rooms
        .iter()
        .filter_map(|r| room_polygon(l, r).map(|poly| (r, poly)))
        .filter(|(_, poly)| contains(poly, p))
        .min_by(|a, b| {
            crate::geom::signed_area(&a.1)
                .abs()
                .total_cmp(&crate::geom::signed_area(&b.1).abs())
        })
        .map(|(r, _)| r.name.clone())
}

/// The context of the camera named `camera`, if it is on the plan.
pub fn camera_context(plan: &Plan, camera: &str) -> Option<CameraContext> {
    let (l, c) = plan.levels.iter().find_map(|l| {
        l.cameras
            .iter()
            .find(|c| c.camera == camera)
            .map(|c| (l, c))
    })?;
    let cone = camera_cone(c, 24);
    let mut sees_rooms = Vec::new();
    for r in &l.rooms {
        let Some(poly) = room_polygon(l, r) else {
            continue;
        };
        // In view if the room's label point or any corner is in the cone,
        // or the camera sits inside the room.
        let hit = contains(&cone, r.at)
            || poly.iter().any(|q| contains(&cone, *q))
            || contains(&poly, c.at);
        if hit {
            sees_rooms.push(r.name.clone());
        }
    }
    let mut sees_doors = Vec::new();
    for o in l.openings.iter().filter(|o| o.kind == OpeningKind::Door) {
        let Some((centre, u, t)) = opening_frame(l, o) else {
            continue;
        };
        if !contains(&cone, centre) {
            continue;
        }
        let n = crate::geom::pt(-u.y, u.x).scale(t / 2.0 + 300.0);
        let a = room_at(l, centre + n).unwrap_or_else(|| "outside".into());
        let b = room_at(l, centre - n).unwrap_or_else(|| "outside".into());
        let d = if a == b {
            format!("door in the {a}")
        } else {
            format!("door between {a} and {b}")
        };
        if !sees_doors.contains(&d) {
            sees_doors.push(d);
        }
    }
    Some(CameraContext {
        level: l.name.clone(),
        room: room_at(l, c.at),
        facing: compass(c.heading).into(),
        fov: c.fov,
        range_m: c.range / 1000.0,
        mount_height_m: c.mount_height / 1000.0,
        sees_rooms,
        sees_doors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::demo_house;

    #[test]
    fn compass_words() {
        assert_eq!(compass(90.0), "north");
        assert_eq!(compass(0.0), "east");
        assert_eq!(compass(270.0), "south");
        assert_eq!(compass(225.0), "south-west");
        assert_eq!(compass(300.0), "south-east");
        assert_eq!(compass(-90.0), "south");
    }

    #[test]
    fn demo_house_cameras() {
        let p = demo_house();
        let door = camera_context(&p, "doorbell").unwrap();
        assert_eq!(door.room, None, "on the front wall, outside");
        assert_eq!(door.facing, "south");
        // It looks away from the house: no rooms or doors in view.
        assert!(
            door.sees_rooms.is_empty() && door.sees_doors.is_empty(),
            "{door:?}"
        );
        let garage = camera_context(&p, "garage").unwrap();
        assert_eq!(garage.room.as_deref(), Some("Garage"));
        assert_eq!(garage.facing, "south-west");
        assert!(garage.sees_rooms.contains(&"Garage".to_string()));
        assert!(
            garage
                .sees_doors
                .iter()
                .any(|d| d.contains("Garage") && d.contains("outside")),
            "{:?}",
            garage.sees_doors
        );
        let text = garage.describe("garage");
        assert!(
            text.contains("inside, in the Garage") && text.contains("facing south-west"),
            "{text}"
        );
        assert!(camera_context(&p, "nope").is_none());
    }
}
