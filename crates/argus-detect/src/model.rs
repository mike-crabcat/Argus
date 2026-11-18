//! The detector: an ONNX object detection model run with ONNX Runtime.
//!
//! Built for D-FINE (Apache-2.0) as exported for Hugging Face
//! (`onnx-community/dfine_s_coco-ONNX`): one input, `pixel_values`
//! `[batch, 3, 640, 640]`, RGB scaled to 0..1 with no mean/std; two outputs,
//! `logits` `[batch, queries, 80]` (COCO classes, before the sigmoid) and
//! `pred_boxes` `[batch, queries, 4]` (centre x, centre y, width, height,
//! 0..1 of the picture). It's a DETR-style model: one box per query and no
//! duplicates, so no non-maximum suppression is needed. RT-DETR exports
//! have the same shape.

use std::path::Path;
use std::sync::OnceLock;

use ort::session::Session;
use ort::value::Tensor;

use crate::decode::Picture;

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("ONNX Runtime: {0}")]
    Ort(String),
    #[error("the model's {0} isn't what Argus expects: {1}")]
    Shape(&'static str, String),
}

impl<T> From<ort::Error<T>> for ModelError {
    fn from(e: ort::Error<T>) -> Self {
        ModelError::Ort(e.to_string())
    }
}

/// Where the model runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    Cpu,
    /// An NVIDIA GPU (needs the `cuda` feature, and CUDA at run time).
    Cuda(i32),
}

/// Something found in a picture.
#[derive(Clone, Debug, PartialEq)]
pub struct Detection {
    /// Argus's label: person, vehicle or animal.
    pub label: &'static str,
    /// The model's own class, e.g. "car" or "dog".
    pub class: &'static str,
    pub score: f32,
    /// Left, top, right, bottom, in the picture's pixels.
    pub bbox: [f32; 4],
    /// For people, where they stand (see [`crate::pose`]).
    pub foot: Option<crate::pose::Foot>,
}

/// COCO class index → (class, Argus label), for the classes Argus keeps.
pub const CLASSES: [(usize, &str, &str); 9] = [
    (0, "person", "person"),
    (1, "bicycle", "vehicle"),
    (2, "car", "vehicle"),
    (3, "motorcycle", "vehicle"),
    (5, "bus", "vehicle"),
    (7, "truck", "vehicle"),
    (14, "bird", "animal"),
    (15, "cat", "animal"),
    (16, "dog", "animal"),
];

/// Fills `out` (`3 * size * size`) with `p` stretched to `size` x `size`,
/// planar RGB scaled to 0..1, by bilinear sampling.
pub fn preprocess(p: &Picture, size: usize, out: &mut [f32]) {
    let (w, h) = (p.width as usize, p.height as usize);
    let plane = size * size;
    let sx = w as f32 / size as f32;
    let sy = h as f32 / size as f32;
    // Per column: left pixel and weight.
    let cols: Vec<(usize, usize, f32)> = (0..size)
        .map(|x| {
            let fx = ((x as f32 + 0.5) * sx - 0.5).clamp(0.0, (w - 1) as f32);
            let x0 = fx as usize;
            (x0, (x0 + 1).min(w - 1), fx - x0 as f32)
        })
        .collect();
    for y in 0..size {
        let fy = ((y as f32 + 0.5) * sy - 0.5).clamp(0.0, (h - 1) as f32);
        let y0 = fy as usize;
        let y1 = (y0 + 1).min(h - 1);
        let wy = fy - y0 as f32;
        let r0 = &p.rgb[y0 * w * 3..(y0 + 1) * w * 3];
        let r1 = &p.rgb[y1 * w * 3..(y1 + 1) * w * 3];
        for (x, &(x0, x1, wx)) in cols.iter().enumerate() {
            for c in 0..3 {
                let a = r0[x0 * 3 + c] as f32 * (1.0 - wx) + r0[x1 * 3 + c] as f32 * wx;
                let b = r1[x0 * 3 + c] as f32 * (1.0 - wx) + r1[x1 * 3 + c] as f32 * wx;
                out[c * plane + y * size + x] = (a * (1.0 - wy) + b * wy) / 255.0;
            }
        }
    }
}

