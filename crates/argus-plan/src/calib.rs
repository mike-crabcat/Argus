//! Camera calibration: which point on a level's floor each point of a
//! camera's picture shows, as a homography (a plane-to-plane projective
//! map) from pairs the user clicks: a spot on the ground in the picture
//! and the same spot on the plan.
//!
//! Picture points are fractions of the picture (0..1 from the top-left),
//! so one calibration serves the main stream's snapshots and the sub
//! stream's detections alike. The fit is the normalised direct linear
//! transform (Hartley); with six pairs or more, RANSAC first sets aside
//! pairs that disagree with the rest (a mis-click), then the fit uses all
//! the others.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::geom::{Point, pt};

/// One clicked pair.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CalPair {
    /// In the picture: [x, y] as fractions of its width and height, from
    /// the top-left.
    pub image: [f64; 2],
    /// The same spot on the plan, mm.
    pub plan: Point,
}

/// A camera's calibration, as stored on its placement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Calibration {
    pub pairs: Vec<CalPair>,
    /// Picture fractions to plan mm, row-major 3x3.
    pub homography: [f64; 9],
    /// How far each pair's picture point lands from its plan point, mm.
    pub errors: Vec<f64>,
    /// Pairs the fit set aside as not agreeing with the rest.
    pub outliers: Vec<usize>,
    /// Root mean square of the kept pairs' errors, mm.
    pub rms_mm: f64,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CalError {
    #[error("at least 4 point pairs are needed (have {0})")]
    TooFew(usize),
    #[error("picture points must be fractions from 0 to 1")]
    OutOfPicture,
    #[error("three of the points are in a line, or two are the same: spread them out")]
    Degenerate,
}

/// Pairs that disagree with the fit by more than this are outliers, mm.
pub const OUTLIER_MM: f64 = 400.0;

/// Maps a picture point through `h`: None at or beyond the horizon.
pub fn project(h: &[f64; 9], p: [f64; 2]) -> Option<Point> {
    let w = h[6] * p[0] + h[7] * p[1] + h[8];
    if w.abs() < 1e-12 {
        return None;
    }
    Some(pt(
        (h[0] * p[0] + h[1] * p[1] + h[2]) / w,
        (h[3] * p[0] + h[4] * p[1] + h[5]) / w,
    ))
}

/// The inverse map (plan to picture), if `h` is invertible.
pub fn invert(h: &[f64; 9]) -> Option<[f64; 9]> {
    let [a, b, c, d, e, f, g, hh, i] = *h;
    let det = a * (e * i - f * hh) - b * (d * i - f * g) + c * (d * hh - e * g);
    if det.abs() < 1e-18 {
        return None;
    }
    let adj = [
        e * i - f * hh,
        c * hh - b * i,
        b * f - c * e,
        f * g - d * i,
        a * i - c * g,
        c * d - a * f,
        d * hh - e * g,
        b * g - a * hh,
        a * e - b * d,
    ];
    Some(adj.map(|v| v / det))
}

/// The plan-to-picture map for [`to_picture`]: the inverse of `h`, scaled
/// so that points in front of the camera come out in front (a homography's
/// scale, and so its sign, is arbitrary).
pub fn picture_map(h: &[f64; 9]) -> Option<[f64; 9]> {
    let mut inv = invert(h)?;
    let c = project(h, [0.5, 0.5])?;
    if inv[6] * c.x + inv[7] * c.y + inv[8] < 0.0 {
        inv = inv.map(|v| -v);
    }
    Some(inv)
}

/// Maps a plan point to the picture (fractions) through a
/// [`picture_map`], if it is in front of the camera.
pub fn to_picture(inv: &[f64; 9], p: Point) -> Option<[f64; 2]> {
    let w = inv[6] * p.x + inv[7] * p.y + inv[8];
    if w <= 1e-12 {
        return None;
    }
    Some([
        (inv[0] * p.x + inv[1] * p.y + inv[2]) / w,
        (inv[3] * p.x + inv[4] * p.y + inv[5]) / w,
    ])
}

/// Translate and scale points so their centroid is 0 and their mean
/// distance from it √2: returns the transform (row-major 3x3).
fn normaliser(ps: &[[f64; 2]]) -> [f64; 9] {
    let n = ps.len() as f64;
    let (cx, cy) = ps
        .iter()
        .fold((0.0, 0.0), |a, p| (a.0 + p[0] / n, a.1 + p[1] / n));
    let d = ps.iter().map(|p| (p[0] - cx).hypot(p[1] - cy)).sum::<f64>() / n;
    let s = if d > 1e-12 {
        std::f64::consts::SQRT_2 / d
    } else {
        1.0
    };
    [s, 0.0, -s * cx, 0.0, s, -s * cy, 0.0, 0.0, 1.0]
}

