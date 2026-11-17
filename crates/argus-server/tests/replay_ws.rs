//! Synced replay over a real socket, from chunk files written by the real
//! chunk writer.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argus_media::{Frame, VideoConfig};
use argus_server::http::{self, AppState, Recordings};
use argus_server::live::{Live, LiveStream};
use argus_store::{ChunkWriter, Index};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "t0k";
/// Recordings start an hour ago, on a whole second.
fn t0() -> i64 {
    (argus_server::recorder::now_us() / 1000 - 3_600_000) / 1000 * 1000
}

/// 30 s of 10 fps video per camera, 1 s GOPs, 10 s chunks. Camera "b"'s
/// frames are captured 20 ms after "a"'s. Sub frames are 40 bytes; camera
/// "a" also has a main stream with 80-byte frames.
fn record(root: &std::path::Path, index: &Arc<Mutex<Index>>, t0: i64) {
    let cfg = VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360).unwrap();
    let main = VideoConfig::from_avcc(&[1, 0x64, 0, 0x33, 0xff, 0xe0, 0], 2560, 1920).unwrap();
    for (cam, stream, cfg, size, offset_ms) in [
        ("a", "sub", &cfg, 40, 0),
        ("b", "sub", &cfg, 40, 20),
        ("a", "main", &main, 80, 0),
    ] {
        let mut w = ChunkWriter::new(root, cam, stream, 10, index.clone());
        w.on_config(cfg).unwrap();
        for i in 0..300i64 {
            w.on_frame(Frame {
                pts_90k: i * 9000,
                wall_us: (t0 + offset_ms + i * 100) * 1000,
                keyframe: i % 10 == 0,
                data: Bytes::from(vec![i as u8; size]),
            })
            .unwrap();
        }
        w.finish().unwrap();
    }
}

struct Server {
    addr: String,
    t0: i64,
    dir: tempfile::TempDir,
    index: Arc<Mutex<Index>>,
}

async fn server() -> Server {
    server_with(None, |_, _| {}).await
}

