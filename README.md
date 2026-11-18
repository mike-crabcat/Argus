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

### Browser app

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version <the wasm-bindgen version in Cargo.lock>
scripts/build-web.sh                # -> web/dist
```

Set `web_root = "web/dist"` under `[http]` and open `http://127.0.0.1:8080/`
(add `?token=...` if you set one). Browsers only decode video in a secure
context, so from other machines use HTTPS, e.g. `tailscale serve`.

### Phones

Signed in with the main token, click **Phones** in the top bar, name the
phone and click **Add phone**. Scan the QR code with the phone's camera: it
opens Argus signed in with a token of the phone's own, which the phone
remembers. Set `public_url` under `[http]` to the address phones reach
Argus at (e.g. your `tailscale serve` URL); otherwise the QR code uses the
address the viewer is open at. **Revoke** signs a phone out at once, including
any streams it has open. Device tokens can watch live and replay but can't
add or revoke phones; they are stored hashed in `devices.json` in the
storage root.

### Replay

Click **Replay** in the viewer's top bar: all cameras play back together
from a minute ago. Click the timeline to jump, Space to pause, ←/→ for
10 s. The same data is available as `/api/recordings`, `/api/seek`,
`/api/media/...` and the `/api/replay` WebSocket (see the architecture doc).

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

### Deploy

The workstation runs the latest `main` automatically: GitHub builds a
Docker image after CI passes and a timer here pulls it. See
[docs/deploy.md](docs/deploy.md).

## Layout

| Crate | Purpose |
| --- | --- |
| `argus-media` | Frames, codec config, wall clock, fMP4 muxer |
| `argus-store` | Chunk writer, GOP seek table, SQLite index, S3 uploader |
| `argus-ingest` | RTSP and HTTP-FLV sources |
| `argus-server` | Config, supervision, HTTP API, the `argus` binary |
| `argus-proto` | Live view wire protocol shared with the browser |
| `argus-web` | Browser app: egui on WebGPU/WebGL2, WebCodecs |

## Develop

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

### Agent (optional)

Argus can use a language model to draw a floor plan from a sketch, photo
or exported plan, and to make changes you describe ("the garage is 6.2 m
deep"). Any OpenAI-compatible chat-completions API works (OpenAI,
OpenRouter, Zhipu GLM, a local server): add an `[agent]` section to
argus.toml (see argus.example.toml) with the key in a file of its own.
In the plan editor, **Import from image…** and **Ask** then produce a
draft over the image; change anything, then **Accept** (one undo step) or
**Discard**. Nothing is saved until you accept. Only the main token can use
the agent. Every run is logged in `agent.sqlite` in the storage root
(prompt, tool calls, tokens, cost), and runs stop at the monthly spending
cap. `GET /api/agent/status` shows the model and this month's spending.

### Event stills, summaries and search

When a Reolink camera reports a detection, Argus takes three snapshots
(at the start, +2 s and +5 s) with the camera's Snap API and keeps them in
`events/` in the storage root, until the event's recording leaves local
disk. With an `[agent]` configured, a background worker sends each
finished event's stills to the model, with the camera's place on the floor
plan, and stores a one-line summary, the objects seen and whether it looks
notable. At most one summary per camera per `summary_gap_seconds` (events
in between are merged); the monthly cap pauses it. A camera opts out with
`summarize = false` under `[camera.events]`.

The search box in the top bar (Search on phones) finds events by their
summary, label or camera; click one to replay it. With a model set up, it
can also read the question first ("anyone at the back gate last night").
The API is `GET /api/events/search?q=...`, and stills are served at
`/api/media/events/{id}/{n}.jpg`.

### Alerts, notifications and the daily brief

The **Alerts** button in the top bar sets up notifications on this device
(Web Push; on an iPhone, add Argus to the Home Screen first) and holds the
alert rules and the daily brief. Write a rule in plain English ("a person
at the front door after 10pm"); with an `[agent]`, the model turns it into
a filter (cameras, detections, a local time window, days, and optionally a
question to ask of the event's stills) that you can see and edit. New
events are matched against the filter; the model is only asked the rule's
question, under the spending cap. Each rule fires at most once per
cooldown; a notification opens the viewer at that event's replay. "Test
on last 24 h" shows when a rule would have fired. Only the main token can
change rules. Alerts can also go to an ntfy topic (`[alerts.ntfy]`).

Every morning (07:00 Australia/Perth by default) the model writes a short
brief of the last 24 hours from the event summaries, notable events first;
it is shown under Alerts and sent as a notification. Without a model it is
skipped. Push keys (VAPID) are made on first start in the storage root
(`vapid.key`, chmod 600).

