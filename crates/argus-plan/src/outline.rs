//! Drawing geometry: wall outlines with mitred joints, and where openings
//! and camera cones sit. Shared by the editor and the map.

use crate::geom::{Point, intersection, pt};
use crate::model::{CameraPlacement, Id, Level, Opening, Wall};

fn unit(v: Point) -> Point {
    let l = v.len();
    if l == 0.0 {
        pt(1.0, 0.0)
    } else {
        v.scale(1.0 / l)
    }
}

fn left(u: Point) -> Point {
    pt(-u.y, u.x)
}

/// One corner of wall `w` at its node `n`: on the left of the direction
/// leaving `n` if `on_left`, else on the right. Meets the neighbouring
/// wall's face there, or ends square.
fn corner(l: &Level, n: Id, w: &Wall, on_left: bool) -> Option<Point> {
    let p = l.node(n)?.at;
    let far = l.node(if w.a == n { w.b } else { w.a })?.at;
    let u = unit(far - p);
    let t = w.thickness / 2.0;
    let side = if on_left {
        left(u)
    } else {
        left(u).scale(-1.0)
    };
    let square = p + side.scale(t);
    // A wall carrying straight on through this node: end square on both
    // faces so the two halves meet flush (a stem at a T butts against them).
    let straight_on = l.walls_at(n).filter(|v| v.id != w.id).any(|v| {
        l.node(if v.a == n { v.b } else { v.a })
            .map(|q| {
                let dv = unit(q.at - p);
                dv.x * u.x + dv.y * u.y < -0.9999
            })
            .unwrap_or(false)
    });
    if straight_on {
        return Some(square);
    }
    let base = u.y.atan2(u.x);
    // The neighbour nearest counter-clockwise (left) or clockwise (right).
    let neighbour = l
        .walls_at(n)
        .filter(|v| v.id != w.id)
        .filter_map(|v| {
            let q = l.node(if v.a == n { v.b } else { v.a })?.at;
            let dv = unit(q - p);
            let mut d = dv.y.atan2(dv.x) - base;
            d = d.rem_euclid(std::f64::consts::TAU);
            let key = if on_left {
                d
            } else {
                std::f64::consts::TAU - d
            };
            (key > 1e-9).then_some((key, dv, v.thickness / 2.0))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0));
    let Some((key, dv, tv)) = neighbour else {
        return Some(square);
    };
    // Straight on (or nearly): end square.
    if (key - std::f64::consts::PI).abs() < 1e-3 {
        return Some(square);
    }
    // The neighbour's face towards this wall.
    let vside = if on_left {
        left(dv).scale(-1.0)
    } else {
        left(dv)
    };
    let a0 = p + side.scale(t);
    let b0 = p + vside.scale(tv);
    let x = intersection(a0, a0 + u, b0, b0 + dv)?;
    if x.dist(p) > 4.0 * t.max(tv) {
        return Some(square);
    }
    Some(x)
}

/// A wall's outline: [a-left, b-left, b-right, a-right], left and right as
/// seen walking from node `a` to `b`.
pub fn wall_outline(l: &Level, w: &Wall) -> Option<[Point; 4]> {
    Some([
        corner(l, w.a, w, true)?,
        corner(l, w.b, w, false)?,
        corner(l, w.b, w, true)?,
        corner(l, w.a, w, false)?,
    ])
}

/// A wall's outline cut into solid pieces around its openings: the first
/// and last keep the mitred ends, the ones between are rectangles.
pub fn wall_pieces(l: &Level, w: &Wall) -> Vec<[Point; 4]> {
    let Some(o) = wall_outline(l, w) else {
        return Vec::new();
    };
    let Some((a, b)) = l.wall_ends(w) else {
        return Vec::new();
    };
    let len = a.dist(b);
    let u = unit(b - a);
    let n = left(u).scale(w.thickness / 2.0);
    let mut gaps: Vec<(f64, f64)> = l
        .openings
        .iter()
        .filter(|x| x.wall == w.id)
        .map(|x| {
            (
                (x.offset - x.width / 2.0).max(0.0),
                (x.offset + x.width / 2.0).min(len),
            )
        })
        .collect();
    if gaps.is_empty() {
        return vec![o];
    }
    gaps.sort_by(|x, y| x.0.total_cmp(&y.0));
    let at = |s: f64| (a + u.scale(s) + n, a + u.scale(s) - n);
    let mut out = Vec::new();
    // The start piece, from the mitred corners to the first gap.
    let (l0, r0) = at(gaps[0].0);
    if gaps[0].0 > 0.5 {
        out.push([o[0], l0, r0, o[3]]);
    }
    for pair in gaps.windows(2) {
        let (s, e) = (pair[0].1, pair[1].0);
        if e - s > 0.5 {
            let ((ls, rs), (le, re)) = (at(s), at(e));
            out.push([ls, le, re, rs]);
        }
    }
    let last = gaps.last().unwrap().1;
    if len - last > 0.5 {
        let (l1, r1) = at(last);
        out.push([l1, o[1], o[2], r1]);
    }
    out
}

