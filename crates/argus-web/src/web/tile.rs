//! One live video: WebSocket → WebCodecs `VideoDecoder` → wgpu texture.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use argus_proto::{ClientText, ClockSync, FrameHeader, ServerText};
use eframe::egui;
use eframe::egui_wgpu::RenderState;
use eframe::wgpu;
use js_sys::{ArrayBuffer, Uint8Array};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{
    BinaryType, CodecState, EncodedVideoChunk, EncodedVideoChunkInit, EncodedVideoChunkType,
    HardwareAcceleration, MessageEvent, VideoDecoder, VideoDecoderConfig, VideoDecoderInit,
    VideoFrame, WebSocket,
};

use super::{now_ms, now_us, ws_url};
use crate::stats::{Rate, Smoothed};

/// More frames than this waiting in the decoder means it can't keep up:
/// skip to the next keyframe rather than fall behind.
const MAX_DECODE_QUEUE: u32 = 3;
/// How often to re-sync clocks with the server.
const PING_MS: f64 = 2000.0;

/// State shared with the browser callbacks.
struct Shared {
    ctx: egui::Context,
    decoder: Option<VideoDecoder>,
    configured: bool,
    waiting_key: bool,
    /// Headers of frames in the decoder, by timestamp, to match outputs.
    pending: VecDeque<(i64, FrameHeader)>,
    /// The newest decoded frame, not yet uploaded.
    latest: Option<(VideoFrame, FrameHeader)>,
    clock: ClockSync,
    pub size: Option<(u32, u32)>,
    pub codec: Option<String>,
    pub status: Status,
    received: Rate,
    decoded: Rate,
    pub dropped: u64,
    /// Server receive → on screen, ms (server clock).
    pub latency_ms: Smoothed,
    /// Frame wall-clock time → on screen, ms (server clock).
    pub age_ms: Smoothed,
    last_frame_ms: f64,
    closed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connecting,
    Live,
    Error(String),
}

pub struct Tile {
    pub camera: String,
    pub stream: String,
    shared: Rc<RefCell<Shared>>,
    ws: WebSocket,
    texture: Option<Texture>,
    /// The frame whose upload was queued last update. wgpu performs the
    /// copy at the next queue submit (egui's paint), so it may only be
    /// closed after that: here, at the start of the following update.
    in_flight: Option<VideoFrame>,
    shown: Rate,
    last_ping_ms: f64,
    pub opened_ms: f64,
    // Keep the JS callbacks alive as long as the tile.
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(JsValue)>,
    _on_output: Closure<dyn FnMut(VideoFrame)>,
    _on_error: Closure<dyn FnMut(JsValue)>,
}

pub(crate) struct Texture {
    texture: wgpu::Texture,
    id: egui::TextureId,
    size: (u32, u32),
}

/// What the UI shows for a tile.
pub struct View {
    pub texture: Option<(egui::TextureId, (u32, u32))>,
    pub status: Status,
    pub fps: f64,
    pub latency_ms: Option<f64>,
    pub age_ms: Option<f64>,
    pub dropped: u64,
    pub codec: Option<String>,
    pub live: bool,
}

