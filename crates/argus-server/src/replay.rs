//! Synced replay of recorded footage for several cameras.
//!
//! One WebSocket carries every camera. The server keeps a master clock (the
//! playback position, advancing in real time while playing) and sends each
//! camera's recorded frames up to [`LOOKAHEAD_MS`] ahead of it. The client
//! decodes ahead and shows, for every camera, the newest frame whose wall
//! time is not after its own copy of the master clock, so all cameras show
//! the same instant. Recorded decode times are wall-clock time, so frames
//! from different cameras line up without any per-camera offset.
//!
//! A seek starts each camera at the GOP containing the target time, so the
//! decoder can begin at a keyframe; frames before the target are decoded
//! but immediately superseded on screen.

use std::collections::VecDeque;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use argus_media::fmp4::{self, Sample};
use argus_proto::{ReplayHeader, ServerText};
use argus_store::{Index, ReplayChunk};
use bytes::{Bytes, BytesMut};
use object_store::{ObjectStore, ObjectStoreExt};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// How far ahead of the master clock frames are sent. Enough to hide a
/// GOP read and network jitter; small enough that the browser isn't asked
/// to hold many decoded frames.
pub const LOOKAHEAD_MS: i64 = 800;

/// The playback position.
#[derive(Clone, Copy, Debug)]
pub struct Master {
    base_ms: i64,
    since: Instant,
    playing: bool,
}

impl Master {
    pub fn new(t_ms: i64, playing: bool) -> Self {
        Self {
            base_ms: t_ms,
            since: Instant::now(),
            playing,
        }
    }

    pub fn now_ms(&self) -> i64 {
        if self.playing {
            self.base_ms + self.since.elapsed().as_millis() as i64
        } else {
            self.base_ms
        }
    }

    pub fn seek(&mut self, t_ms: i64) {
        *self = Self::new(t_ms, self.playing);
    }

    pub fn set_playing(&mut self, playing: bool) {
        if playing != self.playing {
            *self = Self::new(self.now_ms(), playing);
        }
    }

    pub fn playing(&self) -> bool {
        self.playing
    }
}

/// What a cursor hands out: a frame to send, preceded by a codec config
/// when the init segment changes.
pub enum Item {
    Config(ServerText),
    Frame(Bytes),
}

/// Where recorded bytes come from: the local file when it's there, else
/// object storage (chunks evicted after upload, or files lost locally).
#[derive(Clone)]
pub struct Media {
    pub root: Arc<PathBuf>,
    pub remote: Option<Arc<dyn ObjectStore>>,
}

impl Media {
    /// `len` bytes at `offset` of `key` (all of it with `None`), falling
    /// back to `remote_key` in object storage if the local file is gone.
    pub async fn read(
        &self,
        key: &str,
        remote_key: Option<&str>,
        range: Option<(u64, usize)>,
    ) -> Result<Bytes> {
        let path = self.root.join(key);
        let local = match range {
            Some((offset, len)) => read_range(&path, offset, len).await,
            None => tokio::fs::read(&path).await.map(Bytes::from),
        };
        match local {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (Some(store), Some(rk)) = (&self.remote, remote_key) else {
                    return Err(e).with_context(|| format!("{key} is not on local disk"));
                };
                let loc = object_store::path::Path::from(rk);
                let bytes = match range {
                    Some((offset, len)) => store.get_range(&loc, offset..offset + len as u64).await,
                    None => match store.get(&loc).await {
                        Ok(r) => r.bytes().await,
                        Err(e) => Err(e),
                    },
                };
                bytes.with_context(|| format!("reading {rk} from object storage"))
            }
            Err(e) => Err(e).with_context(|| format!("reading {key}")),
        }
    }
}

/// Reads one camera's recordings forward from a time, GOP by GOP.
pub struct Cursor {
    pub index_in_list: u8,
    pub camera: String,
    pub stream: String,
    media: Media,
    index: Arc<Mutex<Index>>,
    chunks: VecDeque<ReplayChunk>,
    /// GOP to read next in the front chunk.
    next_gop: usize,
    frames: VecDeque<Sample>,
    /// Where to look for more chunks (end of the last one queued).
    resume_ms: i64,
    sent_init: Option<String>,
    last_refill: Option<Instant>,
}

