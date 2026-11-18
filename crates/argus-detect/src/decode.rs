//! Camera video to pictures: H.264 decoding with OpenH264 (Cisco's
//! BSD-licensed decoder, built from source; no ffmpeg, no GPU).
//!
//! Detection runs on the sub streams (896x672 on Mike's Reolinks), where a
//! frame decodes in a few milliseconds on one core. Every frame has to be
//! decoded, since later frames refer to earlier ones, but only the frames
//! a [`Sampler`] picks are converted to RGB.

use argus_media::{Codec, Frame, VideoConfig};

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("only H.264 can be decoded for detection, not {0:?}")]
    Unsupported(Codec),
    #[error("the stream's codec config has no usable avcC record")]
    BadConfig,
    #[error("H.264 decoder: {0}")]
    Decoder(#[from] openh264::Error),
}

/// A decoded picture, 8-bit RGB, rows top to bottom.
#[derive(Clone, Debug)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    /// The frame's wall-clock time (µs since the Unix epoch).
    pub wall_us: i64,
}

/// SPS and PPS NAL units, and the NAL length size, from an
/// `AVCDecoderConfigurationRecord`.
#[derive(Debug, PartialEq, Eq)]
pub struct AvcC {
    pub length_size: usize,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

impl AvcC {
    pub fn parse(r: &[u8]) -> Option<Self> {
        if r.len() < 7 || r[0] != 1 {
            return None;
        }
        let length_size = (r[4] & 3) as usize + 1;
        let mut i = 5;
        let take = |i: &mut usize| -> Option<Vec<u8>> {
            let n = u16::from_be_bytes(r.get(*i..*i + 2)?.try_into().ok()?) as usize;
            let v = r.get(*i + 2..*i + 2 + n)?.to_vec();
            *i += 2 + n;
            Some(v)
        };
        let nsps = (r[i] & 0x1F) as usize;
        i += 1;
        let sps = (0..nsps)
            .map(|_| take(&mut i))
            .collect::<Option<Vec<_>>>()?;
        let npps = *r.get(i)? as usize;
        i += 1;
        let pps = (0..npps)
            .map(|_| take(&mut i))
            .collect::<Option<Vec<_>>>()?;
        (!sps.is_empty() && !pps.is_empty()).then_some(Self {
            length_size,
            sps,
            pps,
        })
    }

    /// The record inside a sample entry (or any box tree holding an `avcC`).
    pub fn find(boxes: &[u8]) -> Option<Self> {
        let at = boxes.windows(4).position(|w| w == b"avcC")?;
        let size = u32::from_be_bytes(boxes.get(at - 4..at)?.try_into().ok()?) as usize;
        Self::parse(boxes.get(at + 4..at - 4 + size)?)
    }

    /// The parameter sets as an Annex B byte stream.
    pub fn annex_b(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for n in self.sps.iter().chain(&self.pps) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(n);
        }
        out
    }
}

/// Length-prefixed NAL units (as in MP4 and Argus's [`Frame`]s) to Annex B
/// start codes. None if the lengths don't add up.
pub fn to_annex_b(data: &[u8], length_size: usize, out: &mut Vec<u8>) -> Option<()> {
    out.clear();
    let mut i = 0;
    while i < data.len() {
        let len_bytes = data.get(i..i + length_size)?;
        let n = len_bytes
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | b as usize);
        let nal = data.get(i + length_size..i + length_size + n)?;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        i += length_size + n;
    }
    Some(())
}

/// An OpenH264 decoder that doesn't flush after every frame. Flushing
/// makes it run out of pictures on streams with one reference frame (the
/// Reolink doorbell's), failing every keyframe; without B-frames nothing
/// is held back anyway.
fn new_decoder() -> Result<openh264::decoder::Decoder, openh264::Error> {
    use openh264::decoder::{Decoder, DecoderConfig, Flush};
    Decoder::with_api_config(
        openh264::OpenH264API::from_source(),
        DecoderConfig::new().flush_after_decode(Flush::NoFlush),
    )
}

