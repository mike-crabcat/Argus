//! Fragmented MP4 (CMAF-style) muxing: one init segment per codec config,
//! then one `moof`+`mdat` fragment per GOP. Every fragment starts on a
//! keyframe, so any fragment can be fetched by byte range and decoded alone.
//!
//! Track 1 is always video (90 kHz). When the camera sends audio, track 2 is
//! AAC with the sample rate as its timescale, and each fragment carries the
//! audio frames of its GOP after the video frames.

use crate::mp4box::{put_matrix, write_box, write_full_box};
use crate::{AudioConfig, AudioFrame, Frame, TIMESCALE, VideoConfig};

const TRACK_ID: u32 = 1;
const AUDIO_TRACK_ID: u32 = 2;

/// `ftyp` + `moov` for a video track and an optional audio track, with no
/// samples (they all live in fragments). Without audio the output is the
/// same as before audio support existed.
pub fn init_segment(cfg: &VideoConfig, audio: Option<&AudioConfig>) -> Vec<u8> {
    let mut b = Vec::with_capacity(1024 + cfg.sample_entry.len());
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
            let next_track_id = if audio.is_some() {
                AUDIO_TRACK_ID + 1
            } else {
                TRACK_ID + 1
            };
            b.extend_from_slice(&next_track_id.to_be_bytes());
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
        if let Some(a) = audio {
            audio_trak(b, a);
        }
        write_box(b, b"mvex", |b| {
            for id in std::iter::once(TRACK_ID).chain(audio.map(|_| AUDIO_TRACK_ID)) {
                write_full_box(b, b"trex", 0, 0, |b| {
                    b.extend_from_slice(&id.to_be_bytes());
                    b.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
                    b.extend_from_slice(&[0; 12]); // default duration, size, flags
                });
            }
        });
    });
    b
}

fn audio_trak(b: &mut Vec<u8>, a: &AudioConfig) {
    write_box(b, b"trak", |b| {
        write_full_box(b, b"tkhd", 0, 0x3, |b| {
            b.extend_from_slice(&[0; 8]);
            b.extend_from_slice(&AUDIO_TRACK_ID.to_be_bytes());
            b.extend_from_slice(&[0; 4]); // reserved
            b.extend_from_slice(&0u32.to_be_bytes()); // duration
            b.extend_from_slice(&[0; 8]); // reserved
            b.extend_from_slice(&[0; 4]); // layer + alternate_group
            b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
            b.extend_from_slice(&[0; 2]); // reserved
            put_matrix(b);
            b.extend_from_slice(&[0; 8]); // width, height
        });
        write_box(b, b"mdia", |b| {
            write_full_box(b, b"mdhd", 0, 0, |b| {
                b.extend_from_slice(&[0; 8]);
                b.extend_from_slice(&a.sample_rate.to_be_bytes());
                b.extend_from_slice(&0u32.to_be_bytes());
                b.extend_from_slice(&0x55c4u16.to_be_bytes()); // language "und"
                b.extend_from_slice(&[0; 2]);
            });
            write_full_box(b, b"hdlr", 0, 0, |b| {
                b.extend_from_slice(&[0; 4]);
                b.extend_from_slice(b"soun");
                b.extend_from_slice(&[0; 12]);
                b.extend_from_slice(b"Argus\0");
            });
            write_box(b, b"minf", |b| {
                write_full_box(b, b"smhd", 0, 0, |b| b.extend_from_slice(&[0; 4]));
                write_box(b, b"dinf", |b| {
                    write_full_box(b, b"dref", 0, 0, |b| {
                        b.extend_from_slice(&1u32.to_be_bytes());
                        write_full_box(b, b"url ", 0, 1, |_| {});
                    });
                });
                write_box(b, b"stbl", |b| {
                    write_full_box(b, b"stsd", 0, 0, |b| {
                        b.extend_from_slice(&1u32.to_be_bytes());
                        b.extend_from_slice(&a.sample_entry);
                    });
                    for empty in [b"stts", b"stsc", b"stco"] {
                        write_full_box(b, empty, 0, 0, |b| b.extend_from_slice(&[0; 4]));
                    }
                    write_full_box(b, b"stsz", 0, 0, |b| b.extend_from_slice(&[0; 8]));
                });
            });
        });
    });
}

