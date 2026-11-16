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
  (i64), then the access unit as length-prefixed NAL units. Video only for
  now.

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

**WebTransport** (lower latency, no head-of-line blocking) is planned as a
second transport with the same messages. Safari has it since 26.4; for
Tailscale the `tailscale cert` certificate can be used directly, since
`tailscale serve` doesn't proxy QUIC.

## Phases

1. **Record** (this crate set): RTSP + HTTP-FLV ingest, chunk writer, index.
2. Blob storage: S3 upload by policy, local budget, events from camera
   detections (done); playback from S3, S3 retention (next).
3. Live viewer: WebSocket frame stream and API (done); browser app with
   WebCodecs and egui/WebGPU grid, WebTransport (next).
4. Synced replay: seek API, master clock, scrubbing, reverse.
5. Detection: motion, ONNX object detection (NVDEC decode, no FFmpeg).
6. Floor plans and the AI review agent.
7. Polish: auth behind Tailscale, notifications, Frigate config import.