fn apply3(t: &[f64; 9], p: [f64; 2]) -> [f64; 2] {
    [
        t[0] * p[0] + t[1] * p[1] + t[2],
        t[3] * p[0] + t[4] * p[1] + t[5],
    ]
}

fn mul3(a: &[f64; 9], b: &[f64; 9]) -> [f64; 9] {
    let mut o = [0.0; 9];
    for r in 0..3 {
        for c in 0..3 {
            o[r * 3 + c] = (0..3).map(|k| a[r * 3 + k] * b[k * 3 + c]).sum();
        }
    }
    o
}

/// The eigenvector of a symmetric 9x9 matrix with the smallest eigenvalue
/// (Jacobi rotations).
#[allow(clippy::needless_range_loop)] // matrix maths reads best indexed
fn smallest_eigenvector(mut a: [[f64; 9]; 9]) -> [f64; 9] {
    let mut v = [[0.0; 9]; 9];
    for (i, row) in v.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    for _sweep in 0..100 {
        let off: f64 = (0..9)
            .flat_map(|i| (0..9).filter(move |&j| j != i).map(move |j| (i, j)))
            .map(|(i, j)| a[i][j] * a[i][j])
            .sum();
        if off < 1e-30 {
            break;
        }
        for p in 0..9 {
            for q in p + 1..9 {
                if a[p][q].abs() < 1e-300 {
                    continue;
                }
                let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
                let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let t = if theta == 0.0 { 1.0 } else { t };
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                for k in 0..9 {
                    let (akp, akq) = (a[k][p], a[k][q]);
                    a[k][p] = c * akp - s * akq;
                    a[k][q] = s * akp + c * akq;
                }
                for k in 0..9 {
                    let (apk, aqk) = (a[p][k], a[q][k]);
                    a[p][k] = c * apk - s * aqk;
                    a[q][k] = s * apk + c * aqk;
                }
                for row in v.iter_mut() {
                    let (vp, vq) = (row[p], row[q]);
                    row[p] = c * vp - s * vq;
                    row[q] = s * vp + c * vq;
                }
            }
        }
    }
    let k = (0..9)
        .min_by(|&i, &j| a[i][i].total_cmp(&a[j][j]))
        .unwrap_or(8);
    let mut out = [0.0; 9];
    for (i, o) in out.iter_mut().enumerate() {
        *o = v[i][k];
    }
    out
}

/// Twice the area of a triangle (zero when the points are in a line).
fn area2(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])).abs()
}

/// Whether some three points are (nearly) in a line, relative to their
/// spread.
fn degenerate(ps: &[[f64; 2]]) -> bool {
    let n = ps.len();
    let span = ps
        .iter()
        .flat_map(|a| ps.iter().map(move |b| (a[0] - b[0]).hypot(a[1] - b[1])))
        .fold(0.0, f64::max);
    if span < 1e-9 {
        return true;
    }
    if n == 4 {
        let t = |i, j, k| area2(ps[i], ps[j], ps[k]) / (span * span);
        return [t(0, 1, 2), t(0, 1, 3), t(0, 2, 3), t(1, 2, 3)]
            .iter()
            .any(|&a| a < 1e-3);
    }
    // More points: degenerate only if all lie near one line.
    (0..n).all(|i| {
        (i + 1..n).all(|j| (j + 1..n).all(|k| area2(ps[i], ps[j], ps[k]) / (span * span) < 1e-3))
    })
}

