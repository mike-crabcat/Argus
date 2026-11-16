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

## Phases

1. **Record** (this crate set): RTSP + HTTP-FLV ingest, chunk writer, index.
2. Blob storage: S3 upload, hot cache, retention.
3. Live viewer: WebTransport frame stream, egui/WebGPU grid.
4. Synced replay: seek API, master clock, scrubbing, reverse.
5. Detection: motion, ONNX object detection (NVDEC decode, no FFmpeg).
6. Floor plans and the AI review agent.
7. Polish: auth behind Tailscale, notifications, Frigate config import.
