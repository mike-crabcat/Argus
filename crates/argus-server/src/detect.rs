//! Argus's own detection. Per camera, a thread decodes the stream
//! `[detect]` names (the sub stream by default) as it arrives and picks
//! pictures at a steady rate; one more thread runs the model over the
//! pictures from all cameras, in batches, and [`Tracking`] turns what it
//! finds into tracks: each moving spell is stored, and per camera can also
//! become an event in place of the camera's own.
//!
//! Decoding is steady CPU work (about 0.5-1 ms a frame at 896x672) that
//! shouldn't sit on the async runtime. It reads the same broadcast as live
//! viewers, so a slow decoder only loses frames itself (and waits for the
//! next keyframe). If the model falls behind, pictures are dropped rather
//! than queued.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argus_detect::decode::{Decoder, Picture, Sampler};
use argus_detect::model::{Detection, Prepared};
use argus_detect::track::{Change, Params as TrackParams, Seen, Tracker};
use argus_store::Index;
use argus_store::tracks::TrackBox;
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
    /// The picture's size (the boxes are in its pixels).
    pub width: u32,
    pub height: u32,
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
                        width: p.width,
                        height: p.height,
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
    keep_picture: bool,
) -> impl FnMut(Picture) + use<> {
    let camera: Arc<str> = camera.into();
    let mut dropped = 0u64;
    move |p| {
        let Some(tx) = &tx else { return };
        let mut prepared = Prepared::new(&p);
        if keep_picture {
            prepared.picture = Some(p);
        }
        if let Err(TrySendError::Full(_)) = tx.try_send((camera.clone(), prepared)) {
            dropped += 1;
            if dropped.is_power_of_two() {
                warn!(%camera, dropped, "detection: the model is behind; pictures dropped");
            }
        }
    }
}

/// What a camera's tracks become.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Stored as tracks only, to compare with the camera's own events.
    Shadow,
    /// Also events (source "argus"), in place of the camera's own.
    Events,
}

/// Runs `f` with the id of each new event (to take stills).
pub type OnEvent = Arc<dyn Fn(i64) + Send + Sync>;

/// Boxes are written at most this often while a track moves.
const FLUSH_US: i64 = 1_000_000;

struct Spell {
    track_id: i64,
    event_id: Option<i64>,
    boxes: Vec<TrackBox>,
    flushed_us: i64,
    /// The smoothed floor position so far.
    at: Option<argus_plan::Point>,
}

/// How much each new floor position counts against the path so far
/// (exponential smoothing: foot points jitter a little picture to picture).
const SMOOTHING: f64 = 0.5;

struct CameraTracks {
    mode: Mode,
    tracker: Tracker,
    spells: HashMap<u64, Spell>,
    on_event: Option<OnEvent>,
}

/// Turns what the model finds into tracks (and, per camera, events).
pub struct Tracking {
    index: Arc<Mutex<Index>>,
    cameras: HashMap<Arc<str>, CameraTracks>,
    plan: Option<Arc<crate::plan::PlanStore>>,
    /// Placed cameras by name, with their level, as of `plan_version`.
    placed: HashMap<String, (i64, argus_plan::CameraPlacement)>,
    plan_version: Option<u64>,
}

fn ms(us: i64) -> i64 {
    us.div_euclid(1000)
}

impl Tracking {
    /// With a `plan`, positions are mapped onto it through each camera's
    /// calibration.
    pub fn new(index: Arc<Mutex<Index>>, plan: Option<Arc<crate::plan::PlanStore>>) -> Self {
        Self {
            index,
            cameras: HashMap::new(),
            plan,
            placed: HashMap::new(),
            plan_version: None,
        }
    }

    /// Re-reads camera placements when the plan has changed.
    fn refresh_plan(&mut self) {
        let Some(store) = &self.plan else { return };
        let v = store.version();
        if self.plan_version == Some(v) {
            return;
        }
        self.plan_version = Some(v);
        self.placed = store
            .get()
            .plan
            .levels
            .iter()
            .flat_map(|l| {
                l.cameras
                    .iter()
                    .map(move |c| (c.camera.clone(), (l.id.0 as i64, c.clone())))
            })
            .collect();
    }

