//! The daily brief: at a set local time (default 07:00 Australia/Perth),
//! the model writes a short summary of the last 24 hours from the event
//! summaries, notable events first, then counts per camera. Stored
//! (`briefs.json`, the last 30), shown in the viewer and pushed. Without a
//! model it is skipped.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use argus_agent::runner::{RunSpec, Tool, ToolOutcome};
use argus_agent::{AgentError, Part, ToolSpec};
use argus_store::Index;
use chrono::{TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::agent::AgentHandle;
use crate::push::{Notification, Notifier};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Brief {
    pub at_ms: i64,
    pub text: String,
    pub events: usize,
    pub notable: usize,
    #[serde(default)]
    pub cost_usd: f64,
}

pub struct BriefStore {
    path: PathBuf,
    list: Mutex<Vec<Brief>>,
}

impl BriefStore {
    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join("briefs.json");
        let list = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(_) => Vec::new(),
        };
        Ok(BriefStore {
            path,
            list: Mutex::new(list),
        })
    }

    pub fn add(&self, b: Brief) -> Result<()> {
        let mut l = self.list.lock().unwrap();
        l.push(b);
        let excess = l.len().saturating_sub(30);
        l.drain(..excess);
        let tmp = self.path.with_extension("json.partial");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&*l)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Newest first.
    pub fn recent(&self) -> Vec<Brief> {
        let mut v = self.list.lock().unwrap().clone();
        v.reverse();
        v
    }
}

/// The next time after `now_ms` that it is `at` ("HH:MM") in `tz`.
pub fn next_run(now_ms: i64, at: &str, tz: chrono_tz::Tz) -> Option<i64> {
    let (h, m) = at.split_once(':')?;
    let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    let now = tz.timestamp_millis_opt(now_ms).single()?;
    let mut day = now.date_naive();
    for _ in 0..3 {
        if let Some(t) = tz
            .with_ymd_and_hms(
                chrono::Datelike::year(&day),
                chrono::Datelike::month(&day),
                chrono::Datelike::day(&day),
                h,
                m,
                0,
            )
            .earliest()
            && t.timestamp_millis() > now_ms
        {
            return Some(t.timestamp_millis());
        }
        day = day.succ_opt()?;
    }
    None
}

