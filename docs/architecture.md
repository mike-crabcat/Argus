# Architecture

The full design doc lives at
<https://claude.ai/artifact/8YwbuqgtKy2agHJxES7gsW>. In short:

* **No transcoding.** Camera H.264/H.265 is stored and streamed untouched.
* **Storage:** GOP-aligned fragmented MP4, one `moof`+`mdat` per GOP, in
  ~60 s chunk objects. Local disk is the hot tier; S3 is the system of record.
* **Index:** SQLite with a packed per-chunk GOP table, so a seek is one row
  read and one byte-range fetch.
* **Time:** every frame gets a wall-clock timestamp; fragment decode times are
  wall-clock 90 kHz ticks since the Unix epoch, so all cameras share one
  timeline.
* **Frontend:** egui + wgpu (WebGPU) in wasm, WebCodecs hardware decode.
* **AI review:** a cloud video model with floor-plan context per camera.

## Object storage (phase 2)

Chunk files are closed once a minute and never change, so the uploader
copies whole files (`PUT` per chunk) under the same keys they have locally,
plus an optional prefix. Init segments are uploaded once each, before the
first chunk that needs them.

**Policy, per stream** (`camera.upload.<stream>` in `argus.toml`):

| Policy | Uploads | Default for |
| --- | --- | --- |
| `always` | every chunk | `sub` and other streams |
| `events` | chunks overlapping an event, padded 10 s each side | `main` |
| `never` | nothing; local only | |

So the low-resolution stream is always in S3 and full resolution only
around events. For five typical Reolink cameras that is about 3 Mbit/s of
sub streams around the clock (~1 TB/month) plus main-stream minutes around
events.

**Events** live in the index (`event`: camera, start, end, source, label).
They come from:

* **The cameras' own detections.** For Reolink cameras Argus logs in to the
  HTTP API (token in the URL, password only in the login body) and polls
  `GetMdState` + `GetAiState` about once a second, read-only. An event opens
  when a label (person, vehicle, animal; motion if enabled) goes active, is
  written to the index immediately, and its end moves forward on every poll,
  so main-stream chunks upload while it is still going on. It closes 5 s
  after the detection was last seen. Settings are inferred from the stream
  URLs (Reolink FLV/RTSP paths, host, credentials); `[camera.events]`
  overrides them or turns them off. Each camera polls on its own with
  backoff, so one being down doesn't affect the rest.
* **By hand:** `argus event add --camera X --from T --to T`.
* Later, Argus's own detection (phase 5).

An event added after the fact still works: its main-stream chunks are
uploaded as long as they are still on local disk. `argus event watch`
shows what each camera reports, without recording.

**State is all in the index.** `chunk.uploaded_at` and `remote` mark what is
in S3 (same for `init_segment`), so after a restart the uploader resumes
where it stopped, oldest first. A failed upload retries with exponential
backoff (1 s up to 60 s) and holds the queue, so uploads stay in time order.
An average rate cap (token bucket, `max_upload_mbps`, default 50) keeps
catch-up after an outage from filling the uplink; each chunk still goes out
at line rate once its turn comes.

**Local disk** is capped by `storage.local_budget_gb`. Above it, the oldest
chunk files that are safe to lose locally are deleted: already uploaded
(`tier = remote`), `never` streams, or `events` chunks with no event nearby
that are over an hour old (`tier = discarded`). A chunk waiting to upload is
never deleted, so if S3 is unreachable for long the budget is exceeded
(logged) rather than losing recordings. Rows are kept either way.

Not yet: reading `remote` chunks back for playback, and deleting old
objects in S3 (use a bucket lifecycle rule meanwhile).

## Live view (phase 3)

Every camera stream goes through a `LiveStream` fan-out: the recorder and
any number of viewers subscribe to it. It keeps the current codec config
and the frames since the last keyframe, so a new viewer starts decoding
immediately (the cached GOP is marked "replay": decode it, show only the
newest frame). Publishing never waits for subscribers.

`[http]` turns on the HTTP API (default `127.0.0.1:8080`, optional bearer
token, also accepted as `?token=` because browsers can't set headers on
WebSockets):

* `GET /api/cameras`: cameras, streams, codec strings, sizes, audio codec.
* `GET /api/live/{camera}/{stream}`: WebSocket. A text message
  `{"type":"config", codec, width, height, description}` (`description` is
  the base64 `avcC`/`hvcC` record for WebCodecs), then binary frames: kind
  (u8), flags (u8: keyframe, replay), pts (i64, 90 kHz), wall-clock µs
  (i64), server receive time µs (i64), then the access unit as
  length-prefixed NAL units; see `crates/argus-proto`. The client may send
  `{"type":"ping","client_us":...}` and gets a pong with the server time.
  Video only for now.

