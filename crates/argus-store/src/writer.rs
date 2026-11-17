use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use std::collections::VecDeque;

use argus_media::fmp4::AudioRun;
use argus_media::{AudioConfig, AudioFrame, Frame, VideoConfig, fmp4, stream_fingerprint};
use tracing::{debug, info, warn};

use crate::gop_index::GopEntry;
use crate::index::{ChunkRecord, Index};
use crate::keys;

/// Writes one camera stream to GOP-aligned fMP4 chunks.
///
/// Frames are buffered until the next keyframe arrives, then the whole GOP
/// is written as one fragment. This buffering is for storage only: live
/// viewers get frames straight from ingest without waiting for it.
///
/// Audio frames are buffered alongside, and each fragment takes the audio
/// whose wall time falls before the next GOP starts. Audio that arrives late
/// goes into the following fragment, still placed back to back on the audio
/// track's timeline, so a GOP's byte range always holds its video and the
/// audio around it.
pub struct ChunkWriter {
    root: PathBuf,
    camera: String,
    stream: String,
    chunk_ms: i64,
    index: Arc<Mutex<Index>>,
    video_config: Option<VideoConfig>,
    audio_config: Option<AudioConfig>,
    /// The init segment that fragments are currently written against.
    config: Option<ActiveConfig>,
    gop: Vec<Frame>,
    audio: VecDeque<AudioFrame>,
    /// Where the next audio fragment starts on the audio track, so audio
    /// stays contiguous across fragments despite arrival jitter.
    next_audio_dts: Option<u64>,
    /// Where the next video fragment starts (90 kHz), so fragments never
    /// overlap when the wall clock is pulled earlier between GOPs.
    next_video_dts: Option<u64>,
    open: Option<OpenChunk>,
    sequence: u32,
}

struct ActiveConfig {
    fingerprint: u64,
    init_id: i64,
}

struct OpenChunk {
    file: File,
    partial: PathBuf,
    key: String,
    start_ms: i64,
    bytes: u64,
    gops: Vec<GopEntry>,
    end_ms: i64,
}

impl ChunkWriter {
    pub fn new(
        root: &Path,
        camera: &str,
        stream: &str,
        chunk_secs: u32,
        index: Arc<Mutex<Index>>,
    ) -> Self {
        Self {
            root: root.to_owned(),
            camera: camera.to_owned(),
            stream: stream.to_owned(),
            chunk_ms: i64::from(chunk_secs.max(1)) * 1000,
            index,
            video_config: None,
            audio_config: None,
            config: None,
            gop: Vec::new(),
            audio: VecDeque::new(),
            next_audio_dts: None,
            next_video_dts: None,
            open: None,
            sequence: 1,
        }
    }

    /// New video parameters. A real change finishes the buffered GOP under
    /// the old ones; the next fragment then starts a new init segment and
    /// chunk, since a chunk only ever refers to one init segment.
    pub fn on_config(&mut self, cfg: &VideoConfig) -> Result<()> {
        if self.video_config.as_ref() == Some(cfg) {
            return Ok(());
        }
        self.flush(None)?;
        self.video_config = Some(cfg.clone());
        Ok(())
    }

    /// New audio parameters. Takes effect from the next fragment written
    /// (which starts a new init segment if they changed). Buffered audio
    /// from different old parameters is dropped rather than mislabelled.
    pub fn on_audio_config(&mut self, cfg: &AudioConfig) -> Result<()> {
        if self.audio_config.as_ref() == Some(cfg) {
            return Ok(());
        }
        if self.audio_config.is_some() {
            self.audio.clear();
            self.next_audio_dts = None;
        }
        self.audio_config = Some(cfg.clone());
        Ok(())
    }

    pub fn on_audio(&mut self, frame: AudioFrame) -> Result<()> {
        // Before the first keyframe there is no fragment to put it in.
        if self.audio_config.is_some() && !self.gop.is_empty() {
            self.audio.push_back(frame);
        }
        Ok(())
    }

    pub fn on_frame(&mut self, frame: Frame) -> Result<()> {
        if self.video_config.is_none() {
            return Ok(()); // can't store frames we can't describe
        }
        if frame.keyframe && !self.gop.is_empty() {
            self.flush(Some((frame.pts_90k, frame.wall_us)))?;
        }
        if self.gop.is_empty() && !frame.keyframe {
            return Ok(()); // waiting for the first keyframe
        }
        self.gop.push(frame);
        Ok(())
    }

    /// The source restarted (reconnect, lag): timestamps are no longer
    /// continuous, so finish the GOP and chunk we have.
    pub fn on_discontinuity(&mut self) -> Result<()> {
        self.flush(None)?;
        self.audio.clear();
        self.next_audio_dts = None;
        self.next_video_dts = None;
        self.close_chunk()
    }