/// An opening's placement: its centre, the wall's unit direction (a to b),
/// and the wall's thickness.
pub fn opening_frame(l: &Level, o: &Opening) -> Option<(Point, Point, f64)> {
    let w = l.wall(o.wall)?;
    let (a, b) = l.wall_ends(w)?;
    let u = unit(b - a);
    Some((a + u.scale(o.offset), u, w.thickness))
}

/// A camera's view cone as a fan of points (apex first), `steps` segments
/// along the arc.
pub fn camera_cone(c: &CameraPlacement, steps: usize) -> Vec<Point> {
    let mut v = vec![c.at];
    let half = c.fov.min(359.0) / 2.0;
    for i in 0..=steps {
        let a = c.heading - half + c.fov.min(359.0) * i as f64 / steps.max(1) as f64;
        v.push(c.at.polar(c.range, a));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, End};
    use crate::model::Plan;

    type Seg = ((f64, f64), (f64, f64));

    fn plan(walls: &[Seg]) -> Plan {
        let mut p = Plan::default();
        let level = p.levels[0].id;
        for (a, b) in walls {
            p.apply(&Command::AddWall {
                level,
                id: None,
                a: End::At(pt(a.0, a.1)),
                b: End::At(pt(b.0, b.1)),
                thickness: 200.0,
            })
            .unwrap();
        }
        p
    }

    fn close(a: Point, b: Point) -> bool {
        a.dist(b) < 1e-6
    }

    #[test]
    fn corners_mitre_and_free_ends_are_square() {
        let p = plan(&[((0.0, 0.0), (1000.0, 0.0)), ((0.0, 0.0), (0.0, 1000.0))]);
        let l = &p.levels[0];
        let o = wall_outline(l, &l.walls[0]).unwrap();
        // At the corner: inner (100, 100), outer (-100, -100).
        assert!(close(o[0], pt(100.0, 100.0)), "{o:?}");
        assert!(close(o[3], pt(-100.0, -100.0)), "{o:?}");
        // The free end is square.
        assert!(close(o[1], pt(1000.0, 100.0)) && close(o[2], pt(1000.0, -100.0)));
        let o2 = wall_outline(l, &l.walls[1]).unwrap();
        assert!(o2.iter().any(|q| close(*q, pt(100.0, 100.0))));
        assert!(o2.iter().any(|q| close(*q, pt(-100.0, -100.0))));
    }

    #[test]
    fn straight_joints_and_t_junctions() {
        // A straight run split in two, with a wall teeing off below.
        let p = plan(&[
            ((0.0, 0.0), (2000.0, 0.0)),
            ((1000.0, 0.0), (1000.0, -1000.0)),
        ]);
        let l = &p.levels[0];
        for w in &l.walls {
            let o = wall_outline(l, w).unwrap();
            assert!(o.iter().all(|q| q.is_finite()));
        }
        // The run's halves meet flush at the joint (square ends).
        for w in l.walls.iter().filter(|w| {
            let (a, b) = l.wall_ends(w).unwrap();
            a.y == b.y
        }) {
            let o = wall_outline(l, w).unwrap();
            let at_joint: Vec<_> = o.iter().filter(|q| (q.x - 1000.0).abs() < 1e-6).collect();
            assert_eq!(at_joint.len(), 2, "{o:?}");
        }
        // The stem's top corners meet the run's underside at y = -100.
        let stem = l
            .walls
            .iter()
            .find(|w| {
                let (a, b) = l.wall_ends(w).unwrap();
                a.x == b.x
            })
            .unwrap();
        let o = wall_outline(l, stem).unwrap();
        let top: Vec<_> = o.iter().filter(|q| q.y > -500.0).collect();
        assert!(top.iter().all(|q| (q.y + 100.0).abs() < 1e-6), "{o:?}");
    }

    #[test]
    fn walls_split_around_openings() {
        let mut p = plan(&[((0.0, 0.0), (4000.0, 0.0))]);
        let level = p.levels[0].id;
        let wall = p.levels[0].walls[0].id;
        for (offset, width) in [(1000.0, 800.0), (3000.0, 1000.0)] {
            p.apply(&Command::AddOpening {
                level,
                id: None,
                wall,
                kind: crate::model::OpeningKind::Window,
                offset,
                width,
                swing: None,
            })
            .unwrap();
        }
        let l = &p.levels[0];
        let pieces = wall_pieces(l, &l.walls[0]);
        assert_eq!(pieces.len(), 3);
        let solid: f64 = pieces
            .iter()
            .map(|q| crate::geom::signed_area(q).abs() / 200.0)
            .sum();
        assert!((solid - 2200.0).abs() < 1e-6, "{solid}");
    }

    #[test]
    fn cone_spans_the_field_of_view() {
        let c = CameraPlacement {
            id: Id(1),
            camera: "x".into(),
            at: pt(0.0, 0.0),
            heading: 90.0,
            fov: 90.0,
            range: 1000.0,
            mount_height: 2000.0,
            calibration: None,
        };
        let v = camera_cone(&c, 4);
        assert_eq!(v.len(), 6);
        assert!(close(v[1], pt(0.0, 0.0).polar(1000.0, 45.0)));
        assert!(close(v[5], pt(0.0, 0.0).polar(1000.0, 135.0)));
    }
}
