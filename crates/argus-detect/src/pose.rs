//! Foot points: where a person stands in the picture, from a pose model.
//!
//! The floor plan needs a point on the ground, not a box. The bottom of a
//! person's box is a poor guess when they are half behind a car or a
//! hedge, so a top-down pose model, RTMPose-s (Apache-2.0, OpenMMLab's
//! ONNX export `rtmpose-s_simcc-body7_pt-body7_420e-256x192`), finds their
//! ankles in a close-up of each person box. The foot point is between the
//! ankles it is sure of; with neither, the bottom centre of the box.
//!
//! The model takes `input` `[n, 3, 256, 192]` (RGB, ImageNet mean/std,
//! 0..255 scale) and gives `simcc_x` `[n, 17, 384]` and `simcc_y`
//! `[n, 17, 512]`: per COCO keypoint, scores along each axis at half-pixel
//! steps of the close-up.

use std::path::Path;

use ort::session::Session;
use ort::value::Tensor;

use crate::decode::Picture;
use crate::model::{Device, ModelError, open_session};
use crate::track::BBox;

const W: usize = 192;
const H: usize = 256;
/// Close-ups take the box this much larger, for context.
const PADDING: f32 = 1.25;
const SPLIT: f32 = 2.0;
const MEAN: [f32; 3] = [123.675, 116.28, 103.53];
const STD: [f32; 3] = [58.395, 57.12, 57.375];
/// COCO keypoints: left and right ankle.
const ANKLES: [usize; 2] = [15, 16];

/// Where a person stands, in the picture's pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Foot {
    pub x: f32,
    pub y: f32,
    /// From the ankles (true), or the box's bottom centre (false).
    pub ankles: bool,
}

/// The bottom centre of a box: the fallback foot point.
pub fn box_foot(b: &BBox) -> Foot {
    Foot {
        x: (b[0] + b[2]) / 2.0,
        y: b[3],
        ankles: false,
    }
}

/// The close-up around a box: centre and size, at the model's aspect.
fn crop(b: &BBox) -> (f32, f32, f32, f32) {
    let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
    let (mut w, mut h) = ((b[2] - b[0]).max(1.0), (b[3] - b[1]).max(1.0));
    let aspect = W as f32 / H as f32;
    if w > aspect * h {
        h = w / aspect;
    } else {
        w = h * aspect;
    }
    (cx, cy, w * PADDING, h * PADDING)
}

/// Fills `out` (`3 * H * W`) with the close-up of `b`, normalised.
pub fn preprocess(p: &Picture, b: &BBox, out: &mut [f32]) {
    let (cx, cy, cw, ch) = crop(b);
    let (pw, ph) = (p.width as usize, p.height as usize);
    let (x0, y0) = (cx - cw / 2.0, cy - ch / 2.0);
    let (sx, sy) = (cw / W as f32, ch / H as f32);
    let plane = W * H;
    for y in 0..H {
        let fy = y0 + (y as f32 + 0.5) * sy - 0.5;
        for x in 0..W {
            let fx = x0 + (x as f32 + 0.5) * sx - 0.5;
            // Outside the picture: black (0 before normalising).
            let px = if fx >= 0.0 && fy >= 0.0 && fx <= (pw - 1) as f32 && fy <= (ph - 1) as f32 {
                let (ix, iy) = (fx as usize, fy as usize);
                let (ix1, iy1) = ((ix + 1).min(pw - 1), (iy + 1).min(ph - 1));
                let (wx, wy) = (fx - ix as f32, fy - iy as f32);
                let at = |xx: usize, yy: usize, c: usize| p.rgb[(yy * pw + xx) * 3 + c] as f32;
                let mut v = [0.0; 3];
                for (c, v) in v.iter_mut().enumerate() {
                    let top = at(ix, iy, c) * (1.0 - wx) + at(ix1, iy, c) * wx;
                    let bot = at(ix, iy1, c) * (1.0 - wx) + at(ix1, iy1, c) * wx;
                    *v = top * (1.0 - wy) + bot * wy;
                }
                v
            } else {
                [0.0; 3]
            };
            for c in 0..3 {
                out[c * plane + y * W + x] = (px[c] - MEAN[c]) / STD[c];
            }
        }
    }
}

/// One keypoint from the model's scores along each axis, mapped back to
/// the picture: (x, y, score).
fn keypoint(sx: &[f32], sy: &[f32], b: &BBox) -> (f32, f32, f32) {
    let argmax = |v: &[f32]| {
        v.iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map_or((0, 0.0), |(i, &s)| (i, s))
    };
    let (ix, mx) = argmax(sx);
    let (iy, my) = argmax(sy);
    let (cx, cy, cw, ch) = crop(b);
    let x = cx - cw / 2.0 + (ix as f32 / SPLIT + 0.5) * cw / W as f32;
    let y = cy - ch / 2.0 + (iy as f32 / SPLIT + 0.5) * ch / H as f32;
    (x, y, mx.min(my))
}

/// The foot point from a person's ankle keypoints (x, y, score), or the
/// box's bottom centre when the model is sure of neither.
pub fn foot(ankles: &[(f32, f32, f32)], b: &BBox, min_score: f32) -> Foot {
    let sure: Vec<_> = ankles.iter().filter(|a| a.2 >= min_score).collect();
    if sure.is_empty() {
        return box_foot(b);
    }
    let n = sure.len() as f32;
    Foot {
        x: sure.iter().map(|a| a.0).sum::<f32>() / n,
        // The lower ankle: the foot is on the ground below the ankle.
        y: sure.iter().map(|a| a.1).fold(f32::MIN, f32::max),
        ankles: true,
    }
}