    /// Tracks `camera`: `hold` is how long a track may sit still before
    /// its moving spell (and event) ends.
    pub fn camera(&mut self, camera: &str, mode: Mode, hold: Duration, on_event: Option<OnEvent>) {
        let params = TrackParams {
            hold_us: hold.as_micros() as i64,
            ..TrackParams::default()
        };
        self.cameras.insert(
            camera.into(),
            CameraTracks {
                mode,
                tracker: Tracker::new(params),
                spells: HashMap::new(),
                on_event,
            },
        );
    }

    pub fn feed(&mut self, f: &Found) {
        if !self.cameras.contains_key(&f.camera) {
            return;
        }
        self.refresh_plan();
        let placed = self
            .placed
            .get(&*f.camera)
            .filter(|(_, c)| c.calibration.is_some());
        let level = placed.map(|(l, _)| *l);
        let size = (f.width as f64, f.height as f64);
        let Some(c) = self.cameras.get_mut(&f.camera) else {
            return;
        };
        // A track's box (and where it stands, if calibrated).
        let make = |t: &argus_detect::track::Track, at: &mut Option<argus_plan::Point>| {
            let mut b = tbox(t);
            if let Some((_, cam)) = placed {
                let foot = t.foot.unwrap_or([(t.bbox[0] + t.bbox[2]) / 2.0, t.bbox[3]]);
                let image = [foot[0] as f64 / size.0, foot[1] as f64 / size.1];
                if let Some(p) = argus_plan::calib::floor_point(cam, image) {
                    let s = match *at {
                        Some(prev) => prev.scale(1.0 - SMOOTHING) + p.scale(SMOOTHING),
                        None => p,
                    };
                    *at = Some(s);
                    b.plan = Some([s.x, s.y]);
                }
            }
            b
        };
        let seen: Vec<Seen> = f
            .detections
            .iter()
            .map(|d| Seen {
                label: d.label,
                class: d.class,
                score: d.score,
                bbox: d.bbox,
                foot: d.foot.map(|f| [f.x, f.y]),
            })
            .collect();
        let changes = c.tracker.update(f.wall_us, &seen);
        if changes.is_empty() {
            return;
        }
        let index = self.index.lock().unwrap();
        for ch in changes {
            let r = match ch {
                Change::Moving(t) => {
                    let start = ms(t.moving_since_us.unwrap_or(t.last_us));
                    let event_id = match c.mode {
                        Mode::Events => index
                            .add_event(&f.camera, start, ms(t.last_us), "argus", t.label)
                            .ok(),
                        Mode::Shadow => None,
                    };
                    let r = index.add_track(
                        &f.camera,
                        t.label,
                        t.class,
                        start,
                        t.best_score,
                        f.width,
                        f.height,
                        event_id,
                        level,
                    );
                    if let Some(id) = event_id {
                        info!(camera = %f.camera, label = t.label, "event started (detection)");
                        if let Some(cb) = &c.on_event {
                            cb(id);
                        }
                    }
                    r.map(|track_id| {
                        let mut at = None;
                        let first = make(&t, &mut at);
                        c.spells.insert(
                            t.id,
                            Spell {
                                track_id,
                                event_id,
                                boxes: vec![first],
                                flushed_us: t.last_us,
                                at,
                            },
                        );
                    })
                }
                Change::StillMoving(t) => match c.spells.get_mut(&t.id) {
                    Some(s) => {
                        let b = make(&t, &mut s.at);
                        s.boxes.push(b);
                        if t.last_us - s.flushed_us >= FLUSH_US {
                            s.flushed_us = t.last_us;
                            flush(&index, s, &t, ms(t.last_us))
                        } else {
                            Ok(())
                        }
                    }
                    None => Ok(()),
                },
                Change::Stopped(t) => match c.spells.remove(&t.id) {
                    Some(mut s) => {
                        let end = ms(t.last_us);
                        s.boxes.retain(|b| b.t_ms <= end);
                        flush(&index, &mut s, &t, end)
                    }
                    None => Ok(()),
                },
                Change::Ended(_) => Ok(()),
            };
            if let Err(e) = r {
                warn!(camera = %f.camera, "storing a track: {e:#}");
            }
        }
    }
}

fn tbox(t: &argus_detect::track::Track) -> TrackBox {
    TrackBox {
        t_ms: ms(t.last_us),
        bbox: t.bbox,
        score: t.best_score,
        foot: t.foot,
        plan: None,
    }
}