/// Sample flags (ISO/IEC 14496-12 §8.8.3.1).
const FLAGS_SYNC: u32 = 0x0200_0000; // sample_depends_on = 2 (independent)
const FLAGS_NON_SYNC: u32 = 0x0101_0000; // depends_on = 1, is_non_sync_sample = 1

/// The audio that goes into one fragment.
pub struct AudioRun<'a> {
    /// Decode time of the first frame, in sample-rate units.
    pub base_decode_time: u64,
    pub frames: &'a [AudioFrame],
    /// Duration of every frame in sample-rate units (1024 for AAC-LC).
    pub sample_duration: u32,
}

/// Builds one `moof`+`mdat` fragment for a GOP of video only.
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
    fragment_with_audio(sequence, base_decode_time, frames, durations, None)
}

/// Like [`fragment`], plus a second `traf` for the audio when there is any.
/// The `mdat` holds the video samples, then the audio samples; each `trun`'s
/// data offset (relative to the `moof`) points at its own samples.
pub fn fragment_with_audio(
    sequence: u32,
    base_decode_time: u64,
    frames: &[Frame],
    durations: &[u32],
    audio: Option<&AudioRun>,
) -> Vec<u8> {
    assert_eq!(frames.len(), durations.len());
    let audio = audio.filter(|a| !a.frames.is_empty());
    let video_payload: usize = frames.iter().map(|f| f.data.len()).sum();
    let audio_payload: usize = audio.map_or(0, |a| a.frames.iter().map(|f| f.data.len()).sum());
    let audio_count = audio.map_or(0, |a| a.frames.len());
    let mut b = Vec::with_capacity(
        200 + frames.len() * 12 + audio_count * 8 + video_payload + audio_payload,
    );
    let mut data_offset_at = 0;
    let mut audio_offset_at = None;
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
        if let Some(a) = audio {
            write_box(b, b"traf", |b| {
                write_full_box(b, b"tfhd", 0, 0x02_0000, |b| {
                    b.extend_from_slice(&AUDIO_TRACK_ID.to_be_bytes())
                });
                write_full_box(b, b"tfdt", 1, 0, |b| {
                    b.extend_from_slice(&a.base_decode_time.to_be_bytes())
                });
                // data-offset, sample-duration, sample-size present; every
                // AAC frame is a sync sample (trex default flags = 0).
                write_full_box(b, b"trun", 0, 0x00_0301, |b| {
                    b.extend_from_slice(&(a.frames.len() as u32).to_be_bytes());
                    audio_offset_at = Some(b.len());
                    b.extend_from_slice(&[0; 4]);
                    for f in a.frames {
                        b.extend_from_slice(&a.sample_duration.to_be_bytes());
                        b.extend_from_slice(&(f.data.len() as u32).to_be_bytes());
                    }
                });
            });
        }
    });
    let data_offset = (b.len() + 8) as u32; // moof size + mdat header
    b[data_offset_at..data_offset_at + 4].copy_from_slice(&data_offset.to_be_bytes());
    if let Some(at) = audio_offset_at {
        let audio_offset = data_offset + video_payload as u32;
        b[at..at + 4].copy_from_slice(&audio_offset.to_be_bytes());
    }
    write_box(&mut b, b"mdat", |b| {
        for f in frames {
            b.extend_from_slice(&f.data);
        }
        for f in audio.map_or(&[][..], |a| a.frames) {
            b.extend_from_slice(&f.data);
        }
    });
    b
}

/// One video sample read back from a fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Decode time in 90 kHz ticks. Argus writes wall-clock time since the
    /// Unix epoch here, so this is also the frame's wall time.
    pub dts_90k: u64,
    pub duration: u32,
    pub keyframe: bool,
    pub data: bytes::Bytes,
}

