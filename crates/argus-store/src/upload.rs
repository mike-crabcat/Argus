//! Copies closed chunks to object storage (S3 or compatible) and frees local
//! disk.
//!
//! All state lives in the index (`uploaded_at`, `remote` and `tier` on
//! chunks and init segments), so after a restart the uploader carries on
//! where it stopped. Which chunks get uploaded is a per-stream [`Policy`].

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures::future::BoxFuture;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::index::{Index, StoredChunk, tier};

/// Main-stream chunks within this much of an event are kept at full
/// resolution, so the lead-up and aftermath are included.
pub const EVENT_PAD_MS: i64 = 10_000;

/// An events-policy chunk with no event is only evicted once it is this old,
/// so an event added a little late (camera trigger, manual mark) still finds
/// its footage.
pub const EVENT_GRACE_MS: i64 = 3_600_000;

/// Which chunks of a stream go to object storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Every chunk.
    Always,
    /// Only chunks overlapping an event (padded by [`EVENT_PAD_MS`]).
    Events,
    /// None; the stream is kept on local disk only, within the budget.
    Never,
}

impl Policy {
    /// `main` (full resolution) uploads around events; everything else,
    /// such as `sub`, uploads always.
    pub fn default_for(stream: &str) -> Self {
        if stream == "main" {
            Policy::Events
        } else {
            Policy::Always
        }
    }
}

impl FromStr for Policy {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "always" => Policy::Always,
            "events" => Policy::Events,
            "never" => Policy::Never,
            _ => bail!("upload policy {s:?} must be \"always\", \"events\" or \"never\""),
        })
    }
}

/// The upload policy of one recorded stream.
#[derive(Clone, Debug)]
pub struct StreamPolicy {
    pub camera: String,
    pub stream: String,
    pub policy: Policy,
}

/// Where uploads go. Implemented for any `object_store` backend; tests can
/// plug in a flaky one.
pub trait UploadTarget: Send + Sync {
    fn put<'a>(&'a self, key: &'a str, data: Bytes) -> BoxFuture<'a, Result<()>>;
}

impl UploadTarget for Arc<dyn ObjectStore> {
    fn put<'a>(&'a self, key: &'a str, data: Bytes) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ObjectStoreExt::put(
                self.as_ref(),
                &object_store::path::Path::from(key),
                PutPayload::from(data),
            )
            .await?;
            Ok(())
        })
    }
}

/// Average-rate limiter. Each upload pays for its bytes before it starts;
/// unused allowance builds up to `burst` bytes.
#[derive(Debug)]
pub struct TokenBucket {
    bytes_per_sec: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// `mbps` is megabits per second; 0 means unlimited. The bucket starts
    /// full, with two seconds' worth.
    pub fn new(mbps: f64) -> Self {
        let bytes_per_sec = mbps * 1_000_000.0 / 8.0;
        Self {
            bytes_per_sec,
            burst: bytes_per_sec * 2.0,
            tokens: bytes_per_sec * 2.0,
            last: Instant::now(),
        }
    }

    pub async fn acquire(&mut self, bytes: u64) {
        if self.bytes_per_sec <= 0.0 {
            return;
        }
        let now = Instant::now();
        let refill = now.duration_since(self.last).as_secs_f64() * self.bytes_per_sec;
        self.tokens = (self.tokens + refill).min(self.burst) - bytes as f64;
        self.last = now;
        if self.tokens < 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(-self.tokens / self.bytes_per_sec)).await;
        }
    }
}

pub struct Uploader<T> {
    target: T,
    index: Arc<Mutex<Index>>,
    root: PathBuf,
    prefix: String,
    streams: Vec<StreamPolicy>,
    limiter: TokenBucket,
    /// First retry delay; doubles up to `max_backoff`.
    pub min_backoff: Duration,
    pub max_backoff: Duration,
    /// How often to look for new chunks when idle.
    pub poll: Duration,
}

impl<T: UploadTarget> Uploader<T> {
    pub fn new(
        target: T,
        index: Arc<Mutex<Index>>,
        root: &Path,
        prefix: &str,
        streams: Vec<StreamPolicy>,
        max_upload_mbps: f64,
    ) -> Self {
        Self {
            target,
            index,
            root: root.to_owned(),
            prefix: prefix.trim_matches('/').to_owned(),
            streams,
            limiter: TokenBucket::new(max_upload_mbps),
            min_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            poll: Duration::from_secs(5),
        }
    }

