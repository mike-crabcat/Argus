//! The uploader against a real S3 implementation: MinIO in Docker.
//!
//! Skipped when Docker isn't available. The image defaults to
//! `bitnamilegacy/minio` (it can pre-create a bucket); override it with
//! `ARGUS_MINIO_IMAGE`.

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argus_media::VideoConfig;
use argus_store::{ChunkRecord, GopEntry, Index, Policy, StreamPolicy, Uploader};
use futures::TryStreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

const USER: &str = "argus-test";
const PASSWORD: &str = "argus-test-password";
const BUCKET: &str = "argus-test";
const T0: i64 = 1_800_000_000_000;

/// Removes the container when the test ends, pass or fail.
struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

fn docker_ok() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Starts MinIO on a random local port; returns the container and endpoint.
fn start_minio() -> (Container, String) {
    let image =
        std::env::var("ARGUS_MINIO_IMAGE").unwrap_or_else(|_| "bitnamilegacy/minio:latest".into());
    let name = format!("argus-minio-test-{}", std::process::id());
    let out = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "-p",
            "127.0.0.1::9000",
        ])
        .args(["-e", &format!("MINIO_ROOT_USER={USER}")])
        .args(["-e", &format!("MINIO_ROOT_PASSWORD={PASSWORD}")])
        .args(["-e", &format!("MINIO_DEFAULT_BUCKETS={BUCKET}")])
        .arg(&image)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "docker run {image}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let container = Container(name.clone());
    let out = Command::new("docker")
        .args(["port", &name, "9000/tcp"])
        .output()
        .unwrap();
    let addr = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .expect("no published port")
        .trim()
        .to_owned();
    (container, format!("http://{addr}"))
}

fn s3(endpoint: &str) -> Arc<dyn ObjectStore> {
    Arc::new(
        AmazonS3Builder::new()
            .with_endpoint(endpoint)
            .with_allow_http(true)
            .with_region("us-east-1")
            .with_bucket_name(BUCKET)
            .with_access_key_id(USER)
            .with_secret_access_key(PASSWORD)
            .build()
            .unwrap(),
    )
}

/// MinIO takes a few seconds to start and create the bucket.
async fn wait_ready(store: &Arc<dyn ObjectStore>) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let probe = object_store::path::Path::from("probe");
        match store.put(&probe, PutPayload::from_static(b"ok")).await {
            Ok(_) => {
                store.delete(&probe).await.unwrap();
                return;
            }
            Err(e) if Instant::now() > deadline => panic!("MinIO never became ready: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

fn add_chunk(index: &Index, root: &std::path::Path, stream: &str, minute: i64) -> Vec<u8> {
    let cfg = VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360).unwrap();
    let init_key = format!("init/door/{stream}/1.mp4");
    let key = format!("rec/door/{stream}/{minute}.m4s");
    let body: Vec<u8> = (0..50_000u32)
        .map(|i| (i as i64 * 31 + minute) as u8)
        .collect();
    for (k, data) in [(&init_key, &b"init"[..]), (&key, &body[..])] {
        let p = root.join(k);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
    let init_id = index
        .upsert_init("door", stream, 1, &init_key, &cfg, None)
        .unwrap();
    index
        .insert_chunk(&ChunkRecord {
            camera: "door".into(),
            stream: stream.into(),
            start_ms: T0 + minute * 60_000,
            end_ms: T0 + (minute + 1) * 60_000,
            object_key: key,
            bytes: body.len() as u64,
            init_id,
            gops: vec![GopEntry {
                start_offset_ms: 0,
                byte_offset: 0,
                byte_len: body.len() as u32,
                frame_count: 1,
            }],
        })
        .unwrap();
    body
}

async fn list(store: &Arc<dyn ObjectStore>) -> Vec<String> {
    let mut keys: Vec<String> = store
        .list(None)
        .map_ok(|m| m.location.to_string())
        .try_collect()
        .await
        .unwrap();
    keys.sort();
    keys
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_to_minio_by_policy_and_resumes() {
    if !docker_ok() {
        eprintln!("Docker not available; skipping MinIO test");
        return;
    }
    let (_container, endpoint) = start_minio();
    let store = s3(&endpoint);
    wait_ready(&store).await;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.sqlite");
    let streams = vec![
        StreamPolicy {
            camera: "door".into(),
            stream: "sub".into(),
            policy: Policy::Always,
        },
        StreamPolicy {
            camera: "door".into(),
            stream: "main".into(),
            policy: Policy::Events,
        },
    ];
    let index = Arc::new(Mutex::new(Index::open(&db).unwrap()));
    let mut sub_bodies = Vec::new();
    {
        let idx = index.lock().unwrap();
        for m in 0..3 {
            sub_bodies.push(add_chunk(&idx, dir.path(), "sub", m));
            add_chunk(&idx, dir.path(), "main", m);
        }
        idx.add_event("door", T0 + 70_000, T0 + 80_000, "test", "person")
            .unwrap();
    }
    let mut up = Uploader::new(
        store.clone(),
        index.clone(),
        dir.path(),
        "argus",
        streams.clone(),
        50.0,
    );
    assert_eq!(up.upload_pending().await.unwrap(), 4);
    assert_eq!(
        list(&store).await,
        [
            "argus/init/door/main/1.mp4",
            "argus/init/door/sub/1.mp4",
            "argus/rec/door/main/1.m4s",
            "argus/rec/door/sub/0.m4s",
            "argus/rec/door/sub/1.m4s",
            "argus/rec/door/sub/2.m4s",
        ]
    );
    let got = store
        .get(&object_store::path::Path::from("argus/rec/door/sub/2.m4s"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&got[..], &sub_bodies[2][..]);

    // "Restart": a fresh index handle and uploader. One new chunk arrived
    // meanwhile; only it is uploaded.
    drop(up);
    let index = Arc::new(Mutex::new(Index::open(&db).unwrap()));
    add_chunk(&index.lock().unwrap(), dir.path(), "sub", 3);
    let mut up = Uploader::new(store.clone(), index, dir.path(), "argus", streams, 50.0);
    assert_eq!(up.upload_pending().await.unwrap(), 1);
    assert_eq!(list(&store).await.len(), 7);
}
