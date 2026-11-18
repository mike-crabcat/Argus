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
   later, as an option): D-FINE-S (Apache-2.0 code and weights), with
   RT-DETRv2-S or YOLOX-S (both Apache-2.0) as fallbacks. No AGPL models.
   The model file is configured by path and never committed:
   `onnx/model.onnx` from `onnx-community/dfine_s_coco-ONNX`. Pictures are
   resized to 640x640 on each camera's thread, then the model thread takes
   whatever has arrived within 60 ms as one batch. ONNX Runtime comes from
   the `ort` crate's prebuilt binaries; the Docker image adds CUDA 13 and
   cuDNN 9 from NVIDIA's redistributable wheels. COCO classes map to
   Argus's labels: person; car, truck, bus, motorcycle, bicycle as vehicle;
   dog, cat, bird as animal.
3. **Tracking** (`argus-detect::track`, done). Boxes are linked from
   picture to picture by overlap (IoU, best pairs first, within a label);
   a track is real after three sightings and over after 2 s unseen. A
   parked car is detected in every picture, so a track alone is no event:
   what counts is a *moving spell*, from when the box centre first moves a
   quarter of the box's size (at least 12 px) until it has sat still for
   the camera's `hold_ms`. Each spell is stored in the `track` table with
   its boxes (`track_box`, one per picture, in the sub stream's pixels),
   kept 30 days. Per camera, `events.detect` decides what else happens:
   `"shadow"` (the default) only stores, so `/api/detect/compare` can set
   the spells against the camera's own events; `"events"` also makes each
   spell an event (source `argus`, with stills) and stops reading the
   camera's detections.
4. **Pose** (`argus-detect::pose`, done). RTMPose-s (Apache-2.0,
   OpenMMLab's ONNX export `rtmpose-s_simcc-body7_pt-body7_420e-256x192`)
   runs on a close-up of each person box, about 2 ms a person on the GPU.
   The foot point is between the ankles it is sure of (at the lower one),
   or the bottom centre of the box when it is sure of neither; it is
   stored with each track box (`foot_x`, `foot_y`). It is good when the
   feet are in view; when they are hidden (behind a car) it guesses
   below the box, so calibration should prefer points marked `ankles`
   from unobstructed views.

## Rollout

Run in shadow mode for a few days, check `/api/detect/compare?hours=72`
and `/api/detect/tracks`, then switch cameras one at a time with
`events.detect = "events"`.

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
