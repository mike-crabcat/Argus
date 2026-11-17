//! 2D geometry in millimetres. +x is east (right), +y is north (up), angles
//! are degrees counter-clockwise from +x.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A point in millimetres.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

pub const fn pt(x: f64, y: f64) -> Point {
    Point { x, y }
}

impl std::ops::Add for Point {
    type Output = Point;
    fn add(self, o: Point) -> Point {
        pt(self.x + o.x, self.y + o.y)
    }
}

impl std::ops::Sub for Point {
    type Output = Point;
    fn sub(self, o: Point) -> Point {
        pt(self.x - o.x, self.y - o.y)
    }
}

impl Point {
    pub fn dist(self, o: Point) -> f64 {
        (self.x - o.x).hypot(self.y - o.y)
    }
    pub fn scale(self, k: f64) -> Point {
        pt(self.x * k, self.y * k)
    }
    pub fn len(self) -> f64 {
        self.x.hypot(self.y)
    }
    pub fn lerp(self, o: Point, t: f64) -> Point {
        pt(self.x + (o.x - self.x) * t, self.y + (o.y - self.y) * t)
    }
    /// Angle of the vector from `self` to `o`, degrees CCW from +x.
    pub fn angle_to(self, o: Point) -> f64 {
        (o.y - self.y).atan2(o.x - self.x).to_degrees()
    }
    /// The point `len` mm from `self` at `deg` degrees.
    pub fn polar(self, len: f64, deg: f64) -> Point {
        let r = deg.to_radians();
        pt(self.x + len * r.cos(), self.y + len * r.sin())
    }
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// Where `p` projects onto segment a-b: (parameter t clamped to 0..1, the
/// closest point, its distance).
pub fn project(p: Point, a: Point, b: Point) -> (f64, Point, f64) {
    let ab = b - a;
    let l2 = ab.x * ab.x + ab.y * ab.y;
    let t = if l2 == 0.0 {
        0.0
    } else {
        (((p.x - a.x) * ab.x + (p.y - a.y) * ab.y) / l2).clamp(0.0, 1.0)
    };
    let c = a.lerp(b, t);
    (t, c, p.dist(c))
}

/// Signed area (positive when counter-clockwise), mm².
pub fn signed_area(poly: &[Point]) -> f64 {
    let n = poly.len();
    (0..n)
        .map(|i| {
            let (a, b) = (poly[i], poly[(i + 1) % n]);
            a.x * b.y - b.x * a.y
        })
        .sum::<f64>()
        / 2.0
}

pub fn contains(poly: &[Point], p: Point) -> bool {
    let mut inside = false;
    let n = poly.len();
    let mut j = n.wrapping_sub(1);
    for i in 0..n {
        let (a, b) = (poly[i], poly[j]);
        if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// The area-weighted centre of a simple polygon (or the vertex mean when
/// degenerate).
pub fn centroid(poly: &[Point]) -> Point {
    let a = signed_area(poly);
    if a.abs() < 1e-9 {
        let n = poly.len().max(1) as f64;
        let s = poly.iter().fold(pt(0.0, 0.0), |s, p| s + *p);
        return s.scale(1.0 / n);
    }
    let n = poly.len();
    let (mut cx, mut cy) = (0.0, 0.0);
    for i in 0..n {
        let (p, q) = (poly[i], poly[(i + 1) % n]);
        let f = p.x * q.y - q.x * p.y;
        cx += (p.x + q.x) * f;
        cy += (p.y + q.y) * f;
    }
    pt(cx / (6.0 * a), cy / (6.0 * a))
}

/// Whether segments a-b and c-d cross at a point inside both (touching at
/// an end does not count).
pub fn segments_cross(a: Point, b: Point, c: Point, d: Point) -> bool {
    let o = |p: Point, q: Point, r: Point| (q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x);
    let (d1, d2, d3, d4) = (o(c, d, a), o(c, d, b), o(a, b, c), o(a, b, d));
    let eps = 1e-6;
    ((d1 > eps && d2 < -eps) || (d1 < -eps && d2 > eps))
        && ((d3 > eps && d4 < -eps) || (d3 < -eps && d4 > eps))
}

/// Where lines a-b and c-d meet (None if parallel).
pub fn intersection(a: Point, b: Point, c: Point, d: Point) -> Option<Point> {
    let r = b - a;
    let s = d - c;
    let den = r.x * s.y - r.y * s.x;
    if den.abs() < 1e-12 {
        return None;
    }
    let t = ((c.x - a.x) * s.y - (c.y - a.y) * s.x) / den;
    Some(a + r.scale(t))
}

/// Normalises degrees to [0, 360).
pub fn norm_deg(d: f64) -> f64 {
    let r = d.rem_euclid(360.0);
    if r >= 360.0 { 0.0 } else { r }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let sq = [pt(0.0, 0.0), pt(10.0, 0.0), pt(10.0, 10.0), pt(0.0, 10.0)];
        assert_eq!(signed_area(&sq), 100.0);
        assert!(contains(&sq, pt(5.0, 5.0)));
        assert!(!contains(&sq, pt(15.0, 5.0)));
        assert_eq!(centroid(&sq), pt(5.0, 5.0));
        let (t, c, d) = project(pt(5.0, 3.0), pt(0.0, 0.0), pt(10.0, 0.0));
        assert_eq!((t, c, d), (0.5, pt(5.0, 0.0), 3.0));
        assert!(segments_cross(
            pt(0.0, 0.0),
            pt(10.0, 10.0),
            pt(0.0, 10.0),
            pt(10.0, 0.0)
        ));
        assert!(!segments_cross(
            pt(0.0, 0.0),
            pt(10.0, 0.0),
            pt(10.0, 0.0),
            pt(10.0, 10.0)
        ));
        assert_eq!(norm_deg(-90.0), 270.0);
        let p = pt(0.0, 0.0).polar(1000.0, 90.0);
        assert!((p.x).abs() < 1e-9 && (p.y - 1000.0).abs() < 1e-9);
    }
}
