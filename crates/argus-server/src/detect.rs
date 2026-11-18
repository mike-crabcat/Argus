//! Argus's own detection. Per camera, a thread decodes the stream
//! `[detect]` names (the sub stream by default) as it arrives and picks
//! pictures at a steady rate; one more thread runs the model over the
//! pictures from all cameras, in batches. For now what it finds only feeds
//! counters in the log; tracks and events come next.
//!
//! Decoding is steady CPU work (about 0.5-1 ms a frame at 896x672) that
//! shouldn't sit on the async runtime. It reads the same broadcast as live
//! viewers, so a slow decoder only loses frames itself (and waits for the
//! next keyframe). If the model falls behind, pictures are dropped rather
//! than queued.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use argus_detect::decode::{Decoder, Picture, Sampler};
use argus_detect::model::{Detection, Prepared};
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::live::LiveStream;
use crate::recorder::StreamEvent;

/// How often counters are logged, unless `[detect] log_seconds` says.
static REPORT_EVERY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(600);

fn report_every() -> Duration {
    Duration::from_secs(REPORT_EVERY.load(std::sync::atomic::Ordering::Relaxed))
}

/// Sets how often counters are logged.
pub fn set_report_every(seconds: u64) {
    REPORT_EVERY.store(seconds.max(1), std::sync::atomic::Ordering::Relaxed);
}

/// What one camera's decoder has done.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Counts {
    pub frames: u64,
    pub pictures: u64,
    pub errors: u64,
    /// Times frames were lost (the decoder fell behind or the camera
    /// reconnected) and decoding waited for a keyframe.
    pub resyncs: u64,
    pub decode_time: Duration,
}

/// Decodes `stream` until it closes, handing each sampled picture to
/// `sink`. Returns the counts.
pub fn run(stream: &LiveStream, fps: f64, mut sink: impl FnMut(Picture)) -> Counts {
    let camera = stream.camera.clone();
    let mut c = Counts::default();
    let mut last = Counts::default();
    let mut d = match Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            warn!(%camera, "detection: {e}");
            return c;
        }
    };
    let mut sampler = Sampler::new(fps);
    // Start from the current GOP, so the first picture comes at once.
    let snap = stream.snapshot();
    let mut rx = snap.rx;
    let configure = |d: &mut Decoder, cfg: &argus_media::VideoConfig| {
        if let Err(e) = d.configure(cfg) {
            warn!(%camera, "detection can't decode this stream: {e}");
        }
    };
    if let Some(cfg) = &snap.video {
        configure(&mut d, cfg);
    }
    let mut frame = |d: &mut Decoder, c: &mut Counts, f: &argus_media::Frame, live: bool| {
        let want = live && sampler.take(f.wall_us);
        let t = Instant::now();
        let r = d.decode(f, want);
        c.decode_time += t.elapsed();
        c.frames += 1;
        match r {
            Ok(Some(p)) => {
                c.pictures += 1;
                Some(p)
            }
            Ok(None) => None,
            Err(_) => {
                c.errors += 1;
                None
            }
        }
    };
    for f in &snap.gop {
        frame(&mut d, &mut c, f, false);
    }
    let mut reported = Instant::now();
    loop {
        match rx.blocking_recv() {
            Ok(StreamEvent::Config(cfg)) => configure(&mut d, &cfg),
            Ok(StreamEvent::Frame(f)) => {
                if let Some(p) = frame(&mut d, &mut c, &f, true) {
                    sink(p);
                }
            }
            Ok(StreamEvent::Discontinuity) => {
                d.reset();
                c.resyncs += 1;
            }
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => {
                d.reset();
                c.resyncs += 1;
            }
            Err(RecvError::Closed) => break,
        }
        if reported.elapsed() >= report_every() {
            report(&stream.camera, &last, &c, reported.elapsed());
            last = c.clone();
            reported = Instant::now();
        }
    }
    c
}

fn report(camera: &str, last: &Counts, now: &Counts, over: Duration) {
    let frames = now.frames - last.frames;
    let busy = (now.decode_time - last.decode_time).as_secs_f64();
    info!(
        %camera,
        frames,
        pictures = now.pictures - last.pictures,
        errors = now.errors - last.errors,
        resyncs = now.resyncs - last.resyncs,
        ms_per_frame = format!("{:.2}", busy * 1e3 / frames.max(1) as f64),
        core_pct = format!("{:.1}", busy / over.as_secs_f64() * 100.0),
        "detection: decoded"
    );
}

/// What the model found in one picture.
#[derive(Debug, Clone)]
pub struct Found {
    pub camera: Arc<str>,
    pub wall_us: i64,
    pub detections: Vec<Detection>,
}

/// At most this many pictures go to the model at once.
pub const MAX_BATCH: usize = 8;
/// How long the model waits for more pictures to fill a batch.
const GATHER: Duration = Duration::from_millis(60);

