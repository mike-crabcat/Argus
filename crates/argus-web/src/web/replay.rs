//! Synced replay: one WebSocket for every camera, one decoder each, and a
//! master clock. Each camera shows its newest decoded frame whose wall time
//! is not after the master clock, so all tiles show the same instant.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use argus_proto::{ClientText, ServerText};
use eframe::egui;
use eframe::egui_wgpu::RenderState;
use js_sys::{ArrayBuffer, Uint8Array};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{
    BinaryType, CodecState, EncodedVideoChunk, EncodedVideoChunkInit, EncodedVideoChunkType,
    MessageEvent, VideoDecoder, VideoDecoderInit, VideoFrame, WebSocket,
};

use super::tile::{Texture, configure, upload};
use super::{now_ms, ws_url};

/// More frames than this waiting in a decoder means it can't keep up with
/// real time: skip to the next keyframe rather than drift out of sync.
const MAX_DECODE_QUEUE: u32 = 8;
/// Right after a seek the server sends the GOP up to the target in one
/// burst; let the decoder work through it instead of skipping.
const SEEK_DECODE_QUEUE: u32 = 32;
const SEEK_GRACE_MS: f64 = 1500.0;
/// Sync spread is reported over this many recent repaints.
const SPREAD_WINDOW: usize = 120;

/// Decoded frames held per camera at most (memory guard; the server sends
/// under a second ahead).
const MAX_READY: usize = 48;

/// The client's copy of the playback position.
#[derive(Clone, Copy, Debug)]
struct Master {
    base_ms: f64,
    since: f64,
    playing: bool,
}

impl Master {
    fn now_ms(&self) -> f64 {
        if self.playing {
            self.base_ms + (now_ms() - self.since)
        } else {
            self.base_ms
        }
    }
}

struct Dec {
    decoder: VideoDecoder,
    configured: bool,
    waiting_key: bool,
    /// Decoded frames waiting for their time, oldest first (timestamp is
    /// the frame's wall time in µs).
    ready: VecDeque<VideoFrame>,
    codec: Option<String>,
    /// Wall time (ms) of the last frame received for this camera.
    last_recv_ms: Option<f64>,
    /// Frames skipped because the decoder fell behind.
    skipped: u64,
    _on_output: Closure<dyn FnMut(VideoFrame)>,
    _on_error: Closure<dyn FnMut(JsValue)>,
}

impl Dec {
    fn clear(&mut self) {
        for f in self.ready.drain(..) {
            f.close();
        }
    }
}

struct Shared {
    ctx: egui::Context,
    cameras: Vec<String>,
    decs: Vec<Dec>,
    /// The seek we're waiting for; frames before its `replay` reply are
    /// stale.
    seq: u32,
    active: bool,
    master: Master,
    error: Option<String>,
    /// When the current seek was sent (page clock, ms).
    seek_sent_ms: f64,
}

/// What a camera tile shows during replay.
pub struct CamView {
    pub camera: String,
    pub texture: Option<(egui::TextureId, (u32, u32))>,
    /// Master clock minus the shown frame's wall time.
    pub behind_ms: Option<f64>,
    pub waiting: bool,
}

pub struct Replay {
    ws: WebSocket,
    shared: Rc<RefCell<Shared>>,
    textures: Vec<Option<Texture>>,
    in_flight: Vec<Option<VideoFrame>>,
    /// Wall time (ms) of the frame each camera shows.
    shown_wall: Vec<Option<f64>>,
    /// Seek-to-first-frame per camera for the current seek, ms.
    first_frame: Vec<Option<f64>>,
    /// Spread of shown frames' wall times across cameras, ms, recent.
    spread: VecDeque<f64>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
}