impl Tile {
    pub fn open(ctx: &egui::Context, camera: &str, stream: &str, token: Option<&str>) -> Self {
        let shared = Rc::new(RefCell::new(Shared {
            ctx: ctx.clone(),
            decoder: None,
            configured: false,
            waiting_key: true,
            pending: VecDeque::new(),
            latest: None,
            clock: ClockSync::default(),
            size: None,
            codec: None,
            status: Status::Connecting,
            received: Rate::new(2000.0),
            decoded: Rate::new(2000.0),
            dropped: 0,
            latency_ms: Smoothed::default(),
            age_ms: Smoothed::default(),
            last_frame_ms: 0.0,
            closed: false,
        }));

        // Decoder callbacks: keep only the newest frame.
        let s = shared.clone();
        let on_output = Closure::<dyn FnMut(VideoFrame)>::new(move |frame: VideoFrame| {
            let mut sh = s.borrow_mut();
            if sh.closed {
                frame.close();
                return;
            }
            let ts = frame.timestamp() as i64;
            while sh.pending.front().is_some_and(|(t, _)| *t < ts) {
                sh.pending.pop_front();
            }
            let Some((_, header)) = sh.pending.pop_front() else {
                frame.close();
                return;
            };
            if !header.replay {
                sh.decoded.tick(now_ms());
            }
            if let Some((old, old_header)) = sh.latest.replace((frame, header)) {
                old.close();
                if !old_header.replay {
                    sh.dropped += 1; // decoded but never shown: superseded
                }
            }
            sh.ctx.request_repaint();
        });
        let s = shared.clone();
        let on_error = Closure::<dyn FnMut(JsValue)>::new(move |e: JsValue| {
            let mut sh = s.borrow_mut();
            sh.status = Status::Error(format!("decoder: {}", js_message(&e)));
            sh.configured = false;
            sh.ctx.request_repaint();
        });
        let init = VideoDecoderInit::new(
            on_error.as_ref().unchecked_ref(),
            on_output.as_ref().unchecked_ref(),
        );
        match VideoDecoder::new(&init) {
            Ok(d) => shared.borrow_mut().decoder = Some(d),
            Err(e) => {
                shared.borrow_mut().status = Status::Error(format!(
                    "WebCodecs unavailable ({}); HTTPS is required",
                    js_message(&e)
                ))
            }
        }

        let path = format!("/api/live/{camera}/{stream}");
        let ws = WebSocket::new(&ws_url(&path, token)).expect("WebSocket");
        ws.set_binary_type(BinaryType::Arraybuffer);

        let s = shared.clone();
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
            let data = ev.data();
            if let Some(text) = data.as_string() {
                on_text(&s, &text);
            } else if let Ok(buf) = data.dyn_into::<ArrayBuffer>() {
                on_binary(&s, &buf);
            }
        });
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        let s = shared.clone();
        let on_close = Closure::<dyn FnMut(JsValue)>::new(move |_| {
            let mut sh = s.borrow_mut();
            if !sh.closed && !matches!(sh.status, Status::Error(_)) {
                sh.status = Status::Error("disconnected".into());
            }
            sh.ctx.request_repaint();
        });
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));

        Self {
            camera: camera.to_owned(),
            stream: stream.to_owned(),
            shared,
            ws,
            texture: None,
            in_flight: None,
            shown: Rate::new(2000.0),
            last_ping_ms: f64::MIN,
            opened_ms: now_ms(),
            _on_message: on_message,
            _on_close: on_close,
            _on_output: on_output,
            _on_error: on_error,
        }
    }

    pub fn failed(&self) -> bool {
        matches!(self.shared.borrow().status, Status::Error(_))
    }

    /// Uploads the newest decoded frame, if any, and sends clock pings.
    pub fn update(&mut self, rs: &RenderState) {
        if let Some(f) = self.in_flight.take() {
            f.close();
        }
        let now = now_ms();
        if now - self.last_ping_ms > PING_MS && self.ws.ready_state() == WebSocket::OPEN {
            self.last_ping_ms = now;
            let ping = ClientText::Ping {
                client_us: now_us(),
            };
            let _ = self
                .ws
                .send_with_str(&serde_json::to_string(&ping).unwrap());
        }
        let latest = self.shared.borrow_mut().latest.take();
        let Some((frame, header)) = latest else {
            return;
        };
        let size = (frame.display_width(), frame.display_height());
        if size.0 == 0 || size.1 == 0 {
            frame.close();
            return;
        }
        upload(rs, &mut self.texture, &frame, size);
        self.in_flight = Some(frame);
        let mut sh = self.shared.borrow_mut();
        sh.size = Some(size);
        sh.last_frame_ms = now;
        if !header.replay {
            self.shown.tick(now);
            if let Some(offset) = sh.clock.offset_us() {
                let server_now = now_us() + offset;
                sh.latency_ms
                    .add((server_now - header.recv_us) as f64 / 1000.0);
                sh.age_ms.add((server_now - header.wall_us) as f64 / 1000.0);
            }
        }
    }

    pub fn view(&mut self) -> View {
        let now = now_ms();
        let fps = self.shown.per_sec(now);
        let mut sh = self.shared.borrow_mut();
        let _ = sh.received.per_sec(now);
        View {
            texture: self.texture.as_ref().map(|t| (t.id, t.size)),
            status: sh.status.clone(),
            fps,
            latency_ms: sh.latency_ms.get(),
            age_ms: sh.age_ms.get(),
            dropped: sh.dropped,
            codec: sh.codec.clone(),
            live: now - sh.last_frame_ms < 2000.0 && sh.last_frame_ms > 0.0,
        }
    }

    pub fn decoded_fps(&self) -> f64 {
        self.shared.borrow_mut().decoded.per_sec(now_ms())
    }

    pub fn received_fps(&self) -> f64 {
        self.shared.borrow_mut().received.per_sec(now_ms())
    }

    /// Frees the GPU texture registration. Call before dropping.
    pub fn release(&mut self, rs: &RenderState) {
        if let Some(t) = self.texture.take() {
            rs.renderer.write().free_texture(&t.id);
        }
    }
}

