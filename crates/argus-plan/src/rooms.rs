//! Rooms from the wall graph: the enclosed areas (faces) of the planar
//! graph of wall centrelines.

use crate::geom::{Point, contains, signed_area};
use crate::model::{Id, Level, Room};

/// Every enclosed area of a level's walls, as counter-clockwise polygons
/// along the wall centrelines. Dead-end walls are left out of outlines.
pub fn faces(l: &Level) -> Vec<Vec<Point>> {
    // Outgoing half-edges per node, sorted by angle.
    let mut out: std::collections::HashMap<Id, Vec<(f64, Id)>> = Default::default();
    for w in &l.walls {
        let Some((a, b)) = l.wall_ends(w) else {
            continue;
        };
        out.entry(w.a).or_default().push((a.angle_to(b), w.b));
        out.entry(w.b).or_default().push((b.angle_to(a), w.a));
    }
    for v in out.values_mut() {
        v.sort_by(|x, y| x.0.total_cmp(&y.0));
        v.dedup_by(|x, y| x.1 == y.1);
    }
    let mut seen: std::collections::HashSet<(Id, Id)> = Default::default();
    let mut faces = Vec::new();
    let mut keys: Vec<Id> = out.keys().copied().collect();
    keys.sort();
    for &u in &keys {
        for &(_, v) in &out[&u] {
            if seen.contains(&(u, v)) {
                continue;
            }
            // Walk keeping the face on the left: at each node take the
            // edge just clockwise of the way back.
            let mut cycle = Vec::new();
            let (mut a, mut b) = (u, v);
            loop {
                if !seen.insert((a, b)) {
                    break;
                }
                cycle.push(a);
                let edges = &out[&b];
                let back = edges.iter().position(|e| e.1 == a).unwrap();
                let next = edges[(back + edges.len() - 1) % edges.len()].1;
                (a, b) = (b, next);
                if (a, b) == (u, v) {
                    break;
                }
            }
            let pts: Vec<Point> = strip_spurs(cycle)
                .iter()
                .filter_map(|id| l.node(*id).map(|n| n.at))
                .collect();
            if pts.len() >= 3 && signed_area(&pts) > 1.0 {
                faces.push(pts);
            }
        }
    }
    faces
}

/// Removes back-and-forth runs (a dead-end wall walked out and back).
fn strip_spurs(mut c: Vec<Id>) -> Vec<Id> {
    loop {
        let n = c.len();
        if n < 3 {
            return c;
        }
        let spur = (0..n).find(|&i| c[(i + n - 1) % n] == c[(i + 1) % n]);
        match spur {
            Some(i) => {
                // Drop the tip and one copy of the repeated node.
                let j = (i + 1) % n;
                let (hi, lo) = if i > j { (i, j) } else { (j, i) };
                c.remove(hi);
                c.remove(lo);
            }
            None => return c,
        }
    }
}

/// A room's outline: its explicit polygon, or the smallest enclosed area
/// around its label point.
pub fn room_polygon(l: &Level, r: &Room) -> Option<Vec<Point>> {
    if let Some(p) = &r.polygon {
        return Some(p.clone());
    }
    faces(l)
        .into_iter()
        .filter(|f| contains(f, r.at))
        .min_by(|a, b| signed_area(a).total_cmp(&signed_area(b)))
}

/// Area in square metres.
pub fn area_m2(poly: &[Point]) -> f64 {
    signed_area(poly).abs() / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, End};
    use crate::geom::pt;
    use crate::model::Plan;

    fn wall(p: &mut Plan, a: Point, b: Point) {
        let level = p.levels[0].id;
        p.apply(&Command::AddWall {
            level,
            id: None,
            a: End::At(a),
            b: End::At(b),
            thickness: 100.0,
        })
        .unwrap();
    }

    #[test]
    fn two_rooms_and_a_spur() {
        let mut p = Plan::default();
        // A 6 x 4 m box split by a wall at x = 4 m (a T-junction each end).
        wall(&mut p, pt(0.0, 0.0), pt(6000.0, 0.0));
        wall(&mut p, pt(6000.0, 0.0), pt(6000.0, 4000.0));
        wall(&mut p, pt(6000.0, 4000.0), pt(0.0, 4000.0));
        wall(&mut p, pt(0.0, 4000.0), pt(0.0, 0.0));
        wall(&mut p, pt(4000.0, 0.0), pt(4000.0, 4000.0));
        // A dead-end wall poking into the left room.
        wall(&mut p, pt(0.0, 2000.0), pt(1000.0, 2000.0));
        let l = &p.levels[0];
        let mut areas: Vec<f64> = faces(l).iter().map(|f| area_m2(f)).collect();
        areas.sort_by(f64::total_cmp);
        assert_eq!(areas, vec![8.0, 16.0]);
        let r = Room {
            id: crate::model::Id(999),
            name: "Lounge".into(),
            at: pt(1000.0, 1000.0),
            polygon: None,
        };
        assert_eq!(area_m2(&room_polygon(l, &r).unwrap()), 16.0);
        let outside = Room {
            at: pt(9000.0, 0.0),
            ..r
        };
        assert!(room_polygon(l, &outside).is_none());
    }
}