/// One picture's detections from the model's raw outputs (`logits` and
/// `boxes` for that picture alone): the best kept class per query, at
/// `threshold` or above, best first.
pub fn postprocess(
    logits: &[f32],
    boxes: &[f32],
    classes: usize,
    width: f32,
    height: f32,
    threshold: f32,
) -> Vec<Detection> {
    let mut out = Vec::new();
    for (scores, b) in logits.chunks(classes).zip(boxes.chunks(4)) {
        let Some(&(i, class, label)) = CLASSES
            .iter()
            .filter(|(i, ..)| *i < scores.len())
            .max_by(|a, b| scores[a.0].total_cmp(&scores[b.0]))
        else {
            continue;
        };
        let score = 1.0 / (1.0 + (-scores[i]).exp());
        if score < threshold {
            continue;
        }
        let (cx, cy, bw, bh) = (b[0] * width, b[1] * height, b[2] * width, b[3] * height);
        out.push(Detection {
            label,
            class,
            score,
            bbox: [
                (cx - bw / 2.0).max(0.0),
                (cy - bh / 2.0).max(0.0),
                (cx + bw / 2.0).min(width),
                (cy + bh / 2.0).min(height),
            ],
            foot: None,
        });
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

fn environment() -> Result<&'static ort::environment::Environment, ModelError> {
    static ENV: OnceLock<ort::environment::Environment> = OnceLock::new();
    if let Some(e) = ENV.get() {
        return Ok(e);
    }
    let env = ort::init().with_name("argus").build()?;
    Ok(ENV.get_or_init(|| env))
}

/// Loads an ONNX model on `device`. With `Device::Cuda` it fails if CUDA
/// can't be used, rather than quietly running on the CPU.
pub(crate) fn open_session(path: &Path, device: Device) -> Result<Session, ModelError> {
    let env = environment()?;
    let mut b = Session::builder(env)?;
    match device {
        Device::Cpu => {
            b = b.with_intra_threads(2)?;
        }
        #[cfg(feature = "cuda")]
        Device::Cuda(id) => {
            b = b.with_execution_providers([ort::ep::CUDA::default()
                .with_device_id(id)
                .build()
                .error_on_failure()])?;
        }
        #[cfg(not(feature = "cuda"))]
        Device::Cuda(_) => {
            return Err(ModelError::Ort(
                "this build of Argus has no CUDA support".into(),
            ));
        }
    }
    Ok(b.commit_from_file(path)?)
}

pub struct Detector {
    session: Session,
    classes: usize,
    pub threshold: f32,
    pub device: Device,
    input: Vec<f32>,
}

impl Detector {
    /// Loads the model. With `Device::Cuda` it fails if CUDA can't be used,
    /// rather than quietly running on the CPU.
    pub fn open(path: &Path, device: Device) -> Result<Self, ModelError> {
        let session = open_session(path, device)?;
        let inputs: Vec<String> = session
            .inputs()
            .iter()
            .map(|i| i.name().to_owned())
            .collect();
        let outputs: Vec<String> = session
            .outputs()
            .iter()
            .map(|o| o.name().to_owned())
            .collect();
        if inputs != ["pixel_values"] {
            return Err(ModelError::Shape("inputs", format!("{inputs:?}")));
        }
        if !outputs.iter().any(|o| o == "logits") || !outputs.iter().any(|o| o == "pred_boxes") {
            return Err(ModelError::Shape("outputs", format!("{outputs:?}")));
        }
        Ok(Self {
            session,
            classes: 80,
            threshold: 0.5,
            device,
            input: Vec::new(),
        })
    }

    /// Detects objects in each picture (one batch).
    pub fn detect(&mut self, pictures: &[&Picture]) -> Result<Vec<Vec<Detection>>, ModelError> {
        let prepared: Vec<Prepared> = pictures.iter().map(|p| Prepared::new(p)).collect();
        self.detect_prepared(&prepared.iter().collect::<Vec<_>>())
    }

    /// Detects objects in pictures already prepared (one batch).
    pub fn detect_prepared(
        &mut self,
        pictures: &[&Prepared],
    ) -> Result<Vec<Vec<Detection>>, ModelError> {
        if pictures.is_empty() {
            return Ok(Vec::new());
        }
        let n = pictures.len();
        let per = 3 * INPUT * INPUT;
        self.input.clear();
        for p in pictures {
            self.input.extend_from_slice(&p.data[..per]);
        }
        let s = INPUT as i64;
        let tensor = Tensor::from_array(([n as i64, 3, s, s], std::mem::take(&mut self.input)))?;
        let outputs = self.session.run(ort::inputs!["pixel_values" => tensor])?;
        let (lshape, logits) = outputs["logits"].try_extract_tensor::<f32>()?;
        let (_, boxes) = outputs["pred_boxes"].try_extract_tensor::<f32>()?;
        let classes = *lshape.last().unwrap_or(&0) as usize;
        if classes != self.classes {
            return Err(ModelError::Shape("logits", format!("{lshape:?}")));
        }
        let lper = logits.len() / n;
        let bper = boxes.len() / n;
        let found = pictures
            .iter()
            .enumerate()
            .map(|(i, p)| {
                postprocess(
                    &logits[i * lper..(i + 1) * lper],
                    &boxes[i * bper..(i + 1) * bper],
                    classes,
                    p.width as f32,
                    p.height as f32,
                    self.threshold,
                )
            })
            .collect();
        Ok(found)
    }
}

/// The model's input size (square).
pub const INPUT: usize = 640;

/// A picture as the model takes it: resized and planar, ready to batch.
/// Preparing takes a few milliseconds of CPU, so it is done where the
/// picture is decoded, in parallel per camera, not on the model's thread.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub data: Vec<f32>,
    /// The picture itself, for the pose model's close-ups of people.
    pub picture: Option<Picture>,
    /// The original picture's size, to map boxes back.
    pub width: u32,
    pub height: u32,
    pub wall_us: i64,
}