impl Replay {
    pub fn open(
        ctx: &egui::Context,
        cameras: &[String],
        stream: &str,
        t_ms: i64,
        token: Option<&str>,
    ) -> Self {
        let shared = Rc::new(RefCell::new(Shared {
            ctx: ctx.clone(),
            cameras: Vec::new(),
            decs: Vec::new(),
            seq: 0,
            active: false,
            master: Master {
                base_ms: t_ms as f64,
                since: now_ms(),
                playing: true,
            },
            error: None,
            seek_sent_ms: now_ms(),
        }));
        let path = format!(
            "/api/replay?stream={stream}&t={t_ms}&cameras={}",
            cameras.join(",")
        );
        // ws_url adds ?token=; our path already has a query.
        let url = ws_url(&path, token).replacen("?token=", "&token=", 1);
        let ws = WebSocket::new(&url).expect("WebSocket");
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
        let n = cameras.len();
        Self {
            ws,
            shared,
            textures: (0..n).map(|_| None).collect(),
            in_flight: (0..n).map(|_| None).collect(),
            shown_wall: vec![None; n],
            first_frame: vec![None; n],
            spread: VecDeque::new(),
            _on_message: on_message,
        }
    }

    pub fn position_ms(&self) -> f64 {
        self.shared.borrow().master.now_ms()
    }

    pub fn playing(&self) -> bool {
        self.shared.borrow().master.playing
    }

    pub fn error(&self) -> Option<String> {
        self.shared.borrow().error.clone()
    }

    pub fn seek(&mut self, t_ms: f64) {
        let mut sh = self.shared.borrow_mut();
        sh.seq += 1;
        sh.active = false;
        sh.seek_sent_ms = now_ms();
        sh.master.base_ms = t_ms;
        sh.master.since = now_ms();
        for d in &mut sh.decs {
            d.clear();
            if d.decoder.state() != CodecState::Closed {
                let _ = d.decoder.reset();
            }
            d.configured = false;
            d.waiting_key = true;
        }
        let msg = ClientText::Seek {
            t_ms: t_ms as i64,
            seq: sh.seq,
        };
        let _ = self.ws.send_with_str(&serde_json::to_string(&msg).unwrap());
        self.first_frame.iter_mut().for_each(|f| *f = None);
        self.shown_wall.iter_mut().for_each(|f| *f = None);
        self.spread.clear();
    }

    pub fn set_playing(&mut self, playing: bool) {
        let mut sh = self.shared.borrow_mut();
        if sh.master.playing == playing {
            return;
        }
        sh.master = Master {
            base_ms: sh.master.now_ms(),
            since: now_ms(),
            playing,
        };
        let msg = if playing {
            ClientText::Play
        } else {
            ClientText::Pause
        };
        let _ = self.ws.send_with_str(&serde_json::to_string(&msg).unwrap());
    }

    /// Picks each camera's frame for the master clock and uploads it.
    pub fn update(&mut self, rs: &RenderState) {
        for f in self.in_flight.iter_mut() {
            if let Some(f) = f.take() {
                f.close();
            }
        }
        let mut sh = self.shared.borrow_mut();
        let master = sh.master.now_ms();
        let seek_sent = sh.seek_sent_ms;
        let active = sh.active;
        for (i, d) in sh.decs.iter_mut().enumerate() {
            if i >= self.textures.len() {
                break;
            }
            let mut show = None;
            while d
                .ready
                .front()
                .is_some_and(|f| f.timestamp() / 1000.0 <= master)
            {
                if let Some(old) = show.replace(d.ready.pop_front().unwrap()) {
                    old.close();
                }
            }
            let Some(frame) = show else { continue };
            let size = (frame.display_width(), frame.display_height());
            if size.0 == 0 || size.1 == 0 {
                frame.close();
                continue;
            }
            upload(rs, &mut self.textures[i], &frame, size);
            self.shown_wall[i] = Some(frame.timestamp() / 1000.0);
            if active && self.first_frame[i].is_none() {
                self.first_frame[i] = Some(now_ms() - seek_sent);
            }
            self.in_flight[i] = Some(frame);
        }
        if sh.master.playing {
            sh.ctx.request_repaint();
        }
        drop(sh);
        // Inter-camera spread of what's on screen, once every camera shows
        // something from this seek.
        let walls: Vec<f64> = self.shown_wall.iter().flatten().copied().collect();
        if !walls.is_empty() && walls.len() == self.shown_wall.len() {
            let (lo, hi) = walls
                .iter()
                .fold((f64::MAX, f64::MIN), |(a, b), &w| (a.min(w), b.max(w)));
            self.spread.push_back(hi - lo);
            if self.spread.len() > SPREAD_WINDOW {
                self.spread.pop_front();
            }
        }
    }

