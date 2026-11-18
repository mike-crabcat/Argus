//! Event summaries: a background worker that, when a model is configured,
//! sends each finished event's stills to it with the camera's name, the
//! time, the camera's detection label and where the camera is on the
//! floor plan, and stores the answer (a one-line summary, objects seen,
//! notable or routine, confidence) on the event for search.
//!
//! Budget: at most one summary per camera per `summary_gap_seconds`;
//! events in that window are merged into one. The monthly cap pauses the
//! worker. Cameras can opt out (`[camera.events] summarize = false`).

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use argus_agent::runner::{RunSpec, Tool, ToolOutcome};
use argus_agent::{AgentError, Part, ToolSpec};
use argus_store::Index;
use argus_store::summaries::{EventRow, Summary};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::agent::AgentHandle;
use crate::plan::PlanStore;
use crate::stills::StillStore;

/// Events are summarised once they have been over this long (their last
/// still is taken 5 s in).
const SETTLE_MS: i64 = 8_000;
/// Events older than this are never summarised (no backfill of history).
const LOOKBACK_MS: i64 = 24 * 3_600_000;
/// After hitting the spending cap, check again this much later.
const CAP_PAUSE_MS: i64 = 10 * 60_000;
/// At most this many stills per summary.
const MAX_IMAGES: usize = 6;

pub struct Worker {
    pub agent: Arc<AgentHandle>,
    pub index: Arc<Mutex<Index>>,
    pub plan: Option<Arc<PlanStore>>,
    pub stills: StillStore,
    pub opted_out: HashSet<String>,
    pub gap_ms: i64,
    pub paused_until: i64,
}

/// What one pass did.
#[derive(Debug, Default, PartialEq)]
pub struct Tick {
    pub summarized: usize,
    pub merged: usize,
    pub skipped: usize,
    pub capped: bool,
}

/// "Tuesday 18 November 2026, 13:05:12 AWST": local time for the model.
pub fn local_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%A %-d %B %Y, %H:%M:%S (UTC%:z)")
                .to_string()
        })
        .unwrap_or_default()
}

fn summary_tool<'a>() -> Tool<'a> {
    Tool {
        spec: ToolSpec {
            name: "submit_summary".into(),
            description: "Submit what happened in this event.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "summary": {"type": "string", "description": "One plain sentence: who or what, doing what, where. No speculation about identity."},
                    "objects": {"type": "array", "items": {"type": "string"}, "description": "Things seen, lower case: person, car, van, parcel, dog..."},
                    "notable": {"type": "boolean", "description": "True for things worth a look (a delivery, a stranger at a door, someone trying a door or gate, an unfamiliar vehicle staying); false for routine (passing cars, the household's own comings and goings, pets, wind)."},
                    "notable_reason": {"type": "string"},
                    "confidence": {"type": "number", "minimum": 0, "maximum": 1}
                },
                "required": ["summary", "objects", "notable", "confidence"]
            }),
        },
        run: Box::new(|args: &Value| {
            if args["summary"].as_str().is_none_or(|s| s.trim().is_empty()) {
                return ToolOutcome::Reply("summary must be a sentence.".into());
            }
            ToolOutcome::Finish(args.clone())
        }),
    }
}

