//! Glue between ingest and storage for one camera stream.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use argus_ingest::SourceEvent;
use argus_media::{AudioConfig, AudioFrame, Frame, VideoConfig, WallClock};
use argus_store::ChunkWriter;
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

/// What every subscriber of a stream sees: storage now, live viewers and
/// analysis in later phases.
#[derive(Clone, Debug)]
pub enum StreamEvent {
    Config(VideoConfig),
    Frame(Frame),
    AudioConfig(AudioConfig),
    Audio(AudioFrame),
    /// The source reconnected; timestamps restart.
    Discontinuity,
}

pub fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Assigns wall-clock times to raw frames. Audio uses the video's anchor,
/// since both come from the same camera clock.
#[derive(Default)]
pub struct Timestamper {
    clock: WallClock,
    audio_rate: Option<u32>,
}

impl Timestamper {
    pub fn apply(&mut self, ev: SourceEvent, arrival_us: i64) -> StreamEvent {
        match ev {
            SourceEvent::Config(c) => StreamEvent::Config(c),
            SourceEvent::Frame(f) => StreamEvent::Frame(Frame {
                wall_us: self.clock.map(f.pts_90k, arrival_us),
                pts_90k: f.pts_90k,
                keyframe: f.keyframe,
                data: f.data,
            }),
            SourceEvent::AudioConfig(c) => {
                self.audio_rate = Some(c.sample_rate);
                StreamEvent::AudioConfig(c)
            }
            SourceEvent::Audio(a) => {
                let wall_us = match self.clock.estimate(a.pts_90k) {
                    Some(t) => t,
                    None => self.clock.map(a.pts_90k, arrival_us),
                };
                let rate = i64::from(self.audio_rate.unwrap_or(90_000));
                StreamEvent::Audio(AudioFrame {
                    pts: a.pts_90k * rate / 90_000,
                    wall_us,
                    data: a.data,
                })
            }
        }
    }
}

/// Keeps one camera stream connected forever, with capped exponential
/// backoff, publishing everything it receives on `out`.
pub async fn supervise(
    camera: String,
    stream: String,
    source: String,
    out: broadcast::Sender<StreamEvent>,
) {
    let mut backoff = Duration::from_secs(1);
    let redacted = argus_ingest::redact(&source);
    loop {
        info!(%camera, %stream, "connecting to {redacted}");
        let (tx, mut rx) = mpsc::channel(256);
        let src = source.clone();
        let conn = tokio::spawn(async move { argus_ingest::run(&src, tx).await });
        let mut ts = Timestamper::default();
        let mut got_frames = false;
        while let Some(ev) = rx.recv().await {
            got_frames |= matches!(ev, SourceEvent::Frame(_));
            let _ = out.send(ts.apply(ev, now_us()));
        }
        match conn.await {
            Ok(Ok(())) => return, // receiver closed: shutting down
            Ok(Err(e)) => warn!(%camera, %stream, "stream error: {e:#}"),
            Err(e) => warn!(%camera, %stream, "ingest task failed: {e}"),
        }
        let _ = out.send(StreamEvent::Discontinuity);
        if got_frames {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Drains a stream into a chunk writer. Runs on a blocking thread because
/// it does synchronous file and SQLite I/O.
pub fn record(mut rx: broadcast::Receiver<StreamEvent>, mut writer: ChunkWriter) -> Result<()> {
    loop {
        let res = match rx.blocking_recv() {
            Ok(StreamEvent::Config(c)) => writer.on_config(&c),
            Ok(StreamEvent::Frame(f)) => writer.on_frame(f),
            Ok(StreamEvent::AudioConfig(c)) => writer.on_audio_config(&c),
            Ok(StreamEvent::Audio(a)) => writer.on_audio(a),
            Ok(StreamEvent::Discontinuity) => writer.on_discontinuity(),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("recorder fell behind, skipped {n} events");
                writer.on_discontinuity()
            }
            Err(broadcast::error::RecvError::Closed) => return writer.finish(),
        };
        if let Err(e) = res {
            warn!("recording error: {e:#}");
        }
    }
}