    pub fn finish(&mut self) -> Result<()> {
        self.on_discontinuity()
    }

    /// Writes the buffered GOP. `next` is the (pts, wall time) of the
    /// keyframe that starts the following GOP, when known.
    fn flush(&mut self, next: Option<(i64, i64)>) -> Result<()> {
        if self.gop.is_empty() {
            return Ok(());
        }
        self.use_current_config()?;
        let gop = std::mem::take(&mut self.gop);
        let audio_end = match next {
            Some((_, wall)) => self.audio.partition_point(|a| a.wall_us < wall),
            None => self.audio.len(),
        };
        let audio: Vec<AudioFrame> = self.audio.drain(..audio_end).collect();
        let durations = fmp4::durations(&gop, next.map(|(pts, _)| pts));
        let total: u64 = durations.iter().map(|&d| u64::from(d)).sum();
        // Decode time is wall-clock time in 90 kHz ticks since the Unix epoch,
        // so every camera's fragments share one timeline and chunks can be
        // concatenated or fed to a player back to back. If the wall clock
        // was corrected earlier since the last GOP (by up to a second),
        // continue where that one ended instead of overlapping it.
        let from_wall = u64::try_from(gop[0].wall_us).unwrap_or(0) * 9 / 100;
        let base = match self.next_video_dts {
            Some(n) if from_wall < n && n - from_wall <= 90_000 => n,
            _ => from_wall,
        };
        self.next_video_dts = Some(base + total);
        let gop_start_ms = (base / 90) as i64;
        if self.open.as_ref().is_some_and(|c| {
            gop_start_ms - c.start_ms >= self.chunk_ms || gop_start_ms < c.start_ms
        }) {
            self.close_chunk()?;
        }
        if self.open.is_none() {
            self.open = Some(self.open_chunk(gop_start_ms)?);
        }
        let c = self.open.as_mut().unwrap();
        let audio_run = match (&self.audio_config, audio.first()) {
            (Some(cfg), Some(first)) => {
                // The audio track's decode time is wall-clock time in samples
                // since the epoch, the same timeline as the video. Continue
                // from the previous fragment unless that is off by more than
                // half a second (a gap or a clock jump).
                let rate = u64::from(cfg.sample_rate);
                let wall = u64::try_from(first.wall_us).unwrap_or(0);
                let from_wall = (u128::from(wall) * u128::from(rate) / 1_000_000) as u64;
                let base = match self.next_audio_dts {
                    Some(n) if n.abs_diff(from_wall) <= rate / 2 => n,
                    _ => from_wall,
                };
                self.next_audio_dts =
                    Some(base + audio.len() as u64 * u64::from(cfg.samples_per_frame));
                Some(AudioRun {
                    base_decode_time: base,
                    frames: &audio,
                    sample_duration: cfg.samples_per_frame,
                })
            }
            _ => None,
        };
        let frag =
            fmp4::fragment_with_audio(self.sequence, base, &gop, &durations, audio_run.as_ref());
        self.sequence = self.sequence.wrapping_add(1);
        c.file.write_all(&frag)?;
        c.gops.push(GopEntry {
            start_offset_ms: (gop_start_ms - c.start_ms) as u32,
            byte_offset: c.bytes as u32,
            byte_len: frag.len() as u32,
            frame_count: gop.len().min(usize::from(u16::MAX)) as u16,
        });
        c.bytes += frag.len() as u64;
        c.end_ms = c.end_ms.max(gop_start_ms + (total as i64) / 90);
        Ok(())
    }

    /// Makes sure fragments are written against an init segment matching the
    /// current video and audio configs, starting a new one (and a new chunk)
    /// if they changed.
    fn use_current_config(&mut self) -> Result<()> {
        let cfg = self
            .video_config
            .clone()
            .expect("frames are only buffered after a video config");
        let audio = self.audio_config.clone();
        let fingerprint = stream_fingerprint(&cfg, audio.as_ref());
        if self
            .config
            .as_ref()
            .is_some_and(|c| c.fingerprint == fingerprint)
        {
            return Ok(());
        }
        self.close_chunk()?;
        let key = keys::init_key(&self.camera, &self.stream, fingerprint);
        let path = self.root.join(&key);
        if !path.exists() {
            write_atomic(&path, &fmp4::init_segment(&cfg, audio.as_ref()))?;
        }
        let init_id = self.index.lock().unwrap().upsert_init(
            &self.camera,
            &self.stream,
            fingerprint,
            &key,
            &cfg,
            audio.as_ref(),
        )?;
        info!(camera = %self.camera, stream = %self.stream, codec = %cfg.rfc6381,
              width = cfg.width, height = cfg.height,
              audio = audio.as_ref().map_or("none", |a| a.rfc6381.as_str()),
              "new init segment {key}");
        self.config = Some(ActiveConfig {
            fingerprint,
            init_id,
        });
        Ok(())
    }

