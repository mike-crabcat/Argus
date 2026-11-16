//! HTTP API and live frame stream for the browser viewer.
//!
//! * `GET /api/cameras`: cameras, their streams, codec strings and sizes.
//! * `GET /api/live/{camera}/{stream}`: WebSocket. The server sends a text
//!   message `{"type":"config", ...}` (codec string, size, and the
//!   `avcC`/`hvcC` record as base64 for WebCodecs' `description`), then
//!   binary frames, starting with a keyframe:
//!
//!   ```text
//!   0      u8   kind: 1 = video access unit
//!   1      u8   flags: bit 0 keyframe, bit 1 replay (cached GOP, sent so
//!               decoding can start at once; display only the last one)
//!   2..10  i64  pts, 90 kHz, big endian
//!   10..18 i64  wall-clock time, µs since the Unix epoch, big endian
//!   18..        the access unit: 4-byte length-prefixed NAL units
//!   ```
//!
//!   A `{"type":"config"}` again means new parameters (reconfigure before the
//!   next frame); `{"type":"discontinuity"}` means the camera reconnected.
//!
//! Auth: with a token configured, every request needs
//! `Authorization: Bearer <token>`, or `?token=<token>` (browsers can't set
//! headers on WebSockets).
//!
//! Each viewer has a bounded queue. If it fills (slow network, background
//! tab), frames are dropped until the next keyframe, so the viewer resumes
//! cleanly and never holds up recording or other viewers.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/cameras", get(cameras))
        .route("/api/live/{camera}/{stream}", get(live_ws))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state)
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
    },
}

fn config_message(s: &LiveStream, v: &argus_media::VideoConfig) -> String {
    json!({
        "type": "config",
        "camera": s.camera,
        "stream": s.stream,
        "codec": v.rfc6381,
        "width": v.width,
        "height": v.height,
        "description": base64(v.decoder_record()),
    })
    .to_string()
}

pub fn encode_frame(f: &argus_media::Frame, replay: bool) -> Bytes {
    let mut b = BytesMut::with_capacity(18 + f.data.len());
    b.put_u8(1);
    b.put_u8(u8::from(f.keyframe) | (u8::from(replay) << 1));
    b.put_i64(f.pts_90k);
    b.put_i64(f.wall_us);
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
                    Out::Frame { frame, replay, received } => {
                        let r = sink.send(Message::Binary(encode_frame(&frame, replay))).await;
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
                let _ = q.try_send(Out::Text(json!({"type": "discontinuity"}).to_string()));
            }
            StreamEvent::AudioConfig(_) | StreamEvent::Audio(_) => {}
        }
    }
}

/// Standard base64 with padding.
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Keeps viewers' sockets from idling out behind proxies.
pub const PING_EVERY: Duration = Duration::from_secs(20);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn frame_header_layout() {
        let f = argus_media::Frame {
            pts_90k: 0x0102,
            wall_us: -1,
            keyframe: true,
            data: Bytes::from_static(&[9, 9]),
        };
        let b = encode_frame(&f, true);
        assert_eq!(b[0], 1);
        assert_eq!(b[1], 0b11);
        assert_eq!(&b[2..10], &0x0102i64.to_be_bytes());
        assert_eq!(&b[10..18], &[0xff; 8]);
        assert_eq!(&b[18..], &[9, 9]);
    }

    #[test]
    fn token_compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