impl Drop for Tile {
    fn drop(&mut self) {
        let mut sh = self.shared.borrow_mut();
        sh.closed = true;
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
        let _ = self.ws.close();
        if let Some(d) = sh.decoder.take() {
            if d.state() != CodecState::Closed {
                let _ = d.close();
            }
        }
        if let Some((f, _)) = sh.latest.take() {
            f.close();
        }
        if let Some(f) = self.in_flight.take() {
            f.close();
        }
    }
}

/// Copies a decoded frame into `texture` (created or resized as needed).
/// wgpu performs the copy at the next queue submit, so the caller must keep
/// `frame` open until then.
pub(crate) fn upload(
    rs: &RenderState,
    texture: &mut Option<Texture>,
    frame: &VideoFrame,
    size: (u32, u32),
) {
    if texture.as_ref().is_none_or(|t| t.size != size) {
        *texture = Some(new_texture(rs, size, texture.as_ref().map(|t| t.id)));
    }
    let tex = texture.as_ref().unwrap();
    rs.queue.copy_external_image_to_texture(
        &wgpu::CopyExternalImageSourceInfo {
            source: wgpu::ExternalImageSource::VideoFrame(Clone::clone(frame)),
            origin: wgpu::Origin2d::ZERO,
            flip_y: false,
        },
        wgpu::CopyExternalImageDestInfo {
            texture: &tex.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
            color_space: wgpu::PredefinedColorSpace::Srgb,
            premultiplied_alpha: false,
        },
        wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
    );
}

impl Texture {
    pub(crate) fn id_and_size(&self) -> (egui::TextureId, (u32, u32)) {
        (self.id, self.size)
    }

    pub(crate) fn free(self, rs: &RenderState) {
        rs.renderer.write().free_texture(&self.id);
    }
}

/// Configures a WebCodecs decoder from a config message.
pub(crate) fn configure(
    dec: &VideoDecoder,
    codec: &str,
    width: u16,
    height: u16,
    description: &str,
) -> Result<(), String> {
    let cfg = VideoDecoderConfig::new(codec);
    if let Some(desc) = argus_proto::base64_decode(description) {
        cfg.set_description(&Uint8Array::from(&desc[..]));
    }
    cfg.set_coded_width(u32::from(width));
    cfg.set_coded_height(u32::from(height));
    // "no-preference" uses the GPU decoder when there is one;
    // "prefer-hardware" is rejected outright where there isn't
    // (headless browsers, some VMs), so it would show nothing.
    cfg.set_hardware_acceleration(HardwareAcceleration::NoPreference);
    cfg.set_optimize_for_latency(true);
    dec.configure(&cfg)
        .map_err(|e| format!("unsupported codec {codec}: {}", js_message(&e)))
}