    pub fn view(&self, i: usize) -> CamView {
        let sh = self.shared.borrow();
        let master = sh.master.now_ms();
        CamView {
            camera: sh.cameras.get(i).cloned().unwrap_or_default(),
            texture: self
                .textures
                .get(i)
                .and_then(|t| t.as_ref().map(|t| t.id_and_size())),
            behind_ms: self
                .shown_wall
                .get(i)
                .copied()
                .flatten()
                .map(|w| master - w),
            waiting: !sh.active || self.shown_wall.get(i).copied().flatten().is_none(),
        }
    }

    /// Numbers for `window.argusStats`.
    pub fn stats(&self) -> serde_json::Value {
        let sh = self.shared.borrow();
        let mut spread: Vec<f64> = self.spread.iter().copied().collect();
        spread.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| {
            spread
                .get(((spread.len() as f64 - 1.0) * p).round().max(0.0) as usize)
                .copied()
        };
        serde_json::json!({
            "position_ms": sh.master.now_ms(),
            "playing": sh.master.playing,
            "cameras": sh.cameras,
            "codecs": sh.decs.iter().map(|d| d.codec.clone()).collect::<Vec<_>>(),
            "seek_to_first_frame_ms": self.first_frame,
            "shown_wall_ms": self.shown_wall,
            "recv_ahead_ms": sh.decs.iter().map(|d| d.last_recv_ms.map(|r| r - sh.master.now_ms())).collect::<Vec<_>>(),
            "decode_queue": sh.decs.iter().map(|d| d.decoder.decode_queue_size()).collect::<Vec<_>>(),
            "ready": sh.decs.iter().map(|d| d.ready.len()).collect::<Vec<_>>(),
            "skipped": sh.decs.iter().map(|d| d.skipped).collect::<Vec<_>>(),
            "ready_span_ms": sh.decs.iter().map(|d| d.ready.front().zip(d.ready.back()).map(|(a, b)| (b.timestamp() - a.timestamp()) / 1000.0)).collect::<Vec<_>>(),
            "sync_spread_ms": {"p50": pct(0.5), "p95": pct(0.95), "max": pct(1.0), "samples": spread.len()},
            "error": sh.error,
        })
    }

    pub fn release(&mut self, rs: &RenderState) {
        for t in self.textures.iter_mut() {
            if let Some(t) = t.take() {
                t.free(rs);
            }
        }
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        self.ws.set_onmessage(None);
        let _ = self.ws.close();
        for f in self.in_flight.iter_mut() {
            if let Some(f) = f.take() {
                f.close();
            }
        }
        let mut sh = self.shared.borrow_mut();
        for d in sh.decs.iter_mut() {
            d.clear();
            if d.decoder.state() != CodecState::Closed {
                let _ = d.decoder.close();
            }
        }
    }
}

fn new_dec(s: &Rc<RefCell<Shared>>, i: usize) -> Option<Dec> {
    let shared = s.clone();
    let on_output = Closure::<dyn FnMut(VideoFrame)>::new(move |frame: VideoFrame| {
        let Ok(mut sh) = shared.try_borrow_mut() else {
            frame.close();
            return;
        };
        let Some(d) = sh.decs.get_mut(i) else {
            frame.close();
            return;
        };
        d.ready.push_back(frame);
        while d.ready.len() > MAX_READY {
            d.ready.pop_front().unwrap().close();
        }
        sh.ctx.request_repaint();
    });
    let shared = s.clone();
    let on_error = Closure::<dyn FnMut(JsValue)>::new(move |e: JsValue| {
        if let Ok(mut sh) = shared.try_borrow_mut() {
            sh.error = Some(format!("decoder: {e:?}"));
            if let Some(d) = sh.decs.get_mut(i) {
                d.configured = false;
            }
        }
    });
    let init = VideoDecoderInit::new(
        on_error.as_ref().unchecked_ref(),
        on_output.as_ref().unchecked_ref(),
    );
    let decoder = VideoDecoder::new(&init).ok()?;
    Some(Dec {
        decoder,
        configured: false,
        waiting_key: true,
        ready: VecDeque::new(),
        codec: None,
        last_recv_ms: None,
        skipped: 0,
        _on_output: on_output,
        _on_error: on_error,
    })
}