fn flush(
    index: &Index,
    s: &mut Spell,
    t: &argus_detect::track::Track,
    end_ms: i64,
) -> anyhow::Result<()> {
    index.extend_track(s.track_id, end_ms, t.class, t.best_score, &s.boxes)?;
    s.boxes.clear();
    if let Some(id) = s.event_id {
        index.set_event_end(id, end_ms)?;
    }
    Ok(())
}

/// How `[detect]` runs the model.
pub struct ModelSpec {
    pub path: std::path::PathBuf,
    pub device: argus_detect::model::Device,
    pub threshold: f32,
    /// The pose model, for people's foot points.
    pub pose: Option<std::path::PathBuf>,
}

/// Adds foot points to the people found in each picture. If the pose
/// model fails, people keep the bottom centre of their box.
fn add_feet(
    pose: &mut argus_detect::pose::PoseModel,
    pictures: &[&Prepared],
    found: &mut [Vec<Detection>],
) {
    for (p, dets) in pictures.iter().zip(found.iter_mut()) {
        let people: Vec<usize> = (0..dets.len())
            .filter(|&i| dets[i].label == "person")
            .collect();
        if people.is_empty() {
            continue;
        }
        let boxes: Vec<_> = people.iter().map(|&i| dets[i].bbox).collect();
        let feet = match &p.picture {
            Some(pic) => pose.feet(pic, &boxes).unwrap_or_else(|e| {
                warn!("pose model: {e}");
                boxes.iter().map(argus_detect::pose::box_foot).collect()
            }),
            None => boxes.iter().map(argus_detect::pose::box_foot).collect(),
        };
        for (&i, f) in people.iter().zip(feet) {
            dets[i].foot = Some(f);
        }
    }
}

