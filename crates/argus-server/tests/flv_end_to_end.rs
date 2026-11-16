//! Real H.264 (from ffmpeg's test source) served as HTTP-FLV, through the
//! ingest, timestamping and chunk writer, out to fMP4 files that ffmpeg can
//! decode. This is the phase 1 pipeline minus the camera.

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use argus_ingest::SourceEvent;
use argus_server::recorder::{StreamEvent, Timestamper};
use argus_store::{ChunkWriter, Index};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const FIXTURE: &[u8] = include_bytes!("fixtures/testsrc-160x90-10fps-4s.flv");

async fn serve_once(body: &'static [u8]) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut req = [0u8; 1024];
        let _ = sock.read(&mut req).await.unwrap();
        sock.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        // Dribble it out in small pieces to exercise the incremental parser.
        for piece in body.chunks(777) {
            sock.write_all(piece).await.unwrap();
        }
    });
    format!(
        "http://{addr}/flv?port=1935&app=bcs&stream=channel0_sub.bcs&user=admin&password=secret"
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn flv_stream_becomes_decodable_chunks() {
    let url = serve_once(FIXTURE).await;
    let (tx, mut rx) = mpsc::channel(64);
    let conn = tokio::spawn(async move { argus_ingest::run(&url, tx).await });

    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
    let mut writer = ChunkWriter::new(dir.path(), "driveway", "sub", 2, index.clone());
    let mut ts = Timestamper::default();
    let base_us = 1_794_834_872_000_000;
    let (mut configs, mut frames) = (0, 0);
    while let Some(ev) = rx.recv().await {
        // Pretend frames arrive in real time.
        let arrival = match &ev {
            SourceEvent::Frame(f) => base_us + f.pts_90k * 100 / 9,
            SourceEvent::Config(_) => base_us,
        };
        match ts.apply(ev, arrival) {
            StreamEvent::Config(c) => {
                configs += 1;
                assert_eq!((c.width, c.height), (160, 90));
                assert!(c.rfc6381.starts_with("avc1.4D"), "{}", c.rfc6381);
                writer.on_config(&c).unwrap();
            }
            StreamEvent::Frame(f) => {
                frames += 1;
                writer.on_frame(f).unwrap();
            }
            StreamEvent::Discontinuity => unreachable!(),
        }
    }
    writer.finish().unwrap();
    let err = conn.await.unwrap().unwrap_err();
    assert!(err.to_string().contains("ended"), "{err:#}");
    assert_eq!((configs, frames), (1, 40));

    // 4 s of 1 s GOPs in 2 s chunks.
    let chunks = index
        .lock()
        .unwrap()
        .chunks_between("driveway", "sub", 0, i64::MAX)
        .unwrap();
    assert_eq!(
        chunks.iter().map(|c| c.gops.len()).collect::<Vec<_>>(),
        [2, 2]
    );
    assert_eq!(chunks[0].start_ms, base_us / 1000);
    assert_eq!(chunks[1].end_ms, base_us / 1000 + 4000);

    // Seeking to 2.5 s lands on the GOP starting at 2 s.
    let seek = index
        .lock()
        .unwrap()
        .seek("driveway", "sub", base_us / 1000 + 2500)
        .unwrap()
        .unwrap();
    assert_eq!(seek.gop_start_ms, base_us / 1000 + 2000);

    // init + one GOP fetched by byte range must decode on its own: this is
    // exactly what the browser will do on seek.
    let init = std::fs::read(dir.path().join(&seek.init_key)).unwrap();
    let chunk = std::fs::read(dir.path().join(&seek.object_key)).unwrap();
    let gop = &chunk[seek.byte_offset as usize..(seek.byte_offset + seek.byte_len) as usize];
    let single = dir.path().join("single-gop.mp4");
    std::fs::write(&single, [init.as_slice(), gop].concat()).unwrap();

    let mut full = init.clone();
    for c in &chunks {
        full.extend(std::fs::read(dir.path().join(&c.object_key)).unwrap());
    }
    let whole = dir.path().join("whole.mp4");
    std::fs::write(&whole, full).unwrap();

    if Command::new("ffprobe").arg("-version").output().is_err() {
        eprintln!("ffprobe not installed; skipping decode checks");
        return;
    }
    assert_eq!(decoded_frames(&whole), 40);
    assert_eq!(decoded_frames(&single), 10);
}

fn decoded_frames(path: &Path) -> u32 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "decoder complained: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}
