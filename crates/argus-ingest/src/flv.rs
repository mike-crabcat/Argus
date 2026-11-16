//! HTTP-FLV ingest. Reolink cameras serve H.264 as FLV over HTTP
//! (`http://cam/flv?port=1935&app=bcs&stream=channel0_main.bcs&user=..&password=..`),
//! which is more reliable than their RTSP server. FLV video tags carry the
//! same length-prefixed NAL units MP4 uses, so this is still passthrough.

use anyhow::{Context, Result, bail};
use argus_media::{AudioConfig, VideoConfig};
use bytes::{Buf, Bytes, BytesMut};
use futures::StreamExt;
use h264_reader::avcc::AvcDecoderConfigurationRecord;
use tokio::sync::mpsc;
use tracing::debug;
use url::Url;

use crate::{RawAudio, RawFrame, SourceEvent};

pub async fn run(url: Url, tx: mpsc::Sender<SourceEvent>) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(10))
        .build()?;
    // reqwest errors embed the request URL, and Reolink puts the password in
    // the query string, so every error drops the URL before it can be logged.
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .context("FLV request")?
        .error_for_status()
        .map_err(reqwest::Error::without_url)?;
    let mut body = resp.bytes_stream();
    let mut parser = FlvParser::default();
    while let Some(chunk) = body.next().await {
        let chunk = chunk
            .map_err(reqwest::Error::without_url)
            .context("FLV read")?;
        for ev in parser.push(&chunk)? {
            if tx.send(ev).await.is_err() {
                return Ok(());
            }
        }
    }
    bail!("FLV stream ended")
}

/// Incremental FLV demuxer that keeps H.264 video and AAC audio.
#[derive(Default)]
pub struct FlvParser {
    buf: BytesMut,
    header_done: bool,
    nal_length_size: usize,
    video_ts: Monotonic,
    audio_ts: Monotonic,
    /// Whether an AAC sequence header has been accepted.
    audio_ready: bool,
    /// Whether unusable audio has already been logged (once per connection).
    audio_skipped: bool,
}

const TAG_AUDIO: u8 = 8;
const TAG_VIDEO: u8 = 9;
const CODEC_AVC: u8 = 7;
const SOUND_AAC: u8 = 10;

impl FlvParser {
    pub fn push(&mut self, data: &[u8]) -> Result<Vec<SourceEvent>> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        if !self.header_done {
            if self.buf.len() < 13 {
                return Ok(out);
            }
            if &self.buf[..3] != b"FLV" {
                bail!("not an FLV stream");
            }
            let header_len = u32::from_be_bytes(self.buf[5..9].try_into().unwrap()) as usize;
            if self.buf.len() < header_len + 4 {
                return Ok(out);
            }
            self.buf.advance(header_len + 4); // header + PreviousTagSize0
            self.header_done = true;
        }
        // Tag: type(1) size(3) timestamp(3) ts_ext(1) stream_id(3) body(size) prev_size(4)
        while self.buf.len() >= 11 {
            let size = u32::from_be_bytes([0, self.buf[1], self.buf[2], self.buf[3]]) as usize;
            if self.buf.len() < 11 + size + 4 {
                break;
            }
            let tag_type = self.buf[0] & 0x1f;
            let ts = i64::from(u32::from_be_bytes([
                self.buf[7],
                self.buf[4],
                self.buf[5],
                self.buf[6],
            ]));
            let body = self
                .buf
                .split_to(11 + size + 4)
                .freeze()
                .slice(11..11 + size);
            let ev = match tag_type {
                TAG_VIDEO => self.video_tag(ts, body)?,
                TAG_AUDIO => self.audio_tag(ts, body),
                _ => None,
            };
            out.extend(ev);
        }
        Ok(out)
    }

    fn video_tag(&mut self, dts_ms: i64, body: Bytes) -> Result<Option<SourceEvent>> {
        if body.len() < 5 {
            return Ok(None);
        }
        let frame_type = body[0] >> 4;
        let codec = body[0] & 0x0f;
        if codec != CODEC_AVC {
            bail!("FLV video codec {codec} unsupported (only H.264); use RTSP for H.265 streams");
        }
        let packet_type = body[1];
        let cts = (i32::from_be_bytes([body[2], body[3], body[4], 0]) >> 8) as i64; // signed 24-bit
        let payload = body.slice(5..);
        match packet_type {
            0 => {
                let record = AvcDecoderConfigurationRecord::try_from(&payload[..])
                    .map_err(|e| anyhow::anyhow!("bad AVC sequence header: {e:?}"))?;
                self.nal_length_size = usize::from(record.length_size_minus_one()) + 1;
                let ctx = record
                    .create_context()
                    .map_err(|e| anyhow::anyhow!("bad SPS/PPS: {e:?}"))?;
                let sps = ctx.sps().next().context("sequence header has no SPS")?;
                let (w, h) = sps
                    .pixel_dimensions()
                    .map_err(|e| anyhow::anyhow!("SPS dimensions: {e:?}"))?;
                let cfg = VideoConfig::from_avcc(&payload, u16::try_from(w)?, u16::try_from(h)?)?;
                Ok(Some(SourceEvent::Config(cfg)))
            }
            1 => {
                if self.nal_length_size == 0 {
                    return Ok(None); // no sequence header yet
                }
                let data = to_four_byte_lengths(payload, self.nal_length_size)?;
                let dts_90k = self.video_ts.next_ms(dts_ms) * 90;
                Ok(Some(SourceEvent::Frame(RawFrame {
                    pts_90k: dts_90k + cts * 90,
                    keyframe: frame_type == 1,
                    data,
                })))
            }
            _ => Ok(None), // end of sequence
        }
    }
}

