//! Snapping for drawing tools: to nodes, wall midpoints, points on walls,
//! angle steps from the previous point, and the grid. Typed lengths and
//! angles go through [`polar`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::geom::{Point, project, pt};
use crate::model::{Id, Level};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapOptions {
    /// Grid spacing, mm (0: no grid snapping).
    pub grid: f64,
    /// How far (mm) the pointer may be from a node or wall to snap to it.
    /// The editor sets it from the zoom, e.g. 10 screen pixels.
    pub radius: f64,
    /// Angle step from the previous point, degrees (0: none).
    pub angle_step: f64,
    /// How close (degrees) to a step the angle must be to snap.
    pub angle_tolerance: f64,
}

impl Default for SnapOptions {
    fn default() -> Self {
        SnapOptions {
            grid: 100.0,
            radius: 150.0,
            angle_step: 15.0,
            angle_tolerance: 4.0,
        }
    }
}

/// What a point snapped to, for the editor's indicator.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnapKind {
    Node { node: Id },
    Midpoint { wall: Id },
    OnWall { wall: Id, offset: f64 },
    Angle { degrees: f64 },
    Grid,
    Free,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snapped {
    pub at: Point,
    pub kind: SnapKind,
}

/// Snaps pointer position `p`; `from` is the previous point of a chain.
/// Nodes win over midpoints, then points on walls, then the angle step
/// (with the length rounded to the grid), then the grid.
pub fn snap(l: &Level, p: Point, from: Option<Point>, o: &SnapOptions) -> Snapped {
    if let Some(n) = l.node_near(p, o.radius) {
        return Snapped {
            at: n.at,
            kind: SnapKind::Node { node: n.id },
        };
    }
    let mut best_mid: Option<(f64, Point, Id)> = None;
    let mut best_on: Option<(f64, Point, Id, f64)> = None;
    for w in &l.walls {
        let Some((a, b)) = l.wall_ends(w) else {
            continue;
        };
        let mid = a.lerp(b, 0.5);
        let d = mid.dist(p);
        if d <= o.radius && best_mid.is_none_or(|m| d < m.0) {
            best_mid = Some((d, mid, w.id));
        }
        let (t, c, d) = project(p, a, b);
        if d <= o.radius && best_on.is_none_or(|m| d < m.0) {
            best_on = Some((d, c, w.id, t * a.dist(b)));
        }
    }
    if let Some((_, at, wall)) = best_mid {
        return Snapped {
            at,
            kind: SnapKind::Midpoint { wall },
        };
    }
    if let Some((_, at, wall, offset)) = best_on {
        // Round the position along the wall to the grid, if there is one.
        let (a, b) = l.wall_ends(l.wall(wall).unwrap()).unwrap();
        let len = a.dist(b);
        let off = if o.grid > 0.0 {
            ((offset / o.grid).round() * o.grid).clamp(0.0, len)
        } else {
            offset
        };
        let at = if len > 0.0 { a.lerp(b, off / len) } else { at };
        return Snapped {
            at,
            kind: SnapKind::OnWall { wall, offset: off },
        };
    }
    if let (Some(f), true) = (from, o.angle_step > 0.0) {
        let ang = f.angle_to(p);
        let step = (ang / o.angle_step).round() * o.angle_step;
        if (ang - step).abs() <= o.angle_tolerance {
            let mut len = f.dist(p);
            if o.grid > 0.0 {
                len = (len / o.grid).round() * o.grid;
            }
            return Snapped {
                at: polar(f, len, step),
                kind: SnapKind::Angle {
                    degrees: crate::geom::norm_deg(step),
                },
            };
        }
    }
    if o.grid > 0.0 {
        return Snapped {
            at: pt(
                (p.x / o.grid).round() * o.grid,
                (p.y / o.grid).round() * o.grid,
            ),
            kind: SnapKind::Grid,
        };
    }
    Snapped {
        at: p,
        kind: SnapKind::Free,
    }
}

/// The point `len` mm from `from` at `deg` degrees, with results within a
/// micrometre of a whole millimetre rounded to it (so 90° is exact).
pub fn polar(from: Point, len: f64, deg: f64) -> Point {
    let p = from.polar(len, deg);
    let r = |v: f64| {
        let k = v.round();
        if (v - k).abs() < 1e-3 { k } else { v }
    };
    pt(r(p.x), r(p.y))
}

/// Parses a typed length: "3600", "3.6m", "360cm", "3600 mm". Plain
/// numbers are millimetres.
pub fn parse_length(s: &str) -> Option<f64> {
    let s = s.trim().to_ascii_lowercase().replace(' ', "");
    let (num, k) = if let Some(n) = s.strip_suffix("mm") {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix("cm") {
        (n, 10.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 1000.0)
    } else {
        (s.as_str(), 1.0)
    };
    let v: f64 = num.parse().ok()?;
    (v.is_finite() && v > 0.0).then_some(v * k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, End};
    use crate::model::Plan;

    #[test]
    fn snapping_priorities() {
        let mut p = Plan::default();
        let level = p.levels[0].id;
        p.apply(&Command::AddWall {
            level,
            id: None,
            a: End::At(pt(0.0, 0.0)),
            b: End::At(pt(4000.0, 0.0)),
            thickness: 100.0,
        })
        .unwrap();
        let l = &p.levels[0];
        let o = SnapOptions::default();
        let s = snap(l, pt(30.0, 40.0), None, &o);
        assert_eq!(s.at, pt(0.0, 0.0));
        assert!(matches!(s.kind, SnapKind::Node { .. }));
        let s = snap(l, pt(2010.0, 50.0), None, &o);
        assert_eq!(s.at, pt(2000.0, 0.0));
        assert!(matches!(s.kind, SnapKind::Midpoint { .. }));
        let s = snap(l, pt(1230.0, 60.0), None, &o);
        assert_eq!(s.at, pt(1200.0, 0.0));
        assert!(matches!(s.kind, SnapKind::OnWall { .. }));
        // 44 degrees from the origin, far from walls: snaps to 45.
        let f = pt(0.0, 5000.0);
        let q = f.polar(2000.0, 44.0);
        let s = snap(l, q, Some(f), &o);
        assert_eq!(s.kind, SnapKind::Angle { degrees: 45.0 });
        assert!((s.at.dist(f) - 2000.0).abs() < 1e-6);
        // Straight up from a point: exact.
        assert_eq!(polar(pt(0.0, 0.0), 3600.0, 90.0), pt(0.0, 3600.0));
        let s = snap(l, pt(733.0, 3333.0), None, &o);
        assert_eq!((s.at, s.kind), (pt(700.0, 3300.0), SnapKind::Grid));
    }

    #[test]
    fn typed_lengths() {
        assert_eq!(parse_length("3600"), Some(3600.0));
        assert_eq!(parse_length("3.6m"), Some(3600.0));
        assert_eq!(parse_length("360 cm"), Some(3600.0));
        assert_eq!(parse_length("90mm"), Some(90.0));
        assert_eq!(parse_length("-1"), None);
        assert_eq!(parse_length("abc"), None);
    }
}
