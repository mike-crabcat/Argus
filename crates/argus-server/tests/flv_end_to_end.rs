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
/// The same video plus a 440 Hz tone as 16 kHz mono AAC-LC (64 frames).
const FIXTURE_AAC: &[u8] = include_bytes!("fixtures/testsrc-160x90-10fps-4s-aac16k.flv");

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

struct Recorded {
    dir: tempfile::TempDir,
    index: Arc<Mutex<Index>>,
    configs: u32,
    frames: u32,
    audio_configs: u32,
    audio_frames: u32,
    /// Wall time of the first video frame. ffmpeg's AAC encoder delays the
    /// video by its 64 ms priming in the audio fixture.
    start_ms: i64,
}

/// Serves an FLV file over HTTP and records it through ingest,
/// timestamping and the chunk writer into 2 s chunks.
async fn record(fixture: &'static [u8]) -> Recorded {
    let url = serve_once(fixture).await;
    let (tx, mut rx) = mpsc::channel(64);
    let conn = tokio::spawn(async move { argus_ingest::run(&url, tx).await });

    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
    let mut writer = ChunkWriter::new(dir.path(), "driveway", "sub", 2, index.clone());
    let mut ts = Timestamper::default();
    let base_us = 1_794_834_872_000_000;
    let mut r = (0, 0, 0, 0);
    let mut start_ms = None;
    while let Some(ev) = rx.recv().await {
        // Pretend frames arrive in real time.
        let arrival = match &ev {
            SourceEvent::Frame(f) => base_us + f.pts_90k * 100 / 9,
            SourceEvent::Audio(a) => base_us + a.pts_90k * 100 / 9,
            SourceEvent::Config(_) | SourceEvent::AudioConfig(_) => base_us,
        };
        match ts.apply(ev, arrival) {
            StreamEvent::Config(c) => {
                r.0 += 1;
                assert_eq!((c.width, c.height), (160, 90));
                assert!(c.rfc6381.starts_with("avc1.4D"), "{}", c.rfc6381);
                writer.on_config(&c).unwrap();
            }
            StreamEvent::Frame(f) => {
                r.1 += 1;
                start_ms.get_or_insert(f.wall_ms());
                writer.on_frame(f).unwrap();
            }
            StreamEvent::AudioConfig(c) => {
                r.2 += 1;
                assert_eq!((c.sample_rate, c.channels), (16000, 1));
                assert_eq!(c.rfc6381, "mp4a.40.2");
                writer.on_audio_config(&c).unwrap();
            }
            StreamEvent::Audio(a) => {
                r.3 += 1;
                writer.on_audio(a).unwrap();
            }
            StreamEvent::Discontinuity => unreachable!(),
        }
    }
    writer.finish().unwrap();
    let err = conn.await.unwrap().unwrap_err();
    assert!(err.to_string().contains("ended"), "{err:#}");
    Recorded {
        dir,
        index,
        configs: r.0,
        frames: r.1,
        audio_configs: r.2,
        audio_frames: r.3,
        start_ms: start_ms.expect("no video frames"),
    }
}

/// Writes init + all chunks as whole.mp4, and init + the GOP at 2.5 s
/// (fetched by byte range, as a browser does on seek) as single-gop.mp4.
fn assemble(r: &Recorded) -> (std::path::PathBuf, std::path::PathBuf) {
    // 4 s of 1 s GOPs in 2 s chunks.
    let chunks = r
        .index
        .lock()
        .unwrap()
        .chunks_between("driveway", "sub", 0, i64::MAX)
        .unwrap();
    assert_eq!(
        chunks.iter().map(|c| c.gops.len()).collect::<Vec<_>>(),
        [2, 2]
    );
    assert_eq!(chunks[0].start_ms, r.start_ms);
    assert_eq!(chunks[1].end_ms, r.start_ms + 4000);

    // Seeking to 2.5 s lands on the GOP starting at 2 s.
    let seek = r
        .index
        .lock()
        .unwrap()
        .seek("driveway", "sub", r.start_ms + 2500)
        .unwrap()
        .unwrap();
    assert_eq!(seek.gop_start_ms, r.start_ms + 2000);

    let dir = r.dir.path();
    let init = std::fs::read(dir.join(&seek.init_key)).unwrap();
    let chunk = std::fs::read(dir.join(&seek.object_key)).unwrap();
    let gop = &chunk[seek.byte_offset as usize..(seek.byte_offset + seek.byte_len) as usize];
    let single = dir.join("single-gop.mp4");
    std::fs::write(&single, [init.as_slice(), gop].concat()).unwrap();

    let mut full = init.clone();
    for c in &chunks {
        full.extend(std::fs::read(dir.join(&c.object_key)).unwrap());
    }
    let whole = dir.join("whole.mp4");
    std::fs::write(&whole, full).unwrap();
    (whole, single)
}

fn have_ffmpeg() -> bool {
    let ok = Command::new("ffprobe").arg("-version").output().is_ok()
        && Command::new("ffmpeg").arg("-version").output().is_ok();
    if !ok {
        eprintln!("ffmpeg/ffprobe not installed; skipping decode checks");
    }
    ok
}

#[tokio::test(flavor = "multi_thread")]
async fn flv_stream_becomes_decodable_chunks() {
    let r = record(FIXTURE).await;
    assert_eq!((r.configs, r.frames), (1, 40));
    assert_eq!(r.start_ms, 1_794_834_872_000);
    assert_eq!((r.audio_configs, r.audio_frames), (0, 0));
    let (whole, single) = assemble(&r);
    if !have_ffmpeg() {
        return;
    }
    assert_eq!(stream_codecs(&whole), ["h264"]);
    assert_eq!(decoded_frames(&whole), 40);
    assert_eq!(decoded_frames(&single), 10);
}

#[tokio::test(flavor = "multi_thread")]
async fn flv_audio_is_stored_as_second_track() {
    let r = record(FIXTURE_AAC).await;
    assert_eq!((r.configs, r.frames), (1, 40));
    // The audio (pts 0) anchored the clock; video at pts 64 ms follows it.
    assert_eq!(r.start_ms, 1_794_834_872_064);
    assert_eq!((r.audio_configs, r.audio_frames), (1, 64));
    let (whole, single) = assemble(&r);
    if !have_ffmpeg() {
        return;
    }
    assert_eq!(stream_codecs(&whole), ["h264", "aac,16000,1"]);
    assert_eq!(stream_codecs(&single), ["h264", "aac,16000,1"]);
    assert_eq!(decoded_frames(&whole), 40);
    assert_eq!(decoded_frames(&single), 10);
    // Every AAC frame from the first keyframe on is stored, once. The frame
    // at 0 ms comes before the first keyframe (64 ms), where recording starts.
    assert_eq!(audio_packets(&whole), 63);
    assert!(audio_packets(&single) > 0);
    assert_clean_decode(&whole, &["-map", "0:a"]);
    assert_clean_decode(&whole, &[]);
    assert_clean_decode(&single, &[]);
}

/// codec[,sample_rate,channels] per stream, in order.
fn stream_codecs(path: &Path) -> Vec<String> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,sample_rate,channels",
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
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim_end_matches(',').to_owned())
        .collect()
}

fn audio_packets(path: &Path) -> u32 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-count_packets",
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// `ffmpeg -v error -i path [args] -f null -` must succeed with no output.
fn assert_clean_decode(path: &Path, args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(args)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    assert!(out.status.success(), "ffmpeg failed on {}", path.display());
    assert!(
        out.stderr.is_empty(),
        "ffmpeg {args:?} complained about {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
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