/// `prepare` runs after recording, before the server starts.
async fn server_with(
    remote: Option<Arc<dyn object_store::ObjectStore>>,
    prepare: impl FnOnce(&std::path::Path, &Arc<Mutex<Index>>),
) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(Mutex::new(
        Index::open(&dir.path().join("index.sqlite")).unwrap(),
    ));
    let t0 = t0();
    record(dir.path(), &index, t0);
    prepare(dir.path(), &index);
    let mut live = Live::default();
    live.add(LiveStream::new("a", "sub", 16));
    live.add(LiveStream::new("b", "sub", 16));
    let state = AppState {
        live: Arc::new(live),
        token: Some(Arc::from(TOKEN)),
        web_root: None,
        recordings: Some(Recordings {
            index: index.clone(),
            root: Arc::new(dir.path().to_owned()),
            remote,
        }),
        devices: None,
        public_url: None,
        plan: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(http::serve(listener, state));
    Server {
        addr,
        t0,
        dir,
        index,
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

enum Msg {
    Text(serde_json::Value),
    Frame(argus_proto::ReplayHeader),
}

async fn next(ws: &mut Ws) -> Option<Msg> {
    loop {
        let m = tokio::time::timeout(Duration::from_millis(1500), ws.next())
            .await
            .ok()??
            .ok()?;
        match m {
            Message::Text(t) => return Some(Msg::Text(serde_json::from_str(&t).unwrap())),
            Message::Binary(b) => {
                let (h, au) = argus_proto::parse_replay_frame(&b).unwrap();
                assert!(au.len() == 40 || au.len() == 80, "{}", au.len());
                return Some(Msg::Frame(h));
            }
            _ => continue,
        }
    }
}

/// Everything that arrives within `window`.
async fn drain(
    ws: &mut Ws,
    window: Duration,
) -> (Vec<serde_json::Value>, Vec<argus_proto::ReplayHeader>) {
    let (mut texts, mut frames) = (vec![], vec![]);
    let end = tokio::time::Instant::now() + window;
    while let Ok(Some(m)) = tokio::time::timeout_at(end, next(ws)).await {
        match m {
            Msg::Text(t) => texts.push(t),
            Msg::Frame(h) => frames.push(h),
        }
    }
    (texts, frames)
}

fn ms(h: &argus_proto::ReplayHeader) -> i64 {
    h.wall_us / 1000
}

#[tokio::test(flavor = "multi_thread")]
async fn replays_cameras_in_sync_from_a_seek_point_and_paces_them() {
    let s = server().await;
    let t = s.t0 + 12_500;
    let url = format!(
        "ws://{}/api/replay?t={t}&stream=sub&cameras=a,b&token={TOKEN}",
        s.addr
    );
    let started = Instant::now();
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    let Some(Msg::Text(hello)) = next(&mut ws).await else {
        panic!("no replay message")
    };
    assert_eq!(hello["type"], "replay");
    assert_eq!(hello["cameras"], serde_json::json!(["a", "b"]));

    // The first burst: configs, then each camera from the keyframe of the
    // GOP containing t, up to the lookahead.
    let (texts, frames) = drain(&mut ws, Duration::from_millis(250)).await;
    let first_frame_after = started.elapsed();
    assert_eq!(
        texts.iter().filter(|t| t["type"] == "config").count(),
        2,
        "{texts:?}"
    );
    assert_eq!(texts[0]["codec"], "avc1.4D001F");
    for cam in [0u8, 1] {
        let f: Vec<_> = frames.iter().filter(|h| h.camera == cam).collect();
        assert!(f[0].keyframe, "camera {cam} starts on a keyframe");
        assert_eq!(ms(f[0]), s.t0 + 12_000 + 20 * i64::from(cam));
        // Nothing far beyond t + lookahead yet: paced, not dumped.
        let last = ms(f.last().unwrap());
        assert!(last <= t + 1_200, "camera {cam} sent ahead to {}", last - t);
    }
    assert!(
        first_frame_after < Duration::from_millis(800),
        "{first_frame_after:?}"
    );

    // Over the next second the stream advances about a second, at the
    // master clock's pace.
    let (_, frames) = drain(&mut ws, Duration::from_millis(1000)).await;
    let last = frames.iter().map(ms).max().unwrap();
    let elapsed = started.elapsed().as_millis() as i64;
    let ahead = last - (t + elapsed);
    assert!(
        (0..=1_000).contains(&ahead),
        "{ahead} ms ahead of the master clock"
    );

    // Seek: a new replay message with our seq, then each camera from the
    // GOP containing the new time. Stale frames may arrive before it.
    let t2 = s.t0 + 25_300;
    ws.send(Message::Text(
        format!(r#"{{"type":"seek","t_ms":{t2},"seq":7}}"#).into(),
    ))
    .await
    .unwrap();
    loop {
        match next(&mut ws).await.expect("replay after seek") {
            Msg::Text(m) if m["type"] == "replay" => {
                assert_eq!((m["seq"].as_u64(), m["t_ms"].as_i64()), (Some(7), Some(t2)));
                break;
            }
            _ => continue,
        }
    }
    let (_, frames) = drain(&mut ws, Duration::from_millis(250)).await;
    for cam in [0u8, 1] {
        let f = frames.iter().find(|h| h.camera == cam).unwrap();
        assert!(f.keyframe);
        assert_eq!(ms(f), s.t0 + 25_000 + 20 * i64::from(cam));
    }

    // Pause: once the lookahead is sent, nothing more arrives.
    ws.send(Message::Text(r#"{"type":"pause"}"#.into()))
        .await
        .unwrap();
    let _ = drain(&mut ws, Duration::from_millis(300)).await;
    let (_, after) = drain(&mut ws, Duration::from_millis(700)).await;
    assert!(after.is_empty(), "{} frames while paused", after.len());
    // Play resumes.
    ws.send(Message::Text(r#"{"type":"play"}"#.into()))
        .await
        .unwrap();
    let (_, resumed) = drain(&mut ws, Duration::from_millis(500)).await;
    assert!(resumed.len() >= 6, "{} frames after play", resumed.len());
}

async fn get(url: &str, range: Option<&str>) -> (u16, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let mut sock = tokio::net::TcpStream::connect(host).await.unwrap();
    let r = range.map_or(String::new(), |r| format!("Range: {r}\r\n"));
    let req = format!(
        "GET /{path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {TOKEN}\r\n{r}Connection: close\r\n\r\n"
    );
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    sock.read_to_end(&mut resp).await.unwrap();
    let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let status = String::from_utf8_lossy(&resp[9..12]).parse().unwrap();
    (status, resp[split + 4..].to_vec())
}

#[tokio::test(flavor = "multi_thread")]
async fn events_for_the_map() {
    let s = server().await;
    {
        let i = s.index.lock().unwrap();
        i.add_event("a", s.t0, s.t0 + 5000, "reolink", "person")
            .unwrap();
        i.add_event("b", s.t0 + 1000, s.t0 + 2000, "reolink", "vehicle")
            .unwrap();
        i.add_event("a", s.t0 - 7_200_000, s.t0 - 7_100_000, "cli", "manual")
            .unwrap();
    }
    let base = format!("http://{}", s.addr);
    let (st, body) = get(&format!("{base}/api/events?from={}", s.t0 - 60_000), None).await;
    assert_eq!(st, 200);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let ev = v["events"].as_array().unwrap();
    assert_eq!(ev.len(), 2, "{v}");
    assert_eq!(
        (ev[0]["camera"].as_str(), ev[0]["label"].as_str()),
        (Some("a"), Some("person"))
    );
    assert_eq!(ev[1]["label"], "vehicle");
    let (_, body) = get(
        &format!("{base}/api/events?from={}&cameras=b", s.t0 - 60_000),
        None,
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["events"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn recordings_seek_and_ranged_media() {
    let s = server().await;
    let base = format!("http://{}", s.addr);
    let (st, body) = get(
        &format!("{base}/api/recordings?stream=sub&from={}", s.t0 - 1000),
        None,
    )
    .await;
    assert_eq!(st, 200);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["cameras"][0]["name"], "a");
    // Three 10 s chunks merge into one 30 s span.
    assert_eq!(
        v["cameras"][0]["spans"],
        serde_json::json!([[s.t0, s.t0 + 30_000]])
    );

    let (st, body) = get(
        &format!("{base}/api/seek?t={}&cameras=b", s.t0 + 12_500),
        None,
    )
    .await;
    assert_eq!(st, 200);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let b = &v["cameras"][0];
    assert_eq!(b["camera"], "b");
    assert_eq!(b["gop_start_ms"], s.t0 + 12_020);

    // The GOP by byte range is one moof+mdat fragment.
    let (off, len) = (
        b["byte_offset"].as_u64().unwrap(),
        b["byte_len"].as_u64().unwrap(),
    );
    let range = format!("bytes={off}-{}", off + len - 1);
    let (st, body) = get(
        &format!("{base}/api/media/{}", b["chunk"].as_str().unwrap()),
        Some(&range),
    )
    .await;
    assert_eq!((st, body.len() as u64), (206, len));
    assert_eq!(&body[4..8], b"moof");
    let samples = argus_media::fmp4::read_video_samples(&Bytes::from(body)).unwrap();
    assert_eq!(samples.len(), 10);

    let (st, init) = get(
        &format!("{base}/api/media/{}", b["init"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!((st, &init[4..8]), (200, &b"ftyp"[..]));
    assert_eq!(
        get(&format!("{base}/api/media/../index.sqlite"), None)
            .await
            .0,
        404
    );
    assert_eq!(
        get(&format!("{base}/api/media/rec/../../etc/passwd"), None)
            .await
            .0,
        404
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn focus_switches_one_camera_to_its_main_stream() {
    let s = server().await;
    let t = s.t0 + 12_500;
    let url = format!(
        "ws://{}/api/replay?t={t}&stream=sub&cameras=a,b&token={TOKEN}",
        s.addr
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let _ = drain(&mut ws, Duration::from_millis(400)).await;
    ws.send(Message::Text(
        r#"{"type":"stream","camera":"a","stream":"main"}"#.into(),
    ))
    .await
    .unwrap();
    // Everything after the acknowledgement for camera a is main.
    loop {
        match next(&mut ws).await.expect("stream reply") {
            Msg::Text(m) if m["type"] == "stream" => {
                assert_eq!(
                    (m["camera"].as_str(), m["stream"].as_str()),
                    (Some("a"), Some("main"))
                );
                break;
            }
            _ => continue,
        }
    }
    // Read raw messages here to see access unit sizes (sub 40, main 80).
    let (mut cfg, mut frames) = (None, Vec::new());
    let end = tokio::time::Instant::now() + Duration::from_millis(600);
    while let Ok(Some(Ok(m))) = tokio::time::timeout_at(end, ws.next()).await {
        match m {
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "config" {
                    cfg = Some(v);
                }
            }
            Message::Binary(b) => {
                let (h, au) = argus_proto::parse_replay_frame(&b).unwrap();
                frames.push((h, au.len()));
            }
            _ => {}
        }
    }
    let cfg = cfg.expect("main config");
    assert_eq!(
        (cfg["camera"].as_str(), cfg["width"].as_u64()),
        (Some("a"), Some(2560))
    );
    let first_a = frames.iter().find(|(h, _)| h.camera == 0).unwrap();
    assert!(first_a.0.keyframe, "main restarts on a keyframe");
    assert!(
        frames
            .iter()
            .filter(|(h, _)| h.camera == 0)
            .all(|(_, l)| *l == 80)
    );
    assert!(
        frames
            .iter()
            .filter(|(h, _)| h.camera == 1)
            .all(|(_, l)| *l == 40)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn plays_chunks_that_are_only_in_object_storage() {
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
    let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    // Upload camera a's sub chunks and its init segment, then evict them
    // locally, exactly as the uploader and eviction would.
    let uploads = Arc::new(Mutex::new(Vec::new()));
    let up = uploads.clone();
    let s = server_with(Some(store.clone()), move |root, index| {
        let idx = index.lock().unwrap();
        for c in idx.local_chunks_oldest_first(None, 100).unwrap() {
            if c.camera != "a" || c.stream != "sub" {
                continue;
            }
            let (init_key, _) = idx.init_upload_state(c.init_id).unwrap();
            for (key, is_init) in [(c.object_key.clone(), false), (init_key, true)] {
                let remote = format!("nvr/{key}");
                if let Ok(bytes) = std::fs::read(root.join(&key)) {
                    up.lock().unwrap().push((remote.clone(), bytes));
                    let _ = std::fs::remove_file(root.join(&key));
                }
                if is_init {
                    idx.mark_init_uploaded(c.init_id, &remote, 1).unwrap();
                } else {
                    idx.mark_chunk_uploaded(c.id, &remote, 1).unwrap();
                    idx.set_tier(c.id, "remote").unwrap();
                }
            }
        }
    })
    .await;
    let pending: Vec<_> = uploads.lock().unwrap().drain(..).collect();
    for (key, bytes) in pending {
        store
            .put(
                &object_store::path::Path::from(key),
                PutPayload::from(bytes),
            )
            .await
            .unwrap();
    }
    fn m4s(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir).map_or(0, |d| {
            d.flatten()
                .map(|e| {
                    let p = e.path();
                    if p.is_dir() {
                        m4s(&p)
                    } else {
                        usize::from(p.extension().is_some_and(|x| x == "m4s"))
                    }
                })
                .sum()
        })
    }
    let local_a = m4s(&s.dir.path().join("rec/a/sub"));
    assert_eq!(local_a, 0, "camera a's sub chunks are gone locally");

    let t = s.t0 + 12_500;
    let url = format!(
        "ws://{}/api/replay?t={t}&stream=sub&cameras=a&token={TOKEN}",
        s.addr
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let (texts, frames) = drain(&mut ws, Duration::from_millis(600)).await;
    assert!(
        texts.iter().any(|t| t["type"] == "config"),
        "config from the remote init"
    );
    assert!(frames[0].keyframe);
    assert_eq!(ms(&frames[0]), s.t0 + 12_000);
    // Crossing into the next chunk (also remote) keeps going.
    ws.send(Message::Text(
        format!(r#"{{"type":"seek","t_ms":{},"seq":1}}"#, s.t0 + 19_600).into(),
    ))
    .await
    .unwrap();
    let (_, frames) = drain(&mut ws, Duration::from_millis(1500)).await;
    assert!(
        frames.iter().any(|h| ms(h) >= s.t0 + 20_000),
        "played into the next remote chunk"
    );

    // /api/media serves a GOP of an evicted chunk by range from storage.
    let seek = s
        .index
        .lock()
        .unwrap()
        .seek("a", "sub", s.t0 + 12_500)
        .unwrap()
        .unwrap();
    let range = format!(
        "bytes={}-{}",
        seek.byte_offset,
        seek.byte_offset + seek.byte_len - 1
    );
    let (st, body) = get(
        &format!("http://{}/api/media/{}", s.addr, seek.object_key),
        Some(&range),
    )
    .await;
    assert_eq!((st, body.len() as u64), (206, seek.byte_len));
    assert_eq!(&body[4..8], b"moof");
}
