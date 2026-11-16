//! Media primitives shared by every Argus component.
//!
//! Nothing in here decodes or re-encodes video. Frames are carried exactly as
//! the camera produced them (4-byte length-prefixed H.264/H.265 NAL units) and
//! are only re-framed into fragmented MP4 for storage.

pub mod clock;
pub mod config;
pub mod fmp4;
mod mp4box;

pub use clock::WallClock;
pub use config::{Codec, VideoConfig};

use bytes::Bytes;

/// 90 kHz, the RTP video clock and the timescale used for all stored media.
pub const TIMESCALE: u32 = 90_000;

/// One encoded video frame (an "access unit"), untouched from the camera.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Presentation time in 90 kHz units, monotonic within one camera session.
    pub pts_90k: i64,
    /// Estimated wall-clock capture time in microseconds since the Unix epoch.
    /// This, not `pts_90k`, is what multi-camera sync is based on.
    pub wall_us: i64,
    /// True for IDR / IRAP frames, where decoding can start.
    pub keyframe: bool,
    /// NAL units, each prefixed with a 4-byte big-endian length.
    pub data: Bytes,
}

impl Frame {
    pub fn wall_ms(&self) -> i64 {
        self.wall_us.div_euclid(1000)
    }
}
