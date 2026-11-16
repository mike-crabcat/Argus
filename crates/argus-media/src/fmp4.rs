//! Fragmented MP4 (CMAF-style) muxing: one init segment per codec config,
//! then one `moof`+`mdat` fragment per GOP. Every fragment starts on a
//! keyframe, so any fragment can be fetched by byte range and decoded alone.

use crate::mp4box::{put_matrix, write_box, write_full_box};
use crate::{Frame, TIMESCALE, VideoConfig};

const TRACK_ID: u32 = 1;

/// `ftyp` + `moov` for a single video track with no samples (they all live in
/// fragments).
pub fn init_segment(cfg: &VideoConfig) -> Vec<u8> {
    let mut b = Vec::with_capacity(768 + cfg.sample_entry.len());
    write_box(&mut b, b"ftyp", |b| {
        b.extend_from_slice(b"isom");
        b.extend_from_slice(&0x200u32.to_be_bytes());
        for brand in [b"isom", b"iso6", b"mp41"] {
            b.extend_from_slice(brand);
        }
    });
    write_box(&mut b, b"moov", |b| {
        write_full_box(b, b"mvhd", 0, 0, |b| {
            b.extend_from_slice(&[0; 8]); // creation + modification time
            b.extend_from_slice(&TIMESCALE.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes()); // duration: unknown/fragmented
            b.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
            b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
            b.extend_from_slice(&[0; 10]);
            put_matrix(b);
            b.extend_from_slice(&[0; 24]); // pre_defined
            b.extend_from_slice(&(TRACK_ID + 1).to_be_bytes()); // next_track_ID
        });
        write_box(b, b"trak", |b| {
            write_full_box(b, b"tkhd", 0, 0x3, |b| {
                b.extend_from_slice(&[0; 8]);
                b.extend_from_slice(&TRACK_ID.to_be_bytes());
                b.extend_from_slice(&[0; 4]); // reserved
                b.extend_from_slice(&0u32.to_be_bytes()); // duration
                b.extend_from_slice(&[0; 8]); // reserved
                b.extend_from_slice(&[0; 4]); // layer + alternate_group
                b.extend_from_slice(&[0; 4]); // volume (0 for video) + reserved
                put_matrix(b);
                b.extend_from_slice(&(u32::from(cfg.width) << 16).to_be_bytes());
                b.extend_from_slice(&(u32::from(cfg.height) << 16).to_be_bytes());
            });
            write_box(b, b"mdia", |b| {
                write_full_box(b, b"mdhd", 0, 0, |b| {
                    b.extend_from_slice(&[0; 8]);
                    b.extend_from_slice(&TIMESCALE.to_be_bytes());
                    b.extend_from_slice(&0u32.to_be_bytes());
                    b.extend_from_slice(&0x55c4u16.to_be_bytes()); // language "und"
                    b.extend_from_slice(&[0; 2]);
                });
                write_full_box(b, b"hdlr", 0, 0, |b| {
                    b.extend_from_slice(&[0; 4]);
                    b.extend_from_slice(b"vide");
                    b.extend_from_slice(&[0; 12]);
                    b.extend_from_slice(b"Argus\0");
                });
                write_box(b, b"minf", |b| {
                    write_full_box(b, b"vmhd", 0, 1, |b| b.extend_from_slice(&[0; 8]));
                    write_box(b, b"dinf", |b| {
                        write_full_box(b, b"dref", 0, 0, |b| {
                            b.extend_from_slice(&1u32.to_be_bytes());
                            write_full_box(b, b"url ", 0, 1, |_| {});
                        });
                    });
                    write_box(b, b"stbl", |b| {
                        write_full_box(b, b"stsd", 0, 0, |b| {
                            b.extend_from_slice(&1u32.to_be_bytes());
                            b.extend_from_slice(&cfg.sample_entry);
                        });
                        for empty in [b"stts", b"stsc", b"stco"] {
                            write_full_box(b, empty, 0, 0, |b| b.extend_from_slice(&[0; 4]));
                        }
                        write_full_box(b, b"stsz", 0, 0, |b| b.extend_from_slice(&[0; 8]));
                    });
                });
            });
        });
        write_box(b, b"mvex", |b| {
            write_full_box(b, b"trex", 0, 0, |b| {
                b.extend_from_slice(&TRACK_ID.to_be_bytes());
                b.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
                b.extend_from_slice(&[0; 12]); // default duration, size, flags
            });
        });
    });
    b
}

/// Sample flags (ISO/IEC 14496-12 §8.8.3.1).
const FLAGS_SYNC: u32 = 0x0200_0000; // sample_depends_on = 2 (independent)
const FLAGS_NON_SYNC: u32 = 0x0101_0000; // depends_on = 1, is_non_sync_sample = 1

