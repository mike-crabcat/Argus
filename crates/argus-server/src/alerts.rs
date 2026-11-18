//! Alert rules written in plain English ("a person at the front door
//! after 10pm"). The agent turns each rule once into a structured filter
//! (cameras, labels, a local time window, days, and optionally a question
//! to ask of the event's stills). New events are matched against the
//! filter, cheaply; the model is only asked the question, under the spend
//! cap. A rule fires at most once per cooldown, as a push notification.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use argus_agent::runner::{RunSpec, Tool, ToolOutcome};
use argus_agent::{AgentError, Part, ToolSpec};
use argus_store::Index;
use argus_store::summaries::{EventRow, SearchQuery};
use chrono::{Datelike, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::agent::AgentHandle;
use crate::push::{Notification, Notifier};
use crate::stills::StillStore;

/// The structured form of a rule.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    /// Camera names; empty for any.
    #[serde(default)]
    pub cameras: Vec<String>,
    /// Detection labels (person, vehicle, animal, motion); empty for any.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Local time window "HH:MM"; `to` before `from` wraps past midnight.
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    /// Days, 1 = Monday ... 7 = Sunday; empty for every day.
    #[serde(default)]
    pub days: Vec<u8>,
    /// A yes/no question for the model about the event's stills.
    #[serde(default)]
    pub check: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    /// What the user wrote.
    pub text: String,
    pub filter: Filter,
    #[serde(default = "default_cooldown")]
    pub cooldown_s: u64,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// What the compiler couldn't express ("while we're away").
    #[serde(default)]
    pub note: Option<String>,
    pub created_ms: i64,
    #[serde(default)]
    pub last_fired_ms: Option<i64>,
}

fn default_cooldown() -> u64 {
    600
}
fn yes() -> bool {
    true
}

/// A fired alert, for the log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fired {
    pub rule_id: String,
    pub event_id: i64,
    pub camera: String,
    pub at_ms: i64,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub answer: Option<String>,
}

fn hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

impl Filter {
    /// Problems with the filter, for the editor.
    pub fn check_valid(&self) -> Result<(), String> {
        for t in [&self.from, &self.to].into_iter().flatten() {
            if hhmm(t).is_none() {
                return Err(format!("{t:?} isn't a time like 22:00"));
            }
        }
        if self.days.iter().any(|d| !(1..=7).contains(d)) {
            return Err("days are 1 (Monday) to 7 (Sunday)".into());
        }
        Ok(())
    }

    /// Whether an event (camera, label, start) passes, in local time `tz`.
    pub fn matches(&self, camera: &str, label: &str, at_ms: i64, tz: chrono_tz::Tz) -> bool {
        if !self.cameras.is_empty() && !self.cameras.iter().any(|c| c == camera) {
            return false;
        }
        if !self.labels.is_empty() && !self.labels.iter().any(|l| l == label) {
            return false;
        }
        let Some(t) = tz.timestamp_millis_opt(at_ms).single() else {
            return false;
        };
        let minute = t.hour() * 60 + t.minute();
        let in_window = match (
            self.from.as_deref().and_then(hhmm),
            self.to.as_deref().and_then(hhmm),
        ) {
            (Some(a), Some(b)) if a <= b => (a..b).contains(&minute),
            (Some(a), Some(b)) => minute >= a || minute < b,
            (Some(a), None) => minute >= a,
            (None, Some(b)) => minute < b,
            (None, None) => true,
        };
        if !in_window {
            return false;
        }
        // Days count from the window's start: 02:00 Saturday after a
        // "Friday night from 22:00" belongs to Friday.
        let wraps = matches!((self.from.as_deref().and_then(hhmm), self.to.as_deref().and_then(hhmm)), (Some(a), Some(b)) if a > b);
        let mut day = t.weekday().number_from_monday() as u8;
        if wraps && minute < self.to.as_deref().and_then(hhmm).unwrap_or(0) {
            day = if day == 1 { 7 } else { day - 1 };
        }
        self.days.is_empty() || self.days.contains(&day)
    }