/// What the model thread has done, per camera.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ModelCounts {
    pub pictures: u64,
    /// Objects found, by label.
    pub objects: BTreeMap<&'static str, u64>,
}

/// The model's side: takes pictures from `rx` in batches of whatever is
/// waiting, runs `model` on them, and hands each result to `out`, until
/// every sender is gone.
pub fn run_model(
    rx: Receiver<(Arc<str>, Prepared)>,
    mut model: impl FnMut(&[&Prepared]) -> Result<Vec<Vec<Detection>>, String>,
    mut out: impl FnMut(Found),
) -> BTreeMap<Arc<str>, ModelCounts> {
    let mut counts: BTreeMap<Arc<str>, ModelCounts> = BTreeMap::new();
    let mut last = counts.clone();
    let (mut busy, mut batches, mut failures) = (Duration::ZERO, 0u64, 0u64);
    let mut reported = Instant::now();
    while let Ok(first) = rx.recv() {
        // Wait a moment for other cameras' pictures: a batch of five costs
        // the GPU much less than five batches of one.
        let mut batch = vec![first];
        let until = Instant::now() + GATHER;
        while batch.len() < MAX_BATCH {
            let left = until.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(p) => batch.push(p),
                Err(_) => break,
            }
        }
        let pics: Vec<&Prepared> = batch.iter().map(|(_, p)| p).collect();
        let t = Instant::now();
        let r = model(&pics);
        busy += t.elapsed();
        batches += 1;
        match r {
            Ok(found) => {
                for ((camera, p), detections) in batch.iter().zip(found) {
                    let c = counts.entry(camera.clone()).or_default();
                    c.pictures += 1;
                    for d in &detections {
                        *c.objects.entry(d.label).or_default() += 1;
                    }
                    out(Found {
                        camera: camera.clone(),
                        wall_us: p.wall_us,
                        detections,
                    });
                }
            }
            Err(e) => {
                failures += 1;
                if failures <= 3 || failures.is_power_of_two() {
                    warn!(failures, "detection model: {e}");
                }
            }
        }
        if reported.elapsed() >= report_every() {
            let over = reported.elapsed().as_secs_f64();
            for (camera, c) in &counts {
                let before = last.get(camera).cloned().unwrap_or_default();
                let objects: Vec<String> = c
                    .objects
                    .iter()
                    .map(|(l, n)| format!("{l} {}", n - before.objects.get(l).unwrap_or(&0)))
                    .collect();
                info!(%camera, pictures = c.pictures - before.pictures,
                    objects = objects.join(", "), "detection: found");
            }
            info!(
                batches,
                ms_per_batch = format!("{:.1}", busy.as_secs_f64() * 1e3 / batches.max(1) as f64),
                busy_pct = format!("{:.1}", busy.as_secs_f64() / over * 100.0),
                "detection: model"
            );
            last = counts.clone();
            (busy, batches) = (Duration::ZERO, 0);
            reported = Instant::now();
        }
    }
    counts
}

/// Prepares pictures for the model and hands them to its thread, dropping
/// them if it is behind.
fn sender(
    camera: &str,
    tx: Option<SyncSender<(Arc<str>, Prepared)>>,
) -> impl FnMut(Picture) + use<> {
    let camera: Arc<str> = camera.into();
    let mut dropped = 0u64;
    move |p| {
        let Some(tx) = &tx else { return };
        if let Err(TrySendError::Full(_)) = tx.try_send((camera.clone(), Prepared::new(&p))) {
            dropped += 1;
            if dropped.is_power_of_two() {
                warn!(%camera, dropped, "detection: the model is behind; pictures dropped");
            }
        }
    }
}

/// How `[detect]` runs the model.
pub struct ModelSpec {
    pub path: std::path::PathBuf,
    pub device: argus_detect::model::Device,
    pub threshold: f32,
}