/// Reads the video samples (track 1) of one `moof`+`mdat` fragment as
/// [`fragment`] writes it: `tfhd` with default-base-is-moof, a version 1
/// `tfdt`, and a `trun` with data offset, durations, sizes and flags.
/// Returns `None` if the bytes aren't such a fragment.
pub fn read_video_samples(frag: &bytes::Bytes) -> Option<Vec<Sample>> {
    let be32 = |b: &[u8], i: usize| -> Option<u32> {
        Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?))
    };
    let children = |b: &[u8]| -> Option<Vec<([u8; 4], std::ops::Range<usize>)>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= b.len() {
            let len = be32(b, i)? as usize;
            if len < 8 || i + len > b.len() {
                return None;
            }
            out.push((b[i + 4..i + 8].try_into().ok()?, i + 8..i + len));
            i += len;
        }
        Some(out)
    };
    let top = children(frag)?;
    let (_, moof) = top.iter().find(|(t, _)| t == b"moof")?.clone();
    let moof_start = moof.start - 8;
    for (t, traf) in children(&frag[moof.clone()])? {
        if &t != b"traf" {
            continue;
        }
        let traf = &frag[moof.start + traf.start..moof.start + traf.end];
        let kids = children(traf)?;
        let body = |name: &[u8; 4]| {
            kids.iter()
                .find(|(t, _)| t == name)
                .map(|(_, r)| &traf[r.clone()])
        };
        let tfhd = body(b"tfhd")?;
        if be32(tfhd, 4)? != TRACK_ID {
            continue;
        }
        let tfdt = body(b"tfdt")?;
        let mut dts = match tfdt[0] {
            1 => u64::from_be_bytes(tfdt.get(4..12)?.try_into().ok()?),
            _ => u64::from(be32(tfdt, 4)?),
        };
        let trun = body(b"trun")?;
        let flags = be32(trun, 0)? & 0x00ff_ffff;
        if flags != 0x00_0701 {
            return None; // not the layout Argus writes
        }
        let count = be32(trun, 4)? as usize;
        let mut offset = moof_start + be32(trun, 8)? as usize;
        let mut out = Vec::with_capacity(count);
        for k in 0..count {
            let e = 12 + k * 12;
            let duration = be32(trun, e)?;
            let size = be32(trun, e + 4)? as usize;
            let sample_flags = be32(trun, e + 8)?;
            if offset + size > frag.len() {
                return None;
            }
            out.push(Sample {
                dts_90k: dts,
                duration,
                keyframe: sample_flags & 0x0001_0000 == 0,
                data: frag.slice(offset..offset + size),
            });
            offset += size;
            dts += u64::from(duration);
        }
        return Some(out);
    }
    None
}