/// Decodes one stream's frames. Feed it the stream's config, then every
/// frame in order; after a gap ([`Decoder::reset`]) or an error it waits
/// for the next keyframe.
pub struct Decoder {
    dec: openh264::decoder::Decoder,
    length_size: usize,
    configured: bool,
    synced: bool,
    buf: Vec<u8>,
    /// SPS and PPS (Annex B), sent again in front of every keyframe.
    headers: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Result<Self, DecodeError> {
        Ok(Self {
            dec: new_decoder()?,
            length_size: 4,
            configured: false,
            synced: false,
            buf: Vec::new(),
            headers: Vec::new(),
        })
    }

    /// A new codec config (the camera connected or changed its settings).
    pub fn configure(&mut self, cfg: &VideoConfig) -> Result<(), DecodeError> {
        if cfg.codec != Codec::H264 {
            self.configured = false;
            return Err(DecodeError::Unsupported(cfg.codec));
        }
        let avcc = AvcC::find(&cfg.sample_entry).ok_or(DecodeError::BadConfig)?;
        // A fresh decoder: the picture size may have changed.
        self.dec = new_decoder()?;
        self.length_size = avcc.length_size;
        self.headers = avcc.annex_b();
        self.configured = true;
        self.synced = false;
        Ok(())
    }

    /// Frames were lost: wait for the next keyframe.
    pub fn reset(&mut self) {
        self.synced = false;
    }

    /// Whether frames are being decoded (configured, and past a keyframe).
    pub fn synced(&self) -> bool {
        self.configured && self.synced
    }

    /// Decodes `f`. With `picture`, returns it as RGB (if the decoder had a
    /// picture to give); otherwise only advances the decoder.
    pub fn decode(&mut self, f: &Frame, picture: bool) -> Result<Option<Picture>, DecodeError> {
        if !self.configured || (!self.synced && !f.keyframe) {
            return Ok(None);
        }
        if to_annex_b(&f.data, self.length_size, &mut self.buf).is_none() {
            self.synced = false;
            return Ok(None);
        }
        if f.keyframe {
            self.buf.splice(0..0, self.headers.iter().copied());
        }
        let yuv = match self.dec.decode(&self.buf) {
            Ok(y) => y,
            Err(e) => {
                self.synced = false;
                return Err(e.into());
            }
        };
        self.synced = true;
        let (Some(yuv), true) = (yuv, picture) else {
            return Ok(None);
        };
        use openh264::formats::YUVSource;
        let (w, h) = yuv.dimensions();
        let mut rgb = vec![0; w * h * 3];
        yuv.write_rgb8(&mut rgb);
        Ok(Some(Picture {
            width: w as u32,
            height: h as u32,
            rgb,
            wall_us: f.wall_us,
        }))
    }
}

/// Picks frames at a steady rate by their wall-clock time, e.g. 5 a second
/// from a 15 fps stream.
#[derive(Debug, Clone)]
pub struct Sampler {
    every_us: i64,
    next_us: Option<i64>,
}

impl Sampler {
    pub fn new(fps: f64) -> Self {
        Self {
            every_us: (1e6 / fps.max(0.01)) as i64,
            next_us: None,
        }
    }