    fn open_chunk(&self, start_ms: i64) -> Result<OpenChunk> {
        let key = keys::chunk_key(&self.camera, &self.stream, start_ms);
        let partial = self.root.join(format!("{key}.partial"));
        fs::create_dir_all(partial.parent().unwrap())?;
        let file =
            File::create(&partial).with_context(|| format!("creating {}", partial.display()))?;
        debug!(camera = %self.camera, stream = %self.stream, "opened {key}");
        Ok(OpenChunk {
            file,
            partial,
            key,
            start_ms,
            bytes: 0,
            gops: Vec::new(),
            end_ms: start_ms,
        })
    }

    fn close_chunk(&mut self) -> Result<()> {
        let Some(mut c) = self.open.take() else {
            return Ok(());
        };
        c.file.flush()?;
        c.file.sync_data()?;
        drop(c.file);
        if c.gops.is_empty() {
            let _ = fs::remove_file(&c.partial);
            return Ok(());
        }
        fs::rename(&c.partial, self.root.join(&c.key))?;
        let init_id = self.config.as_ref().expect("chunk without config").init_id;
        let rec = ChunkRecord {
            camera: self.camera.clone(),
            stream: self.stream.clone(),
            start_ms: c.start_ms,
            end_ms: c.end_ms,
            object_key: c.key.clone(),
            bytes: c.bytes,
            init_id,
            gops: c.gops,
        };
        if let Err(e) = self.index.lock().unwrap().insert_chunk(&rec) {
            warn!("indexing {} failed: {e:#}", rec.object_key);
            return Err(e);
        }
        info!(camera = %self.camera, stream = %self.stream, bytes = rec.bytes,
              secs = (rec.end_ms - rec.start_ms) / 1000, "closed {}", rec.object_key);
        Ok(())
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("partial");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn frame(i: i64, key: bool) -> Frame {
        // 10 fps, starting at a fixed wall time.
        Frame {
            pts_90k: i * 9000,
            wall_us: 1_700_000_000_000_000 + i * 100_000,
            keyframe: key,
            data: Bytes::from_static(&[0, 0, 0, 1, 0x65]),
        }
    }

    #[test]
    fn rolls_chunks_on_gop_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
        let mut w = ChunkWriter::new(dir.path(), "cam", "sub", 2, index.clone());
        let cfg = VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360).unwrap();
        w.on_frame(frame(0, true)).unwrap(); // dropped: no config yet
        w.on_config(&cfg).unwrap();
        w.on_frame(frame(1, false)).unwrap(); // dropped: no keyframe yet
        for i in 10..60 {
            w.on_frame(frame(i, i % 10 == 0)).unwrap(); // 1 s GOPs, 5 s total
        }
        w.finish().unwrap();
        let chunks = index
            .lock()
            .unwrap()
            .chunks_between("cam", "sub", 0, i64::MAX)
            .unwrap();
        // 2 s chunks of 1 s GOPs: [1,2], [3,4], [5].
        assert_eq!(
            chunks.iter().map(|c| c.gops.len()).collect::<Vec<_>>(),
            [2, 2, 1]
        );
        assert!(
            chunks
                .iter()
                .all(|c| dir.path().join(&c.object_key).exists())
        );
        assert_eq!(chunks[0].end_ms, chunks[1].start_ms);
        let t = index
            .lock()
            .unwrap()
            .seek("cam", "sub", chunks[1].start_ms + 1500)
            .unwrap()
            .unwrap();
        assert_eq!(t.gop_start_ms, chunks[1].start_ms + 1000);
    }

    /// tfdt of each fragment in a chunk file, in order.
    fn tfdts(bytes: &[u8]) -> Vec<u64> {
        bytes
            .windows(4)
            .enumerate()
            .filter(|(_, w)| *w == b"tfdt")
            .map(|(i, _)| u64::from_be_bytes(bytes[i + 8..i + 16].try_into().unwrap()))
            .collect()
    }

    #[test]
    fn earlier_clock_correction_does_not_overlap_fragments() {
        let dir = tempfile::tempdir().unwrap();
        let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
        let mut w = ChunkWriter::new(dir.path(), "cam", "sub", 60, index.clone());
        let cfg = VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360).unwrap();
        w.on_config(&cfg).unwrap();
        for i in 0..30 {
            let mut f = frame(i, i % 10 == 0);
            if i >= 10 {
                // The wall clock was pulled 30 ms earlier after the first GOP.
                f.wall_us -= 30_000;
            }
            w.on_frame(f).unwrap();
        }
        w.finish().unwrap();
        let chunk = index
            .lock()
            .unwrap()
            .chunks_between("cam", "sub", 0, i64::MAX)
            .unwrap()
            .remove(0);
        let t = tfdts(&std::fs::read(dir.path().join(&chunk.object_key)).unwrap());
        // 1 s GOPs at 90 kHz, back to back: the second GOP starts where the
        // first ended, not 30 ms inside it.
        assert_eq!(t.len(), 3);
        assert_eq!(t[1] - t[0], 90_000);
        assert_eq!(t[2] - t[1], 90_000);
        let starts: Vec<_> = chunk.gops.iter().map(|g| g.start_offset_ms).collect();
        assert_eq!(starts, [0, 1000, 2000]);
    }

    fn audio(i: i64) -> AudioFrame {
        // 16 kHz AAC: 1024 samples = 64 ms.
        AudioFrame {
            pts: i * 1024,
            wall_us: 1_700_000_000_000_000 + i * 64_000,
            data: Bytes::from_static(&[0x21, 0x10, 0x04]),
        }
    }

    #[test]
    fn audio_goes_into_the_gop_it_belongs_to() {
        let dir = tempfile::tempdir().unwrap();
        let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
        let mut w = ChunkWriter::new(dir.path(), "cam", "main", 60, index.clone());
        let cfg = VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360).unwrap();
        w.on_config(&cfg).unwrap();
        w.on_audio_config(&AudioConfig::from_aac_asc(&[0x14, 0x08]).unwrap())
            .unwrap();
        w.on_audio(audio(0)).unwrap(); // dropped: no keyframe yet
        // 3 s of video in 1 s GOPs, audio interleaved by wall time.
        let mut next_audio = 0;
        for i in 0..30 {
            let f = frame(i, i % 10 == 0);
            while audio(next_audio).wall_us < f.wall_us {
                w.on_audio(audio(next_audio)).unwrap();
                next_audio += 1;
            }
            w.on_frame(f).unwrap();
        }
        w.finish().unwrap();
        let chunks = index
            .lock()
            .unwrap()
            .chunks_between("cam", "main", 0, i64::MAX)
            .unwrap();
        assert_eq!(chunks.len(), 1);
        let bytes = std::fs::read(dir.path().join(&chunks[0].object_key)).unwrap();
        // Count audio samples in each GOP's moof: the second traf's trun.
        let per_gop: Vec<u32> = chunks[0]
            .gops
            .iter()
            .map(|g| {
                let frag = &bytes[g.byte_offset as usize..(g.byte_offset + g.byte_len) as usize];
                let at = frag.windows(4).rposition(|w| w == b"trun").unwrap();
                u32::from_be_bytes(frag[at + 8..at + 12].try_into().unwrap())
            })
            .collect();
        // Audio 0..=15 start before the 1 s keyframe (15 * 64 ms = 960 ms),
        // 16..=31 before 2 s; the last GOP takes what arrived before the
        // final video frame at 2.9 s (32..=45).
        assert_eq!(per_gop, [16, 16, 14]);
        let fp = {
            let a = AudioConfig::from_aac_asc(&[0x14, 0x08]).unwrap();
            stream_fingerprint(&cfg, Some(&a))
        };
        assert!(dir.path().join(keys::init_key("cam", "main", fp)).exists());
        assert!(
            !dir.path()
                .join(keys::init_key("cam", "main", cfg.fingerprint()))
                .exists(),
            "no video-only init segment should be written first"
        );
    }

    #[test]
    fn audio_config_change_starts_new_init_segment() {
        let dir = tempfile::tempdir().unwrap();
        let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
        let mut w = ChunkWriter::new(dir.path(), "cam", "main", 60, index.clone());
        let cfg = VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360).unwrap();
        w.on_config(&cfg).unwrap();
        w.on_audio_config(&AudioConfig::from_aac_asc(&[0x14, 0x08]).unwrap())
            .unwrap();
        for i in 0..20 {
            w.on_frame(frame(i, i % 10 == 0)).unwrap();
        }
        w.on_audio_config(&AudioConfig::from_aac_asc(&[0x11, 0x90]).unwrap())
            .unwrap();
        for i in 20..40 {
            w.on_frame(frame(i, i % 10 == 0)).unwrap();
        }
        w.finish().unwrap();
        let chunks = index
            .lock()
            .unwrap()
            .chunks_between("cam", "main", 0, i64::MAX)
            .unwrap();
        assert_eq!(chunks.len(), 2);
        assert_ne!(chunks[0].init_id, chunks[1].init_id);
    }
}