/// Starts a decoding thread for each stream and, with a model, the thread
/// that runs it, feeding `tracking`.
pub fn spawn(
    streams: Vec<Arc<LiveStream>>,
    fps: f64,
    model: Option<ModelSpec>,
    mut tracking: Tracking,
) {
    let keep_pictures = model.as_ref().is_some_and(|m| m.pose.is_some());
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
                let mut pose = spec.pose.as_ref().and_then(|path| {
                    match argus_detect::pose::PoseModel::open(path, spec.device) {
                        Ok(p) => {
                            info!("pose model loaded");
                            Some(p)
                        }
                        Err(e) => {
                            warn!(model = %path.display(), "pose model didn't load: {e}");
                            None
                        }
                    }
                });
                run_model(
                    rx,
                    |p| {
                        let mut found = m.detect_prepared(p).map_err(|e| e.to_string())?;
                        if let Some(pose) = &mut pose {
                            add_feet(pose, p, &mut found);
                        }
                        Ok(found)
                    },
                    |f| tracking.feed(&f),
                );
            });
        if let Err(e) = r {
            warn!("detection model thread: {e}");
        }
        tx
    });
    for s in streams {
        let name = format!("detect-{}", s.camera);
        let sink = sender(&s.camera, tx.clone(), keep_pictures);
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
            picture: None,
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
                            foot: None,
                        }];
                        if p.wall_us % 2 == 1 {
                            v.push(Detection {
                                label: "animal",
                                class: "dog",
                                score: 0.7,
                                bbox: [0.0; 4],
                                foot: None,
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
        let mut send = sender("door", Some(tx), false);
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

    fn walker(camera: &str, i: i64, present: bool) -> Found {
        Found {
            camera: camera.into(),
            wall_us: 1_800_000_000_000_000 + i * 200_000,
            width: 896,
            height: 672,
            detections: if present {
                let x = 100.0 + 15.0 * i as f32;
                vec![Detection {
                    label: "person",
                    class: "person",
                    score: 0.9,
                    bbox: [x, 200.0, x + 50.0, 320.0],
                    foot: Some(argus_detect::pose::Foot {
                        x: x + 25.0,
                        y: 318.0,
                        ankles: true,
                    }),
                }]
            } else {
                Vec::new()
            },
        }
    }

    #[test]
    fn tracks_become_events_only_where_asked() {
        let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
        let started = Arc::new(Mutex::new(Vec::new()));
        let s2 = started.clone();
        let mut t = Tracking::new(index.clone(), None);
        t.camera(
            "door",
            Mode::Events,
            Duration::from_secs(5),
            Some(Arc::new(move |id| s2.lock().unwrap().push(id))),
        );
        t.camera("yard", Mode::Shadow, Duration::from_secs(5), None);
        // A person walks past each camera for 4 s, then is gone for 3 s.
        for i in 0..35 {
            t.feed(&walker("door", i, i < 20));
            t.feed(&walker("yard", i, i < 20));
        }
        // An unknown camera is ignored.
        t.feed(&walker("garage", 0, true));
        let i = index.lock().unwrap();
        let tracks = i.tracks(None, 0, i64::MAX).unwrap();
        assert_eq!(tracks.len(), 2, "{tracks:?}");
        let door = tracks.iter().find(|t| t.camera == "door").unwrap();
        let yard = tracks.iter().find(|t| t.camera == "yard").unwrap();
        assert!(yard.event_id.is_none());
        let ev = door.event_id.expect("an event on the door camera");
        assert_eq!(*started.lock().unwrap(), [ev], "stills were asked for");
        let t0 = 1_800_000_000_000i64;
        // Moving from picture 2 (confirmed at its third sighting, having
        // moved 15 px, more than a quarter of its width); last seen at 19.
        assert_eq!(door.start_ms, t0 + 400);
        assert_eq!(door.end_ms, t0 + 19 * 200);
        assert_eq!(
            (door.width, door.height, door.label.as_str()),
            (896, 672, "person")
        );
        let events = i.events("door", 0, i64::MAX).unwrap();
        assert_eq!(
            events,
            [(door.start_ms, door.end_ms, "argus".into(), "person".into())]
        );
        assert!(i.events("yard", 0, i64::MAX).unwrap().is_empty());
        // Every picture from when the spell was reported (its third moving
        // picture) to the end has its box.
        let boxes = i.track_boxes(door.id).unwrap();
        assert_eq!(
            boxes.len(),
            16,
            "{:?}",
            boxes.iter().map(|b| b.t_ms - t0).collect::<Vec<_>>()
        );
        assert_eq!(boxes.last().unwrap().t_ms, door.end_ms);
        let last = boxes.last().unwrap();
        assert_eq!(
            last.foot,
            Some([last.bbox[0] + 25.0, 318.0]),
            "foot points kept"
        );
    }
}

/// The detection API: tracks, and how they compare with the cameras' own
/// events (for shadow mode).
pub mod http {
    use std::collections::HashMap;

    use axum::extract::{Query, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Json, Response};
    use serde_json::json;

    use crate::http::AppState;
    use crate::replay::http::{int, recordings_or_404};

    /// `GET /api/detect/compare?hours=24`: per camera, the camera's own
    /// events and Argus's tracks over the last `hours`, and how many of
    /// each the other also saw.
    pub async fn compare(
        State(st): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Response {
        let rec = match recordings_or_404(&st) {
            Ok(r) => r,
            Err(e) => return *e,
        };
        let now = crate::recorder::now_us() / 1000;
        let hours = int(&q, "hours").unwrap_or(24).clamp(1, 24 * 31);
        let from = now - hours * 3_600_000;
        match rec.index.lock().unwrap().compare_tracks(from, now) {
            Ok(c) => Json(json!({"from": from, "to": now, "cameras": c})).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    }

    /// `GET /api/cameras/{camera}/snapshot`: a JPEG of what the camera
    /// sees now (for calibrating it), from its own snapshot API.
    pub async fn snapshot(
        State(st): State<AppState>,
        axum::extract::Path(camera): axum::extract::Path<String>,
    ) -> Response {
        let Some(src) = st.snapshots.as_ref().and_then(|m| m.get(&camera)).cloned() else {
            return (StatusCode::NOT_FOUND, "no snapshots for that camera").into_response();
        };
        match src.grab().await {
            Ok(jpeg) => (
                [
                    (axum::http::header::CONTENT_TYPE, "image/jpeg"),
                    (axum::http::header::CACHE_CONTROL, "no-store"),
                ],
                jpeg,
            )
                .into_response(),
            Err(e) => (StatusCode::BAD_GATEWAY, format!("snapshot: {e:#}")).into_response(),
        }
    }

    #[derive(serde::Deserialize)]
    pub struct CalibrateBody {
        pub pairs: Vec<argus_plan::calib::CalPair>,
    }

    /// `POST /api/plan/calibrate` with `{"pairs": [...]}`: the fit those
    /// pairs give (homography, each pair's error in mm, outliers), without
    /// saving anything. `set_calibration` saves it.
    pub async fn calibrate(axum::Json(b): axum::Json<CalibrateBody>) -> Response {
        match argus_plan::calib::fit(&b.pairs) {
            Ok(c) => Json(c).into_response(),
            Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        }
    }

    /// `GET /api/detect/tracks?camera=&from=&to=&boxes=1`: tracks (moving
    /// spells), oldest first, optionally with their boxes.
    pub async fn tracks(
        State(st): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Response {
        let rec = match recordings_or_404(&st) {
            Ok(r) => r,
            Err(e) => return *e,
        };
        let now = crate::recorder::now_us() / 1000;
        let from = int(&q, "from").unwrap_or(now - 3_600_000);
        let to = int(&q, "to").unwrap_or(now + 60_000);
        let with_boxes = q.get("boxes").is_some_and(|v| v == "1");
        let index = rec.index.lock().unwrap();
        let rows = match index.tracks(q.get("camera").map(String::as_str), from, to) {
            Ok(r) => r,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
        let out: Vec<serde_json::Value> = rows
            .iter()
            .take(5000)
            .map(|t| {
                let mut v = serde_json::to_value(t).unwrap_or_default();
                if with_boxes {
                    v["boxes"] = serde_json::to_value(index.track_boxes(t.id).unwrap_or_default())
                        .unwrap_or_default();
                }
                v
            })
            .collect();
        Json(json!({"from": from, "to": to, "tracks": out})).into_response()
    }
}

#[cfg(test)]
mod position_tests {
    use super::*;

    #[test]
    fn positions_on_the_demo_house() {
        use argus_plan::calib::{picture_map, pinhole, to_picture};
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::plan::PlanStore::open(dir.path()).unwrap());
        let mut v = store.get();
        v.plan = argus_plan::fixture::demo_house();
        argus_plan::fixture::calibrate_ideal(&mut v.plan).unwrap();
        store.replace(v).unwrap();
        let plan = store.get().plan;
        let cam = plan.levels[0]
            .cameras
            .iter()
            .find(|c| c.camera == "back")
            .unwrap()
            .clone();
        let level = plan.levels[0].id.0 as i64;
        // Someone walks across the back camera's view, 5 m out: where would
        // their feet be in a 896x672 picture?
        let inv = picture_map(&pinhole(&cam, 4.0 / 3.0).unwrap()).unwrap();
        let (h, a) = (cam.heading.to_radians(), cam.at);
        let walk = |i: i64| {
            let side = -2000.0 + 150.0 * i as f64; // across, left to right
            let (fx, fy) = (h.cos(), h.sin());
            argus_plan::pt(a.x + 5000.0 * fx + side * fy, a.y + 5000.0 * fy - side * fx)
        };
        let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
        let mut t = Tracking::new(index.clone(), Some(store.clone()));
        t.camera("back", Mode::Shadow, Duration::from_secs(5), None);
        let t0 = 1_800_000_000_000_000i64;
        for i in 0..25 {
            let p = walk(i);
            let [u, v] = to_picture(&inv, p).unwrap();
            let (fx, fy) = ((u * 896.0) as f32, (v * 672.0) as f32);
            t.feed(&Found {
                camera: "back".into(),
                wall_us: t0 + i * 200_000,
                width: 896,
                height: 672,
                detections: vec![Detection {
                    label: "person",
                    class: "person",
                    score: 0.9,
                    bbox: [fx - 20.0, fy - 120.0, fx + 20.0, fy],
                    foot: Some(argus_detect::pose::Foot {
                        x: fx,
                        y: fy,
                        ankles: true,
                    }),
                }],
            });
        }
        for i in 25..40 {
            t.feed(&Found {
                camera: "back".into(),
                wall_us: t0 + i * 200_000,
                width: 896,
                height: 672,
                detections: vec![],
            });
        }
        let i = index.lock().unwrap();
        let tracks = i.tracks(Some("back"), 0, i64::MAX).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].level, Some(level));
        let boxes = i.track_boxes(tracks[0].id).unwrap();
        assert!(boxes.len() > 15);
        // Each position is near where they were (smoothing lags a little).
        for b in &boxes {
            let k = (b.t_ms * 1000 - t0) / 200_000;
            let want = walk(k);
            let [x, y] = b.plan.expect("a position");
            let off = argus_plan::pt(x, y).dist(want);
            assert!(off < 250.0, "picture {k}: {off} mm off");
        }
    }
}
