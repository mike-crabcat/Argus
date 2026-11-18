//! Argus's own detection, per camera: decodes the stream `[detect]` names
//! (the sub stream by default) as it arrives and picks pictures at a steady
//! rate. For now the pictures only feed counters; the detector comes next.
//!
//! Each camera gets its own thread: decoding is steady CPU work (about
//! 0.5-1 ms a frame at 896x672) that shouldn't sit on the async runtime.
//! It reads the same broadcast as live viewers, so a slow decoder only
//! loses frames itself (and waits for the next keyframe).

use std::sync::Arc;
use std::time::{Duration, Instant};

use argus_detect::decode::{Decoder, Picture, Sampler};
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::live::LiveStream;
use crate::recorder::StreamEvent;

/// How often each camera's counters are logged.
const REPORT_EVERY: Duration = Duration::from_secs(600);

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
        if reported.elapsed() >= REPORT_EVERY {
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

/// Starts a decoding thread for each stream. Pictures go nowhere yet.
pub fn spawn(streams: Vec<Arc<LiveStream>>, fps: f64) {
    for s in streams {
        let name = format!("detect-{}", s.camera);
        let r = std::thread::Builder::new().name(name).spawn(move || {
            run(&s, fps, |_| {});
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
}
