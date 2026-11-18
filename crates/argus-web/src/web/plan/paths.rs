//! Tracks on the map: where people, cars and animals went, from calibrated
//! cameras. Each moving spell is a path, coloured by label and fading with
//! age, with a dot where it was last seen; tapping a path replays it from
//! its start. Live mode shows only what is moving now, polled every second.

use argus_plan::{Id, Point, pt};
use eframe::egui::{self, Color32, Pos2, Stroke};

use super::{MapAction, Msg, PlanView, WINDOWS, label_color, sp};
use crate::plandoc::View;
use crate::web::send;

/// A track's path on the plan.
#[derive(Clone, Debug)]
pub(super) struct TrackPath {
    pub camera: String,
    pub label: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub level: Option<u64>,
    pub points: Vec<(i64, Point)>,
}

/// Polling: the window every few seconds, live every second.
const EVERY_MS: f64 = 5_000.0;
const LIVE_EVERY_MS: f64 = 1_000.0;
/// Live mode shows tracks seen this recently.
const LIVE_MS: i64 = 8_000;

fn parse(v: &serde_json::Value) -> Vec<TrackPath> {
    v["tracks"]
        .as_array()
        .map(|ts| {
            ts.iter()
                .filter_map(|t| {
                    let points: Vec<(i64, Point)> = t["boxes"]
                        .as_array()?
                        .iter()
                        .filter_map(|b| {
                            let p = b["plan"].as_array()?;
                            Some((
                                b["t_ms"].as_i64()?,
                                pt(p.first()?.as_f64()?, p.get(1)?.as_f64()?),
                            ))
                        })
                        .collect();
                    (!points.is_empty()).then(|| TrackPath {
                        camera: t["camera"].as_str().unwrap_or("").to_owned(),
                        label: t["label"].as_str().unwrap_or("").to_owned(),
                        start_ms: t["start_ms"].as_i64().unwrap_or(0),
                        end_ms: t["end_ms"].as_i64().unwrap_or(0),
                        level: t["level"].as_u64(),
                        points,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl PlanView {
    pub(super) fn fetch_tracks(&mut self, ctx: &egui::Context) {
        let every = if self.live { LIVE_EVERY_MS } else { EVERY_MS };
        if super::now_ms() - self.tracks_ms < every {
            return;
        }
        self.tracks_ms = super::now_ms();
        let now = crate::web::now_us() / 1000;
        let from = if self.live {
            now - LIVE_MS
        } else {
            now - WINDOWS[self.window].1 as i64
        };
        let path = format!("/api/detect/tracks?from={from}&boxes=1");
        let (inbox, token, ctx) = (self.inbox.clone(), self.token.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok((200, body)) = send("GET", &path, None, None, token.as_deref()).await
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
            {
                inbox.borrow_mut().push(Msg::Tracks(parse(&v)));
                ctx.request_repaint();
            }
        });
    }

    /// Draws the paths on `level`; remembers them for taps.
    pub(super) fn draw_paths(&mut self, p: &egui::Painter, view: &View, level: Id) {
        self.path_hits.clear();
        let now = crate::web::now_us() / 1000;
        let window = if self.live {
            LIVE_MS as f64
        } else {
            WINDOWS[self.window].1
        };
        for (i, t) in self.tracks.iter().enumerate() {
            if t.level.is_some_and(|l| l != level.0) {
                continue;
            }
            if self.live && now - t.end_ms > LIVE_MS {
                continue;
            }
            let age = (now - t.end_ms).max(0) as f64;
            let fade = (1.0 - age / window).clamp(0.15, 1.0) as f32;
            let col = label_color(&t.label).gamma_multiply(fade);
            let pts: Vec<Pos2> = t.points.iter().map(|(_, q)| sp(view, *q)).collect();
            if pts.len() > 1 {
                p.add(egui::Shape::line(pts.clone(), Stroke::new(2.5, col)));
            }
            if let Some(last) = pts.last() {
                p.circle_filled(*last, if self.live { 6.0 } else { 4.5 }, col);
                p.circle_stroke(
                    *last,
                    if self.live { 6.0 } else { 4.5 },
                    Stroke::new(1.0, Color32::from_black_alpha(140)),
                );
            }
            self.path_hits.push((pts, i));
        }
    }

    /// A tap near a path replays it. True if one was hit.
    pub(super) fn tap_path(&mut self, at: Pos2) -> bool {
        let dist = |pts: &[Pos2]| {
            if pts.len() == 1 {
                return pts[0].distance(at);
            }
            pts.windows(2)
                .map(|w| {
                    let (a, b) = (w[0], w[1]);
                    let ab = b - a;
                    let t = ((at - a).dot(ab) / ab.length_sq().max(1e-6)).clamp(0.0, 1.0);
                    (a + ab * t).distance(at)
                })
                .fold(f32::INFINITY, f32::min)
        };
        let hit = self
            .path_hits
            .iter()
            .map(|(pts, i)| (dist(pts), *i))
            .filter(|(d, _)| *d < 10.0)
            .min_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((_, i)) = hit {
            let t = &self.tracks[i];
            self.action = Some(MapAction::Replay {
                camera: t.camera.clone(),
                t_ms: t.start_ms as f64,
            });
            return true;
        }
        false
    }

    pub(super) fn paths_stats(&self) -> serde_json::Value {
        serde_json::json!({
            "live": self.live,
            "tracks": self.tracks.len(),
            "drawn": self.path_hits.iter().map(|(pts, i)| serde_json::json!({
                "camera": self.tracks[*i].camera, "label": self.tracks[*i].label,
                "points": pts.len(),
                "last": pts.last().map(|p| [p.x, p.y]),
            })).collect::<Vec<_>>(),
        })
    }
}
