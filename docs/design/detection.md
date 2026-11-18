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
2. **Detector** (`argus-detect::model`, done; findings only logged so far).
   An ONNX model run with `ort` on the CUDA execution provider (TensorRT
   later, as an option): D-FINE-S (Apache-2.0 code and weights), with RT-
   DETRv2-S or YOLOX-S (both Apache-2.0) as fallbacks. No AGPL models. The
   model file is configured by path and never committed: `onnx/model.onnx`
   from `onnx-community/dfine_s_coco-ONNX`. Pictures are resized to 640x640
   on each camera's thread, then the model thread takes whatever has
   arrived within 60 ms as one batch. ONNX Runtime comes from the `ort`
   crate's prebuilt binaries; the Docker image adds CUDA 13 and cuDNN 9
   from NVIDIA's redistributable wheels. COCO classes map to Argus's
   labels: person; car, truck, bus, motorcycle, bicycle as vehicle; dog,
   cat, bird as animal.
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

Measured live on five cameras at 5 pictures a second each, with Frigate
still running on the same RTX 3060: the GPU 17-23% busy with batching
(31-55% without), about 1.1 GB of VRAM, and about 0.7 of one CPU core
for decoding, resizing and the model's host side. On its own, D-FINE-S
takes 12 ms a picture in batches (15 ms one at a time; 130 ms on the
CPU). An FP16 export was no faster under CUDA; TensorRT FP16 is the
next step if the GPU gets busy.

## Configuration

```toml
[detect]
fps = 5          # pictures a second to look at, per camera
stream = "sub"   # which stream to decode (H.264)
cameras = []     # empty: all
```