impl FlvParser {
    /// AAC audio is passed through untouched. Anything else (G.711, MP3,
    /// a bad AudioSpecificConfig) is skipped without affecting the video.
    fn audio_tag(&mut self, ts_ms: i64, body: Bytes) -> Option<SourceEvent> {
        let format = *body.first()? >> 4;
        if format != SOUND_AAC {
            self.skip_audio(&format!("FLV sound format {format} is not AAC"));
            return None;
        }
        match *body.get(1)? {
            0 => match AudioConfig::from_aac_asc(&body[2..]) {
                Ok(cfg) => {
                    self.audio_ready = true;
                    Some(SourceEvent::AudioConfig(cfg))
                }
                Err(e) => {
                    self.audio_ready = false;
                    self.skip_audio(&e.to_string());
                    None
                }
            },
            1 if self.audio_ready => Some(SourceEvent::Audio(RawAudio {
                pts_90k: self.audio_ts.next_ms(ts_ms) * 90,
                data: body.slice(2..),
            })),
            _ => None,
        }
    }

    fn skip_audio(&mut self, why: &str) {
        if !self.audio_skipped {
            self.audio_skipped = true;
            debug!("not recording audio: {why}");
        }
    }
}

/// FLV timestamps are whole milliseconds and come straight from the camera,
/// which can repeat a value or jump backwards (clock reset, 32-bit wrap).
/// This shifts such a frame, and everything after it, so each frame lands at
/// least 1 ms after the previous one. The shift is permanent, so later
/// frames keep their spacing instead of piling up.
#[derive(Default)]
struct Monotonic {
    offset_ms: i64,
    last_ms: Option<i64>,
}

impl Monotonic {
    fn next_ms(&mut self, raw_ms: i64) -> i64 {
        let mut t = raw_ms + self.offset_ms;
        if let Some(last) = self.last_ms {
            if t <= last {
                self.offset_ms += last + 1 - t;
                t = last + 1;
            }
        }
        self.last_ms = Some(t);
        t
    }
}