    /// The object key for a local key, under the configured prefix.
    pub fn remote_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{}/{key}", self.prefix)
        }
    }

    /// Uploads forever.
    pub async fn run(mut self) -> Result<()> {
        loop {
            let n = self.upload_pending().await?;
            if n == 0 {
                tokio::time::sleep(self.poll).await;
            }
        }
    }

    /// Uploads every chunk that is eligible right now, oldest first across
    /// all streams, and returns how many it uploaded.
    pub async fn upload_pending(&mut self) -> Result<usize> {
        let mut done = 0;
        let mut bytes = 0;
        loop {
            let batch = self.next_batch(32)?;
            if batch.is_empty() {
                break;
            }
            for c in batch {
                if self.upload_chunk(&c).await? {
                    done += 1;
                    bytes += c.bytes;
                }
            }
        }
        if done > 0 {
            info!(
                chunks = done,
                mb = bytes / 1_000_000,
                "uploaded to object storage"
            );
        }
        Ok(done)
    }

    fn next_batch(&self, per_stream: usize) -> Result<Vec<StoredChunk>> {
        let index = self.index.lock().unwrap();
        let mut all = Vec::new();
        for s in &self.streams {
            let pad = match s.policy {
                Policy::Always => None,
                Policy::Events => Some(EVENT_PAD_MS),
                Policy::Never => continue,
            };
            all.extend(index.pending_uploads(&s.camera, &s.stream, pad, per_stream)?);
        }
        all.sort_by_key(|c| (c.start_ms, c.id));
        all.truncate(per_stream);
        Ok(all)
    }

    /// Returns false if the chunk's file is gone and it was skipped.
    async fn upload_chunk(&mut self, c: &StoredChunk) -> Result<bool> {
        let (init_key, init_uploaded) = self.index.lock().unwrap().init_upload_state(c.init_id)?;
        if !init_uploaded {
            let data = tokio::fs::read(self.root.join(&init_key))
                .await
                .with_context(|| format!("reading init segment {init_key}"))?;
            let remote = self.remote_key(&init_key);
            self.put_with_retry(&remote, Bytes::from(data)).await;
            self.index
                .lock()
                .unwrap()
                .mark_init_uploaded(c.init_id, &remote, now_ms())?;
        }
        let data = match tokio::fs::read(self.root.join(&c.object_key)).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                warn!(key = %c.object_key, "chunk file missing; not uploading it");
                self.index.lock().unwrap().set_tier(c.id, tier::DISCARDED)?;
                return Ok(false);
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", c.object_key)),
        };
        self.limiter.acquire(data.len() as u64).await;
        let remote = self.remote_key(&c.object_key);
        self.put_with_retry(&remote, Bytes::from(data)).await;
        self.index
            .lock()
            .unwrap()
            .mark_chunk_uploaded(c.id, &remote, now_ms())?;
        debug!(key = %remote, bytes = c.bytes, "uploaded");
        Ok(true)
    }

    /// Retries until it succeeds: the chunk stays queued (and protected from
    /// eviction) until then, and later chunks wait so uploads stay in order.
    async fn put_with_retry(&self, key: &str, data: Bytes) {
        let mut delay = self.min_backoff;
        loop {
            match self.target.put(key, data.clone()).await {
                Ok(()) => return,
                Err(e) => {
                    warn!(key, retry_in = ?delay, "upload failed: {e:#}");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(self.max_backoff);
                }
            }
        }
    }
}

/// What one eviction pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Evicted {
    pub chunks: usize,
    pub bytes: u64,
    /// Local bytes still over budget because nothing else was safe to delete.
    pub over_budget: u64,
}

