//! Live fan-out: every camera stream's events go to the recorder and to any
//! number of live viewers, and new viewers can start decoding at once.
//!
//! [`LiveStream::publish`] keeps the current codec config and the frames
//! since the last keyframe (the current GOP), and broadcasts the event, all
//! under one lock. A viewer takes a snapshot and subscribes under the same
//! lock, so it gets the GOP so far and then every later frame, with no gap
//! and no duplicate. The broadcast channel never blocks the publisher: a
//! slow subscriber only loses events itself.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use argus_media::{AudioConfig, Frame, VideoConfig};
use tokio::sync::broadcast;

use crate::recorder::StreamEvent;

/// Frames kept for new viewers. A GOP is normally 1-2 s; this bounds memory
/// if a camera stops sending keyframes.
const MAX_CACHED_FRAMES: usize = 300;

/// What a new viewer needs to start.
pub struct Snapshot {
    pub video: Option<VideoConfig>,
    /// The current GOP, starting with its keyframe (empty if none yet).
    pub gop: Vec<Frame>,
    pub rx: broadcast::Receiver<StreamEvent>,
}

struct State {
    /// None once closed.
    tx: Option<broadcast::Sender<StreamEvent>>,
    video: Option<VideoConfig>,
    audio: Option<AudioConfig>,
    gop: Vec<Frame>,
}

pub struct LiveStream {
    pub camera: String,
    pub stream: String,
    state: Mutex<State>,
}

impl LiveStream {
    pub fn new(camera: &str, stream: &str, capacity: usize) -> Self {
        Self {
            camera: camera.to_owned(),
            stream: stream.to_owned(),
            state: Mutex::new(State {
                tx: Some(broadcast::channel(capacity).0),
                video: None,
                audio: None,
                gop: Vec::new(),
            }),
        }
    }

    /// Ends the stream: subscribers see the channel close once they have
    /// drained it (this is how the recorder learns to finish its chunk).
    pub fn close(&self) {
        self.state.lock().unwrap().tx = None;
    }

    pub fn publish(&self, ev: StreamEvent) {
        let mut st = self.state.lock().unwrap();
        match &ev {
            StreamEvent::Config(c) => {
                if st.video.as_ref() != Some(c) {
                    st.gop.clear();
                }
                st.video = Some(c.clone());
            }
            StreamEvent::Frame(f) => {
                if f.keyframe {
                    st.gop.clear();
                }
                if !st.gop.is_empty() || f.keyframe {
                    if st.gop.len() >= MAX_CACHED_FRAMES {
                        st.gop.clear(); // wait for the next keyframe
                    } else {
                        st.gop.push(f.clone());
                    }
                }
            }
            StreamEvent::AudioConfig(c) => st.audio = Some(c.clone()),
            StreamEvent::Audio(_) => {}
            StreamEvent::Discontinuity => st.gop.clear(),
        }
        // Errors only mean nobody is subscribed right now.
        if let Some(tx) = &st.tx {
            let _ = tx.send(ev);
        }
    }

    fn receiver(st: &State) -> broadcast::Receiver<StreamEvent> {
        match &st.tx {
            Some(tx) => tx.subscribe(),
            None => broadcast::channel(1).1, // closed: ends immediately
        }
    }

    /// For the recorder: every event from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<StreamEvent> {
        Self::receiver(&self.state.lock().unwrap())
    }

    /// For a viewer: the state so far, and every event after it.
    pub fn snapshot(&self) -> Snapshot {
        let st = self.state.lock().unwrap();
        Snapshot {
            video: st.video.clone(),
            gop: st.gop.clone(),
            rx: Self::receiver(&st),
        }
    }

    pub fn video_config(&self) -> Option<VideoConfig> {
        self.state.lock().unwrap().video.clone()
    }

    pub fn audio_config(&self) -> Option<AudioConfig> {
        self.state.lock().unwrap().audio.clone()
    }

    /// Number of live viewers plus the recorder.
    pub fn subscribers(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .tx
            .as_ref()
            .map_or(0, |t| t.receiver_count())
    }
}

/// All live streams, by (camera, stream).
#[derive(Default)]
pub struct Live {
    streams: BTreeMap<(String, String), Arc<LiveStream>>,
}

impl Live {
    pub fn add(&mut self, stream: LiveStream) -> Arc<LiveStream> {
        let s = Arc::new(stream);
        self.streams
            .insert((s.camera.clone(), s.stream.clone()), s.clone());
        s
    }

    pub fn get(&self, camera: &str, stream: &str) -> Option<Arc<LiveStream>> {
        self.streams
            .get(&(camera.to_owned(), stream.to_owned()))
            .cloned()
    }

    pub fn all(&self) -> impl Iterator<Item = &Arc<LiveStream>> {
        self.streams.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    pub fn frame(i: i64, key: bool) -> Frame {
        Frame {
            pts_90k: i * 9000,
            wall_us: i * 100_000,
            keyframe: key,
            data: Bytes::from_static(&[0, 0, 0, 1, 0x65]),
        }
    }

    #[test]
    fn snapshot_holds_current_gop_and_no_gap() {
        let s = LiveStream::new("c", "s", 64);
        s.publish(StreamEvent::Frame(frame(0, false))); // before any keyframe: not cached
        s.publish(StreamEvent::Frame(frame(1, true)));
        s.publish(StreamEvent::Frame(frame(2, false)));
        let mut snap = s.snapshot();
        assert_eq!(
            snap.gop
                .iter()
                .map(|f| f.pts_90k / 9000)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        s.publish(StreamEvent::Frame(frame(3, false)));
        let StreamEvent::Frame(f) = snap.rx.try_recv().unwrap() else {
            panic!()
        };
        assert_eq!(f.pts_90k / 9000, 3);
        s.publish(StreamEvent::Frame(frame(4, true)));
        assert_eq!(s.snapshot().gop.len(), 1);
        s.publish(StreamEvent::Discontinuity);
        assert!(s.snapshot().gop.is_empty());
    }
}