Each viewer has a bounded queue (90 frames). When it is full the viewer
skips to the next keyframe, so a slow phone or a background tab never
holds up recording or other viewers. When a viewer leaves, the server logs
frames sent, frames dropped and send latency (receipt from the camera to
the socket; tens of microseconds on a LAN).

**Browsers need HTTPS.** WebCodecs only exists in secure contexts, so
plain `http://100.x.y.z:8080` over Tailscale won't decode. Use
`tailscale serve --bg https / http://127.0.0.1:8080`, which gives a real
certificate for the machine's `*.ts.net` name and keeps Argus on
localhost.

**Browser app** (`crates/argus-web`): eframe/egui on wgpu, WebGPU first
and WebGL2 as the fallback. Each grid tile opens
`/api/live/{camera}/sub`, configures a WebCodecs `VideoDecoder` from the
config message (codec string + `avcC`, `optimizeForLatency`, hardware
acceleration "no-preference" so the browser uses its GPU decoder when it has
one), and uploads each decoded `VideoFrame` with
`copy_external_image_to_texture`; egui draws the texture. Only the newest
frame is kept: older decoded frames are closed unshown, and if the decoder
queue grows past 3 frames the tile skips to the next keyframe. A frame is
closed after the queue submit that copies it. Double-click a tile for its
main stream full window (Escape or double-click to return). The page passes
`?token=` from its own URL to the API.

The latency figure on each tile is **server receipt → on screen**: the
client estimates the server clock offset from ping/pong round trips
(keeping the fastest), and at upload computes `server_now - recv_us`. It
leaves out the camera's own encode and network delay, which can't be
measured without filming a clock.

Build it with `scripts/build-web.sh` (wasm-bindgen CLI at the version in
`Cargo.lock`), then set `[http] web_root = "web/dist"`. The wasm bundle is
about 10 MB (2.5 MB gzipped), mostly wgpu's WebGL shader translation and the
three Geist fonts.

**WebTransport** (lower latency, no head-of-line blocking) is planned as a
second transport with the same messages. Safari has it since 26.4; for
Tailscale the `tailscale cert` certificate can be used directly, since
`tailscale serve` doesn't proxy QUIC.

## Synced replay (phase 4)

Recorded fragments carry wall-clock decode times (90 kHz since the epoch),
so frames from different cameras line up with no per-camera offset. The
server reads them back out of the chunk files GOP by GOP using the index's
byte ranges (`fmp4::read_video_samples`).

* `GET /api/recordings?stream=sub&from=&to=`: recorded spans per camera.
* `GET /api/seek?t=&stream=&cameras=`: per camera, the init segment and the
  GOP containing `t` (chunk key, byte offset and length).
* `GET /api/media/{key}`: init segments and chunks, with `Range`.
* `GET /api/replay?t=&stream=&cameras=`: one WebSocket for every camera.
  The server keeps a master clock and sends each camera's frames up to
  800 ms ahead of it (binary kind 2: camera index, keyframe flag, pts,
  wall µs, access unit). The client sends `seek` (with a sequence number,
  echoed in the `replay` reply so stale frames are dropped), `play` and
  `pause`. A seek starts every camera at the GOP containing the target.

The browser decodes ahead and, on every repaint, shows for each camera the
newest decoded frame whose wall time is not after its master clock, so all
tiles show the same instant. If a decoder falls more than 8 frames behind
(32 for the first 1.5 s after a seek, while it works through the GOP up to
the target) it skips to the next keyframe rather than drift out of sync.
The Replay switch in the top bar opens it; the timeline shows the last hour
with recorded spans; Space plays/pauses, ←/→ jump 10 s.

Double-click a replay tile to show that camera full window: the client
sends `{"type":"stream","camera":…,"stream":"main"}`, the server restarts
just that camera's cursor on its main stream at the playhead and answers
with a `stream` message (the client drops that camera's frames until then);
leaving focus switches it back to `sub`.

Replay reads each GOP from the local chunk file when it is there and
otherwise from object storage by byte range (`[s3]`, using the `remote`
key the uploader recorded), so footage evicted from local disk still plays.
Init segments and `/api/media` fall back the same way.

Not yet: speeds other than 1×, reverse, scrubbing previews, or presigned
URLs (S3 reads go through the server).

## Phases

1. **Record** (this crate set): RTSP + HTTP-FLV ingest, chunk writer, index.
2. Blob storage: S3 upload by policy, local budget, events from camera
   detections (done); playback from S3, S3 retention (next).
3. Live viewer: WebSocket frame stream and API, browser grid with
   WebCodecs decode on egui/WebGPU (done); WebTransport, audio (next).
4. Synced replay: seek API, replay stream with a master clock, timeline
   with play/pause/seek (done); main streams, S3 chunks, speeds, reverse,
   scrubbing (next).
5. Detection: motion, ONNX object detection (NVDEC decode, no FFmpeg).
6. Floor plans and the AI review agent.
7. Polish: auth behind Tailscale, notifications, Frigate config import.
