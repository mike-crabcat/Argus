//! HTTP API, live frame stream and the browser app.
//!
//! * `GET /api/cameras`: cameras, their streams, codec strings and sizes.
//! * `GET /api/live/{camera}/{stream}`: WebSocket carrying the
//!   [`argus_proto`] messages: a config text message, then binary frames
//!   starting with a keyframe. The client may send pings to sync clocks.
//! * `GET /api/recordings?stream=sub&from=MS&to=MS`: recorded spans per
//!   camera, for a timeline.
//! * `GET /api/events?from=MS&to=MS&cameras=a,b`: events (camera, start,
//!   end, source, label), for the map.
//! * `GET /api/events/search?q=...&from=MS&to=MS&cameras=a,b&smart=1`:
//!   events by summary, label and camera, with their stills
//!   (`/api/media/events/{id}/{n}.jpg`).
//! * `GET /api/seek?t=MS&stream=sub&cameras=a,b`: for each camera, the init
//!   segment and the GOP containing `t` (chunk key and byte range).
//! * `GET /api/media/{key}`: an init segment or chunk file, with `Range`.
//! * `GET /api/replay?t=MS&stream=sub&cameras=a,b`: WebSocket, synced replay
//!   of several cameras (see [`crate::replay`]).
//! * `GET /api/info`: who the caller is (`admin` or `device`) and the
//!   configured `public_url`, for the viewer.
//! * `GET /api/devices`, `POST /api/devices` (`{"name": ...}`),
//!   `DELETE /api/devices/{id}`: phones added by QR code (main token only).
//! * `GET /api/plan`, `PUT /api/plan`, `POST /api/plan/commands`,
//!   `POST /api/plan/assets`, `GET /api/plan/assets/{id}`: the floor plan
//!   (see [`crate::plan`] and docs/plan-commands.md).
//! * `POST /api/plan/import` (`{asset, level}`), `POST /api/plan/ask`
//!   (`{text, level}`): the agent drafts plan commands (main token only;
//!   nothing is saved until the editor sends them).
//! * `/api/push/...`, `/api/alerts/...`, `/api/brief`: notifications,
//!   alert rules and the daily brief (see [`crate::alerts_api`]).
//! * `GET /api/agent/status`: whether a model is set up; spending for the
//!   main token.
//! * Anything else: static files from `[http] web_root` (the browser app),
//!   if configured. These need no token; the API does.
//!
//! Auth: with a token configured, every API request needs
//! `Authorization: Bearer <token>`, or `?token=<token>` (browsers can't set
//! headers on WebSockets). The main token is the admin; a device token
//! (see [`crate::devices`]) can view everything but not manage devices.
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
    /// Recordings, for replay. None disables the replay routes.
    pub recordings: Option<Recordings>,
    /// Per-device tokens. None: only the main token works.
    pub devices: Option<Arc<crate::devices::Devices>>,
    /// The address phones reach this server at (`[http] public_url`).
    pub public_url: Option<Arc<str>>,
    /// The floor plan. None disables the plan routes.
    pub plan: Option<Arc<crate::plan::PlanStore>>,
    /// The model, if `[agent]` is configured.
    pub agent: Option<Arc<crate::agent::AgentHandle>>,
    /// Notifications, alert rules and briefs.
    pub alerts: Option<Arc<crate::alerts_api::AlertsState>>,
}

/// Who made a request, set by the auth middleware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The main token (or no token configured).
    Admin,
    /// A device token: viewing only.
    Device,
}

/// The device a request came from (device tokens only).
#[derive(Clone, Debug)]
pub struct DeviceId(pub String);

/// Resolves when the caller's device is revoked; never for the main token.
/// WebSocket handlers race their session against it.
pub fn until_revoked(
    st: &AppState,
    device: Option<axum::Extension<DeviceId>>,
) -> impl std::future::Future<Output = ()> + Send + 'static {
    let watch = device.and_then(|d| st.devices.clone().map(|s| (s, d.0.0)));
    async move {
        match watch {
            Some((devices, id)) => {
                devices.revoked(&id).await;
                info!(%id, "closing a revoked device's stream");
            }
            None => std::future::pending().await,
        }
    }
}

#[derive(Clone)]
pub struct Recordings {
    pub index: Arc<std::sync::Mutex<argus_store::Index>>,
    pub root: Arc<PathBuf>,
    /// Object storage, for chunks no longer on local disk.
    pub remote: Option<Arc<dyn object_store::ObjectStore>>,
}