impl Cursor {
    pub fn new(
        index_in_list: u8,
        camera: &str,
        stream: &str,
        media: &Media,
        index: Arc<Mutex<Index>>,
        t_ms: i64,
    ) -> Result<Self> {
        let mut c = Self {
            index_in_list,
            camera: camera.to_owned(),
            stream: stream.to_owned(),
            media: media.clone(),
            index,
            chunks: VecDeque::new(),
            next_gop: 0,
            frames: VecDeque::new(),
            resume_ms: t_ms,
            sent_init: None,
            last_refill: None,
        };
        c.refill()?;
        // Start at the GOP containing t (or the first GOP of a later chunk).
        if let Some(chunk) = c.chunks.front().map(|r| &r.chunk) {
            if t_ms > chunk.start_ms {
                let offset = u32::try_from(t_ms - chunk.start_ms).unwrap_or(u32::MAX);
                c.next_gop = argus_store::gop_index::find(&chunk.gops, offset).unwrap_or(0);
            }
        }
        Ok(c)
    }

    fn refill(&mut self) -> Result<()> {
        self.last_refill = Some(Instant::now());
        let more = self.index.lock().unwrap().replay_chunks_from(
            &self.camera,
            &self.stream,
            self.resume_ms,
            4,
        )?;
        for rc in more {
            if self
                .chunks
                .iter()
                .any(|c| c.chunk.object_key == rc.chunk.object_key)
            {
                continue;
            }
            // Only in object storage, and no store configured: skip it.
            if !rc.local && self.media.remote.is_none() {
                self.resume_ms = rc.chunk.end_ms;
                continue;
            }
            self.resume_ms = rc.chunk.end_ms;
            self.chunks.push_back(rc);
        }
        Ok(())
    }

    /// Wall time (ms) of the next frame, loading the next GOP if needed.
    /// `None` when there's nothing more recorded yet.
    pub async fn peek_ms(&mut self) -> Result<Option<i64>> {
        while self.frames.is_empty() {
            if !self.load_gop().await? {
                return Ok(None);
            }
        }
        Ok(self.frames.front().map(|s| (s.dts_90k / 90) as i64))
    }

    /// Reads the next GOP into `frames`. False if there is none yet.
    async fn load_gop(&mut self) -> Result<bool> {
        loop {
            let Some(rc) = self.chunks.front() else {
                // At the end of what's recorded: look again now and then,
                // since recording continues while we play.
                let due = self
                    .last_refill
                    .is_none_or(|t| t.elapsed() > Duration::from_secs(1));
                if !due {
                    return Ok(false);
                }
                self.refill()?;
                if self.chunks.is_empty() {
                    return Ok(false);
                }
                continue;
            };
            let chunk = &rc.chunk;
            let Some(gop) = chunk.gops.get(self.next_gop).copied() else {
                self.chunks.pop_front();
                self.next_gop = 0;
                if self.chunks.is_empty() {
                    self.refill()?;
                }
                continue;
            };
            let (key, remote) = (chunk.object_key.clone(), rc.remote.clone());
            self.next_gop += 1;
            let bytes = self
                .media
                .read(
                    &key,
                    remote.as_deref(),
                    Some((u64::from(gop.byte_offset), gop.byte_len as usize)),
                )
                .await?;
            if let Some(samples) = fmp4::read_video_samples(&bytes) {
                self.frames.extend(samples);
                return Ok(true);
            }
        }
    }

    /// The next frame as a binary message, preceded by a config message if
    /// its init segment differs from the last one sent.
    pub async fn next_items(&mut self) -> Result<Vec<Item>> {
        let mut out = Vec::new();
        let init = self
            .chunks
            .front()
            .map(|c| (c.init_key.clone(), c.init_remote.clone()));
        let Some(sample) = self.frames.pop_front() else {
            return Ok(out);
        };
        if let Some((init, init_remote)) = init {
            if self.sent_init.as_ref() != Some(&init) {
                out.push(Item::Config(
                    self.config(&init, init_remote.as_deref()).await?,
                ));
                self.sent_init = Some(init);
            }
        }
        let h = ReplayHeader {
            camera: self.index_in_list,
            keyframe: sample.keyframe,
            pts_90k: sample.dts_90k as i64,
            wall_us: (sample.dts_90k * 100 / 9) as i64,
        };
        let mut b = BytesMut::with_capacity(argus_proto::REPLAY_HEADER_LEN + sample.data.len());
        b.extend_from_slice(&h.encode());
        b.extend_from_slice(&sample.data);
        out.push(Item::Frame(b.freeze()));
        Ok(out)
    }

