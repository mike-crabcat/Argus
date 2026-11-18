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

/// Applies `cmd` with every new entity's id made explicit, and records it.
fn rec(p: &mut Plan, out: &mut Vec<Command>, mut cmd: Command) {
    let next = Some(Id(p.next_id));
    match &mut cmd {
        Command::AddWall { id, .. }
        | Command::AddOpening { id, .. }
        | Command::AddRoom { id, .. }
        | Command::AddDimension { id, .. }
        | Command::PlaceCamera { id, .. } => *id = id.or(next),
        _ => {}
    }
    p.apply(&cmd).unwrap();
    out.push(cmd);
}

/// The demo house as one command batch for an empty level `level`, with
/// explicit ids from `first_id` up, so the same batch can be replayed (by
/// a scripted model, say) on any plan whose next free id is at most
/// `first_id`.
pub fn demo_house_commands(level: Id, first_id: u64) -> Vec<Command> {
    let mut p = Plan::empty(first_id);
    p.levels.push(Level::new(level, "Ground"));
    let mut out: Vec<Command> = Vec::new();
    let wall = |p: &mut Plan, out: &mut Vec<Command>, a: (f64, f64), b: (f64, f64), t: f64| {
        rec(
            p,
            out,
            Command::AddWall {
                level,
                id: None,
                a: End::At(pt(a.0, a.1)),
                b: End::At(pt(b.0, b.1)),
                thickness: t,
            },
        );
    };
    // Outside: 16 x 11 m, the front (south) along y = 0.
    for (a, b) in [
        ((0.0, 0.0), (16000.0, 0.0)),
        ((16000.0, 0.0), (16000.0, 11000.0)),
        ((16000.0, 11000.0), (0.0, 11000.0)),
        ((0.0, 11000.0), (0.0, 0.0)),
    ] {
        wall(&mut p, &mut out, a, b, EXT);
    }
    // Garage (double), then the rooms.
    wall(&mut p, &mut out, (10000.0, 0.0), (10000.0, 6500.0), EXT);
    wall(&mut p, &mut out, (10000.0, 6500.0), (16000.0, 6500.0), EXT);
    for (a, b) in [
        ((0.0, 4500.0), (10000.0, 4500.0)),
        ((4000.0, 0.0), (4000.0, 11000.0)),
        ((6000.0, 0.0), (6000.0, 7500.0)),
        ((0.0, 7500.0), (6000.0, 7500.0)),
        ((13000.0, 6500.0), (13000.0, 11000.0)),
        ((13000.0, 9000.0), (16000.0, 9000.0)),
    ] {
        wall(&mut p, &mut out, a, b, INT);
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
        rec(
            &mut p,
            &mut out,
            Command::AddOpening {
                level,
                id: None,
                wall,
                kind,
                offset,
                width,
                swing,
            },
        );
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
        rec(
            &mut p,
            &mut out,
            Command::AddRoom {
                level,
                id: None,
                name: name.into(),
                at: pt(x, y),
                polygon: None,
            },
        );
    }
    for (a, b, offset) in [
        (pt(0.0, 0.0), pt(16000.0, 0.0), -1500.0),
        (pt(16000.0, 0.0), pt(16000.0, 11000.0), -1500.0),
    ] {
        rec(
            &mut p,
            &mut out,
            Command::AddDimension {
                level,
                id: None,
                a,
                b,
                offset,
            },
        );
    }
    // camera, x, y, heading, fov, range, mount height
    for (camera, x, y, heading, fov, range, mount) in [
        ("doorbell", 4550.0, -200.0, 270.0, 150.0, 7000.0, 1300.0),
        ("driveway", 10050.0, -200.0, 300.0, 100.0, 14000.0, 2700.0),
        ("garage", 15800.0, 6300.0, 225.0, 100.0, 9000.0, 2400.0),
        ("laundry", 16200.0, 10800.0, 270.0, 80.0, 12000.0, 2400.0),
        ("back", 1000.0, 11200.0, 60.0, 110.0, 14000.0, 2700.0),
    ] {
        rec(
            &mut p,
            &mut out,
            Command::PlaceCamera {
                level,
                id: None,
                camera: camera.into(),
                at: pt(x, y),
                heading,
                fov,
                range,
                mount_height: mount,
            },
        );
    }
    out
}

pub fn demo_house() -> Plan {
    let mut p = Plan::default();
    let cmds = demo_house_commands(p.levels[0].id, p.next_id);
    p.apply_all(&cmds).unwrap();
    p
}

/// Calibrates every camera of a plan as an ideal camera of its placement
/// would see (4:3 pictures), with `set_calibration` commands: for tests
/// and demos of positions on the map.
pub fn calibrate_ideal(p: &mut Plan) -> Result<(), crate::PlanError> {
    let picks = [
        [0.15, 0.6],
        [0.85, 0.6],
        [0.1, 0.95],
        [0.9, 0.95],
        [0.5, 0.75],
    ];
    let mut cmds = Vec::new();
    for l in &p.levels {
        for c in &l.cameras {
            let Some(h) = crate::calib::pinhole(c, 4.0 / 3.0) else {
                continue;
            };
            let pairs = picks
                .iter()
                .filter_map(|&i| {
                    crate::calib::project(&h, i)
                        .map(|plan| crate::calib::CalPair { image: i, plan })
                })
                .collect();
            cmds.push(Command::SetCalibration {
                level: l.id,
                camera: c.id,
                pairs: Some(pairs),
            });
        }
    }
    for c in cmds {
        p.apply(&c)?;
    }
    Ok(())
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
