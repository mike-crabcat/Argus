//! Test streams: synthetic H.264, encoded with OpenH264, as Argus frames.
//! (No camera footage: the repository is public.)

use argus_media::{Frame, VideoConfig};
use bytes::Bytes;
use openh264::encoder::Encoder;
use openh264::formats::{RgbSliceU8, YUVBuffer};

pub const W: usize = 320;
pub const H: usize = 240;

/// A test picture: a gradient with a bright square at `(x, 60)`.
pub fn picture(x: usize) -> Vec<u8> {
    let mut rgb = vec![0u8; W * H * 3];
    for y in 0..H {
        for c in 0..W {
            let i = (y * W + c) * 3;
            let inside = (x..x + 40).contains(&c) && (60..100).contains(&y);
            let v = if inside { 250 } else { (c / 4 + y / 4) as u8 };
            rgb[i..i + 3].copy_from_slice(&[v, v, if inside { 40 } else { v }]);
        }
    }
    rgb
}

/// Encodes `n` frames (a keyframe every `gop`) to Argus frames:
/// length-prefixed NALs, with the SPS and PPS as a VideoConfig.
pub fn encode(n: usize, gop: usize) -> (VideoConfig, Vec<Frame>) {
    let mut enc = Encoder::new().unwrap();
    let (mut sps, mut pps) = (None, None);
    let mut frames = Vec::new();
    for i in 0..n {
        if i % gop == 0 {
            enc.force_intra_frame();
        }
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&picture(i * 4), (W, H)));
        let bs = enc.encode(&yuv).unwrap();
        let annexb = bs.to_vec();
        let mut data = Vec::new();
        let mut key = false;
        for nal in openh264::nal_units(&annexb) {
            let nal = nal
                .strip_prefix(&[0, 0, 0, 1])
                .or_else(|| nal.strip_prefix(&[0, 0, 1]))
                .unwrap();
            match nal[0] & 0x1F {
                7 => sps = Some(nal.to_vec()),
                8 => pps = Some(nal.to_vec()),
                t => {
                    key |= t == 5;
                    data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    data.extend_from_slice(nal);
                }
            }
        }
        frames.push(Frame {
            pts_90k: i as i64 * 6000,
            wall_us: 1_000_000 + i as i64 * 66_667,
            keyframe: key,
            data: Bytes::from(data),
        });
    }
    let (sps, pps) = (sps.unwrap(), pps.unwrap());
    let mut rec = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
    rec.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    rec.extend_from_slice(&sps);
    rec.push(1);
    rec.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    rec.extend_from_slice(&pps);
    (
        VideoConfig::from_avcc(&rec, W as u16, H as u16).unwrap(),
        frames,
    )
}
