//! Connects to a live stream and reports what arrives:
//!
//! ```sh
//! ARGUS_TOKEN=... cargo run --example live_probe -- ws://127.0.0.1:8080/api/live/doorbell/sub 20
//! ```
//!
//! Prints the codec config, whether the first frame is a keyframe, frame
//! rate, bitrate, and latency from the frame's wall-clock time (camera
//! frame time as Argus mapped it) to arrival here.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let url = args.next().expect("usage: live_probe WS_URL [SECONDS]");
    let secs: u64 = args.next().map_or(10, |s| s.parse().unwrap());
    let url = match std::env::var("ARGUS_TOKEN") {
        Ok(t) => format!("{url}?token={t}"),
        Err(_) => url,
    };
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");
    let start = Instant::now();
    let end = start + Duration::from_secs(secs);
    let (mut frames, mut replay, mut bytes, mut keyframes) = (0u64, 0u64, 0u64, 0u64);
    let mut first_frame = None;
    let mut lat_ms = Vec::new();
    while let Ok(Some(msg)) = tokio::time::timeout_at(end.into(), ws.next()).await {
        match msg.expect("websocket") {
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                println!(
                    "{:>6} ms  text: type={} codec={} {}x{}",
                    start.elapsed().as_millis(),
                    v["type"],
                    v["codec"],
                    v["width"],
                    v["height"]
                );
            }
            Message::Binary(b) => {
                let (h, _) = argus_proto::parse_frame(&b).expect("frame");
                let (key, is_replay, wall_us) = (h.keyframe, h.replay, h.wall_us);
                if first_frame.is_none() {
                    first_frame = Some((start.elapsed(), key, is_replay));
                }
                if is_replay {
                    replay += 1;
                    continue;
                }
                let now_us = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_micros() as i64;
                lat_ms.push((now_us - wall_us) as f64 / 1000.0);
                frames += 1;
                keyframes += u64::from(key);
                bytes += b.len() as u64;
            }
            _ => {}
        }
    }
    let el = start.elapsed().as_secs_f64();
    lat_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| {
        lat_ms
            .get(((lat_ms.len() as f64 - 1.0) * p) as usize)
            .copied()
            .unwrap_or(0.0)
    };
    if let Some((t, key, r)) = first_frame {
        println!(
            "first frame after {} ms: keyframe={key} replay={r}",
            t.as_millis()
        );
    }
    println!(
        "{frames} live frames ({keyframes} keyframes) + {replay} replayed in {el:.1} s: {:.1} fps, {:.2} Mbit/s",
        frames as f64 / el,
        bytes as f64 * 8.0 / el / 1e6
    );
    println!(
        "wall-clock time to here: p50 {:.1} ms, p90 {:.1} ms, p99 {:.1} ms, max {:.1} ms",
        pct(0.5),
        pct(0.9),
        pct(0.99),
        pct(1.0)
    );
}
