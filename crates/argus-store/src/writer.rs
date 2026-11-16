use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use argus_media::{Frame, VideoConfig, fmp4};
use tracing::{debug, info, warn};

use crate::gop_index::GopEntry;
use crate::index::{ChunkRecord, Index};
use crate::keys;

/// Writes one camera stream to GOP-aligned fMP4 chunks.
///
/// Frames are buffered until the next keyframe arrives, then the whole GOP
/// is written as one fragment. This buffering is for storage only: live
/// viewers get frames straight from ingest without waiting for it.
pub struct ChunkWriter {
    root: PathBuf,
    camera: String,
    stream: String,
    chunk_ms: i64,
    index: Arc<Mutex<Index>>,
    config: Option<ActiveConfig>,
    gop: Vec<Frame>,
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
            config: None,
            gop: Vec::new(),
            open: None,
            sequence: 1,
        }
    }

    /// New codec parameters. A real change closes the current chunk, since a
    /// chunk only ever refers to one init segment.
    pub fn on_config(&mut self, cfg: &VideoConfig) -> Result<()> {
        let fingerprint = cfg.fingerprint();
        if self
            .config
            .as_ref()
            .is_some_and(|c| c.fingerprint == fingerprint)
        {
            return Ok(());
        }
        self.flush(None)?;
        self.close_chunk()?;
        let key = keys::init_key(&self.camera, &self.stream, fingerprint);
        let path = self.root.join(&key);
        if !path.exists() {
            write_atomic(&path, &fmp4::init_segment(cfg))?;
        }
        let init_id = self.index.lock().unwrap().upsert_init(
            &self.camera,
            &self.stream,
            fingerprint,
            &key,
            cfg,
        )?;
        info!(camera = %self.camera, stream = %self.stream, codec = %cfg.rfc6381,
              width = cfg.width, height = cfg.height, "new init segment {key}");
        self.config = Some(ActiveConfig {
            fingerprint,
            init_id,
        });
        Ok(())
    }

    pub fn on_frame(&mut self, frame: Frame) -> Result<()> {
        if self.config.is_none() {
            return Ok(()); // can't store frames we can't describe
        }
        if frame.keyframe && !self.gop.is_empty() {
            self.flush(Some(frame.pts_90k))?;
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
        self.close_chunk()
    }

    pub fn finish(&mut self) -> Result<()> {
        self.on_discontinuity()
    }

    fn flush(&mut self, next_pts: Option<i64>) -> Result<()> {
        if self.gop.is_empty() {
            return Ok(());
        }
        let gop = std::mem::take(&mut self.gop);
        let gop_start_ms = gop[0].wall_ms();
        if self.open.as_ref().is_some_and(|c| {
            gop_start_ms - c.start_ms >= self.chunk_ms || gop_start_ms < c.start_ms
        }) {
            self.close_chunk()?;
        }
        if self.open.is_none() {
            self.open = Some(self.open_chunk(gop_start_ms)?);
        }
        let c = self.open.as_mut().unwrap();
        let durations = fmp4::durations(&gop, next_pts);
        // Decode time is wall-clock time in 90 kHz ticks since the Unix epoch,
        // so every camera's fragments share one timeline and chunks can be
        // concatenated or fed to a player back to back.
        let base = u64::try_from(gop[0].wall_us).unwrap_or(0) * 9 / 100;
        let frag = fmp4::fragment(self.sequence, base, &gop, &durations);
        self.sequence = self.sequence.wrapping_add(1);
        c.file.write_all(&frag)?;
        let total: u64 = durations.iter().map(|&d| u64::from(d)).sum();
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
}
