# Object detection

Argus finds people, vehicles and animals itself, on the GPU, instead of
relying on each camera's own detections. This is the plan; the parts are
landing one PR at a time.

## Pipeline

1. **Decoding** (`argus-detect::decode`, done). Each camera's sub stream
   (896x672 H.264 on the Reolinks here) is decoded on the CPU with
   OpenH264: Cisco's BSD-licensed decoder, built from source by the
   `openh264` crate. No ffmpeg, no GPU. Every frame is decoded (later
   frames depend on earlier ones), but only the frames a `Sampler` picks,
   5 a second by default, are converted to RGB. Measured on recordings from
   five cameras: 0.5 to 1.1 ms a frame, about 1 to 2% of one core per
   camera. NVDEC would save little at this size; it's worth revisiting only
   for main streams.
2. **Detector** (next). An ONNX model run with `ort` on the CUDA execution
   provider (TensorRT later, as an option): D-FINE-S (Apache-2.0 code and
   weights), with RT-DETRv2-S or YOLOX-S (both Apache-2.0) as fallbacks.
   No AGPL models. Weights are downloaded by URL and checked against a
   SHA-256, never committed. COCO classes map to Argus's labels: person;
   car, truck, bus, motorcycle, bicycle as vehicle; dog, cat, bird as
   animal.
3. **Tracking.** An IoU/ByteTrack-style tracker links boxes into tracks; a
   track seen for three or more pictures becomes an event in the existing
   event table, so stills, summaries, alerts and search work unchanged.
   Each track's boxes, scores and foot points go in a `detection` table.
4. **Pose.** RTMPose-s (Apache-2.0, MMPose) on person boxes only. The foot
   point is the middle of the two ankles, or the bottom centre of the box
   when the ankles aren't seen; it is what places people on the floor plan
   once cameras are calibrated.

## Rollout

A camera's `events.source` becomes `"reolink"`, `"argus"`, `"both"`, or
`"shadow"` (Argus detects and stores tracks but makes no events), so the
two can be compared for a few days before switching.

## Load

At 5 pictures a second from five cameras: 25 inferences a second, batched
per tick. D-FINE-S takes about 6-8 ms a picture on an RTX 3060 (FP32,
CUDA), so the GPU is busy 15-20% of the time, less with TensorRT FP16;
about 1 GB of VRAM. CPU: well under one core in all, mostly decoding.

## Configuration

```toml
[detect]
fps = 5          # pictures a second to look at, per camera
stream = "sub"   # which stream to decode (H.264)
cameras = []     # empty: all
```