/// The homography through `pairs` (at least 4) by the normalised DLT.
#[allow(clippy::needless_range_loop)]
pub fn dlt(pairs: &[CalPair]) -> Result<[f64; 9], CalError> {
    if pairs.len() < 4 {
        return Err(CalError::TooFew(pairs.len()));
    }
    let img: Vec<[f64; 2]> = pairs.iter().map(|p| p.image).collect();
    let pln: Vec<[f64; 2]> = pairs.iter().map(|p| [p.plan.x, p.plan.y]).collect();
    if degenerate(&img) || degenerate(&pln) {
        return Err(CalError::Degenerate);
    }
    let (ti, tp) = (normaliser(&img), normaliser(&pln));
    let mut ata = [[0.0; 9]; 9];
    for (a, b) in img.iter().zip(&pln) {
        let [x, y] = apply3(&ti, *a);
        let [u, v] = apply3(&tp, *b);
        let rows = [
            [-x, -y, -1.0, 0.0, 0.0, 0.0, u * x, u * y, u],
            [0.0, 0.0, 0.0, -x, -y, -1.0, v * x, v * y, v],
        ];
        for r in rows {
            for i in 0..9 {
                for j in 0..9 {
                    ata[i][j] += r[i] * r[j];
                }
            }
        }
    }
    let hn = smallest_eigenvector(ata);
    // H = Tp⁻¹ · Hn · Ti
    let tp_inv = invert(&tp).ok_or(CalError::Degenerate)?;
    let mut h = mul3(&mul3(&tp_inv, &hn), &ti);
    let s = if h[8].abs() > 1e-12 {
        h[8]
    } else {
        h.iter().map(|v| v.abs()).fold(0.0, f64::max)
    };
    for v in &mut h {
        *v /= s;
    }
    if h.iter().any(|v| !v.is_finite()) {
        return Err(CalError::Degenerate);
    }
    Ok(h)
}

fn error(h: &[f64; 9], p: &CalPair) -> f64 {
    project(h, p.image).map_or(f64::INFINITY, |q| q.dist(p.plan))
}

/// Fits a calibration to `pairs`: with six or more, the largest set of
/// pairs that agree (RANSAC over all fours, or a sample of them), then a
/// least-squares fit to those; with four or five, all of them.
pub fn fit(pairs: &[CalPair]) -> Result<Calibration, CalError> {
    if pairs.len() < 4 {
        return Err(CalError::TooFew(pairs.len()));
    }
    if pairs
        .iter()
        .any(|p| !p.image.iter().all(|v| (0.0..=1.0).contains(v)) || !p.plan.is_finite())
    {
        return Err(CalError::OutOfPicture);
    }
    let n = pairs.len();
    let mut keep: Vec<usize> = (0..n).collect();
    // Any four pairs fit exactly, so a mis-click can only show against at
    // least five others that agree: look for one from six pairs up.
    if n >= 6 {
        let mut best: Option<(usize, f64, Vec<usize>)> = None;
        let mut consider = |idx: [usize; 4]| {
            let sample: Vec<CalPair> = idx.iter().map(|&i| pairs[i]).collect();
            let Ok(h) = dlt(&sample) else { return };
            let errs: Vec<f64> = pairs.iter().map(|p| error(&h, p)).collect();
            let inl: Vec<usize> = (0..n).filter(|&i| errs[i] <= OUTLIER_MM).collect();
            let sse: f64 = inl.iter().map(|&i| errs[i] * errs[i]).sum();
            let better = match &best {
                None => true,
                Some((c, e, _)) => inl.len() > *c || (inl.len() == *c && sse < *e),
            };
            if better && inl.len() >= 4 {
                best = Some((inl.len(), sse, inl));
            }
        };
        // Every four of up to 12 pairs (495); beyond that, a fixed sample.
        if n <= 12 {
            for a in 0..n {
                for b in a + 1..n {
                    for c in b + 1..n {
                        for d in c + 1..n {
                            consider([a, b, c, d]);
                        }
                    }
                }
            }
        } else {
            let mut seed = 0x9e3779b97f4a7c15u64;
            let mut next = |m: usize| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed % m as u64) as usize
            };
            for _ in 0..1000 {
                let mut idx = [0; 4];
                for k in 0..4 {
                    loop {
                        let r = next(n);
                        if !idx[..k].contains(&r) {
                            idx[k] = r;
                            break;
                        }
                    }
                }
                consider(idx);
            }
        }
        if let Some((_, _, inl)) = best {
            keep = inl;
        }
    }
    let used: Vec<CalPair> = keep.iter().map(|&i| pairs[i]).collect();
    let h = dlt(&used)?;
    let errors: Vec<f64> = pairs.iter().map(|p| error(&h, p)).collect();
    let rms_mm =
        (keep.iter().map(|&i| errors[i] * errors[i]).sum::<f64>() / keep.len() as f64).sqrt();
    Ok(Calibration {
        pairs: pairs.to_vec(),
        homography: h,
        errors,
        outliers: (0..n).filter(|i| !keep.contains(i)).collect(),
        rms_mm,
    })
}

