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
Today they come from `argus event add --camera X --from T --to T`; camera
motion/AI triggers (Reolink) and Argus's own detection add rows later. An
event added after the fact still works: its main-stream chunks are uploaded
as long as they are still on local disk.

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

## Phases

1. **Record** (this crate set): RTSP + HTTP-FLV ingest, chunk writer, index.
2. Blob storage: S3 upload by policy, local budget (done); events from
   camera triggers, playback from S3, S3 retention (next).
3. Live viewer: WebTransport frame stream, egui/WebGPU grid.
4. Synced replay: seek API, master clock, scrubbing, reverse.
5. Detection: motion, ONNX object detection (NVDEC decode, no FFmpeg).
6. Floor plans and the AI review agent.
7. Polish: auth behind Tailscale, notifications, Frigate config import.