/// Writes a brief of the 24 h before `now_ms`.
pub async fn write(
    agent: &argus_agent::runner::Agent,
    index: &Mutex<Index>,
    tz: chrono_tz::Tz,
    now_ms: i64,
) -> Result<Brief, AgentError> {
    let events = crate::alerts::last_day(&index.lock().unwrap(), now_ms);
    let local = |ms: i64| {
        tz.timestamp_millis_opt(ms)
            .single()
            .map(|t| t.format("%a %H:%M").to_string())
            .unwrap_or_default()
    };
    let notable: Vec<String> = events
        .iter()
        .filter(|e| e.summary.as_ref().is_some_and(|s| s.notable))
        .map(|e| {
            format!(
                "- {} {}: {}",
                local(e.start_ms),
                e.camera,
                e.summary.as_ref().unwrap().summary
            )
        })
        .collect();
    let other: Vec<String> = events
        .iter()
        .filter(|e| e.summary.as_ref().is_some_and(|s| !s.notable))
        .take(80)
        .map(|e| {
            format!(
                "- {} {}: {}",
                local(e.start_ms),
                e.camera,
                e.summary.as_ref().unwrap().summary
            )
        })
        .collect();
    let counts: Vec<String> = crate::alerts::counts(&events)
        .iter()
        .map(|(cam, labels)| {
            format!(
                "- {cam}: {}",
                labels
                    .iter()
                    .map(|(l, n)| format!("{n} {l}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect();
    let text = format!(
        "The last 24 hours, to {}.\nNotable events ({}):\n{}\nOther summarised events (up to 80):\n{}\nDetections per camera:\n{}",
        local(now_ms),
        notable.len(),
        if notable.is_empty() {
            "- none".into()
        } else {
            notable.join("\n")
        },
        if other.is_empty() {
            "- none".into()
        } else {
            other.join("\n")
        },
        if counts.is_empty() {
            "- none".into()
        } else {
            counts.join("\n")
        },
    );
    let tool = Tool {
        spec: ToolSpec {
            name: "submit_brief".into(),
            description: "Submit the brief.".into(),
            parameters: json!({"type": "object", "properties": {
                "text": {"type": "string", "description": "Plain text, at most about 120 words: notable events first (time, camera, what), then a line of counts per camera. No headings."}
            }, "required": ["text"]}),
        },
        run: Box::new(|a: &Value| {
            if a["text"].as_str().is_none_or(|t| t.trim().is_empty()) {
                ToolOutcome::Reply("text is empty".into())
            } else {
                ToolOutcome::Finish(a.clone())
            }
        }),
    };
    let out = agent
        .run(RunSpec {
            kind: "daily-brief".into(),
            system:
                "You write a home's daily security brief from its camera events. Short, calm and \
                     factual. Call submit_brief once."
                    .into(),
            user: vec![Part::Text { text }],
            tools: vec![tool],
        })
        .await?;
    let v = out
        .result
        .ok_or_else(|| AgentError::Other("the model didn't submit a brief".into()))?;
    Ok(Brief {
        at_ms: now_ms,
        text: v["text"].as_str().unwrap_or("").trim().to_owned(),
        events: events.len(),
        notable: notable.len(),
        cost_usd: out.cost_usd,
    })
}

/// Writes, stores and pushes a brief now.
pub async fn run_once(
    agent: &AgentHandle,
    index: &Mutex<Index>,
    store: &BriefStore,
    notifier: &Notifier,
    tz: chrono_tz::Tz,
    now_ms: i64,
) -> Result<Brief, AgentError> {
    let a = agent
        .agent
        .as_ref()
        .map_err(|_| AgentError::NotConfigured)?;
    let b = write(a, index, tz, now_ms).await?;
    store
        .add(b.clone())
        .map_err(|e| AgentError::Other(e.to_string()))?;
    let body: String = b.text.chars().take(240).collect();
    notifier
        .send(
            &Notification {
                title: "Daily brief".into(),
                body,
                url: "/?brief=1".into(),
                tag: "brief".into(),
            },
            None,
        )
        .await;
    info!(
        events = b.events,
        notable = b.notable,
        "daily brief written"
    );
    Ok(b)
}

/// Runs the brief every day at `at` in `tz`.
pub async fn schedule(
    agent: Option<Arc<AgentHandle>>,
    index: Arc<Mutex<Index>>,
    store: Arc<BriefStore>,
    notifier: Arc<Notifier>,
    at: String,
    tz: chrono_tz::Tz,
) {
    loop {
        let now = argus_store::upload::now_ms();
        let Some(next) = next_run(now, &at, tz) else {
            warn!(%at, "daily brief: brief_time isn't HH:MM; not scheduling");
            return;
        };
        tokio::time::sleep(std::time::Duration::from_millis((next - now).max(0) as u64)).await;
        match &agent {
            Some(h) if h.agent.is_ok() => {
                if let Err(e) = run_once(
                    h,
                    &index,
                    &store,
                    &notifier,
                    tz,
                    argus_store::upload::now_ms(),
                )
                .await
                {
                    warn!("daily brief failed: {e}");
                }
            }
            _ => info!("daily brief skipped: no model set up"),
        }
        // Past the minute, so the next run is tomorrow.
        tokio::time::sleep(std::time::Duration::from_secs(61)).await;
    }
}

/// When the next brief is due, for status.
pub fn describe_next(now_ms: i64, at: &str, tz: chrono_tz::Tz) -> Option<String> {
    let t = next_run(now_ms, at, tz)?;
    tz.timestamp_millis_opt(t).single().map(|d| {
        format!(
            "{} {:02}:{:02} {}",
            d.format("%a %-d %b"),
            d.hour(),
            d.minute(),
            tz.name()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Australia::Perth;

    #[test]
    fn next_brief_time() {
        let at = |y, mo, d, h, mi| {
            Perth
                .with_ymd_and_hms(y, mo, d, h, mi, 0)
                .unwrap()
                .timestamp_millis()
        };
        // 06:59 Perth: today 07:00. 07:00 exactly or later: tomorrow.
        assert_eq!(
            next_run(at(2026, 11, 18, 6, 59), "07:00", Perth),
            Some(at(2026, 11, 18, 7, 0))
        );
        assert_eq!(
            next_run(at(2026, 11, 18, 7, 0), "07:00", Perth),
            Some(at(2026, 11, 19, 7, 0))
        );
        assert_eq!(
            next_run(at(2026, 12, 31, 23, 0), "07:00", Perth),
            Some(at(2027, 1, 1, 7, 0))
        );
        // Perth is UTC+8 all year: 07:00 there is 23:00 UTC the day before.
        let t = next_run(at(2026, 11, 18, 12, 0), "07:00", Perth).unwrap();
        let utc = chrono::DateTime::from_timestamp_millis(t).unwrap();
        assert_eq!(utc.format("%Y-%m-%d %H:%M").to_string(), "2026-11-18 23:00");
        assert_eq!(next_run(0, "7am", Perth), None);
        // A zone with daylight saving.
        let syd = chrono_tz::Australia::Sydney;
        let t = next_run(at(2026, 11, 18, 12, 0), "07:00", syd).unwrap();
        let local = syd.timestamp_millis_opt(t).unwrap();
        assert_eq!((local.hour(), local.minute()), (7, 0));
    }
}