impl Recordings {
    pub fn media(&self) -> crate::replay::Media {
        crate::replay::Media {
            root: self.root.clone(),
            remote: self.remote.clone(),
        }
    }
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/cameras", get(cameras))
        .route("/api/live/{camera}/{stream}", get(live_ws))
        .route("/api/recordings", get(crate::replay::http::recordings))
        .route("/api/seek", get(crate::replay::http::seek))
        .route("/api/events", get(crate::replay::http::events))
        .route("/api/events/search", get(crate::search::search))
        .route("/api/media/{*key}", get(crate::replay::http::media))
        .route("/api/replay", get(crate::replay::http::replay_ws))
        .route("/api/info", get(info))
        .route("/api/devices", get(list_devices).post(create_device))
        .route("/api/devices/{id}", axum::routing::delete(revoke_device))
        .route(
            "/api/plan",
            get(crate::plan::get_plan).put(crate::plan::put_plan),
        )
        .route(
            "/api/plan/commands",
            axum::routing::post(crate::plan::post_commands),
        )
        .route(
            "/api/plan/assets",
            axum::routing::post(crate::plan::post_asset).layer(
                axum::extract::DefaultBodyLimit::max(crate::plan::MAX_ASSET_BYTES + 1024),
            ),
        )
        .route("/api/plan/assets/{id}", get(crate::plan::get_asset))
        .route("/api/agent/status", get(crate::agent::status))
        .route("/api/push/key", get(crate::alerts_api::push_key))
        .route(
            "/api/push/subscribe",
            axum::routing::post(crate::alerts_api::subscribe)
                .delete(crate::alerts_api::unsubscribe),
        )
        .route(
            "/api/push/test",
            axum::routing::post(crate::alerts_api::push_test),
        )
        .route(
            "/api/alerts/rules",
            get(crate::alerts_api::list_rules).post(crate::alerts_api::add_rule),
        )
        .route(
            "/api/alerts/rules/{id}",
            axum::routing::put(crate::alerts_api::edit_rule).delete(crate::alerts_api::delete_rule),
        )
        .route(
            "/api/alerts/rules/{id}/test",
            axum::routing::post(crate::alerts_api::test_rule),
        )
        .route("/api/alerts/log", get(crate::alerts_api::alert_log))
        .route("/api/brief", get(crate::alerts_api::get_brief))
        .route(
            "/api/brief/run",
            axum::routing::post(crate::alerts_api::run_brief),
        )
        .route(
            "/api/plan/import",
            axum::routing::post(crate::plan_agent::import),
        )
        .route("/api/plan/ask", axum::routing::post(crate::plan_agent::ask))
        .layer(middleware::from_fn_with_state(state.clone(), auth));
    api.route("/healthz", get(healthz))
        .fallback(static_file)
        .with_state(state)
}

/// The build this server runs, from the `ARGUS_GIT_SHA` build variable.
pub const VERSION: &str = match option_env!("ARGUS_GIT_SHA") {
    Some(v) => v,
    None => "dev",
};

/// Liveness for Docker and deploy scripts: no token, no camera details.
async fn healthz() -> String {
    format!("ok {VERSION}\n")
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
        Some("webmanifest") => "application/manifest+json",
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
    mut req: Request,
    next: Next,
) -> Response {
    let role = match &st.token {
        None => Some(Role::Admin),
        Some(token) => {
            let bearer = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));
            match bearer.or(q.get("token").map(String::as_str)) {
                Some(g) if constant_time_eq(g.as_bytes(), token.as_bytes()) => Some(Role::Admin),
                Some(g) => match st.devices.as_ref().and_then(|d| d.verify(g, now_ms())) {
                    Some(id) => {
                        req.extensions_mut().insert(DeviceId(id));
                        Some(Role::Device)
                    }
                    None => None,
                },
                None => None,
            }
        }
    };
    match role {
        Some(role) => {
            req.extensions_mut().insert(role);
            next.run(req).await
        }
        None => (StatusCode::UNAUTHORIZED, "missing or wrong token").into_response(),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The caller's role and the server's public address.
async fn info(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
) -> Json<serde_json::Value> {
    Json(json!({
        "role": match role { Role::Admin => "admin", Role::Device => "device" },
        "public_url": st.public_url.as_deref(),
        "version": VERSION,
    }))
}

/// The device store, for an admin; otherwise the error response.
fn devices_for(
    st: &AppState,
    role: Role,
) -> Result<&Arc<crate::devices::Devices>, (StatusCode, &'static str)> {
    if role != Role::Admin {
        return Err((
            StatusCode::FORBIDDEN,
            "only the main token can manage devices",
        ));
    }
    st.devices
        .as_ref()
        .ok_or((StatusCode::NOT_FOUND, "devices are not enabled"))
}

async fn list_devices(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
) -> Response {
    match devices_for(&st, role) {
        Ok(d) => Json(json!({ "devices": d.list() })).into_response(),
        Err(r) => r.into_response(),
    }
}

#[derive(serde::Deserialize)]
struct NewDevice {
    name: String,
}

/// Adds a device. The response is the only place its token appears.
async fn create_device(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
    Json(body): Json<NewDevice>,
) -> Response {
    let d = match devices_for(&st, role) {
        Ok(d) => d,
        Err(r) => return r.into_response(),
    };
    match d.create(&body.name, now_ms()) {
        Ok((device, token)) => {
            info!(id = %device.id, name = %device.name, "device added");
            (
                StatusCode::CREATED,
                [(header::CACHE_CONTROL, "no-store")],
                Json(json!({ "device": device, "token": token })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn revoke_device(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
    Path(id): Path<String>,
) -> Response {
    let d = match devices_for(&st, role) {
        Ok(d) => d,
        Err(r) => return r.into_response(),
    };
    match d.revoke(&id) {
        Ok(true) => {
            if let Some(al) = &st.alerts {
                let _ = al.notifier.subs.remove_device(&id);
            }
            info!(%id, "device revoked");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
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
    device: Option<axum::Extension<DeviceId>>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(s) = st.live.get(&camera, &stream) else {
        return (StatusCode::NOT_FOUND, "no such camera stream").into_response();
    };
    let revoked = until_revoked(&st, device);
    ws.on_upgrade(move |socket| async move {
        tokio::select! {
            _ = viewer(socket, s) => {}
            _ = revoked => {}
        }
    })
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