/// Where on the floor a picture point (fractions) of a calibrated camera
/// is, or None if it isn't calibrated or the point lands outside what the
/// camera can see: behind it, outside its field of view (with 10° to
/// spare), or further than 1.5 times its range. A bad click or a person's
/// head above the horizon lands far off; better no point than a wrong one.
pub fn floor_point(cam: &crate::model::CameraPlacement, image: [f64; 2]) -> Option<Point> {
    let c = cam.calibration.as_ref()?;
    let p = project(&c.homography, image)?;
    let d = p - cam.at;
    let dist = d.len();
    if dist > 1.5 * cam.range {
        return None;
    }
    if dist > 1.0 {
        let angle = d.y.atan2(d.x).to_degrees();
        let off = ((angle - cam.heading + 540.0).rem_euclid(360.0) - 180.0).abs();
        if off > cam.fov / 2.0 + 10.0 {
            return None;
        }
    }
    Some(p)
}

/// The map an ideal camera would have: placed as `cam`, tilted so the
/// middle of its picture looks at the floor half its range away, with the
/// given picture aspect (width / height). Picture fractions to plan mm.
/// For tests and demos; real cameras are calibrated from clicked pairs.
pub fn pinhole(cam: &crate::model::CameraPlacement, aspect: f64) -> Option<[f64; 9]> {
    let (th, h) = (cam.heading.to_radians(), cam.mount_height.max(1.0));
    let phi = (h / (cam.range.max(1.0) / 2.0)).atan();
    let f = [th.cos() * phi.cos(), th.sin() * phi.cos(), -phi.sin()];
    let r = [th.sin(), -th.cos(), 0.0];
    let down = [-th.cos() * phi.sin(), -th.sin() * phi.sin(), -phi.cos()];
    let fx = 0.5 / (cam.fov.to_radians() / 2.0).tan();
    let fy = fx * aspect;
    // d = (x - cx, y - cy, -h): each of d·r, d·down, d·f is linear in
    // (x, y, 1).
    let row = |a: [f64; 3]| [a[0], a[1], -a[0] * cam.at.x - a[1] * cam.at.y - a[2] * h];
    let (rr, rd, rf) = (row(r), row(down), row(f));
    // u = 0.5 + fx (d·r)/(d·f), v = 0.5 + fy (d·down)/(d·f)
    let m = [
        fx * rr[0] + 0.5 * rf[0],
        fx * rr[1] + 0.5 * rf[1],
        fx * rr[2] + 0.5 * rf[2],
        fy * rd[0] + 0.5 * rf[0],
        fy * rd[1] + 0.5 * rf[1],
        fy * rd[2] + 0.5 * rf[2],
        rf[0],
        rf[1],
        rf[2],
    ];
    invert(&m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plausible camera: picture fractions to a floor 2-12 m in front.
    const TRUE_H: [f64; 9] = [
        8000.0, -2000.0, -1000.0, 300.0, -9000.0, 11000.0, 0.1, 0.8, 1.0,
    ];

    fn pairs(points: &[[f64; 2]], noise_mm: f64) -> Vec<CalPair> {
        points
            .iter()
            .enumerate()
            .map(|(i, &p)| {
                let q = project(&TRUE_H, p).unwrap();
                // Deterministic "noise": alternating offsets.
                let k = if i % 2 == 0 { 1.0 } else { -1.0 };
                CalPair {
                    image: p,
                    plan: pt(q.x + k * noise_mm, q.y - k * noise_mm * 0.5),
                }
            })
            .collect()
    }

    const GRID: [[f64; 2]; 6] = [
        [0.1, 0.4],
        [0.9, 0.45],
        [0.15, 0.9],
        [0.85, 0.95],
        [0.5, 0.6],
        [0.3, 0.75],
    ];

    #[test]
    fn four_exact_pairs_recover_the_map() {
        let c = fit(&pairs(&GRID[..4], 0.0)).unwrap();
        for p in [[0.5, 0.5], [0.2, 0.8], [0.7, 0.7]] {
            let want = project(&TRUE_H, p).unwrap();
            let got = project(&c.homography, p).unwrap();
            assert!(got.dist(want) < 1e-6, "{got:?} vs {want:?}");
        }
        assert!(c.rms_mm < 1e-6 && c.outliers.is_empty());
    }

    #[test]
    fn noise_is_averaged_and_reported() {
        let c = fit(&pairs(&GRID, 30.0)).unwrap();
        assert!(c.outliers.is_empty(), "{:?}", c.errors);
        assert!(c.rms_mm > 5.0 && c.rms_mm < 60.0, "{}", c.rms_mm);
        let want = project(&TRUE_H, [0.5, 0.7]).unwrap();
        assert!(project(&c.homography, [0.5, 0.7]).unwrap().dist(want) < 80.0);
    }

    #[test]
    fn a_misclick_is_set_aside() {
        let mut ps = pairs(&GRID, 0.0);
        ps[4].plan = ps[4].plan + pt(1500.0, -900.0);
        let c = fit(&ps).unwrap();
        assert_eq!(c.outliers, [4]);
        assert!(c.rms_mm < 1e-3);
        assert!(c.errors[4] > 1000.0, "the bad pair's error is shown");
    }

    #[test]
    fn round_trip_through_the_inverse() {
        let c = fit(&pairs(&GRID, 0.0)).unwrap();
        let inv = picture_map(&c.homography).unwrap();
        for p in GRID {
            let back = to_picture(&inv, project(&c.homography, p).unwrap()).unwrap();
            assert!((back[0] - p[0]).abs() < 1e-9 && (back[1] - p[1]).abs() < 1e-9);
        }
    }

    #[test]
    fn bad_input_is_refused() {
        assert_eq!(fit(&pairs(&GRID[..3], 0.0)), Err(CalError::TooFew(3)));
        let line = [[0.1, 0.5], [0.3, 0.5], [0.6, 0.5], [0.9, 0.5]];
        assert_eq!(fit(&pairs(&line, 0.0)), Err(CalError::Degenerate));
        let mut ps = pairs(&GRID[..4], 0.0);
        ps[0].image = [1.2, 0.5];
        assert_eq!(fit(&ps), Err(CalError::OutOfPicture));
    }

    #[test]
    fn an_ideal_camera_sees_the_floor_ahead() {
        let cam = crate::model::CameraPlacement {
            id: crate::model::Id(1),
            camera: "door".into(),
            at: pt(1000.0, 0.0),
            heading: 90.0, // north
            fov: 90.0,
            range: 8000.0,
            mount_height: 2500.0,
            calibration: None,
        };
        let h = pinhole(&cam, 4.0 / 3.0).unwrap();
        // The middle of the picture: straight ahead, half the range away.
        let mid = project(&h, [0.5, 0.5]).unwrap();
        assert!(mid.dist(pt(1000.0, 4000.0)) < 1e-6, "{mid:?}");
        // Lower in the picture is nearer; right of the middle is east.
        let low = project(&h, [0.5, 0.9]).unwrap();
        assert!(low.y < 4000.0 && low.y > 0.0, "{low:?}");
        let right = project(&h, [0.8, 0.5]).unwrap();
        assert!(right.x > 1000.0, "{right:?}");
        // Pairs made from it fit back exactly.
        let ps: Vec<CalPair> = [
            [0.2, 0.6],
            [0.8, 0.6],
            [0.1, 0.95],
            [0.9, 0.95],
            [0.5, 0.75],
        ]
        .iter()
        .map(|&p| CalPair {
            image: p,
            plan: project(&h, p).unwrap(),
        })
        .collect();
        let c = fit(&ps).unwrap();
        assert!(c.rms_mm < 1e-3);
    }

    #[test]
    fn floor_points_outside_the_cone_are_refused() {
        let mut cam = crate::model::CameraPlacement {
            id: crate::model::Id(1),
            camera: "door".into(),
            at: pt(0.0, 0.0),
            heading: 0.0, // east
            fov: 90.0,
            range: 6000.0,
            mount_height: 2500.0,
            calibration: None,
        };
        assert_eq!(floor_point(&cam, [0.5, 0.5]), None, "not calibrated");
        let h = pinhole(&cam, 4.0 / 3.0).unwrap();
        let ps: Vec<CalPair> = [[0.2, 0.6], [0.8, 0.6], [0.1, 0.95], [0.9, 0.95]]
            .iter()
            .map(|&p| CalPair {
                image: p,
                plan: project(&h, p).unwrap(),
            })
            .collect();
        cam.calibration = Some(fit(&ps).unwrap());
        let mid = floor_point(&cam, [0.5, 0.5]).unwrap();
        assert!(mid.dist(pt(3000.0, 0.0)) < 1.0, "{mid:?}");
        // High in the picture, towards the horizon: 7 m is fine, 12 m is
        // beyond 1.5 x the 6 m range.
        assert!(floor_point(&cam, [0.5, 0.25]).is_some());
        assert_eq!(floor_point(&cam, [0.5, 0.15]), None);
        // A wrong calibration that puts a point beside the camera.
        let mut bad = cam.clone();
        let c = bad.calibration.as_mut().unwrap();
        c.homography = [0.0, 0.0, 0.0, 0.0, 0.0, 3000.0, 0.0, 0.0, 1.0];
        assert_eq!(
            floor_point(&bad, [0.5, 0.5]),
            None,
            "due north of an east-facing camera"
        );
    }
}