    /// One line for people: "person on doorbell, 22:00–06:00, weekdays".
    pub fn describe(&self) -> String {
        let what = if self.labels.is_empty() {
            "anything".into()
        } else {
            self.labels.join(" or ")
        };
        let mut s = format!(
            "{what} on {}",
            if self.cameras.is_empty() {
                "any camera".into()
            } else {
                self.cameras.join(", ")
            }
        );
        match (&self.from, &self.to) {
            (Some(a), Some(b)) => s.push_str(&format!(", {a}–{b}")),
            (Some(a), None) => s.push_str(&format!(", after {a}")),
            (None, Some(b)) => s.push_str(&format!(", before {b}")),
            _ => {}
        }
        if !self.days.is_empty() {
            const D: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
            let names: Vec<&str> = self.days.iter().map(|d| D[(*d as usize - 1) % 7]).collect();
            s.push_str(&format!(", {}", names.join(" ")));
        }
        if let Some(q) = &self.check {
            s.push_str(&format!("; ask: {q}"));
        }
        s
    }
}

/// `alert_rules.json` and `alerts_log.json` in the storage root.
pub struct RuleStore {
    path: PathBuf,
    log_path: PathBuf,
    rules: Mutex<Vec<Rule>>,
    log: Mutex<Vec<Fired>>,
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    let tmp = path.with_extension("json.partial");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

impl RuleStore {
    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join("alert_rules.json");
        let log_path = root.join("alerts_log.json");
        Ok(RuleStore {
            rules: Mutex::new(read_json(&path)?),
            log: Mutex::new(read_json(&log_path)?),
            path,
            log_path,
        })
    }

    pub fn list(&self) -> Vec<Rule> {
        self.rules.lock().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<Rule> {
        self.rules
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .cloned()
    }

    /// Adds or replaces a rule (by id).
    pub fn put(&self, r: Rule) -> Result<()> {
        r.filter.check_valid().map_err(anyhow::Error::msg)?;
        let mut l = self.rules.lock().unwrap();
        match l.iter_mut().find(|x| x.id == r.id) {
            Some(x) => *x = r,
            None => l.push(r),
        }
        write_json(&self.path, &*l)
    }

    pub fn delete(&self, id: &str) -> Result<bool> {
        let mut l = self.rules.lock().unwrap();
        let n = l.len();
        l.retain(|r| r.id != id);
        write_json(&self.path, &*l)?;
        Ok(l.len() != n)
    }

    fn fired(&self, f: Fired) {
        let mut l = self.rules.lock().unwrap();
        if let Some(r) = l.iter_mut().find(|r| r.id == f.rule_id) {
            r.last_fired_ms = Some(f.at_ms);
        }
        let _ = write_json(&self.path, &*l);
        drop(l);
        let mut log = self.log.lock().unwrap();
        log.push(f);
        let excess = log.len().saturating_sub(200);
        log.drain(..excess);
        let _ = write_json(&self.log_path, &*log);
    }

    pub fn log(&self) -> Vec<Fired> {
        self.log.lock().unwrap().clone()
    }
}

/// A new rule id.
pub fn new_id() -> String {
    let mut b = [0u8; 6];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Asks the model to turn `text` into a filter. Returns (filter, note).
pub async fn compile(
    agent: &argus_agent::runner::Agent,
    text: &str,
    cameras: &[String],
    places: &str,
) -> Result<(Filter, Option<String>), AgentError> {
    let tool = Tool {
        spec: ToolSpec {
            name: "set_filter".into(),
            description: "Set the rule's filter.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "cameras": {"type": "array", "items": {"type": "string", "enum": cameras}, "description": "Cameras it applies to; empty for all."},
                    "labels": {"type": "array", "items": {"type": "string", "enum": ["person", "vehicle", "animal", "motion"]}, "description": "Detections it applies to; empty for any."},
                    "from": {"type": "string", "description": "Local start time HH:MM, or omit."},
                    "to": {"type": "string", "description": "Local end time HH:MM, or omit. Before from means past midnight."},
                    "days": {"type": "array", "items": {"type": "integer", "minimum": 1, "maximum": 7}, "description": "1 = Monday ... 7 = Sunday; empty for every day."},
                    "check": {"type": "string", "description": "Only if the rule needs judgement the filter can't express: a yes/no question to ask of the event's pictures, e.g. \"Is this a delivery driver?\""},
                    "note": {"type": "string", "description": "Anything in the rule you couldn't express (e.g. whether people are home), said plainly."}
                },
                "required": ["cameras", "labels"]
            }),
        },
        run: Box::new(|args: &Value| {
            let f: Filter = match serde_json::from_value(args.clone()) {
                Ok(f) => f,
                Err(e) => return ToolOutcome::Reply(format!("That doesn't fit the schema: {e}")),
            };
            match f.check_valid() {
                Ok(()) => ToolOutcome::Finish(args.clone()),
                Err(e) => ToolOutcome::Reply(format!("{e}. Fix it and call set_filter again.")),
            }
        }),
    };
    let out = agent
        .run(RunSpec {
            kind: "alert-compile".into(),
            system: "You turn a home's alert rules, written in plain English, into a filter over \
                     camera events. Cameras detect: person, vehicle, animal, motion. Use only the \
                     cameras listed. Call set_filter once."
                .into(),
            user: vec![Part::Text {
                text: format!("Cameras:\n{places}\n\nThe rule: {text}"),
            }],
            tools: vec![tool],
        })
        .await?;
    let v = out
        .result
        .ok_or_else(|| AgentError::Other("the model didn't set a filter".into()))?;
    let filter: Filter =
        serde_json::from_value(v.clone()).map_err(|e| AgentError::Other(e.to_string()))?;
    let note = v["note"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(String::from);
    Ok((filter, note))
}

