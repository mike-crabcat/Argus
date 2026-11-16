//! HTTP API, live frame stream and the browser app.
//!
//! * `GET /api/cameras`: cameras, their streams, codec strings and sizes.
//! * `GET /api/live/{camera}/{stream}`: WebSocket carrying the
//!   [`argus_proto`] messages: a config text message, then binary frames
//!   starting with a keyframe. The client may send pings to sync clocks.
//! * Anything else: static files from `[http] web_root` (the browser app),
//!   if configured. These need no token; the API does.
//!
//! Auth: with a token configured, every API request needs
//! `Authorization: Bearer <token>`, or `?token=<token>` (browsers can't set
//! headers on WebSockets).
//!
//! Each viewer has a bounded queue. If it fills (slow network, background
//! tab), frames are dropped until the next keyframe, so the viewer resumes
//! cleanly and never holds up recording or other viewers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use argus_proto::{ClientText, FrameHeader, ServerText};
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use bytes::{BufMut, Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info};

use crate::live::{Live, LiveStream};
use crate::recorder::StreamEvent;

/// Frames a viewer may have queued before it starts dropping to the next
/// keyframe: a few seconds of video.
pub const CLIENT_QUEUE: usize = 90;

#[derive(Clone)]
pub struct AppState {
    pub live: Arc<Live>,
    pub token: Option<Arc<str>>,
    /// Directory with the built browser app, served at `/`.
    pub web_root: Option<Arc<PathBuf>>,
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/cameras", get(cameras))
        .route("/api/live/{camera}/{stream}", get(live_ws))
        .layer(middleware::from_fn_with_state(state.clone(), auth));
    api.fallback(static_file).with_state(state)
}

/// Serves the browser app's files. Only plain relative paths inside
/// `web_root` are served.
async fn static_file(State(st): State<AppState>, uri: axum::http::Uri) -> Response {
    let Some(root) = &st.web_root else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let rel = uri.path().trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let safe = rel
        .split('/')
        .all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains('\\'));
    if !safe || rel.starts_with("api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let content_type = match rel.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };
    match tokio::fs::read(root.join(rel)).await {
        Ok(body) => (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Serves until the listener fails or the task is aborted.
pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

async fn auth(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(token) = &st.token else {
        return next.run(req).await;
    };
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let given = bearer.or(q.get("token").map(String::as_str));
    if given.is_some_and(|g| constant_time_eq(g.as_bytes(), token.as_bytes())) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or wrong token").into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn cameras(State(st): State<AppState>) -> Json<serde_json::Value> {
    let mut cams: Vec<(String, Vec<serde_json::Value>)> = Vec::new();
    for s in st.live.all() {
        let video = s.video_config();
        let info = json!({
            "name": s.stream,
            "live": video.is_some(),
            "codec": video.as_ref().map(|v| v.rfc6381.clone()),
            "width": video.as_ref().map(|v| v.width),
            "height": video.as_ref().map(|v| v.height),
            "audio": s.audio_config().map(|a| a.rfc6381),
        });
        match cams.last_mut() {
            Some((name, streams)) if *name == s.camera => streams.push(info),
            _ => cams.push((s.camera.clone(), vec![info])),
        }
    }
    Json(json!({
        "cameras": cams
            .into_iter()
            .map(|(name, streams)| json!({"name": name, "streams": streams}))
            .collect::<Vec<_>>()
    }))
}

async fn live_ws(
    State(st): State<AppState>,
    Path((camera, stream)): Path<(String, String)>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(s) = st.live.get(&camera, &stream) else {
        return (StatusCode::NOT_FOUND, "no such camera stream").into_response();
    };
    ws.on_upgrade(move |socket| viewer(socket, s))
}

enum Out {
    Text(String),
    Frame {
        frame: argus_media::Frame,
        replay: bool,
        received: Instant,
        /// `received` on the wall clock, for the client's latency figure.
        recv_us: i64,
    },
}

fn text(msg: &ServerText) -> String {
    serde_json::to_string(msg).expect("serializable")
}

fn config_message(s: &LiveStream, v: &argus_media::VideoConfig) -> String {
    text(&ServerText::Config {
        camera: s.camera.clone(),
        stream: s.stream.clone(),
        codec: v.rfc6381.clone(),
        width: v.width,
        height: v.height,
        description: argus_proto::base64_encode(v.decoder_record()),
    })
}

pub fn encode_frame(f: &argus_media::Frame, replay: bool, recv_us: i64) -> Bytes {
    let h = FrameHeader {
        keyframe: f.keyframe,
        replay,
        pts_90k: f.pts_90k,
        wall_us: f.wall_us,
        recv_us,
    };
    let mut b = BytesMut::with_capacity(argus_proto::HEADER_LEN + f.data.len());
    b.put_slice(&h.encode());
    b.put_slice(&f.data);
    b.freeze()
}

/// Per-viewer counters, logged when the viewer leaves.
#[derive(Default)]
struct Stats {
    sent: u64,
    dropped: u64,
    /// Server-side latency, frame received from the camera to written to
    /// the socket, in µs (live frames only).
    latency_us: Vec<u32>,
}

impl Stats {
    fn percentile(&self, p: f64) -> u32 {
        let mut v = self.latency_us.clone();
        v.sort_unstable();
        v.get(((v.len() as f64 - 1.0) * p).round() as usize)
            .copied()
            .unwrap_or(0)
    }
}

async fn viewer(socket: WebSocket, s: Arc<LiveStream>) {
    let snap = s.snapshot();
    let (q_tx, mut q_rx) = mpsc::channel::<Out>(CLIENT_QUEUE);
    let dropped = Arc::new(AtomicU64::new(0));
    let feeder = tokio::spawn(feed(s.clone(), snap, q_tx, dropped.clone()));
    let (mut sink, mut incoming) = socket.split();
    let mut stats = Stats::default();
    let started = Instant::now();
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if sink.send(Message::Ping(Bytes::new())).await.is_err() {
                    break;
                }
            }
            out = q_rx.recv() => {
                let Some(out) = out else { break };
                let res = match out {
                    Out::Text(t) => sink.send(Message::Text(t.into())).await,
                    Out::Frame { frame, replay, received, recv_us } => {
                        let r = sink
                            .send(Message::Binary(encode_frame(&frame, replay, recv_us)))
                            .await;
                        if !replay && stats.latency_us.len() < 1_000_000 {
                            stats.latency_us.push(received.elapsed().as_micros() as u32);
                        }
                        stats.sent += 1;
                        r
                    }
                };
                if res.is_err() {
                    break;
                }
            }
            msg = incoming.next() => match msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(Message::Text(t))) => {
                    if let Ok(ClientText::Ping { client_us }) = serde_json::from_str(&t) {
                        let pong = ServerText::Pong { client_us, server_us: crate::recorder::now_us() };
                        if sink.send(Message::Text(text(&pong).into())).await.is_err() {
                            break;
                        }
                    }
                }
                Some(Ok(_)) => {}
            },
        }
    }
    feeder.abort();
    stats.dropped = dropped.load(Ordering::Relaxed);
    info!(camera = %s.camera, stream = %s.stream,
          secs = started.elapsed().as_secs(), sent = stats.sent, dropped = stats.dropped,
          latency_p50_us = stats.percentile(0.5), latency_p99_us = stats.percentile(0.99),
          latency_max_us = stats.percentile(1.0), "viewer left");
}

