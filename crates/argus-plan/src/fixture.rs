//! A made-up single-storey house for tests and demos, built only with
//! commands. Five cameras carry the names of a typical setup (doorbell,
//! driveway, garage, laundry, back). It is not anyone's real house.

use crate::command::{Command, End};
use crate::geom::{Point, project, pt};
use crate::model::{Hinge, Id, Level, OpeningKind, Plan, Side, Swing};

const EXT: f64 = 230.0;
const INT: f64 = 90.0;

/// The wall under point `p` and the offset along it.
fn wall_at(l: &Level, p: Point) -> (Id, f64) {
    l.walls
        .iter()
        .find_map(|w| {
            let (a, b) = l.wall_ends(w)?;
            let (t, _, d) = project(p, a, b);
            (d < 1.0).then(|| (w.id, t * a.dist(b)))
        })
        .unwrap_or_else(|| panic!("no wall at {p:?}"))
}

pub fn demo_house() -> Plan {
    let mut p = Plan::default();
    let level = p.levels[0].id;
    let wall = |p: &mut Plan, a: (f64, f64), b: (f64, f64), t: f64| {
        p.apply(&Command::AddWall {
            level,
            id: None,
            a: End::At(pt(a.0, a.1)),
            b: End::At(pt(b.0, b.1)),
            thickness: t,
        })
        .unwrap();
    };
    // Outside: 16 x 11 m, the front (south) along y = 0.
    for (a, b) in [
        ((0.0, 0.0), (16000.0, 0.0)),
        ((16000.0, 0.0), (16000.0, 11000.0)),
        ((16000.0, 11000.0), (0.0, 11000.0)),
        ((0.0, 11000.0), (0.0, 0.0)),
    ] {
        wall(&mut p, a, b, EXT);
    }
    // Garage (double), then the rooms.
    wall(&mut p, (10000.0, 0.0), (10000.0, 6500.0), EXT);
    wall(&mut p, (10000.0, 6500.0), (16000.0, 6500.0), EXT);
    for (a, b) in [
        ((0.0, 4500.0), (10000.0, 4500.0)),
        ((4000.0, 0.0), (4000.0, 11000.0)),
        ((6000.0, 0.0), (6000.0, 7500.0)),
        ((0.0, 7500.0), (6000.0, 7500.0)),
        ((13000.0, 6500.0), (13000.0, 11000.0)),
        ((13000.0, 9000.0), (16000.0, 9000.0)),
    ] {
        wall(&mut p, a, b, INT);
    }

    let swing = |hinge, side| Some(Swing { hinge, side });
    let openings: [(Point, OpeningKind, f64, Option<Swing>); 17] = [
        // Front door, garage door, front windows.
        (
            pt(5000.0, 0.0),
            OpeningKind::Door,
            920.0,
            swing(Hinge::Start, Side::Left),
        ),
        (pt(13000.0, 0.0), OpeningKind::Door, 4800.0, None),
        (pt(2000.0, 0.0), OpeningKind::Window, 1800.0, None),
        (pt(8000.0, 0.0), OpeningKind::Window, 2400.0, None),
        // Back: sliding doors to the yard and a kitchen window.
        (pt(9000.0, 11000.0), OpeningKind::Door, 2400.0, None),
        (pt(5000.0, 11000.0), OpeningKind::Window, 1500.0, None),
        (pt(2000.0, 11000.0), OpeningKind::Window, 1500.0, None),
        // Bedroom 2 window, laundry door to the side path.
        (pt(0.0, 6000.0), OpeningKind::Window, 1500.0, None),
        (
            pt(16000.0, 7800.0),
            OpeningKind::Door,
            820.0,
            swing(Hinge::Start, Side::Left),
        ),
        // Inside.
        (
            pt(4000.0, 3500.0),
            OpeningKind::Door,
            820.0,
            swing(Hinge::Start, Side::Right),
        ),
        (pt(6000.0, 2500.0), OpeningKind::Door, 1400.0, None),
        (pt(5000.0, 4500.0), OpeningKind::Door, 1000.0, None),
        (pt(5000.0, 7500.0), OpeningKind::Door, 1000.0, None),
        (
            pt(4000.0, 6500.0),
            OpeningKind::Door,
            820.0,
            swing(Hinge::End, Side::Right),
        ),
        (
            pt(4000.0, 8200.0),
            OpeningKind::Door,
            820.0,
            swing(Hinge::Start, Side::Left),
        ),
        (
            pt(13000.0, 7700.0),
            OpeningKind::Door,
            820.0,
            swing(Hinge::Start, Side::Left),
        ),
        (
            pt(11500.0, 6500.0),
            OpeningKind::Door,
            820.0,
            swing(Hinge::Start, Side::Right),
        ),
    ];
    for (at, kind, width, swing) in openings {
        let (wall, offset) = wall_at(&p.levels[0], at);
        p.apply(&Command::AddOpening {
            level,
            id: None,
            wall,
            kind,
            offset,
            width,
            swing,
        })
        .unwrap();
    }
    for (name, x, y) in [
        ("Bedroom 1", 2000.0, 2250.0),
        ("Entry", 5000.0, 2250.0),
        ("Lounge", 8000.0, 2250.0),
        ("Garage", 13000.0, 3250.0),
        ("Bedroom 2", 2000.0, 6000.0),
        ("Hall", 5000.0, 6000.0),
        ("Bedroom 3", 2000.0, 9250.0),
        ("Kitchen / Family", 8500.0, 9000.0),
        ("Laundry", 14500.0, 7750.0),
        ("Bathroom", 14500.0, 10000.0),
    ] {
        p.apply(&Command::AddRoom {
            level,
            id: None,
            name: name.into(),
            at: pt(x, y),
            polygon: None,
        })
        .unwrap();
    }
    for (a, b, offset) in [
        (pt(0.0, 0.0), pt(16000.0, 0.0), -1500.0),
        (pt(16000.0, 0.0), pt(16000.0, 11000.0), -1500.0),
    ] {
        p.apply(&Command::AddDimension {
            level,
            id: None,
            a,
            b,
            offset,
        })
        .unwrap();
    }
    // camera, x, y, heading, fov, range, mount height
    for (camera, x, y, heading, fov, range, mount) in [
        ("doorbell", 4550.0, -200.0, 270.0, 150.0, 7000.0, 1300.0),
        ("driveway", 10050.0, -200.0, 300.0, 100.0, 14000.0, 2700.0),
        ("garage", 15800.0, 6300.0, 225.0, 100.0, 9000.0, 2400.0),
        ("laundry", 16200.0, 10800.0, 270.0, 80.0, 12000.0, 2400.0),
        ("back", 1000.0, 11200.0, 60.0, 110.0, 14000.0, 2700.0),
    ] {
        p.apply(&Command::PlaceCamera {
            level,
            id: None,
            camera: camera.into(),
            at: pt(x, y),
            heading,
            fov,
            range,
            mount_height: mount,
        })
        .unwrap();
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_house_is_clean() {
        let p = demo_house();
        let issues = crate::validate::validate(&p);
        assert!(issues.is_empty(), "{issues:#?}");
        let l = &p.levels[0];
        assert_eq!(l.cameras.len(), 5);
        assert_eq!(l.rooms.len(), 10);
        let total: f64 = l
            .rooms
            .iter()
            .map(|r| crate::rooms::area_m2(&crate::rooms::room_polygon(l, r).unwrap()))
            .sum();
        assert!((total - 176.0).abs() < 1e-6, "{total}");
        // Round-trips through JSON exactly.
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Plan>(&json).unwrap(), p);
    }
}