pub struct PoseModel {
    session: Session,
    /// Ankle keypoints less sure than this are ignored.
    pub min_score: f32,
    input: Vec<f32>,
}

impl PoseModel {
    pub fn open(path: &Path, device: Device) -> Result<Self, ModelError> {
        let session = open_session(path, device)?;
        let names =
            |v: &[ort::value::Outlet]| v.iter().map(|o| o.name().to_owned()).collect::<Vec<_>>();
        let (i, o) = (names(session.inputs()), names(session.outputs()));
        if i != ["input"] {
            return Err(ModelError::Shape("inputs", format!("{i:?}")));
        }
        if o != ["simcc_x", "simcc_y"] {
            return Err(ModelError::Shape("outputs", format!("{o:?}")));
        }
        Ok(Self {
            session,
            min_score: 0.3,
            input: Vec::new(),
        })
    }

    /// Foot points for people in one picture (one batch).
    pub fn feet(&mut self, p: &Picture, boxes: &[BBox]) -> Result<Vec<Foot>, ModelError> {
        if boxes.is_empty() {
            return Ok(Vec::new());
        }
        let per = 3 * W * H;
        self.input.resize(boxes.len() * per, 0.0);
        for (i, b) in boxes.iter().enumerate() {
            preprocess(p, b, &mut self.input[i * per..(i + 1) * per]);
        }
        let tensor = Tensor::from_array((
            [boxes.len() as i64, 3, H as i64, W as i64],
            std::mem::take(&mut self.input),
        ))?;
        let out = self.session.run(ort::inputs!["input" => tensor])?;
        let (xs, sx) = out["simcc_x"].try_extract_tensor::<f32>()?;
        let (ys, sy) = out["simcc_y"].try_extract_tensor::<f32>()?;
        let (kx, ky) = (
            *xs.last().unwrap_or(&0) as usize,
            *ys.last().unwrap_or(&0) as usize,
        );
        let k = xs.get(1).copied().unwrap_or(0) as usize;
        if kx != W * 2 || ky != H * 2 || k < 17 {
            return Err(ModelError::Shape("outputs", format!("{xs:?} {ys:?}")));
        }
        Ok(boxes
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let ankles: Vec<_> = ANKLES
                    .iter()
                    .map(|&a| {
                        let r = (i * k + a) * kx;
                        let q = (i * k + a) * ky;
                        keypoint(&sx[r..r + kx], &sy[q..q + ky], b)
                    })
                    .collect();
                foot(&ankles, b, self.min_score)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crops_keep_the_models_shape() {
        // A tall box gets wider, a wide one taller; both padded.
        let (cx, cy, w, h) = crop(&[100.0, 0.0, 150.0, 200.0]);
        assert_eq!((cx, cy), (125.0, 100.0));
        assert!((w / h - 0.75).abs() < 1e-6 && (h - 250.0).abs() < 1e-3);
        let (_, _, w, h) = crop(&[0.0, 0.0, 300.0, 100.0]);
        assert!((w / h - 0.75).abs() < 1e-6 && (w - 375.0).abs() < 1e-3);
    }

    #[test]
    fn keypoints_map_back_to_the_picture() {
        // A box whose close-up is exactly 192x256 picture pixels, at 0,0:
        // 153.6 x 204.8, padded by 1.25.
        let b = [0.0, 0.0, 153.6, 204.8];
        let mut sx = vec![0.0; 384];
        let mut sy = vec![0.0; 512];
        // The middle of the close-up: index 191 / 2 = 95.5, + 0.5 = 96.
        sx[191] = 0.9;
        sy[255] = 0.8;
        let (x, y, s) = keypoint(&sx, &sy, &b);
        assert!((x - 76.8).abs() < 0.5 && (y - 102.4).abs() < 0.5, "{x} {y}");
        assert_eq!(s, 0.8);
    }

    #[test]
    fn foot_points() {
        let b = [100.0, 50.0, 140.0, 250.0];
        // Both ankles sure: between them, at the lower one.
        let f = foot(&[(110.0, 240.0, 0.9), (126.0, 236.0, 0.8)], &b, 0.3);
        assert_eq!(
            f,
            Foot {
                x: 118.0,
                y: 240.0,
                ankles: true
            }
        );
        // One hidden behind a car: the other alone.
        let f = foot(&[(110.0, 240.0, 0.9), (126.0, 200.0, 0.1)], &b, 0.3);
        assert_eq!((f.x, f.y), (110.0, 240.0));
        // Neither: the box.
        let f = foot(&[(0.0, 0.0, 0.1), (0.0, 0.0, 0.2)], &b, 0.3);
        assert_eq!(
            f,
            Foot {
                x: 120.0,
                y: 250.0,
                ankles: false
            }
        );
    }

    #[test]
    fn preprocess_normalises_and_pads_with_black() {
        let p = Picture {
            width: 10,
            height: 10,
            rgb: vec![255; 300],
            wall_us: 0,
        };
        let mut out = vec![0.0; 3 * W * H];
        // A box at the picture's corner: the close-up is half outside.
        preprocess(&p, &[0.0, 0.0, 4.0, 8.0], &mut out);
        let at = |c: usize, x: usize, y: usize| out[c * W * H + y * W + x];
        assert!(
            (at(0, W - 1, H - 1) - (255.0 - MEAN[0]) / STD[0]).abs() < 1e-3,
            "inside: white"
        );
        assert!(
            (at(0, 0, 0) - (0.0 - MEAN[0]) / STD[0]).abs() < 1e-3,
            "outside: black"
        );
    }
}