impl Worker {
    /// One pass over finished events.
    pub async fn tick(&mut self, now_ms: i64) -> Tick {
        let mut out = Tick::default();
        let Ok(agent) = &self.agent.agent else {
            return out;
        };
        if now_ms < self.paused_until {
            out.capped = true;
            return out;
        }
        let todo: Vec<EventRow> = {
            let index = self.index.lock().unwrap();
            let _ = index.index_new_events();
            index
                .events_to_summarize(now_ms - LOOKBACK_MS, now_ms - SETTLE_MS, 500)
                .unwrap_or_default()
        };
        let mut by_camera: BTreeMap<String, Vec<EventRow>> = BTreeMap::new();
        for e in todo {
            by_camera.entry(e.camera.clone()).or_default().push(e);
        }
        let plan = self.plan.as_ref().map(|p| p.get().plan);
        for (camera, events) in by_camera {
            let ids: Vec<i64> = events.iter().map(|e| e.id).collect();
            if self.opted_out.contains(&camera) {
                let _ = self
                    .index
                    .lock()
                    .unwrap()
                    .skip_summary(&ids, "skipped", now_ms);
                out.skipped += ids.len();
                continue;
            }
            // One summary per camera per gap: wait for the window to close
            // and for the gap since the last one.
            let first = &events[0];
            let window_end = first.start_ms + self.gap_ms;
            if now_ms < window_end + SETTLE_MS {
                continue;
            }
            let last = self
                .index
                .lock()
                .unwrap()
                .last_summary_ms(&camera)
                .ok()
                .flatten();
            if last.is_some_and(|t| now_ms - t < self.gap_ms) {
                continue;
            }
            let group: Vec<&EventRow> = events
                .iter()
                .filter(|e| e.start_ms < window_end.max(first.start_ms + 1))
                .collect();
            let group_ids: Vec<i64> = group.iter().map(|e| e.id).collect();
            // Stills: all of the first event's, then one from each other.
            let mut images = Vec::new();
            for (k, e) in group.iter().enumerate() {
                let have = self.stills.list(e.id);
                let take: Vec<u32> = if k == 0 {
                    have
                } else {
                    have.into_iter().take(1).collect()
                };
                for n in take {
                    if images.len() < MAX_IMAGES
                        && let Some(b) = self.stills.read(e.id, n)
                    {
                        images.push(Part::Image {
                            media_type: "image/jpeg".into(),
                            data: b,
                        });
                    }
                }
            }
            if images.is_empty() {
                let _ = self
                    .index
                    .lock()
                    .unwrap()
                    .skip_summary(&group_ids, "no-stills", now_ms);
                out.skipped += group_ids.len();
                continue;
            }
            let labels: Vec<&str> = {
                let mut l: Vec<&str> = group.iter().map(|e| e.label.as_str()).collect();
                l.sort();
                l.dedup();
                l
            };
            let place = plan
                .as_ref()
                .and_then(|p| argus_plan::context::camera_context(p, &camera))
                .map(|c| c.describe(&camera))
                .unwrap_or_else(|| format!("Camera \"{camera}\" isn't on the floor plan."));
            let mut text = format!(
                "Camera: {camera}\nTime: {} to {}\nThe camera's own detection: {}\n{place}\n",
                local_time(first.start_ms),
                local_time(group.last().unwrap().end_ms),
                labels.join(", "),
            );
            if group.len() > 1 {
                text.push_str(&format!(
                    "This covers {} detections in a row; the first stills are from the start, then one from each later detection.\n",
                    group.len()
                ));
            }
            text.push_str(&format!("{} stills follow, in time order.", images.len()));
            let mut user = vec![Part::Text { text }];
            user.extend(images);
            let r = agent
                .run(RunSpec {
                    kind: "event-summary".into(),
                    system:
                        "You write short, factual summaries of home security camera events for the \
                             household. Describe what is visible; don't guess who people are. Call \
                             submit_summary with your answer."
                            .into(),
                    user,
                    tools: vec![summary_tool()],
                })
                .await;
            match r {
                Ok(o) => match o.result {
                    Some(v) => {
                        let s = Summary {
                            summary: v["summary"].as_str().unwrap_or("").trim().to_owned(),
                            objects: v["objects"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(|s| s.to_lowercase()))
                                        .collect()
                                })
                                .unwrap_or_default(),
                            notable: v["notable"].as_bool().unwrap_or(false),
                            confidence: v["confidence"].as_f64().unwrap_or(0.0).clamp(0.0, 1.0),
                            model: agent.provider.model().to_owned(),
                        };
                        let index = self.index.lock().unwrap();
                        if let Err(e) = index.set_summary(first.id, &group_ids[1..], &s, now_ms) {
                            warn!(%camera, "storing a summary: {e:#}");
                        }
                        info!(%camera, event = first.id, merged = group_ids.len() - 1, notable = s.notable, "event summarised");
                        out.summarized += 1;
                        out.merged += group_ids.len() - 1;
                    }
                    None => {
                        let _ = self
                            .index
                            .lock()
                            .unwrap()
                            .skip_summary(&group_ids, "failed", now_ms);
                        out.skipped += group_ids.len();
                    }
                },
                Err(AgentError::CapReached { spent, cap }) => {
                    warn!(
                        spent,
                        cap, "event summaries paused: the monthly model spending cap is reached"
                    );
                    self.paused_until = now_ms + CAP_PAUSE_MS;
                    out.capped = true;
                    return out;
                }
                Err(e) => {
                    warn!(%camera, "summarising an event failed: {e}");
                    let _ = self
                        .index
                        .lock()
                        .unwrap()
                        .skip_summary(&group_ids, "failed", now_ms);
                    out.skipped += group_ids.len();
                }
            }
        }
        out
    }

    /// Runs forever, a pass every few seconds.
    pub async fn run(mut self) {
        loop {
            self.tick(argus_store::upload::now_ms()).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}