    async fn config(&self, init_key: &str, init_remote: Option<&str>) -> Result<ServerText> {
        let init = self.media.read(init_key, init_remote, None).await?;
        let record =
            fmp4::decoder_record_from_init(&init).context("init segment has no avcC/hvcC")?;
        let (codec, width, height) = self
            .index
            .lock()
            .unwrap()
            .init_info(init_key)?
            .context("init segment not in index")?;
        Ok(ServerText::Config {
            camera: self.camera.clone(),
            stream: self.stream.clone(),
            codec,
            width,
            height,
            description: argus_proto::base64_encode(record),
        })
    }
}

async fn read_range(path: &Path, offset: u64, len: usize) -> std::io::Result<Bytes> {
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(SeekFrom::Start(offset)).await?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).await?;
    Ok(Bytes::from(buf))
}

/// HTTP handlers for replay.
pub mod http {
    use std::collections::HashMap;

    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use axum::extract::{Path, Query, State};
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::{IntoResponse, Json, Response};
    use futures::{SinkExt, StreamExt};
    use serde_json::json;
    use tracing::{info, warn};

    use super::*;
    use crate::http::{AppState, Recordings};

    fn recordings_or_404(st: &AppState) -> Result<&Recordings, Box<Response>> {
        st.recordings.as_ref().ok_or_else(|| {
            Box::new((StatusCode::NOT_FOUND, "replay is not enabled").into_response())
        })
    }

    fn camera_list(st: &AppState, q: &HashMap<String, String>) -> Vec<String> {
        let mut all: Vec<String> = st.live.all().map(|s| s.camera.clone()).collect();
        all.dedup();
        match q.get("cameras").filter(|c| !c.is_empty()) {
            Some(list) => list
                .split(',')
                .filter(|c| all.iter().any(|a| a == c))
                .map(str::to_owned)
                .collect(),
            None => all,
        }
    }

    fn int(q: &HashMap<String, String>, k: &str) -> Option<i64> {
        q.get(k).and_then(|v| v.parse().ok())
    }

    pub async fn recordings(
        State(st): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Response {
        let rec = match recordings_or_404(&st) {
            Ok(r) => r,
            Err(e) => return *e,
        };
        let stream = q.get("stream").map_or("sub", String::as_str);
        let now = crate::recorder::now_us() / 1000;
        let from = int(&q, "from").unwrap_or(now - 24 * 3_600_000);
        let to = int(&q, "to").unwrap_or(now + 60_000);
        let index = rec.index.lock().unwrap();
        let mut cams = Vec::new();
        for c in camera_list(&st, &q) {
            match index.coverage(&c, stream, from, to, 2_000) {
                Ok(spans) => cams.push(json!({"name": c, "spans": spans})),
                Err(e) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
                }
            }
        }
        Json(json!({"stream": stream, "from": from, "to": to, "now": now, "cameras": cams}))
            .into_response()
    }