/// Starts a decoding thread for each stream and, with a model, the thread
/// that runs it.
pub fn spawn(streams: Vec<Arc<LiveStream>>, fps: f64, model: Option<ModelSpec>) {
    let tx = model.map(|spec| {
        let (tx, rx) = std::sync::mpsc::sync_channel(2 * MAX_BATCH);
        let r = std::thread::Builder::new()
            .name("detect-model".into())
            .spawn(move || {
                let t = Instant::now();
                let mut m = match argus_detect::model::Detector::open(&spec.path, spec.device) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!(model = %spec.path.display(), "detection model didn't load: {e}");
                        return;
                    }
                };
                m.threshold = spec.threshold;
                info!(device = ?spec.device, secs = format!("{:.1}", t.elapsed().as_secs_f64()),
                    "detection model loaded");
                run_model(
                    rx,
                    |p| m.detect_prepared(p).map_err(|e| e.to_string()),
                    |_| {},
                );
            });
        if let Err(e) = r {
            warn!("detection model thread: {e}");
        }
        tx
    });
    for s in streams {
        let name = format!("detect-{}", s.camera);
        let sink = sender(&s.camera, tx.clone());
        let r = std::thread::Builder::new().name(name).spawn(move || {
            run(&s, fps, sink);
        });
        if let Err(e) = r {
            warn!("detection thread: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_detect::testing::{H, W, encode};

    #[test]
    fn decodes_a_live_stream_at_the_sampled_rate() {
        // 45 frames at 15 fps (3 s), a keyframe every 15.
        let (cfg, frames) = encode(45, 15);
        let s = Arc::new(LiveStream::new("door", "sub", 1024));
        // Joined mid-stream: the GOP so far primes the decoder.
        s.publish(StreamEvent::Config(cfg));
        for f in &frames[..20] {
            s.publish(StreamEvent::Frame(f.clone()));
        }
        let s2 = s.clone();
        let t = std::thread::spawn(move || {
            let mut got = Vec::new();
            let c = run(&s2, 5.0, |p| got.push(p));
            (c, got)
        });
        // Let the thread subscribe before publishing more.
        while s.subscribers() == 0 {
            std::thread::yield_now();
        }
        for f in &frames[20..30] {
            s.publish(StreamEvent::Frame(f.clone()));
        }
        // A reconnect: decoding waits for a keyframe, which frame 30 is.
        s.publish(StreamEvent::Discontinuity);
        for f in &frames[30..] {
            s.publish(StreamEvent::Frame(f.clone()));
        }
        s.close();
        let (c, got) = t.join().unwrap();
        assert_eq!(c.errors, 0);
        assert_eq!(c.resyncs, 1);
        // The GOP so far (frames 15..20), then 25 live frames: 1.6 s at
        // 5 fps is 9 pictures.
        assert_eq!(c.frames, 5 + 25);
        assert_eq!(
            got.len(),
            9,
            "{:?}",
            got.iter().map(|p| p.wall_us).collect::<Vec<_>>()
        );
        assert_eq!(got[0].wall_us, frames[20].wall_us);
        assert!(
            got.iter()
                .all(|p| (p.width, p.height) == (W as u32, H as u32))
        );
        assert_eq!(c.pictures, 9);
    }

    #[test]
    fn the_model_thread_batches_and_counts() {
        let (tx, rx) = std::sync::mpsc::sync_channel(2 * MAX_BATCH);
        let pic = |t: i64| Prepared {
            data: Vec::new(),
            width: 100,
            height: 100,
            wall_us: t,
        };
        // Ten pictures waiting: one batch of 8, then one of 2.
        for i in 0..10 {
            let cam: Arc<str> = if i % 2 == 0 {
                "door".into()
            } else {
                "yard".into()
            };
            tx.send((cam, pic(i))).unwrap();
        }
        drop(tx);
        let mut sizes = Vec::new();
        let mut found = Vec::new();
        let counts = run_model(
            rx,
            |p| {
                sizes.push(p.len());
                Ok(p.iter()
                    .map(|p| {
                        // A person in every picture; a dog in the odd ones.
                        let mut v = vec![Detection {
                            label: "person",
                            class: "person",
                            score: 0.9,
                            bbox: [1.0, 2.0, 3.0, 4.0],
                        }];
                        if p.wall_us % 2 == 1 {
                            v.push(Detection {
                                label: "animal",
                                class: "dog",
                                score: 0.7,
                                bbox: [0.0; 4],
                            });
                        }
                        v
                    })
                    .collect())
            },
            |f| found.push(f),
        );
        assert_eq!(sizes, [8, 2]);
        assert_eq!(found.len(), 10);
        assert_eq!(found[3].wall_us, 3);
        assert_eq!(&*found[3].camera, "yard");
        assert_eq!(found[3].detections.len(), 2);
        let yard = &counts[&Arc::<str>::from("yard")];
        assert_eq!(yard.pictures, 5);
        assert_eq!(yard.objects[&"person"], 5);
        assert_eq!(yard.objects[&"animal"], 5);
        assert!(
            !counts[&Arc::<str>::from("door")]
                .objects
                .contains_key("animal")
        );
    }

    #[test]
    fn a_full_queue_drops_pictures() {
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let mut send = sender("door", Some(tx));
        for t in 0..5 {
            send(Picture {
                width: 1,
                height: 1,
                rgb: vec![0; 3],
                wall_us: t,
            });
        }
        let got: Vec<(i64, usize)> = rx
            .try_iter()
            .map(|(_, p)| (p.wall_us, p.data.len()))
            .collect();
        let n = 3 * argus_detect::model::INPUT * argus_detect::model::INPUT;
        assert_eq!(
            got,
            [(0, n), (1, n)],
            "prepared; the rest dropped, not queued"
        );
    }
}