/// Deletes the oldest local chunk files until local chunks fit in
/// `budget_bytes`, touching only chunks that are safe to lose locally:
///
/// * already uploaded (they become [`tier::REMOTE`]);
/// * `Never` streams, which are local-only by choice;
/// * `Events` streams with no event nearby, once older than
///   [`EVENT_GRACE_MS`].
///
/// A chunk still waiting to upload is never deleted. Index rows stay, with
/// their tier updated. `policy` gives each (camera, stream)'s policy; pass
/// `Never` for everything when uploads are off.
pub fn evict(
    index: &Index,
    root: &Path,
    budget_bytes: u64,
    policy: impl Fn(&str, &str) -> Policy,
    now_ms: i64,
) -> Result<Evicted> {
    let mut local = index.local_bytes()?;
    let mut out = Evicted::default();
    let mut after = None;
    while local > budget_bytes {
        let page = index.local_chunks_oldest_first(after, 256)?;
        let Some(last) = page.last() else { break };
        after = Some((last.start_ms, last.id));
        for c in page {
            if local <= budget_bytes {
                break;
            }
            let new_tier = if c.uploaded {
                tier::REMOTE
            } else {
                match policy(&c.camera, &c.stream) {
                    Policy::Always => continue, // waiting to upload
                    Policy::Never => tier::DISCARDED,
                    Policy::Events => {
                        if c.end_ms > now_ms - EVENT_GRACE_MS
                            || index.chunk_has_event(&c, EVENT_PAD_MS)?
                        {
                            continue; // may still upload
                        }
                        tier::DISCARDED
                    }
                }
            };
            match std::fs::remove_file(root.join(&c.object_key)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("deleting {}", c.object_key)),
            }
            index.set_tier(c.id, new_tier)?;
            local = local.saturating_sub(c.bytes);
            out.chunks += 1;
            out.bytes += c.bytes;
        }
    }
    out.over_budget = local.saturating_sub(budget_bytes);
    Ok(out)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gop_index::GopEntry;
    use crate::index::ChunkRecord;
    use argus_media::VideoConfig;
    use object_store::memory::InMemory;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const T0: i64 = 1_800_000_000_000;
    const MIN: i64 = 60_000;

    /// A store root with an index; chunks are 1 minute long and `size` bytes.
    struct Fixture {
        dir: tempfile::TempDir,
        index: Arc<Mutex<Index>>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let index = Arc::new(Mutex::new(
                Index::open(&dir.path().join("index.sqlite")).unwrap(),
            ));
            Self { dir, index }
        }

        fn reopen(&mut self) {
            self.index = Arc::new(Mutex::new(
                Index::open(&self.dir.path().join("index.sqlite")).unwrap(),
            ));
        }

        fn chunk(&self, camera: &str, stream: &str, minute: i64, size: usize) -> String {
            let cfg = VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360).unwrap();
            let init_key = format!("init/{camera}/{stream}/1.mp4");
            let key = format!("rec/{camera}/{stream}/{minute}.m4s");
            for (k, n) in [(&init_key, 10), (&key, size)] {
                let p = self.dir.path().join(k);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, vec![b'x'; n]).unwrap();
            }
            let index = self.index.lock().unwrap();
            let init_id = index
                .upsert_init(camera, stream, 1, &init_key, &cfg, None)
                .unwrap();
            index
                .insert_chunk(&ChunkRecord {
                    camera: camera.into(),
                    stream: stream.into(),
                    start_ms: T0 + minute * MIN,
                    end_ms: T0 + (minute + 1) * MIN,
                    object_key: key.clone(),
                    bytes: size as u64,
                    init_id,
                    gops: vec![GopEntry {
                        start_offset_ms: 0,
                        byte_offset: 0,
                        byte_len: size as u32,
                        frame_count: 1,
                    }],
                })
                .unwrap();
            key
        }

        fn uploader<T: UploadTarget>(&self, target: T) -> Uploader<T> {
            let streams = ["main", "sub", "local"]
                .iter()
                .map(|s| StreamPolicy {
                    camera: "door".into(),
                    stream: s.to_string(),
                    policy: match *s {
                        "local" => Policy::Never,
                        s => Policy::default_for(s),
                    },
                })
                .collect();
            Uploader::new(
                target,
                self.index.clone(),
                self.dir.path(),
                "/nvr/",
                streams,
                0.0,
            )
        }

        fn tiers(&self) -> Vec<(String, String)> {
            let index = self.index.lock().unwrap();
            let mut st = index
                .conn
                .prepare("SELECT object_key, tier FROM chunk ORDER BY start_ms, stream")
                .unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        }
    }

    async fn keys(store: &Arc<dyn ObjectStore>) -> Vec<String> {
        use futures::TryStreamExt;
        let mut k: Vec<String> = store
            .list(None)
            .map_ok(|m| m.location.to_string())
            .try_collect()
            .await
            .unwrap();
        k.sort();
        k
    }

    #[tokio::test]
    async fn policy_decides_what_uploads() {
        let f = Fixture::new();
        for m in 0..3 {
            f.chunk("door", "sub", m, 100);
            f.chunk("door", "main", m, 1000);
            f.chunk("door", "local", m, 50);
        }
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut up = f.uploader(store.clone());
        assert_eq!(up.upload_pending().await.unwrap(), 3); // the sub chunks
        assert_eq!(
            keys(&store).await,
            [
                "nvr/init/door/sub/1.mp4",
                "nvr/rec/door/sub/0.m4s",
                "nvr/rec/door/sub/1.m4s",
                "nvr/rec/door/sub/2.m4s",
            ]
        );
        // An event from 30 s into minute 1 to 5 s into minute 2. Main
        // minutes 1 and 2 overlap it; minute 0 ends 30 s before it, beyond
        // the 10 s padding.
        f.index
            .lock()
            .unwrap()
            .add_event(
                "door",
                T0 + MIN + 30_000,
                T0 + 2 * MIN + 5_000,
                "test",
                "person",
            )
            .unwrap();
        assert_eq!(up.upload_pending().await.unwrap(), 2);
        let k = keys(&store).await;
        assert!(k.contains(&"nvr/rec/door/main/1.m4s".to_string()), "{k:?}");
        assert!(k.contains(&"nvr/rec/door/main/2.m4s".to_string()), "{k:?}");
        assert!(!k.contains(&"nvr/rec/door/main/0.m4s".to_string()), "{k:?}");
        assert!(!k.iter().any(|k| k.contains("/local/")), "{k:?}");
        // Nothing left to do; uploads are recorded in the index.
        assert_eq!(up.upload_pending().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn event_padding_reaches_neighbouring_chunks() {
        let f = Fixture::new();
        for m in 0..4 {
            f.chunk("door", "main", m, 10);
        }
        // Event from 5 s before minute 2 to 5 s after it starts: padded by
        // 10 s it reaches back into minute 1 and not into minute 3.
        f.index
            .lock()
            .unwrap()
            .add_event(
                "door",
                T0 + 2 * MIN - 5_000,
                T0 + 2 * MIN + 5_000,
                "test",
                "x",
            )
            .unwrap();
        // Another camera's event doesn't count.
        f.index
            .lock()
            .unwrap()
            .add_event("yard", T0, T0 + 4 * MIN, "test", "x")
            .unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        f.uploader(store.clone()).upload_pending().await.unwrap();
        let rec: Vec<_> = keys(&store)
            .await
            .into_iter()
            .filter(|k| k.contains("/rec/"))
            .collect();
        assert_eq!(rec, ["nvr/rec/door/main/1.m4s", "nvr/rec/door/main/2.m4s"]);
    }

    /// Fails the first `fail` puts, then delegates; counts every attempt.
    struct Flaky {
        inner: Arc<dyn ObjectStore>,
        fail: AtomicUsize,
        attempts: AtomicUsize,
    }

    impl UploadTarget for Arc<Flaky> {
        fn put<'a>(&'a self, key: &'a str, data: Bytes) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                let left = self.fail.load(Ordering::SeqCst);
                if left > 0 {
                    self.fail.store(left - 1, Ordering::SeqCst);
                    bail!("simulated outage");
                }
                UploadTarget::put(&self.inner, key, data).await
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_with_capped_backoff() {
        let f = Fixture::new();
        f.chunk("door", "sub", 0, 10);
        let flaky = Arc::new(Flaky {
            inner: Arc::new(InMemory::new()),
            fail: AtomicUsize::new(6),
            attempts: AtomicUsize::new(0),
        });
        let mut up = f.uploader(flaky.clone());
        up.max_backoff = Duration::from_secs(8);
        let start = Instant::now();
        assert_eq!(up.upload_pending().await.unwrap(), 1);
        // Init: 6 failures (1+2+4+8+8+8 s of backoff) then success; chunk: 1.
        assert_eq!(flaky.attempts.load(Ordering::SeqCst), 8);
        assert_eq!(start.elapsed(), Duration::from_secs(31));
    }

    #[tokio::test]
    async fn resumes_after_restart_from_the_index() {
        let mut f = Fixture::new();
        for m in 0..5 {
            f.chunk("door", "sub", m, 10);
        }
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        // First run: uploads the init segment and 2 chunks, then "crashes".
        {
            let up = f.uploader(inner.clone());
            let batch = up.next_batch(2).unwrap();
            let mut up = up;
            for c in batch {
                up.upload_chunk(&c).await.unwrap();
            }
        }
        f.reopen();
        let counting = Arc::new(Flaky {
            inner: inner.clone(),
            fail: AtomicUsize::new(0),
            attempts: AtomicUsize::new(0),
        });
        let mut up = f.uploader(counting.clone());
        assert_eq!(up.upload_pending().await.unwrap(), 3);
        // Exactly the 3 remaining chunks; neither the init segment nor the
        // first two chunks are sent again.
        assert_eq!(counting.attempts.load(Ordering::SeqCst), 3);
        assert_eq!(keys(&inner).await.len(), 6);
    }

    #[tokio::test]
    async fn eviction_never_deletes_pending_uploads() {
        let f = Fixture::new();
        let now = T0 + 120 * MIN;
        // Old enough for the events grace period: minutes 0-2.
        let sub_uploaded = f.chunk("door", "sub", 0, 100);
        let sub_pending = f.chunk("door", "sub", 1, 100);
        let main_no_event = f.chunk("door", "main", 0, 100);
        let main_event = f.chunk("door", "main", 1, 100);
        let local_only = f.chunk("door", "local", 2, 100);
        // Recent: inside the grace period.
        let main_recent = f.chunk("door", "main", 100, 100);
        {
            let index = f.index.lock().unwrap();
            index
                .add_event("door", T0 + MIN + 30_000, T0 + MIN + 31_000, "test", "x")
                .unwrap();
            let c = index.pending_uploads("door", "sub", None, 1).unwrap();
            index.mark_chunk_uploaded(c[0].id, "x", now).unwrap();
        }
        let policy = |_: &str, s: &str| match s {
            "local" => Policy::Never,
            s => Policy::default_for(s),
        };
        // Budget 0: evict everything that is safe.
        let index = f.index.lock().unwrap();
        let ev = evict(&index, f.dir.path(), 0, policy, now).unwrap();
        drop(index);
        assert_eq!(ev.chunks, 3);
        assert_eq!(ev.over_budget, 300);
        let exists = |k: &str| f.dir.path().join(k).exists();
        assert!(!exists(&sub_uploaded) && !exists(&main_no_event) && !exists(&local_only));
        assert!(exists(&sub_pending) && exists(&main_event) && exists(&main_recent));
        let tiers = f.tiers();
        let tier_of = |k: &str| tiers.iter().find(|(key, _)| key == k).unwrap().1.clone();
        assert_eq!(tier_of(&sub_uploaded), tier::REMOTE);
        assert_eq!(tier_of(&main_no_event), tier::DISCARDED);
        assert_eq!(tier_of(&local_only), tier::DISCARDED);
        assert_eq!(tier_of(&sub_pending), tier::LOCAL);
        // Rows are kept; discarded ones no longer show up for playback.
        assert_eq!(tiers.len(), 6);
        let index = f.index.lock().unwrap();
        assert!(index.seek("door", "main", T0 + 30_000).unwrap().is_none());
        assert!(index.seek("door", "sub", T0 + 30_000).unwrap().is_some());
    }

    #[test]
    fn eviction_stops_at_budget_oldest_first() {
        let f = Fixture::new();
        let keys: Vec<_> = (0..5).map(|m| f.chunk("door", "local", m, 100)).collect();
        let index = f.index.lock().unwrap();
        let ev = evict(&index, f.dir.path(), 250, |_, _| Policy::Never, T0).unwrap();
        assert_eq!((ev.chunks, ev.bytes, ev.over_budget), (3, 300, 0));
        let left: Vec<_> = keys.iter().map(|k| f.dir.path().join(k).exists()).collect();
        assert_eq!(left, [false, false, false, true, true]);
        assert_eq!(index.local_bytes().unwrap(), 200);
    }

    #[tokio::test(start_paused = true)]
    async fn token_bucket_limits_average_rate() {
        // 8 Mbit/s = 1 MB/s, starting with a 2 MB burst allowance.
        let mut tb = TokenBucket::new(8.0);
        let start = Instant::now();
        tb.acquire(2_000_000).await; // the burst: no wait
        assert_eq!(start.elapsed(), Duration::ZERO);
        tb.acquire(1_000_000).await; // 1 MB over: 1 s
        tb.acquire(3_000_000).await; // 3 s more
        assert_eq!(start.elapsed(), Duration::from_secs(4));
        // Idle time refills, but only up to the burst size.
        tokio::time::sleep(Duration::from_secs(60)).await;
        let t = Instant::now();
        tb.acquire(4_000_000).await;
        assert_eq!(t.elapsed(), Duration::from_secs(2));
        // 0 means unlimited.
        let t = Instant::now();
        TokenBucket::new(0.0).acquire(u64::MAX).await;
        assert_eq!(t.elapsed(), Duration::ZERO);
    }

    #[test]
    fn policy_parsing_and_defaults() {
        assert_eq!("events".parse::<Policy>().unwrap(), Policy::Events);
        assert!("sometimes".parse::<Policy>().is_err());
        assert_eq!(Policy::default_for("main"), Policy::Events);
        assert_eq!(Policy::default_for("sub"), Policy::Always);
    }
}