fn on_text(s: &Rc<RefCell<Shared>>, text: &str) {
    let Ok(msg) = serde_json::from_str::<ServerText>(text) else {
        return;
    };
    match msg {
        ServerText::Replay {
            seq,
            t_ms,
            playing,
            cameras,
        } => {
            let need_decs = s.borrow().cameras != cameras;
            if need_decs {
                let decs: Vec<Dec> = (0..cameras.len()).filter_map(|i| new_dec(s, i)).collect();
                let mut sh = s.borrow_mut();
                for d in sh.decs.iter_mut() {
                    d.clear();
                    let _ = d.decoder.close();
                }
                if decs.len() != cameras.len() {
                    sh.error = Some("WebCodecs unavailable; HTTPS is required".into());
                }
                sh.decs = decs;
                sh.cameras = cameras;
            }
            let mut sh = s.borrow_mut();
            if seq == sh.seq {
                sh.active = true;
                sh.master = Master {
                    base_ms: t_ms as f64,
                    since: now_ms(),
                    playing,
                };
            }
        }
        ServerText::Config {
            camera,
            codec,
            width,
            height,
            description,
            ..
        } => {
            let mut sh = s.borrow_mut();
            if !sh.active {
                return;
            }
            let Some(i) = sh.cameras.iter().position(|c| *c == camera) else {
                return;
            };
            let result = sh
                .decs
                .get(i)
                .map(|d| configure(&d.decoder, &codec, width, height, &description));
            match result {
                Some(Ok(())) => {
                    let d = &mut sh.decs[i];
                    d.configured = true;
                    d.waiting_key = true;
                    d.codec = Some(codec);
                }
                Some(Err(e)) => sh.error = Some(e),
                None => {}
            }
        }
        _ => {}
    }
}

fn on_binary(s: &Rc<RefCell<Shared>>, buf: &ArrayBuffer) {
    let mut head = [0u8; argus_proto::REPLAY_HEADER_LEN];
    if buf.byte_length() < head.len() as u32 {
        return;
    }
    Uint8Array::new_with_byte_offset_and_length(buf, 0, head.len() as u32).copy_to(&mut head);
    let Ok((h, _)) = argus_proto::parse_replay_frame(&head) else {
        return;
    };
    let mut sh = s.borrow_mut();
    if !sh.active {
        return; // from before the latest seek
    }
    let limit = if now_ms() - sh.seek_sent_ms < SEEK_GRACE_MS {
        SEEK_DECODE_QUEUE
    } else {
        MAX_DECODE_QUEUE
    };
    let Some(d) = sh.decs.get_mut(usize::from(h.camera)) else {
        return;
    };
    d.last_recv_ms = Some(h.wall_us as f64 / 1000.0);
    if !d.configured || d.decoder.state() != CodecState::Configured {
        return;
    }
    if !h.keyframe && (d.waiting_key || d.decoder.decode_queue_size() > limit) {
        d.waiting_key = true;
        d.skipped += 1;
        return;
    }
    d.waiting_key = false;
    let data = Uint8Array::new_with_byte_offset(buf, argus_proto::REPLAY_HEADER_LEN as u32);
    let kind = if h.keyframe {
        EncodedVideoChunkType::Key
    } else {
        EncodedVideoChunkType::Delta
    };
    let init = EncodedVideoChunkInit::new(&data, 0, kind);
    // The frame's wall time (µs) is its timestamp, so decoded frames carry
    // it back out for the master clock comparison.
    init.set_timestamp_f64(h.wall_us as f64);
    if let Ok(chunk) = EncodedVideoChunk::new(&init) {
        let _ = d.decoder.decode(&chunk);
    }
}