/// MP4 storage uses 4-byte NAL lengths; FLV may use 1, 2 or 4.
fn to_four_byte_lengths(payload: Bytes, len_size: usize) -> Result<Bytes> {
    if len_size == 4 {
        return Ok(payload);
    }
    let mut out = BytesMut::with_capacity(payload.len() + 16);
    let mut p = &payload[..];
    while !p.is_empty() {
        if p.len() < len_size {
            bail!("truncated NAL length");
        }
        let n = p[..len_size]
            .iter()
            .fold(0usize, |acc, &b| acc << 8 | usize::from(b));
        p = &p[len_size..];
        if p.len() < n {
            bail!("truncated NAL unit");
        }
        out.extend_from_slice(&(n as u32).to_be_bytes());
        out.extend_from_slice(&p[..n]);
        p = &p[n..];
    }
    Ok(out.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_short_nal_lengths() {
        let out =
            to_four_byte_lengths(Bytes::from_static(&[0, 2, 0xAA, 0xBB, 0, 1, 0xCC]), 2).unwrap();
        assert_eq!(&out[..], &[0, 0, 0, 2, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC]);
    }

    #[test]
    fn decode_times_strictly_increase() {
        let mut m = Monotonic::default();
        // Normal, duplicate, backwards (reset), then normal spacing again.
        let got: Vec<i64> = [0, 50, 50, 100, 20, 70]
            .iter()
            .map(|&ms| m.next_ms(ms) * 90)
            .collect();
        assert_eq!(got, [0, 4500, 4590, 9090, 9180, 13680]);
        assert!(got.windows(2).all(|w| w[1] > w[0]), "{got:?}");
    }

    async fn error_text(url: &str) -> String {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let err = run(Url::parse(url).unwrap(), tx).await.unwrap_err();
        format!("{err:#} {err:?}")
    }

    #[tokio::test]
    async fn connect_errors_do_not_leak_password() {
        // Nothing listens on port 1, so the connect fails immediately.
        let msg = error_text("http://127.0.0.1:1/flv?app=bcs&user=admin&password=secret").await;
        assert!(msg.contains("FLV request"), "{msg}");
        assert!(!msg.contains("secret"), "{msg}");
    }

    #[tokio::test]
    async fn http_status_errors_do_not_leak_password() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 1024];
            let _ = sock.read(&mut req).await;
            let _ = sock
                .write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n")
                .await;
        });
        let msg = error_text(&format!("http://{addr}/flv?user=admin&password=secret")).await;
        assert!(msg.contains("401"), "{msg}");
        assert!(!msg.contains("secret"), "{msg}");
    }

    /// FLV header + one tag per (type, ts_ms, body).
    fn flv(tags: &[(u8, u32, &[u8])]) -> Vec<u8> {
        let mut v = b"FLV\x01\x05\0\0\0\x09\0\0\0\0".to_vec();
        for (kind, ts, body) in tags {
            let n = body.len() as u32;
            v.push(*kind);
            v.extend_from_slice(&n.to_be_bytes()[1..]);
            v.extend_from_slice(&ts.to_be_bytes()[1..]);
            v.push((ts >> 24) as u8);
            v.extend_from_slice(&[0; 3]);
            v.extend_from_slice(body);
            v.extend_from_slice(&(n + 11).to_be_bytes());
        }
        v
    }

    #[test]
    fn passes_aac_through_and_skips_other_audio() {
        let data = flv(&[
            (TAG_AUDIO, 0, &[0xAF, 1, 9, 9]), // AAC frame before its config: dropped
            (TAG_AUDIO, 0, &[0xAF, 0, 0x14, 0x08]),
            (TAG_AUDIO, 64, &[0xAF, 1, 0x21, 0x10]),
            (TAG_AUDIO, 64, &[0xAF, 1, 0x21, 0x11]), // duplicate timestamp
            (TAG_AUDIO, 128, &[0x72, 0xD5]),         // G.711: ignored
        ]);
        let evs = FlvParser::default().push(&data).unwrap();
        assert_eq!(evs.len(), 3, "{evs:?}");
        let SourceEvent::AudioConfig(cfg) = &evs[0] else {
            panic!("{evs:?}")
        };
        assert_eq!((cfg.sample_rate, cfg.channels), (16000, 1));
        let audio: Vec<_> = evs[1..]
            .iter()
            .map(|e| match e {
                SourceEvent::Audio(a) => (a.pts_90k, a.data.to_vec()),
                e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(
            audio,
            [(64 * 90, vec![0x21, 0x10]), (65 * 90, vec![0x21, 0x11])]
        );
    }

    #[test]
    fn bad_audio_config_does_not_stop_video() {
        let data = flv(&[(TAG_AUDIO, 0, &[0xAF, 0, 0x2b, 0x92, 0x08, 0x00])]); // HE-AAC
        assert!(FlvParser::default().push(&data).unwrap().is_empty());
    }

    #[test]
    fn rejects_non_flv() {
        assert!(FlvParser::default().push(b"GET / HTTP/1.1\r\n").is_err());
    }
}