/// The cameras, with their place on the plan, for prompts.
pub fn places(cameras: &[String], plan: Option<&argus_plan::Plan>) -> String {
    cameras
        .iter()
        .map(|c| {
            plan.and_then(|p| argus_plan::context::camera_context(p, c))
                .map(|x| format!("- {}", x.describe(c)))
                .unwrap_or_else(|| format!("- Camera \"{c}\"."))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One event from history against a rule.
#[derive(Debug, Serialize, PartialEq)]
pub struct Tried {
    pub event_id: i64,
    pub camera: String,
    pub label: String,
    pub start_ms: i64,
    pub summary: Option<String>,
    /// It would have fired (or, with a check, been asked).
    pub fires: bool,
    /// Held back by the cooldown.
    pub cooled: bool,
}

/// Which of `events` (oldest first) the rule would have fired on, with
/// its cooldown. The check question isn't asked here (no spending).
pub fn try_history(rule: &Rule, events: &[EventRow], tz: chrono_tz::Tz) -> Vec<Tried> {
    let mut last: Option<i64> = None;
    let mut out = Vec::new();
    for e in events {
        if !rule.filter.matches(&e.camera, &e.label, e.start_ms, tz) {
            continue;
        }
        let cooled = last.is_some_and(|t| e.start_ms - t < rule.cooldown_s as i64 * 1000);
        if !cooled {
            last = Some(e.start_ms);
        }
        out.push(Tried {
            event_id: e.id,
            camera: e.camera.clone(),
            label: e.label.clone(),
            start_ms: e.start_ms,
            summary: e.summary.as_ref().map(|s| s.summary.clone()),
            fires: !cooled,
            cooled,
        });
    }
    out
}

/// The viewer path that replays an event.
pub fn replay_url(camera: &str, start_ms: i64) -> String {
    format!("/?t={start_ms}&camera={camera}")
}

/// Watches new events and fires matching rules.
pub struct Engine {
    pub rules: Arc<RuleStore>,
    pub index: Arc<Mutex<Index>>,
    pub notifier: Arc<Notifier>,
    pub agent: Option<Arc<AgentHandle>>,
    pub stills: StillStore,
    pub tz: chrono_tz::Tz,
    pub last_id: i64,
    /// Events waiting for stills before their check: (rule, event, due).
    pub pending: Vec<(String, EventRow, i64)>,
    /// Cameras' `context` lines from the config.
    pub contexts: std::collections::HashMap<String, String>,
}

/// Stills are taken up to 5 s in.
const STILLS_READY_MS: i64 = 6_000;

impl Engine {
    pub fn new(
        rules: Arc<RuleStore>,
        index: Arc<Mutex<Index>>,
        notifier: Arc<Notifier>,
        agent: Option<Arc<AgentHandle>>,
        stills: StillStore,
        tz: chrono_tz::Tz,
    ) -> Self {
        let last_id = index.lock().unwrap().last_event_id().unwrap_or(0);
        Engine {
            rules,
            index,
            notifier,
            agent,
            stills,
            tz,
            last_id,
            pending: Vec::new(),
            contexts: Default::default(),
        }
    }

    fn cooled(&self, rule_id: &str, now_ms: i64) -> bool {
        self.rules.get(rule_id).is_some_and(|r| {
            r.last_fired_ms
                .is_some_and(|t| now_ms - t < r.cooldown_s as i64 * 1000)
        })
    }

    async fn fire(&self, rule: &Rule, e: &EventRow, answer: Option<String>, now_ms: i64) {
        let local = self
            .tz
            .timestamp_millis_opt(e.start_ms)
            .single()
            .map(|t| t.format("%H:%M").to_string())
            .unwrap_or_default();
        let body = match &answer {
            Some(a) => format!("{} on {} at {local}. {a}", e.label, e.camera),
            None => format!("{} on {} at {local}.", e.label, e.camera),
        };
        let n = Notification {
            title: rule.text.clone(),
            body: body.clone(),
            url: replay_url(&e.camera, e.start_ms),
            tag: format!("rule-{}", rule.id),
        };
        let sent = self.notifier.send(&n, None).await;
        info!(rule = %rule.id, event = e.id, web_push = sent.web_push, "alert fired");
        self.rules.fired(Fired {
            rule_id: rule.id.clone(),
            event_id: e.id,
            camera: e.camera.clone(),
            at_ms: now_ms,
            title: n.title,
            body,
            answer,
        });
    }

    /// Asks the rule's question about the event's stills. None: no.
    async fn ask(&self, question: &str, e: &EventRow) -> Result<Option<String>, AgentError> {
        let agent = self
            .agent
            .as_ref()
            .and_then(|h| h.agent.as_ref().ok())
            .ok_or(AgentError::NotConfigured)?;
        let about = match self.contexts.get(&e.camera) {
            Some(c) => format!(" (it looks at: {c})"),
            None => String::new(),
        };
        let mut user = vec![Part::Text {
            text: format!(
                "Camera {}{about} saw: {}. Question: {question}",
                e.camera, e.label
            ),
        }];
        for n in self.stills.list(e.id).into_iter().take(3) {
            if let Some(b) = self.stills.for_model(e.id, n).await {
                user.push(Part::Image {
                    media_type: "image/jpeg".into(),
                    data: b,
                });
            }
        }
        let tool = Tool {
            spec: ToolSpec {
                name: "answer".into(),
                description: "Answer the question.".into(),
                parameters: json!({"type": "object", "properties": {
                    "yes": {"type": "boolean"},
                    "reason": {"type": "string", "description": "A few words on what you see."}
                }, "required": ["yes"]}),
            },
            run: Box::new(|a: &Value| ToolOutcome::Finish(a.clone())),
        };
        let out = agent
            .run(RunSpec {
                kind: "alert-check".into(),
                system: "You check security camera pictures for a home alert. Answer only what is \
                         visible. Call answer once."
                    .into(),
                user,
                tools: vec![tool],
            })
            .await?;
        let v = out.result.unwrap_or_default();
        Ok((v["yes"] == true).then(|| v["reason"].as_str().unwrap_or("").to_owned()))
    }

    /// One pass: new events against rules, then due checks. Returns how
    /// many alerts fired.
    pub async fn tick(&mut self, now_ms: i64) -> usize {
        let mut fired = 0;
        let new = self
            .index
            .lock()
            .unwrap()
            .events_after(self.last_id, 500)
            .unwrap_or_default();
        let rules: Vec<Rule> = self
            .rules
            .list()
            .into_iter()
            .filter(|r| r.enabled)
            .collect();
        for e in new {
            self.last_id = self.last_id.max(e.id);
            for r in &rules {
                if !r.filter.matches(&e.camera, &e.label, e.start_ms, self.tz)
                    || self.cooled(&r.id, now_ms)
                {
                    continue;
                }
                if r.filter.check.is_some() {
                    self.pending
                        .push((r.id.clone(), e.clone(), e.start_ms + STILLS_READY_MS));
                } else {
                    self.fire(r, &e, None, now_ms).await;
                    fired += 1;
                }
            }
        }
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|p| p.2 <= now_ms);
        self.pending = later;
        for (rid, e, _) in due {
            let Some(r) = self.rules.get(&rid) else {
                continue;
            };
            if !r.enabled || self.cooled(&rid, now_ms) {
                continue;
            }
            let q = r.filter.check.clone().unwrap_or_default();
            match self.ask(&q, &e).await {
                Ok(Some(reason)) => {
                    self.fire(&r, &e, Some(reason), now_ms).await;
                    fired += 1;
                }
                Ok(None) => info!(rule = %rid, event = e.id, "alert check said no"),
                Err(AgentError::NotConfigured) => {
                    // No model: fire on the filter alone, saying so.
                    self.fire(
                        &r,
                        &e,
                        Some("(not checked: no model set up)".into()),
                        now_ms,
                    )
                    .await;
                    fired += 1;
                }
                Err(e2) => warn!(rule = %rid, "alert check failed: {e2}"),
            }
        }
        fired
    }

    pub async fn run(mut self) {
        loop {
            self.tick(argus_store::upload::now_ms()).await;
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    }
}

/// The last 24 h of events, oldest first.
pub fn last_day(index: &Index, now_ms: i64) -> Vec<EventRow> {
    let _ = index.index_new_events();
    let mut v = index
        .search_events(&SearchQuery {
            text: String::new(),
            from_ms: Some(now_ms - 24 * 3_600_000),
            to_ms: Some(now_ms + 1),
            cameras: vec![],
            limit: 5000,
        })
        .unwrap_or_default();
    v.reverse();
    v
}

/// Counts per camera and label, for the brief.
pub fn counts(events: &[EventRow]) -> BTreeMap<String, BTreeMap<String, usize>> {
    let mut m: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for e in events {
        *m.entry(e.camera.clone())
            .or_default()
            .entry(e.label.clone())
            .or_default() += 1;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Australia::Perth;

    fn perth(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        Perth
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn filters_match_in_local_time() {
        let f = Filter {
            cameras: vec!["doorbell".into()],
            labels: vec!["person".into()],
            from: Some("22:00".into()),
            to: Some("06:00".into()),
            days: vec![5], // Friday nights
            check: None,
        };
        // 2026-11-20 is a Friday.
        assert!(f.matches("doorbell", "person", perth(2026, 11, 20, 23, 30), Perth));
        assert!(
            f.matches("doorbell", "person", perth(2026, 11, 21, 2, 0), Perth),
            "Saturday 02:00 is still Friday night"
        );
        assert!(
            !f.matches("doorbell", "person", perth(2026, 11, 21, 23, 0), Perth),
            "Saturday night"
        );
        assert!(!f.matches("doorbell", "person", perth(2026, 11, 20, 21, 59), Perth));
        assert!(!f.matches("driveway", "person", perth(2026, 11, 20, 23, 30), Perth));
        assert!(!f.matches("doorbell", "vehicle", perth(2026, 11, 20, 23, 30), Perth));
        let any = Filter::default();
        assert!(any.matches("x", "motion", 0, Perth));
        assert!(
            Filter {
                from: Some("25:00".into()),
                ..Default::default()
            }
            .check_valid()
            .is_err()
        );
        assert_eq!(f.describe(), "person on doorbell, 22:00–06:00, Fri");
    }

    #[test]
    fn history_respects_the_cooldown() {
        let rule = Rule {
            id: "r".into(),
            text: "person at the door".into(),
            filter: Filter {
                labels: vec!["person".into()],
                ..Default::default()
            },
            cooldown_s: 600,
            enabled: true,
            note: None,
            created_ms: 0,
            last_fired_ms: None,
        };
        let ev = |id, label: &str, min: i64| EventRow {
            id,
            camera: "doorbell".into(),
            start_ms: min * 60_000,
            end_ms: min * 60_000 + 5_000,
            source: "reolink".into(),
            label: label.into(),
            summary: None,
        };
        let t = try_history(
            &rule,
            &[
                ev(1, "person", 0),
                ev(2, "vehicle", 1),
                ev(3, "person", 5),
                ev(4, "person", 11),
            ],
            Perth,
        );
        let fires: Vec<(i64, bool)> = t.iter().map(|x| (x.event_id, x.fires)).collect();
        assert_eq!(fires, vec![(1, true), (3, false), (4, true)]);
    }
}
