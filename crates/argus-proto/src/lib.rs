//! The live view wire protocol (WebSocket today, WebTransport later).
//!
//! The server sends JSON text messages ([`ServerText`]) and binary video
//! frames: a fixed [`FrameHeader`] followed by the access unit as 4-byte
//! length-prefixed NAL units, which WebCodecs decodes with the config's
//! `description`. The client may send [`ClientText::Ping`] to estimate the
//! offset between its clock and the server's.

use serde::{Deserialize, Serialize};

/// Binary message kind: one video access unit.
pub const KIND_VIDEO: u8 = 1;

/// Binary message kind: one recorded video access unit on the replay
/// socket, for one of several cameras. See [`ReplayHeader`].
pub const KIND_REPLAY_VIDEO: u8 = 2;

/// Bytes before the access unit in a replay video message.
pub const REPLAY_HEADER_LEN: usize = 19;

/// Header of a binary replay message:
///
/// ```text
/// 0      u8   kind (2 = replay video)
/// 1      u8   flags: bit 0 keyframe
/// 2      u8   camera: index into the latest `replay` message's list
/// 3..11  i64  pts, 90 kHz (the recorded decode time)
/// 11..19 i64  wall-clock time of the frame, µs since the Unix epoch
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayHeader {
    pub camera: u8,
    pub keyframe: bool,
    pub pts_90k: i64,
    pub wall_us: i64,
}

impl ReplayHeader {
    pub fn encode(&self) -> [u8; REPLAY_HEADER_LEN] {
        let mut b = [0u8; REPLAY_HEADER_LEN];
        b[0] = KIND_REPLAY_VIDEO;
        b[1] = u8::from(self.keyframe);
        b[2] = self.camera;
        b[3..11].copy_from_slice(&self.pts_90k.to_be_bytes());
        b[11..19].copy_from_slice(&self.wall_us.to_be_bytes());
        b
    }
}

/// Splits a binary replay message into its header and access unit.
pub fn parse_replay_frame(msg: &[u8]) -> Result<(ReplayHeader, &[u8]), ParseError> {
    if msg.len() < REPLAY_HEADER_LEN {
        return Err(ParseError::TooShort);
    }
    if msg[0] != KIND_REPLAY_VIDEO {
        return Err(ParseError::UnknownKind(msg[0]));
    }
    let i = |r: std::ops::Range<usize>| i64::from_be_bytes(msg[r].try_into().unwrap());
    Ok((
        ReplayHeader {
            keyframe: msg[1] & 1 != 0,
            camera: msg[2],
            pts_90k: i(3..11),
            wall_us: i(11..19),
        },
        &msg[REPLAY_HEADER_LEN..],
    ))
}

/// Bytes before the access unit in a video message.
pub const HEADER_LEN: usize = 26;

const FLAG_KEYFRAME: u8 = 1;
const FLAG_REPLAY: u8 = 2;

/// Header of a binary video message:
///
/// ```text
/// 0      u8   kind (1 = video)
/// 1      u8   flags: bit 0 keyframe, bit 1 replay
/// 2..10  i64  pts, 90 kHz
/// 10..18 i64  wall-clock time of the frame, µs since the Unix epoch
/// 18..26 i64  when the server received it from the camera, µs since the
///             Unix epoch (server clock; 0 for replayed frames)
/// ```
/// All integers big endian.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub keyframe: bool,
    /// Part of the cached GOP sent on connect so decoding can start at once;
    /// decode it, but it is not live.
    pub replay: bool,
    pub pts_90k: i64,
    pub wall_us: i64,
    pub recv_us: i64,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0] = KIND_VIDEO;
        b[1] = (u8::from(self.keyframe) * FLAG_KEYFRAME) | (u8::from(self.replay) * FLAG_REPLAY);
        b[2..10].copy_from_slice(&self.pts_90k.to_be_bytes());
        b[10..18].copy_from_slice(&self.wall_us.to_be_bytes());
        b[18..26].copy_from_slice(&self.recv_us.to_be_bytes());
        b
    }

    /// Microseconds, the unit WebCodecs timestamps use.
    pub fn pts_us(&self) -> i64 {
        self.pts_90k * 100 / 9
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    TooShort,
    UnknownKind(u8),
}

/// Splits a binary message into its header and access unit.
pub fn parse_frame(msg: &[u8]) -> Result<(FrameHeader, &[u8]), ParseError> {
    if msg.len() < HEADER_LEN {
        return Err(ParseError::TooShort);
    }
    if msg[0] != KIND_VIDEO {
        return Err(ParseError::UnknownKind(msg[0]));
    }
    let i = |r: std::ops::Range<usize>| i64::from_be_bytes(msg[r].try_into().unwrap());
    Ok((
        FrameHeader {
            keyframe: msg[1] & FLAG_KEYFRAME != 0,
            replay: msg[1] & FLAG_REPLAY != 0,
            pts_90k: i(2..10),
            wall_us: i(10..18),
            recv_us: i(18..26),
        },
        &msg[HEADER_LEN..],
    ))
}

/// Text messages from the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerText {
    /// (Re)configure the decoder before the next frame.
    Config {
        camera: String,
        stream: String,
        /// RFC 6381 codec string, e.g. `avc1.640033`.
        codec: String,
        width: u16,
        height: u16,
        /// The `avcC`/`hvcC` record, base64: WebCodecs' `description`.
        description: String,
    },
    /// The camera reconnected; wait for the next config/keyframe.
    Discontinuity,
    Pong {
        client_us: i64,
        server_us: i64,
    },
    /// Replay socket: playback (re)starts at `t_ms` for these cameras.
    /// Binary frames that follow refer to cameras by index in this list;
    /// frames received before it belong to an earlier `seq` and are stale.
    Replay {
        seq: u32,
        t_ms: i64,
        playing: bool,
        cameras: Vec<String>,
    },
}