    /// Whether the frame at `wall_us` is one to look at.
    pub fn take(&mut self, wall_us: i64) -> bool {
        match self.next_us {
            Some(n) if wall_us < n && n - wall_us <= 10 * self.every_us => false,
            // On time, or the clock jumped back: start again from here.
            _ => {
                let base = match self.next_us {
                    Some(n) if wall_us >= n && wall_us - n < self.every_us => n,
                    _ => wall_us,
                };
                self.next_us = Some(base + self.every_us);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testing::{H, W, encode};

    /// The mean colour of a square of the picture.
    fn mean(p: &Picture, x: usize, y: usize) -> [u32; 3] {
        let mut s = [0u32; 3];
        for yy in y..y + 10 {
            for xx in x..x + 10 {
                let i = (yy * p.width as usize + xx) * 3;
                for (acc, &v) in s.iter_mut().zip(&p.rgb[i..i + 3]) {
                    *acc += v as u32;
                }
            }
        }
        s.map(|v| v / 100)
    }

    #[test]
    fn decodes_what_was_encoded() {
        let (cfg, frames) = encode(30, 10);
        assert!(frames[0].keyframe && frames[10].keyframe && !frames[1].keyframe);
        let mut d = Decoder::new().unwrap();
        d.configure(&cfg).unwrap();
        let mut got = Vec::new();
        for f in &frames {
            if let Some(p) = d.decode(f, true).unwrap() {
                got.push(p);
            }
        }
        assert_eq!(got.len(), 30, "a picture for every frame");
        let p = &got[20];
        assert_eq!((p.width, p.height), (W as u32, H as u32));
        assert_eq!(p.wall_us, frames[20].wall_us);
        // The bright square is where frame 20 drew it (x = 80).
        let inside = mean(p, 95, 75);
        assert!(inside[0] > 200 && inside[2] < 100, "{inside:?}");
        let outside = mean(p, 10, 200);
        assert!(outside[0] < 120, "{outside:?}");
    }

    #[test]
    fn waits_for_a_keyframe_after_a_gap() {
        let (cfg, frames) = encode(25, 10);
        let mut d = Decoder::new().unwrap();
        // Nothing before a config.
        assert!(d.decode(&frames[0], true).unwrap().is_none());
        d.configure(&cfg).unwrap();
        // Joining mid-GOP: frames 3..10 are skipped, 10 starts it.
        let mut first = None;
        for (i, f) in frames.iter().enumerate().skip(3) {
            if d.decode(f, true).unwrap().is_some() {
                first.get_or_insert(i);
            }
        }
        assert_eq!(first, Some(10));
        // A gap: wait for the next keyframe again.
        d.reset();
        assert!(!d.synced());
        assert!(d.decode(&frames[15], true).unwrap().is_none());
        assert!(d.decode(&frames[20], true).unwrap().is_some());
        // Decoding without a picture still advances the decoder.
        assert!(d.decode(&frames[21], false).unwrap().is_none());
        let p = d.decode(&frames[22], true).unwrap().unwrap();
        assert!(mean(&p, 103, 75)[0] > 200, "frame 22's square at x = 88");
    }

    #[test]
    fn h265_is_refused() {
        let (mut cfg, _) = encode(1, 1);
        cfg.codec = Codec::H265;
        let mut d = Decoder::new().unwrap();
        assert!(matches!(
            d.configure(&cfg),
            Err(DecodeError::Unsupported(Codec::H265))
        ));
    }

    #[test]
    fn annex_b_and_avcc() {
        let mut out = Vec::new();
        to_annex_b(&[0, 0, 0, 2, 9, 9, 0, 0, 0, 1, 7], 4, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 9, 9, 0, 0, 0, 1, 7]);
        assert!(to_annex_b(&[0, 0, 0, 5, 1], 4, &mut out).is_none());
        let rec = [1, 0x64, 0, 0x1F, 0xFF, 0xE1, 0, 2, 0x67, 1, 1, 0, 1, 0x68];
        let a = AvcC::parse(&rec).unwrap();
        assert_eq!(
            a,
            AvcC {
                length_size: 4,
                sps: vec![vec![0x67, 1]],
                pps: vec![vec![0x68]]
            }
        );
        assert!(AvcC::parse(&rec[..9]).is_none());
    }

    #[test]
    fn sampler_keeps_a_steady_rate() {
        let mut s = Sampler::new(5.0);
        // 15 fps for two seconds: 10 picked.
        let picked = (0..30).filter(|i| s.take(1_000_000 + i * 66_667)).count();
        assert_eq!(picked, 10);
        // The clock jumps back (a reconnect): take at once and go on.
        assert!(s.take(500_000));
        assert!(!s.take(566_667));
        assert!(s.take(700_000));
    }
}
