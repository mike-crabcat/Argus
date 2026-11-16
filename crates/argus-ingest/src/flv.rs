//! HTTP-FLV ingest. Reolink cameras serve H.264 as FLV over HTTP
//! (`http://cam/flv?port=1935&app=bcs&stream=channel0_main.bcs&user=..&password=..`),
//! which is more reliable than their RTSP server. FLV video tags carry the
//! same length-prefixed NAL units MP4 uses, so this is still passthrough.

use anyhow::{Context, Result, bail};
use argus_media::VideoConfig;
use bytes::{Buf, Bytes, BytesMut};
use futures::StreamExt;
use h264_reader::avcc::AvcDecoderConfigurationRecord;
use tokio::sync::mpsc;
use url::Url;

use crate::{RawFrame, SourceEvent};

pub async fn run(url: Url, tx: mpsc::Sender<SourceEvent>) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(10))
        .build()?;
    let resp = client
        .get(url)
        .send()
        .await
        .context("FLV request")?
        .error_for_status()?;
    let mut body = resp.bytes_stream();
    let mut parser = FlvParser::default();
    while let Some(chunk) = body.next().await {
        for ev in parser.push(&chunk.context("FLV read")?)? {
            if tx.send(ev).await.is_err() {
                return Ok(());
            }
        }
    }
    bail!("FLV stream ended")
}

/// Incremental FLV demuxer that keeps only H.264 video.
#[derive(Default)]
pub struct FlvParser {
    buf: BytesMut,
    header_done: bool,
    nal_length_size: usize,
}

const TAG_VIDEO: u8 = 9;
const CODEC_AVC: u8 = 7;

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
            if tag_type == TAG_VIDEO {
                if let Some(ev) = self.video_tag(ts, body)? {
                    out.push(ev);
                }
            }
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
                Ok(Some(SourceEvent::Frame(RawFrame {
                    pts_90k: (dts_ms + cts) * 90,
                    keyframe: frame_type == 1,
                    data,
                })))
            }
            _ => Ok(None), // end of sequence
        }
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
    fn rejects_non_flv() {
        assert!(FlvParser::default().push(b"GET / HTTP/1.1\r\n").is_err());
    }
}