    /// `GET /api/events?from=MS&to=MS&cameras=a,b`: events overlapping the
    /// window (default: the last hour), oldest first, at most 2000.
    pub async fn events(
        State(st): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Response {
        let rec = match recordings_or_404(&st) {
            Ok(r) => r,
            Err(e) => return *e,
        };
        let now = crate::recorder::now_us() / 1000;
        let from = int(&q, "from").unwrap_or(now - 3_600_000);
        let to = int(&q, "to").unwrap_or(now + 60_000);
        let stills = crate::stills::StillStore::new(&rec.root);
        let rows = {
            let index = rec.index.lock().unwrap();
            let _ = index.index_new_events();
            index.search_events(&argus_store::summaries::SearchQuery {
                text: String::new(),
                from_ms: Some(from),
                to_ms: Some(to),
                cameras: camera_list(&st, &q),
                limit: 2000,
            })
        };
        let mut out: Vec<serde_json::Value> = match rows {
            Ok(r) => r
                .iter()
                .map(|e| crate::search::event_json(e, &stills))
                .collect(),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
        out.reverse(); // oldest first
        Json(json!({"from": from, "to": to, "now": now, "events": out})).into_response()
    }

    pub async fn seek(
        State(st): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Response {
        let rec = match recordings_or_404(&st) {
            Ok(r) => r,
            Err(e) => return *e,
        };
        let Some(t) = int(&q, "t") else {
            return (
                StatusCode::BAD_REQUEST,
                "t (ms since the epoch) is required",
            )
                .into_response();
        };
        let stream = q.get("stream").map_or("sub", String::as_str);
        let index = rec.index.lock().unwrap();
        let mut out = Vec::new();
        for c in camera_list(&st, &q) {
            let target = match index.seek(&c, stream, t) {
                Ok(t) => t,
                Err(e) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
                }
            };
            out.push(match target {
                Some(s) => json!({
                    "camera": c,
                    "init": s.init_key,
                    "chunk": s.object_key,
                    "gop_start_ms": s.gop_start_ms,
                    "byte_offset": s.byte_offset,
                    "byte_len": s.byte_len,
                }),
                None => json!({"camera": c, "init": null}),
            });
        }
        Json(json!({"t": t, "stream": stream, "cameras": out})).into_response()
    }

    /// Serves an init segment or chunk file, honouring a single
    /// `Range: bytes=a-b` so a GOP can be fetched on its own.
    pub async fn media(
        State(st): State<AppState>,
        Path(key): Path<String>,
        headers: HeaderMap,
    ) -> Response {
        let rec = match recordings_or_404(&st) {
            Ok(r) => r,
            Err(e) => return *e,
        };
        let still = key.starts_with("events/") && key.ends_with(".jpg");
        let safe = (key.starts_with("rec/") || key.starts_with("init/") || still)
            && key
                .split('/')
                .all(|c| !c.is_empty() && c != "." && c != "..");
        if !safe {
            return StatusCode::NOT_FOUND.into_response();
        }
        let media = rec.media();
        let remote = rec.index.lock().unwrap().remote_key_of(&key).ok().flatten();
        // Size: the local file's, else the object's in storage.
        let len = match tokio::fs::metadata(rec.root.join(&key)).await {
            Ok(m) => m.len(),
            Err(_) => match (&rec.remote, &remote) {
                (Some(store), Some(rk)) => {
                    match store
                        .head(&object_store::path::Path::from(rk.as_str()))
                        .await
                    {
                        Ok(meta) => meta.size,
                        Err(_) => return StatusCode::NOT_FOUND.into_response(),
                    }
                }
                _ => return StatusCode::NOT_FOUND.into_response(),
            },
        };
        let ctype = if still {
            "image/jpeg"
        } else if key.ends_with(".mp4") {
            "video/mp4"
        } else {
            "video/iso.segment"
        };
        let range = headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|v| v.split_once('-'))
            .and_then(|(a, b)| {
                let a: u64 = a.parse().ok()?;
                let b: u64 = if b.is_empty() {
                    len.saturating_sub(1)
                } else {
                    b.parse().ok()?
                };
                (a <= b && b < len).then_some((a, b))
            });
        let read = media
            .read(
                &key,
                remote.as_deref(),
                range.map(|(a, b)| (a, (b - a + 1) as usize)),
            )
            .await;
        match (range, read) {
            (Some((a, b)), Ok(body)) => (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, ctype.to_string()),
                    (header::CONTENT_RANGE, format!("bytes {a}-{b}/{len}")),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                ],
                body,
            )
                .into_response(),
            (None, Ok(body)) => (
                [
                    (header::CONTENT_TYPE, ctype.to_string()),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                ],
                body,
            )
                .into_response(),
            (_, Err(e)) => {
                warn!("media {key}: {e:#}");
                StatusCode::BAD_GATEWAY.into_response()
            }
        }
    }

    pub async fn replay_ws(
        State(st): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
        device: Option<axum::Extension<crate::http::DeviceId>>,
        ws: WebSocketUpgrade,
    ) -> Response {
        let revoked = crate::http::until_revoked(&st, device);
        let rec = match recordings_or_404(&st) {
            Ok(r) => r.clone(),
            Err(e) => return *e,
        };
        let cameras = camera_list(&st, &q);
        let stream = q.get("stream").cloned().unwrap_or_else(|| "sub".into());
        let t = int(&q, "t").unwrap_or_else(|| crate::recorder::now_us() / 1000 - 60_000);
        let playing = !q.contains_key("paused");
        ws.on_upgrade(move |socket| async move {
            tokio::select! {
                r = session(socket, rec, cameras, stream, t, playing) => {
                    if let Err(e) = r {
                        warn!("replay session ended: {e:#}");
                    }
                }
                _ = revoked => {}
            }
        })
    }

    fn text(m: &ServerText) -> Message {
        Message::Text(serde_json::to_string(m).expect("serializable").into())
    }

    async fn session(
        socket: WebSocket,
        rec: Recordings,
        cameras: Vec<String>,
        stream: String,
        t: i64,
        playing: bool,
    ) -> Result<()> {
        let (mut sink, mut incoming) = socket.split();
        let mut master = Master::new(t, playing);
        let mut seq = 0u32;
        let media = rec.media();
        // Each camera's stream; one can switch (e.g. to main when focused).
        let mut streams: Vec<String> = vec![stream.clone(); cameras.len()];
        let open = |t: i64, streams: &[String]| -> Result<Vec<Cursor>> {
            cameras
                .iter()
                .enumerate()
                .map(|(i, c)| Cursor::new(i as u8, c, &streams[i], &media, rec.index.clone(), t))
                .collect()
        };
        let mut cursors = open(t, &streams)?;
        sink.send(text(&ServerText::Replay {
            seq,
            t_ms: t,
            playing,
            cameras: cameras.clone(),
        }))
        .await?;
        info!(cameras = cameras.len(), %stream, t, "replay started");
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let mut sent = 0u64;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let target = master.now_ms() + LOOKAHEAD_MS;
                    for c in cursors.iter_mut() {
                        while let Some(ms) = c.peek_ms().await? {
                            if ms > target {
                                break;
                            }
                            for item in c.next_items().await? {
                                match item {
                                    Item::Config(cfg) => sink.send(text(&cfg)).await?,
                                    Item::Frame(b) => {
                                        sink.send(Message::Binary(b)).await?;
                                        sent += 1;
                                    }
                                }
                            }
                        }
                    }
                }
                msg = incoming.next() => match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Text(t))) => {
                        match serde_json::from_str::<argus_proto::ClientText>(&t) {
                            Ok(argus_proto::ClientText::Seek { t_ms, seq: s }) => {
                                seq = s;
                                master.seek(t_ms);
                                cursors = open(t_ms, &streams)?;
                                sink.send(text(&ServerText::Replay {
                                    seq,
                                    t_ms,
                                    playing: master.playing(),
                                    cameras: cameras.clone(),
                                }))
                                .await?;
                            }
                            Ok(argus_proto::ClientText::Stream { camera, stream: want }) => {
                                if let Some(i) = cameras.iter().position(|c| *c == camera) {
                                    // Restart just this camera, on the other
                                    // stream, from where playback is now.
                                    streams[i] = want.clone();
                                    cursors[i] = Cursor::new(
                                        i as u8,
                                        &camera,
                                        &want,
                                        &media,
                                        rec.index.clone(),
                                        master.now_ms(),
                                    )?;
                                    sink.send(text(&ServerText::Stream { camera, stream: want }))
                                        .await?;
                                }
                            }
                            Ok(argus_proto::ClientText::Play) => master.set_playing(true),
                            Ok(argus_proto::ClientText::Pause) => master.set_playing(false),
                            Ok(argus_proto::ClientText::Ping { client_us }) => {
                                let pong = ServerText::Pong {
                                    client_us,
                                    server_us: crate::recorder::now_us(),
                                };
                                sink.send(text(&pong)).await?;
                            }
                            Err(_) => {}
                        }
                    }
                    Some(Ok(_)) => {}
                },
            }
        }
        info!(frames = sent, "replay ended");
        Ok(())
    }
}