/// The decoder configuration record (`avcC`/`hvcC` body) in an init
/// segment, as WebCodecs' `description` wants it.
pub fn decoder_record_from_init(init: &[u8]) -> Option<&[u8]> {
    let i = init.windows(4).position(|w| w == b"avcC" || w == b"hvcC")?;
    let len = u32::from_be_bytes(init.get(i - 4..i)?.try_into().ok()?) as usize;
    init.get(i + 4..i - 4 + len)
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
        let init = init_segment(&cfg, None);
        let top: Vec<_> = boxes(&init).into_iter().map(|(t, _)| t).collect();
        assert_eq!(top, ["ftyp", "moov"]);
        let moov = boxes(&init)[1].1;
        let kids: Vec<_> = boxes(moov).into_iter().map(|(t, _)| t).collect();
        assert_eq!(kids, ["mvhd", "trak", "mvex"]);
    }

    #[test]
    fn init_segment_with_audio_has_two_tracks() {
        let cfg = VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360).unwrap();
        let audio = AudioConfig::from_aac_asc(&[0x14, 0x08]).unwrap();
        let init = init_segment(&cfg, Some(&audio));
        let moov = boxes(&init)[1].1;
        let kids = boxes(moov);
        let names: Vec<_> = kids.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(names, ["mvhd", "trak", "trak", "mvex"]);
        // mvhd next_track_ID is the last 4 bytes of its body.
        let mvhd = kids[0].1;
        assert_eq!(&mvhd[mvhd.len() - 4..], &3u32.to_be_bytes());
        let mdia = boxes(kids[2].1)[1].1;
        let mdia_kids = boxes(mdia);
        let mdhd = mdia_kids[0].1;
        assert_eq!(&mdhd[12..16], &16000u32.to_be_bytes()); // timescale
        assert_eq!(&mdia_kids[1].1[8..12], b"soun");
        let trex: Vec<_> = boxes(kids[3].1)
            .into_iter()
            .map(|(_, body)| u32::from_be_bytes(body[4..8].try_into().unwrap()))
            .collect();
        assert_eq!(trex, [1, 2]);
        // Video-only output is unchanged by the audio support.
        assert_eq!(
            init_segment(&cfg, None).len() + kids[2].1.len() + 8 + 32,
            init.len()
        );
    }

    #[test]
    fn audio_traf_data_offset_follows_video_payload() {
        let frames = [frame(0, true, 100), frame(6000, false, 20)];
        let audio: Vec<_> = [7usize, 9, 11]
            .iter()
            .map(|&n| AudioFrame {
                pts: 0,
                wall_us: 0,
                data: Bytes::from(vec![0xCD; n]),
            })
            .collect();
        let run = AudioRun {
            base_decode_time: 1234,
            frames: &audio,
            sample_duration: 1024,
        };
        let frag = fragment_with_audio(1, 0, &frames, &[6000, 6000], Some(&run));
        let top = boxes(&frag);
        assert_eq!(top[1].1.len(), 120 + 27);
        let trafs: Vec<_> = boxes(top[0].1).into_iter().skip(1).collect();
        assert_eq!(trafs.len(), 2);
        let trun = |traf: &[u8]| {
            boxes(traf)
                .into_iter()
                .find(|(t, _)| t == "trun")
                .unwrap()
                .1
                .to_vec()
        };
        let (vt, at) = (trun(trafs[0].1), trun(trafs[1].1));
        let voff = u32::from_be_bytes(vt[8..12].try_into().unwrap()) as usize;
        let aoff = u32::from_be_bytes(at[8..12].try_into().unwrap()) as usize;
        assert_eq!(voff, top[0].1.len() + 16);
        assert_eq!(aoff, voff + 120);
        assert_eq!(&frag[voff..voff + 100], &[0xAB; 100]);
        assert_eq!(&frag[aoff..aoff + 27], &[0xCD; 27]);
        assert_eq!(&at[4..8], &3u32.to_be_bytes()); // sample_count
        assert_eq!(&at[12..20], &[0, 0, 4, 0, 0, 0, 0, 7]); // 1024, 7 bytes
        let tfdt = boxes(trafs[1].1)[1].1;
        assert_eq!(&tfdt[4..12], &1234u64.to_be_bytes());
        // No audio frames: byte-identical to a video-only fragment.
        let empty = AudioRun {
            base_decode_time: 0,
            frames: &[],
            sample_duration: 1024,
        };
        assert_eq!(
            fragment_with_audio(1, 0, &frames, &[6000, 6000], Some(&empty)),
            fragment(1, 0, &frames, &[6000, 6000])
        );
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
    fn reads_back_the_video_samples_it_wrote() {
        let frames = [
            frame(0, true, 100),
            frame(6000, false, 20),
            frame(12000, false, 30),
        ];
        let audio = [AudioFrame {
            pts: 0,
            wall_us: 0,
            data: Bytes::from_static(&[1, 2, 3]),
        }];
        let run = AudioRun {
            base_decode_time: 5,
            frames: &audio,
            sample_duration: 1024,
        };
        let frag = Bytes::from(fragment_with_audio(
            1,
            1_000_000,
            &frames,
            &[6000, 6000, 6000],
            Some(&run),
        ));
        let s = read_video_samples(&frag).unwrap();
        assert_eq!(
            s.iter()
                .map(|s| (s.dts_90k, s.keyframe, s.data.len()))
                .collect::<Vec<_>>(),
            [
                (1_000_000, true, 100),
                (1_006_000, false, 20),
                (1_012_000, false, 30)
            ]
        );
        assert!(s.iter().all(|s| s.data.iter().all(|&b| b == 0xAB)));
        assert!(read_video_samples(&Bytes::from_static(b"garbage!")).is_none());
    }

    #[test]
    fn finds_the_decoder_record_in_an_init_segment() {
        let record = [1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0];
        let cfg = VideoConfig::from_avcc(&record, 640, 360).unwrap();
        let audio = AudioConfig::from_aac_asc(&[0x14, 0x08]).unwrap();
        let init = init_segment(&cfg, Some(&audio));
        assert_eq!(decoder_record_from_init(&init), Some(&record[..]));
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