impl Prepared {
    pub fn new(p: &Picture) -> Self {
        let mut data = vec![0.0; 3 * INPUT * INPUT];
        preprocess(p, INPUT, &mut data);
        Self {
            data,
            picture: None,
            width: p.width,
            height: p.height,
            wall_us: p.wall_us,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preprocess_scales_and_planarises() {
        // A 4x2 picture: left half red, right half blue.
        let mut rgb = Vec::new();
        for _y in 0..2 {
            for x in 0..4 {
                rgb.extend_from_slice(if x < 2 { &[255, 0, 0] } else { &[0, 0, 255] });
            }
        }
        let p = Picture {
            width: 4,
            height: 2,
            rgb,
            wall_us: 0,
        };
        let mut out = vec![0.0; 3 * 8 * 8];
        preprocess(&p, 8, &mut out);
        let at = |c: usize, x: usize, y: usize| out[c * 64 + y * 8 + x];
        assert_eq!((at(0, 0, 0), at(2, 0, 0)), (1.0, 0.0), "top left is red");
        assert_eq!(
            (at(0, 7, 7), at(2, 7, 7)),
            (0.0, 1.0),
            "bottom right is blue"
        );
        let mid = at(0, 3, 4) + at(0, 4, 4);
        assert!(mid > 0.4 && mid < 1.6, "blended in the middle: {mid}");
    }

    #[test]
    fn postprocess_keeps_known_classes_over_the_threshold() {
        let classes = 80;
        let mut logits = vec![-10.0f32; 4 * classes];
        let mut boxes = vec![0.0f32; 4 * 4];
        // Query 0: a person, sure. Query 1: a car, unsure. Query 2: a
        // giraffe (not kept). Query 3: a dog, fairly sure.
        logits[0] = 3.0;
        logits[classes + 2] = -1.0;
        logits[2 * classes + 23] = 5.0;
        logits[3 * classes + 16] = 1.0;
        boxes[..4].copy_from_slice(&[0.5, 0.5, 0.2, 0.4]);
        boxes[12..16].copy_from_slice(&[0.1, 0.9, 0.4, 0.4]);
        let d = postprocess(&logits, &boxes, classes, 1000.0, 500.0, 0.5);
        assert_eq!(d.len(), 2, "{d:?}");
        assert_eq!((d[0].label, d[0].class), ("person", "person"));
        assert!((d[0].score - 0.9526).abs() < 1e-3);
        assert_eq!(d[0].bbox, [400.0, 150.0, 600.0, 350.0]);
        assert_eq!((d[1].label, d[1].class), ("animal", "dog"));
        assert_eq!(
            d[1].bbox,
            [0.0, 350.0, 300.0, 500.0],
            "clipped to the picture"
        );
    }
}
