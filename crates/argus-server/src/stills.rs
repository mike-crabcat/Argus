//! Event stills: a few JPEGs of each event, for summaries and search
//! results, kept in `events/{id}/{n}.jpg` under the storage root and
//! deleted once the event's recording has left local disk.
//!
//! For now they come from the camera itself (Reolink's Snap API) at the
//! start of the event and a couple of seconds in. [`StillSource`] keeps
//! that swappable: frames decoded from the recording can replace it later
//! without touching the store or the summaries.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use argus_store::Index;
use tracing::{debug, warn};

use crate::reolink;

/// When stills are taken, after the event starts.
pub const OFFSETS_MS: [u64; 3] = [0, 2_000, 5_000];

/// A JPEG shrunk to fit `edge` pixels on its longer side, or None if it
/// already fits.
pub fn shrink(jpeg: &[u8], edge: u32) -> Result<Option<Vec<u8>>> {
    use image::ImageDecoder;
    let dec = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(jpeg))?;
    let (w, h) = dec.dimensions();
    if w.max(h) <= edge {
        return Ok(None);
    }
    let img = image::DynamicImage::from_decoder(dec)?;
    let small = img.resize(edge, edge, image::imageops::FilterType::Triangle);
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, MODEL_QUALITY)
        .encode_image(&small.to_rgb8())?;
    Ok(Some(out))
}

/// Something that can produce a still of one camera now.
pub trait StillSource: Send + Sync {
    fn grab(&self) -> argus_agent::BoxFuture<'_, Result<Vec<u8>>>;
}

/// Stills from a Reolink camera's Snap API (main stream resolution). Its
/// own API session, separate from event polling.
pub struct ReolinkSnaps {
    client: tokio::sync::Mutex<reolink::Client>,
}

impl ReolinkSnaps {
    pub fn new(client: reolink::Client) -> Self {
        ReolinkSnaps {
            client: tokio::sync::Mutex::new(client),
        }
    }
}

impl StillSource for ReolinkSnaps {
    fn grab(&self) -> argus_agent::BoxFuture<'_, Result<Vec<u8>>> {
        Box::pin(async move { self.client.lock().await.snap().await })
    }
}

/// `events/{id}/{n}.jpg` under the storage root.
#[derive(Clone)]
pub struct StillStore {
    dir: PathBuf,
    /// Stills sent to a model are shrunk to fit this many pixels on their
    /// longer edge (0: as taken). The files on disk stay full size.
    pub model_edge: u32,
}

/// The default for [`StillStore::model_edge`].
pub const MODEL_EDGE: u32 = 1280;
/// JPEG quality of shrunk stills.
const MODEL_QUALITY: u8 = 80;

impl StillStore {
    pub fn new(root: &Path) -> Self {
        StillStore {
            dir: root.join("events"),
            model_edge: MODEL_EDGE,
        }
    }

    pub fn with_model_edge(mut self, edge: u32) -> Self {
        self.model_edge = edge;
        self
    }

    /// Still `n` of event `id` as a model should see it: shrunk to
    /// [`Self::model_edge`]. Falls back to the file as taken if it can't
    /// be decoded.
    pub async fn for_model(&self, id: i64, n: u32) -> Option<Vec<u8>> {
        let jpeg = self.read(id, n)?;
        let edge = self.model_edge;
        if edge == 0 {
            return Some(jpeg);
        }
        tokio::task::spawn_blocking(move || match shrink(&jpeg, edge) {
            Ok(Some(small)) => small,
            Ok(None) => jpeg,
            Err(e) => {
                debug!("shrinking a still: {e:#}");
                jpeg
            }
        })
        .await
        .ok()
    }

    /// The media key of still `n` of event `id` (`/api/media/{key}`).
    pub fn key(id: i64, n: u32) -> String {
        format!("events/{id}/{n}.jpg")
    }

    pub fn save(&self, id: i64, n: u32, jpeg: &[u8]) -> Result<()> {
        anyhow::ensure!(jpeg.starts_with(&[0xFF, 0xD8]), "not a JPEG");
        let d = self.dir.join(id.to_string());
        std::fs::create_dir_all(&d)?;
        let tmp = d.join(format!("{n}.jpg.partial"));
        std::fs::write(&tmp, jpeg)?;
        std::fs::rename(&tmp, d.join(format!("{n}.jpg")))?;
        Ok(())
    }