fn new_texture(rs: &RenderState, size: (u32, u32), reuse: Option<egui::TextureId>) -> Texture {
    let texture = rs.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("camera"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        // copy_external_image_to_texture needs RENDER_ATTACHMENT too.
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut renderer = rs.renderer.write();
    let id = match reuse {
        Some(id) => {
            renderer.update_egui_texture_from_wgpu_texture(
                &rs.device,
                &view,
                wgpu::FilterMode::Linear,
                id,
            );
            id
        }
        None => renderer.register_native_texture(&rs.device, &view, wgpu::FilterMode::Linear),
    };
    Texture { texture, id, size }
}

fn on_text(s: &Rc<RefCell<Shared>>, text: &str) {
    let Ok(msg) = serde_json::from_str::<ServerText>(text) else {
        return;
    };
    let mut sh = s.borrow_mut();
    match msg {
        ServerText::Config {
            codec,
            width,
            height,
            description,
            ..
        } => {
            let Some(dec) = sh.decoder.clone() else {
                return;
            };
            match configure(&dec, &codec, width, height, &description) {
                Ok(()) => {
                    sh.configured = true;
                    sh.waiting_key = true;
                    sh.codec = Some(codec);
                    sh.status = Status::Live;
                }
                Err(e) => sh.status = Status::Error(e),
            }
        }
        ServerText::Discontinuity => sh.waiting_key = true,
        ServerText::Replay { .. } => {} // replay socket only
        ServerText::Pong {
            client_us,
            server_us,
        } => sh.clock.add(client_us, server_us, now_us()),
    }
}

fn on_binary(s: &Rc<RefCell<Shared>>, buf: &ArrayBuffer) {
    let mut head = [0u8; argus_proto::HEADER_LEN];
    if buf.byte_length() < head.len() as u32 {
        return;
    }
    Uint8Array::new_with_byte_offset_and_length(buf, 0, head.len() as u32).copy_to(&mut head);
    let Ok((header, _)) = argus_proto::parse_frame(&head) else {
        return;
    };
    let mut sh = s.borrow_mut();
    if !header.replay {
        sh.received.tick(now_ms());
    }
    let Some(dec) = sh.decoder.clone() else {
        return;
    };
    if !sh.configured || dec.state() != CodecState::Configured {
        return;
    }
    // Never let the decoder fall behind: drop to the next keyframe.
    if !header.keyframe && (sh.waiting_key || dec.decode_queue_size() > MAX_DECODE_QUEUE) {
        sh.waiting_key = true;
        sh.dropped += 1;
        return;
    }
    sh.waiting_key = false;
    let data = Uint8Array::new_with_byte_offset(buf, argus_proto::HEADER_LEN as u32);
    let kind = if header.keyframe {
        EncodedVideoChunkType::Key
    } else {
        EncodedVideoChunkType::Delta
    };
    // web-sys types the constructor's timestamp as i32 (35 min of µs);
    // set the real 64-bit value through the f64 setter.
    let init = EncodedVideoChunkInit::new(&data, 0, kind);
    init.set_timestamp_f64(header.pts_us() as f64);
    let Ok(chunk) = EncodedVideoChunk::new(&init) else {
        return;
    };
    sh.pending.push_back((header.pts_us(), header));
    if sh.pending.len() > 64 {
        sh.pending.pop_front();
    }
    if let Err(e) = dec.decode(&chunk) {
        sh.status = Status::Error(format!("decode: {}", js_message(&e)));
    }
}

fn js_message(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &"message".into())
        .ok()
        .and_then(|m| m.as_string())
        .or_else(|| e.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}