/// Moves events from the stream into a viewer's queue, dropping to the next
/// keyframe whenever the queue is full, counting drops in `dropped`.
async fn feed(
    s: Arc<LiveStream>,
    snap: crate::live::Snapshot,
    q: mpsc::Sender<Out>,
    dropped: Arc<AtomicU64>,
) {
    let drop_n = |n: u64| {
        dropped.fetch_add(n, Ordering::Relaxed);
    };
    let mut rx = snap.rx;
    // Frames up to here were in the snapshot; skip them if the live
    // subscription repeats any (it shouldn't, but pts is cheap to check).
    let mut replayed_until = snap.gop.last().map(|f| f.pts_90k);
    let mut need_key = true;
    let mut pending_config = None;
    if let Some(v) = &snap.video {
        if q.send(Out::Text(config_message(&s, v))).await.is_err() {
            return;
        }
    }
    let now = Instant::now();
    for frame in snap.gop {
        match q.try_send(Out::Frame {
            frame,
            replay: true,
            received: now,
            recv_us: 0,
        }) {
            Ok(()) => need_key = false,
            Err(mpsc::error::TrySendError::Full(_)) => {
                need_key = true;
                break;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
    if need_key {
        replayed_until = None;
    }
    loop {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                drop_n(n);
                need_key = true;
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let received = Instant::now();
        let recv_us = crate::recorder::now_us();
        match ev {
            StreamEvent::Config(c) => {
                pending_config = Some(config_message(&s, &c));
                need_key = true;
            }
            StreamEvent::Frame(f) => {
                if replayed_until.is_some_and(|p| f.pts_90k <= p) {
                    continue;
                }
                replayed_until = None;
                if need_key && !f.keyframe {
                    drop_n(1);
                    continue;
                }
                if need_key {
                    if let Some(c) = pending_config.take() {
                        if q.send(Out::Text(c)).await.is_err() {
                            return;
                        }
                    }
                }
                match q.try_send(Out::Frame {
                    frame: f,
                    replay: false,
                    received,
                    recv_us,
                }) {
                    Ok(()) => need_key = false,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        drop_n(1);
                        if !need_key {
                            debug!(camera = %s.camera, stream = %s.stream,
                                   "viewer is slow; skipping to the next keyframe");
                        }
                        need_key = true;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
            StreamEvent::Discontinuity => {
                need_key = true;
                replayed_until = None;
                let _ = q.try_send(Out::Text(text(&ServerText::Discontinuity)));
            }
            StreamEvent::AudioConfig(_) | StreamEvent::Audio(_) => {}
        }
    }
}

/// Keeps viewers' sockets from idling out behind proxies.
pub const PING_EVERY: Duration = Duration::from_secs(20);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