    /// The stills an event has, in order.
    pub fn list(&self, id: i64) -> Vec<u32> {
        let Ok(rd) = std::fs::read_dir(self.dir.join(id.to_string())) else {
            return Vec::new();
        };
        let mut v: Vec<u32> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str()?.strip_suffix(".jpg")?.parse().ok())
            .collect();
        v.sort();
        v
    }

    pub fn read(&self, id: i64, n: u32) -> Option<Vec<u8>> {
        std::fs::read(self.dir.join(id.to_string()).join(format!("{n}.jpg"))).ok()
    }

    /// Events that have stills.
    pub fn events(&self) -> Vec<i64> {
        std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn remove(&self, id: i64) -> Result<()> {
        std::fs::remove_dir_all(self.dir.join(id.to_string()))
            .with_context(|| format!("removing stills of event {id}"))
    }

    /// Deletes stills of events whose recording is no longer on local disk
    /// (older than the camera's oldest local chunk), and of events that are
    /// gone. Returns how many events lost their stills.
    pub fn expire(&self, index: &Index) -> Result<usize> {
        let mut gone = 0;
        for id in self.events() {
            let stale = match index.event(id)? {
                None => true,
                Some(e) => index
                    .oldest_local_ms(&e.camera)?
                    .is_some_and(|oldest| e.end_ms < oldest),
            };
            if stale {
                self.remove(id)?;
                gone += 1;
            }
        }
        Ok(gone)
    }
}

/// Takes an event's stills at [`OFFSETS_MS`] after now (the event's
/// start). Failures are logged; whatever was taken is kept.
pub async fn capture(
    store: StillStore,
    source: Arc<dyn StillSource>,
    camera: String,
    id: i64,
    offsets: &[u64],
) -> usize {
    let t0 = tokio::time::Instant::now();
    let mut saved = 0;
    for (n, &off) in offsets.iter().enumerate() {
        tokio::time::sleep_until(t0 + Duration::from_millis(off)).await;
        match source.grab().await {
            Ok(jpeg) => match store.save(id, n as u32, &jpeg) {
                Ok(()) => saved += 1,
                Err(e) => warn!(%camera, event = id, "saving a still: {e:#}"),
            },
            Err(e) => debug!(%camera, event = id, "no still: {e:#}"),
        }
    }
    if saved == 0 {
        warn!(%camera, event = id, "no stills for this event");
    }
    saved
}

/// Per-camera still sources, for the event watchers to start captures.
#[derive(Clone, Default)]
pub struct Stills {
    pub sources: Arc<Mutex<std::collections::HashMap<String, Arc<dyn StillSource>>>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Fake {
        n: AtomicU32,
        fail_second: bool,
    }

    impl StillSource for Fake {
        fn grab(&self) -> argus_agent::BoxFuture<'_, Result<Vec<u8>>> {
            Box::pin(async move {
                let n = self.n.fetch_add(1, Ordering::SeqCst);
                if self.fail_second && n == 1 {
                    anyhow::bail!("camera busy");
                }
                Ok(vec![0xFF, 0xD8, n as u8, 0xFF, 0xD9])
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn captures_at_the_offsets_and_keeps_what_it_got() {
        let dir = tempfile::tempdir().unwrap();
        let store = StillStore::new(dir.path());
        let src = Arc::new(Fake {
            n: AtomicU32::new(0),
            fail_second: true,
        });
        let t0 = tokio::time::Instant::now();
        let saved = capture(store.clone(), src, "door".into(), 7, &OFFSETS_MS).await;
        assert_eq!(saved, 2);
        assert_eq!(t0.elapsed(), Duration::from_millis(5_000));
        assert_eq!(store.list(7), vec![0, 2]);
        assert_eq!(store.read(7, 2).unwrap()[2], 2);
        assert!(store.save(7, 9, b"GIF89a").is_err());
        assert_eq!(StillStore::key(7, 2), "events/7/2.jpg");
    }

    #[test]
    fn expire_follows_local_recordings() {
        let dir = tempfile::tempdir().unwrap();
        let store = StillStore::new(dir.path());
        let index = Index::open_in_memory().unwrap();
        let old = index
            .add_event("door", 1_000, 2_000, "reolink", "person")
            .unwrap();
        let new = index
            .add_event("door", 50_000, 51_000, "reolink", "person")
            .unwrap();
        let other = index
            .add_event("back", 1_000, 2_000, "reolink", "animal")
            .unwrap();
        for id in [old, new, other, 999] {
            store.save(id, 0, &[0xFF, 0xD8, 0xFF, 0xD9]).unwrap();
        }
        // "door" keeps recordings from 10 s on; "back" has none locally.
        let cfg = argus_media::VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360)
            .unwrap();
        let init = index
            .upsert_init("door", "main", 1, "init/door/main/1.mp4", &cfg, None)
            .unwrap();
        index
            .insert_chunk(&argus_store::ChunkRecord {
                camera: "door".into(),
                stream: "main".into(),
                start_ms: 10_000,
                end_ms: 20_000,
                object_key: "rec/door/main/x.m4s".into(),
                bytes: 10,
                init_id: init,
                gops: vec![],
            })
            .unwrap();
        assert_eq!(
            store.expire(&index).unwrap(),
            2,
            "old door event and the unknown 999"
        );
        let mut left = store.events();
        left.sort();
        assert_eq!(left, vec![new, other].into_iter().collect::<Vec<_>>());
    }
}
