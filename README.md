# Argus

A Rust NVR built to replace Frigate: zero-transcode recording to S3-compatible
blob storage, and a WebGPU frontend for frame-locked multi-camera replay.

**Status:** phase 1 (recording) of 7. See [docs/architecture.md](docs/architecture.md)
and the [design system](docs/design/README.md).

## What works today

* Ingest over **RTSP** (H.264/H.265, via [retina](https://github.com/scottlamb/retina))
  and **HTTP-FLV** (H.264, the reliable path on Reolink cameras).
* Video is never decoded or re-encoded. Frames are re-framed into
  fragmented MP4, one fragment per GOP, in fixed-length chunk files.
* A SQLite index maps (camera, stream, time) to the exact byte range of the
  GOP to fetch, which is what makes seeking fast.
* Automatic reconnects with backoff; chunks close cleanly on shutdown.

No FFmpeg is needed at runtime. The test suite uses `ffprobe`, if it's installed, to
check that stored chunks decode.

## Run

```sh
cp argus.example.toml argus.toml   # add your cameras
cargo run --release --bin argus -- -c argus.toml
```

Recordings land under `storage.root` using the same keys they'll have in S3:

```text
rec/{camera}/{stream}/{yyyy}/{mm}/{dd}/{hh}/{start_ms}.m4s
init/{camera}/{stream}/{fingerprint}.mp4
index.sqlite
```

To play a chunk locally: `cat init/.../X.mp4 rec/.../Y.m4s > clip.mp4`.

With an `[s3]` section, closed chunks are also uploaded: the `sub` stream
always, `main` only around events (see `argus.example.toml` and
[docs/architecture.md](docs/architecture.md)). Reolink cameras' own
person/vehicle/animal detections become events automatically
(`argus event watch` shows what they report). Mark an event by hand with:

```sh
argus -c argus.toml event add --camera driveway --from now-5m --to now
```

### Live view

With an `[http]` section, `GET /api/cameras` lists cameras and
`/api/live/{camera}/{stream}` streams live frames over WebSocket (see
[docs/architecture.md](docs/architecture.md)). To try it:

```sh
ARGUS_TOKEN=... cargo run --release --example live_probe -- \
  ws://127.0.0.1:8080/api/live/driveway/sub 10
```

### Reolink cameras

* Use the FLV URL for H.264 streams:
  `http://CAM/flv?port=1935&app=bcs&stream=channel0_main.bcs&user=USER&password=PASS`
  (`channel0_sub.bcs` for the substream).
* 4K models send H.265 on the main stream, which is RTSP only:
  `rtsp://USER:PASS@CAM:554/h265Preview_01_main`.
* Set the I-frame interval to 1x the frame rate so every second is seekable.

## Layout

| Crate | Purpose |
| --- | --- |
| `argus-media` | Frames, codec config, wall clock, fMP4 muxer |
| `argus-store` | Chunk writer, GOP seek table, SQLite index, S3 uploader |
| `argus-ingest` | RTSP and HTTP-FLV sources |
| `argus-server` | Config, supervision, the `argus` binary |

## Develop

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```