/// Text messages from the client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientText {
    Ping {
        client_us: i64,
    },
    /// Replay: jump to `t_ms`; the server answers with a `replay` message
    /// carrying the same `seq`.
    Seek {
        t_ms: i64,
        seq: u32,
    },
    Play,
    Pause,
}

/// Estimates `server clock - client clock` from ping/pong round trips,
/// trusting the sample with the shortest round trip (least queueing).
#[derive(Clone, Debug, Default)]
pub struct ClockSync {
    best: Option<(i64, i64)>, // (rtt, offset)
    samples: u32,
}

impl ClockSync {
    /// A pong: ping sent at `client_us` (client clock), answered at
    /// `server_us` (server clock), received at `now_us` (client clock).
    pub fn add(&mut self, client_us: i64, server_us: i64, now_us: i64) {
        let rtt = now_us - client_us;
        if rtt < 0 {
            return;
        }
        let offset = server_us - (client_us + now_us) / 2;
        self.samples += 1;
        if self.best.is_none_or(|(best_rtt, _)| rtt <= best_rtt) {
            self.best = Some((rtt, offset));
        }
    }

    /// Server clock minus client clock, if any pong has arrived.
    pub fn offset_us(&self) -> Option<i64> {
        self.best.map(|(_, o)| o)
    }

    pub fn rtt_us(&self) -> Option<i64> {
        self.best.map(|(r, _)| r)
    }
}

/// Standard base64 with padding.
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decodes standard base64 (padding optional); `None` if malformed.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = B64.iter().position(|&b| b == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip_and_layout() {
        let h = FrameHeader {
            keyframe: true,
            replay: false,
            pts_90k: 0x0102,
            wall_us: -1,
            recv_us: 1_800_000_000_000_000,
        };
        let mut msg = h.encode().to_vec();
        msg.extend_from_slice(&[9, 9]);
        assert_eq!(msg[..2], [1, 1]);
        assert_eq!(&msg[2..10], &0x0102i64.to_be_bytes());
        assert_eq!(&msg[10..18], &[0xff; 8]);
        let (back, au) = parse_frame(&msg).unwrap();
        assert_eq!(back, h);
        assert_eq!(au, &[9, 9]);
        assert_eq!(parse_frame(&msg[..25]), Err(ParseError::TooShort));
        msg[0] = 7;
        assert_eq!(parse_frame(&msg), Err(ParseError::UnknownKind(7)));
        assert_eq!(
            FrameHeader {
                pts_90k: 90_000,
                ..h
            }
            .pts_us(),
            1_000_000
        );
    }

    #[test]
    fn replay_header_roundtrip() {
        let h = ReplayHeader {
            camera: 3,
            keyframe: true,
            pts_90k: 1_800_000_000 * 90,
            wall_us: 1_800_000_000_000_000,
        };
        let mut msg = h.encode().to_vec();
        msg.push(7);
        let (back, au) = parse_replay_frame(&msg).unwrap();
        assert_eq!((back, au), (h, &[7u8][..]));
        assert!(parse_frame(&msg).is_err());
        let seek: ClientText = serde_json::from_str(r#"{"type":"seek","t_ms":5,"seq":2}"#).unwrap();
        assert_eq!(seek, ClientText::Seek { t_ms: 5, seq: 2 });
        let r = serde_json::to_string(&ServerText::Replay {
            seq: 2,
            t_ms: 5,
            playing: true,
            cameras: vec!["door".into()],
        })
        .unwrap();
        assert_eq!(
            r,
            r#"{"type":"replay","seq":2,"t_ms":5,"playing":true,"cameras":["door"]}"#
        );
    }

    #[test]
    fn text_messages_are_tagged_json() {
        let pong = ServerText::Pong {
            client_us: 1,
            server_us: 2,
        };
        let j = serde_json::to_string(&pong).unwrap();
        assert_eq!(j, r#"{"type":"pong","client_us":1,"server_us":2}"#);
        let d: ServerText = serde_json::from_str(r#"{"type":"discontinuity"}"#).unwrap();
        assert_eq!(d, ServerText::Discontinuity);
        let p: ClientText = serde_json::from_str(r#"{"type":"ping","client_us":5}"#).unwrap();
        assert_eq!(p, ClientText::Ping { client_us: 5 });
    }

    #[test]
    fn clock_sync_prefers_shortest_round_trip() {
        let mut c = ClockSync::default();
        assert_eq!(c.offset_us(), None);
        // Server 1000 µs ahead. A slow round trip with asymmetric delay...
        c.add(0, 1000 + 90, 100);
        assert_eq!(c.offset_us(), Some(1040));
        // ...is replaced by a fast symmetric one.
        c.add(200, 1000 + 205, 210);
        assert_eq!(c.offset_us(), Some(1000));
        assert_eq!(c.rtt_us(), Some(10));
        c.add(300, 0, 200); // clock went backwards: ignored
        assert_eq!(c.offset_us(), Some(1000));
    }

    #[test]
    fn base64_roundtrip() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(i.as_bytes()), o);
            assert_eq!(base64_decode(o).unwrap(), i.as_bytes());
        }
        let bin: Vec<u8> = (0..=255).collect();
        assert_eq!(base64_decode(&base64_encode(&bin)).unwrap(), bin);
        assert!(base64_decode("ab$d").is_none());
    }
}
