//! The live view over a real socket: a WebSocket client against the HTTP
//! server, fed by a `LiveStream` the test publishes into.

use std::sync::Arc;
use std::time::Duration;

use argus_media::{Frame, VideoConfig};
use argus_server::http::{self, AppState, CLIENT_QUEUE};
use argus_server::live::{Live, LiveStream};
use argus_server::recorder::StreamEvent;
use bytes::Bytes;
use futures::StreamExt;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "test-token";

fn config() -> VideoConfig {
    VideoConfig::from_avcc(&[1, 0x64, 0, 0x28, 0xff, 0xe0, 0], 2560, 1920).unwrap()
}

/// 10 fps, keyframe every 10 frames; `size` bytes of payload.
fn frame(i: i64, size: usize) -> Frame {
    let mut data = vec![0u8; size.max(5)];
    data[..5].copy_from_slice(&[0, 0, 0, 1, if i % 10 == 0 { 0x65 } else { 0x41 }]);
    Frame {
        pts_90k: i * 9000,
        wall_us: 1_800_000_000_000_000 + i * 100_000,
        keyframe: i % 10 == 0,
        data: Bytes::from(data),
    }
}

async fn server() -> (String, Arc<LiveStream>) {
    server_with(None).await
}

async fn server_with(web_root: Option<std::path::PathBuf>) -> (String, Arc<LiveStream>) {
    let mut live = Live::default();
    let s = live.add(LiveStream::new("door", "main", 1024));
    let state = AppState {
        live: Arc::new(live),
        token: Some(Arc::from(TOKEN)),
        web_root: web_root.map(Arc::new),
        recordings: None,
        devices: None,
        public_url: None,
        plan: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(http::serve(listener, state));
    (addr.to_string(), s)
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(addr: &str) -> Ws {
    let url = format!("ws://{addr}/api/live/door/main?token={TOKEN}");
    tokio_tungstenite::connect_async(url).await.unwrap().0
}

/// Next data message: Ok(text) or Err(binary).
async fn next(ws: &mut Ws) -> Result<serde_json::Value, Bytes> {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out")
            .unwrap()
            .unwrap();
        match msg {
            Message::Text(t) => return Ok(serde_json::from_str(&t).unwrap()),
            Message::Binary(b) => return Err(b),
            _ => continue,
        }
    }
}

/// (frame index, keyframe, replay) of a binary frame message.
fn parse(b: &[u8]) -> (i64, bool, bool) {
    let (h, au) = argus_proto::parse_frame(b).unwrap();
    assert!(au.len() >= 5);
    // Live frames carry the server's receive time; replayed ones don't.
    assert_eq!(h.recv_us == 0, h.replay, "{h:?}");
    (h.pts_90k / 9000, h.keyframe, h.replay)
}

#[tokio::test]
async fn sends_config_then_starts_on_a_keyframe() {
    let (addr, s) = server().await;
    s.publish(StreamEvent::Config(config()));
    for i in 0..5 {
        s.publish(StreamEvent::Frame(frame(i, 100)));
    }
    // Joining mid-GOP: config, then the GOP so far (from its keyframe) as
    // replay, then live frames, without gaps or repeats.
    let mut ws = connect(&addr).await;
    let cfg = next(&mut ws).await.unwrap();
    assert_eq!(cfg["type"], "config");
    assert_eq!(cfg["codec"], "avc1.640028");
    assert_eq!(
        (cfg["width"].as_u64(), cfg["height"].as_u64()),
        (Some(2560), Some(1920))
    );
    assert_eq!(cfg["description"], "AWQAKP/gAA=="); // the avcC record
    for i in 5..15 {
        s.publish(StreamEvent::Frame(frame(i, 100)));
    }
    let mut got = Vec::new();
    for _ in 0..15 {
        got.push(parse(&next(&mut ws).await.unwrap_err()));
    }
    assert_eq!(
        got[0],
        (0, true, true),
        "first frame is the cached keyframe"
    );
    assert!(got[..5].iter().all(|f| f.2), "{got:?}");
    assert!(got[5..].iter().all(|f| !f.2), "{got:?}");
    assert_eq!(
        got.iter().map(|f| f.0).collect::<Vec<_>>(),
        (0..15).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn new_viewer_waits_for_a_keyframe_without_cache() {
    let (addr, s) = server().await;
    let mut ws = connect(&addr).await;
    // Nothing published yet: no config, no frames. Then a reconnect-style
    // start mid-GOP: frames 3..10 are skipped until keyframe 10.
    s.publish(StreamEvent::Config(config()));
    for i in 3..12 {
        s.publish(StreamEvent::Frame(frame(i, 100)));
    }
    assert_eq!(next(&mut ws).await.unwrap()["type"], "config");
    assert_eq!(parse(&next(&mut ws).await.unwrap_err()), (10, true, false));
    assert_eq!(parse(&next(&mut ws).await.unwrap_err()), (11, false, false));
}

#[tokio::test]
async fn slow_viewer_drops_to_next_keyframe_and_never_blocks() {
    let (addr, s) = server().await;
    s.publish(StreamEvent::Config(config()));
    let mut recorder = s.subscribe();
    let mut ws = connect(&addr).await;
    assert_eq!(next(&mut ws).await.unwrap()["type"], "config");
    // Publish far more than the queue and socket buffers hold (60 MB)
    // while the viewer reads nothing. Publishing must not block.
    let n = 600;
    let t = std::time::Instant::now();
    for i in 0..n {
        s.publish(StreamEvent::Frame(frame(i, 100_000)));
        if i % 100 == 0 {
            tokio::task::yield_now().await;
        }
        // The recorder keeps up and sees every frame.
        while let Ok(ev) = recorder.try_recv() {
            assert!(matches!(ev, StreamEvent::Frame(_)));
        }
    }
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "publishing took {:?}",
        t.elapsed()
    );
    // Now read: every gap in the sequence must resume on a keyframe.
    let mut got = Vec::new();
    while got.len() < CLIENT_QUEUE / 2 {
        got.push(parse(&next(&mut ws).await.unwrap_err()));
    }
    s.publish(StreamEvent::Frame(frame(n, 100))); // a keyframe (600 % 10 == 0)
    loop {
        let f = parse(&next(&mut ws).await.unwrap_err());
        got.push(f);
        if f.0 == n {
            break;
        }
    }
    let mut gaps = 0;
    for w in got.windows(2) {
        if w[1].0 != w[0].0 + 1 {
            gaps += 1;
            assert!(
                w[1].1,
                "resumed on a non-keyframe after {:?}: {:?}",
                w[0], w[1]
            );
        }
    }
    assert!(gaps > 0, "expected the slow viewer to drop frames");
    assert!(got.len() < n as usize, "{} frames delivered", got.len());
}

#[tokio::test]
async fn rejects_requests_without_the_token() {
    let (addr, s) = server().await;
    s.publish(StreamEvent::Config(config()));
    let client = reqwest_get(&format!("http://{addr}/api/cameras"), None).await;
    assert_eq!(client.0, 401);
    let wrong = reqwest_get(&format!("http://{addr}/api/cameras"), Some("nope")).await;
    assert_eq!(wrong.0, 401);
    let ok = reqwest_get(&format!("http://{addr}/api/cameras"), Some(TOKEN)).await;
    assert_eq!(ok.0, 200);
    let body: serde_json::Value = serde_json::from_str(&ok.1).unwrap();
    assert_eq!(body["cameras"][0]["name"], "door");
    assert_eq!(body["cameras"][0]["streams"][0]["codec"], "avc1.640028");
    assert_eq!(body["cameras"][0]["streams"][0]["width"], 2560);

    // WebSocket: no token, wrong token, and a bearer header.
    for url in [
        format!("ws://{addr}/api/live/door/main"),
        format!("ws://{addr}/api/live/door/main?token=nope"),
    ] {
        let err = tokio_tungstenite::connect_async(url).await.unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
    }
    let mut req = format!("ws://{addr}/api/live/door/main")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert_eq!(next(&mut ws).await.unwrap()["type"], "config");
    // Unknown stream: 404.
    let err =
        tokio_tungstenite::connect_async(format!("ws://{addr}/api/live/door/nope?token={TOKEN}"))
            .await
            .unwrap_err();
    assert!(err.to_string().contains("404"), "{err}");
}

#[tokio::test]
async fn answers_pings_for_clock_sync() {
    use futures::SinkExt;
    let (addr, s) = server().await;
    s.publish(StreamEvent::Config(config()));
    let mut ws = connect(&addr).await;
    assert_eq!(next(&mut ws).await.unwrap()["type"], "config");
    let before = argus_server::recorder::now_us();
    ws.send(Message::Text(r#"{"type":"ping","client_us":42}"#.into()))
        .await
        .unwrap();
    let pong = next(&mut ws).await.unwrap();
    assert_eq!(pong["type"], "pong");
    assert_eq!(pong["client_us"], 42);
    let server_us = pong["server_us"].as_i64().unwrap();
    assert!(server_us >= before && server_us <= argus_server::recorder::now_us());
}

#[tokio::test]
async fn serves_the_web_app_without_a_token() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<!doctype html>argus").unwrap();
    std::fs::write(dir.path().join("app.wasm"), [0, 97, 115, 109]).unwrap();
    std::fs::write(dir.path().parent().unwrap().join("secret.txt"), "no").ok();
    let (addr, _s) = server_with(Some(dir.path().to_owned())).await;
    let (status, body) = reqwest_get(&format!("http://{addr}/"), None).await;
    assert_eq!((status, body.as_str()), (200, "<!doctype html>argus"));
    assert_eq!(
        reqwest_get(&format!("http://{addr}/app.wasm"), None)
            .await
            .0,
        200
    );
    assert_eq!(
        reqwest_get(&format!("http://{addr}/missing.js"), None)
            .await
            .0,
        404
    );
    // No escaping the web root, and the API still needs the token.
    assert_eq!(
        reqwest_get(&format!("http://{addr}/../secret.txt"), None)
            .await
            .0,
        404
    );
    assert_eq!(
        reqwest_get(&format!("http://{addr}/%2e%2e/secret.txt"), None)
            .await
            .0,
        404
    );
    assert_eq!(
        reqwest_get(&format!("http://{addr}/api/cameras"), None)
            .await
            .0,
        401
    );
}

#[tokio::test]
async fn healthz_needs_no_token() {
    let (addr, _s) = server().await;
    let (status, body) = reqwest_get(&format!("http://{addr}/healthz"), None).await;
    assert_eq!(status, 200);
    assert!(body.starts_with("ok "), "{body}");
}

/// Minimal HTTP GET (status, body) so the test needs no HTTP client crate.
async fn reqwest_get(url: &str, token: Option<&str>) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let mut sock = tokio::net::TcpStream::connect(host).await.unwrap();
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let req = format!("GET /{path} HTTP/1.1\r\nHost: {host}\r\n{auth}Connection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut resp = String::new();
    sock.read_to_string(&mut resp).await.unwrap();
    let status = resp[9..12].parse().unwrap();
    let body = resp
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_owned();
    (status, body)
}