/// Builds one `moof`+`mdat` fragment for a GOP.
///
/// `durations[i]` is the duration of `frames[i]` in 90 kHz units. Frames are
/// assumed to be in decode order with no reordering (IP cameras, including
/// Reolink, don't use B-frames); composition offsets are therefore zero.
pub fn fragment(
    sequence: u32,
    base_decode_time: u64,
    frames: &[Frame],
    durations: &[u32],
) -> Vec<u8> {
    assert_eq!(frames.len(), durations.len());
    let payload: usize = frames.iter().map(|f| f.data.len()).sum();
    let mut b = Vec::with_capacity(128 + frames.len() * 12 + payload);
    let mut data_offset_at = 0;
    write_box(&mut b, b"moof", |b| {
        write_full_box(b, b"mfhd", 0, 0, |b| {
            b.extend_from_slice(&sequence.to_be_bytes())
        });
        write_box(b, b"traf", |b| {
            // default-base-is-moof: data offsets are relative to this moof.
            write_full_box(b, b"tfhd", 0, 0x02_0000, |b| {
                b.extend_from_slice(&TRACK_ID.to_be_bytes())
            });
            write_full_box(b, b"tfdt", 1, 0, |b| {
                b.extend_from_slice(&base_decode_time.to_be_bytes())
            });
            // data-offset, sample-duration, sample-size, sample-flags present.
            write_full_box(b, b"trun", 0, 0x00_0701, |b| {
                b.extend_from_slice(&(frames.len() as u32).to_be_bytes());
                data_offset_at = b.len();
                b.extend_from_slice(&[0; 4]);
                for (f, d) in frames.iter().zip(durations) {
                    b.extend_from_slice(&d.to_be_bytes());
                    b.extend_from_slice(&(f.data.len() as u32).to_be_bytes());
                    let flags = if f.keyframe {
                        FLAGS_SYNC
                    } else {
                        FLAGS_NON_SYNC
                    };
                    b.extend_from_slice(&flags.to_be_bytes());
                }
            });
        });
    });
    let data_offset = (b.len() + 8) as u32; // moof size + mdat header
    b[data_offset_at..data_offset_at + 4].copy_from_slice(&data_offset.to_be_bytes());
    write_box(&mut b, b"mdat", |b| {
        for f in frames {
            b.extend_from_slice(&f.data);
        }
    });
    b
}

/// Durations for a GOP from pts deltas. The last frame's duration comes from
/// `next_pts` (the next GOP's keyframe) when known, else repeats the previous
/// delta. Non-increasing timestamps (camera glitches) get a 1-tick duration
/// rather than corrupting the track.
pub fn durations(frames: &[Frame], next_pts: Option<i64>) -> Vec<u32> {
    let mut out = Vec::with_capacity(frames.len());
    for w in frames.windows(2) {
        out.push(clamp_delta(w[1].pts_90k - w[0].pts_90k));
    }
    if let Some(last) = frames.last() {
        let d = match next_pts {
            Some(n) => clamp_delta(n - last.pts_90k),
            None => out.last().copied().unwrap_or(TIMESCALE / 15),
        };
        out.push(d);
    }
    out
}

fn clamp_delta(d: i64) -> u32 {
    // Over 10 s between frames means a discontinuity, not a real duration.
    d.clamp(1, i64::from(TIMESCALE) * 10) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn boxes(buf: &[u8]) -> Vec<(String, &[u8])> {
        let mut out = vec![];
        let mut i = 0;
        while i + 8 <= buf.len() {
            let len = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
            out.push((
                String::from_utf8_lossy(&buf[i + 4..i + 8]).into_owned(),
                &buf[i + 8..i + len],
            ));
            i += len;
        }
        assert_eq!(i, buf.len(), "boxes must tile the buffer exactly");
        out
    }

    fn frame(pts: i64, key: bool, len: usize) -> Frame {
        Frame {
            pts_90k: pts,
            wall_us: 0,
            keyframe: key,
            data: Bytes::from(vec![0xAB; len]),
        }
    }

    #[test]
    fn init_segment_is_ftyp_then_moov() {
        let cfg = VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360).unwrap();
        let init = init_segment(&cfg);
        let top: Vec<_> = boxes(&init).into_iter().map(|(t, _)| t).collect();
        assert_eq!(top, ["ftyp", "moov"]);
        let moov = boxes(&init)[1].1;
        let kids: Vec<_> = boxes(moov).into_iter().map(|(t, _)| t).collect();
        assert_eq!(kids, ["mvhd", "trak", "mvex"]);
    }

    #[test]
    fn fragment_data_offset_points_at_mdat_payload() {
        let frames = [
            frame(0, true, 100),
            frame(6000, false, 20),
            frame(12000, false, 30),
        ];
        let d = durations(&frames, Some(18000));
        assert_eq!(d, [6000, 6000, 6000]);
        let frag = fragment(7, 0, &frames, &d);
        let top = boxes(&frag);
        assert_eq!(top[0].0, "moof");
        assert_eq!(top[1].0, "mdat");
        assert_eq!(top[1].1.len(), 150);
        // trun is the last box of traf; data_offset follows sample_count.
        let moof_len = top[0].1.len() + 8;
        let traf = boxes(top[0].1)[1].1;
        let trun = boxes(traf)
            .into_iter()
            .find(|(t, _)| t == "trun")
            .unwrap()
            .1;
        let off = u32::from_be_bytes(trun[8..12].try_into().unwrap()) as usize;
        assert_eq!(off, moof_len + 8);
        assert_eq!(&frag[off..off + 3], &[0xAB; 3]);
    }

    #[test]
    fn durations_survive_bad_timestamps() {
        let frames = [
            frame(100, true, 1),
            frame(100, false, 1),
            frame(50, false, 1),
        ];
        assert_eq!(durations(&frames, None), [1, 1, 1]);
        assert_eq!(durations(&[frame(0, true, 1)], None), [6000]);
    }
}
