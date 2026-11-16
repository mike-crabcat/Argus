//! Turns a camera's own detections (Reolink motion and AI) into events in
//! the index, which drive the `events` upload policy.
//!
//! An event opens when a label goes active and is written at once, then its
//! end is moved forward on every poll while the detection lasts, so the
//! uploader can send the matching main-stream chunks while it is still
//! happening. It closes once the label has been inactive for `hold`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use argus_store::Index;
use tracing::{debug, info, warn};

use crate::reolink;

/// One change to apply to the event table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Start { label: &'static str, at_ms: i64 },
    Extend { label: &'static str, to_ms: i64 },
    End { label: &'static str, at_ms: i64 },
}

struct Open {
    start_ms: i64,
    last_active_ms: i64,
}

/// Edge detection with hold-off, per label.
pub struct EdgeDetector {
    hold_ms: i64,
    open: BTreeMap<&'static str, Open>,
}

impl EdgeDetector {
    pub fn new(hold: Duration) -> Self {
        Self {
            hold_ms: hold.as_millis() as i64,
            open: BTreeMap::new(),
        }
    }

    /// Feeds one poll: the labels active at `now_ms`.
    pub fn observe(&mut self, now_ms: i64, active: &[&'static str]) -> Vec<Change> {
        let mut out = Vec::new();
        for &label in active {
            match self.open.get_mut(label) {
                Some(o) => {
                    o.last_active_ms = now_ms;
                    out.push(Change::Extend {
                        label,
                        to_ms: now_ms,
                    });
                }
                None => {
                    self.open.insert(
                        label,
                        Open {
                            start_ms: now_ms,
                            last_active_ms: now_ms,
                        },
                    );
                    out.push(Change::Start {
                        label,
                        at_ms: now_ms,
                    });
                }
            }
        }
        let hold = self.hold_ms;
        let ended: Vec<_> = self
            .open
            .iter()
            .filter(|(l, o)| !active.contains(l) && now_ms - o.last_active_ms >= hold)
            .map(|(l, _)| *l)
            .collect();
        for label in ended {
            let o = self.open.remove(label).unwrap();
            out.push(Change::End {
                label,
                at_ms: o.last_active_ms,
            });
        }
        out
    }

    /// The camera stopped answering: close everything where it was last seen.
    pub fn close_all(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.open)
            .into_iter()
            .map(|(label, o)| Change::End {
                label,
                at_ms: o.last_active_ms,
            })
            .collect()
    }

    /// How long the open event for `label` has run, if any.
    pub fn open_ms(&self, label: &str) -> Option<i64> {
        self.open.get(label).map(|o| o.last_active_ms - o.start_ms)
    }
}

/// Polls one Reolink camera and records its detections as events.
pub struct ReolinkTriggers {
    camera: String,
    client: reolink::Client,
    detector: EdgeDetector,
    /// Labels that become events (others are ignored).
    labels: Vec<String>,
    index: Arc<Mutex<Index>>,
    event_ids: BTreeMap<&'static str, i64>,
}

impl ReolinkTriggers {
    pub fn new(
        camera: &str,
        client: reolink::Client,
        hold: Duration,
        labels: Vec<String>,
        index: Arc<Mutex<Index>>,
    ) -> Self {
        Self {
            camera: camera.to_owned(),
            client,
            detector: EdgeDetector::new(hold),
            labels,
            index,
            event_ids: BTreeMap::new(),
        }
    }

    /// One poll at `now_ms`; returns the changes it made.
    pub async fn step(&mut self, now_ms: i64) -> Result<Vec<Change>> {
        let d = self.client.detections().await?;
        let active: Vec<_> = d
            .active()
            .into_iter()
            .filter(|l| self.labels.iter().any(|w| w == l))
            .collect();
        let changes = self.detector.observe(now_ms, &active);
        self.apply(&changes)?;
        Ok(changes)
    }

    fn apply(&mut self, changes: &[Change]) -> Result<()> {
        let index = self.index.lock().unwrap();
        for c in changes {
            match *c {
                Change::Start { label, at_ms } => {
                    let id = index.add_event(&self.camera, at_ms, at_ms, "reolink", label)?;
                    self.event_ids.insert(label, id);
                    info!(camera = %self.camera, label, "event started");
                }
                Change::Extend { label, to_ms } => {
                    if let Some(&id) = self.event_ids.get(label) {
                        index.set_event_end(id, to_ms)?;
                    }
                }
                Change::End { label, at_ms } => {
                    if let Some(id) = self.event_ids.remove(label) {
                        index.set_event_end(id, at_ms)?;
                    }
                    info!(camera = %self.camera, label, "event ended");
                }
            }
        }
        Ok(())
    }

    /// Polls forever, backing off (1 s up to 30 s) while the camera is
    /// unreachable. Each camera runs on its own, so one being down doesn't
    /// hold up the others.
    pub async fn run(mut self, poll: Duration) {
        let mut backoff = Duration::from_secs(1);
        let mut healthy = true;
        let mut first = true;
        loop {
            match self.step(argus_store::upload::now_ms()).await {
                Ok(_) => {
                    if first || !healthy {
                        info!(camera = %self.camera, logins = self.client.logins,
                              "reading camera detections");
                    }
                    healthy = true;
                    first = false;
                    backoff = Duration::from_secs(1);
                    tokio::time::sleep(poll).await;
                }
                Err(e) => {
                    let closed = self.detector.close_all();
                    if let Err(e) = self.apply(&closed) {
                        warn!(camera = %self.camera, "closing events: {e:#}");
                    }
                    if healthy {
                        warn!(camera = %self.camera, "camera detections unavailable: {e:#}");
                    } else {
                        debug!(camera = %self.camera, "camera detections unavailable: {e:#}");
                    }
                    healthy = false;
                    first = false;
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reolink::tests::{FakeCam, serve};

    const HOLD: Duration = Duration::from_secs(5);

    #[test]
    fn opens_extends_and_closes_after_hold() {
        let mut d = EdgeDetector::new(HOLD);
        assert_eq!(d.observe(0, &[]), []);
        assert_eq!(
            d.observe(1000, &["person"]),
            [Change::Start {
                label: "person",
                at_ms: 1000
            }]
        );
        assert_eq!(
            d.observe(2000, &["person"]),
            [Change::Extend {
                label: "person",
                to_ms: 2000
            }]
        );
        // Inactive, but within the hold-off: still open.
        assert_eq!(d.observe(3000, &[]), []);
        assert_eq!(d.observe(6999, &[]), []);
        // 5 s after it was last seen: closed at the last sighting.
        assert_eq!(
            d.observe(7000, &[]),
            [Change::End {
                label: "person",
                at_ms: 2000
            }]
        );
        assert_eq!(d.observe(8000, &[]), []);
    }

    #[test]
    fn flapping_within_hold_is_one_event() {
        let mut d = EdgeDetector::new(HOLD);
        let mut starts = 0;
        for (t, on) in [
            (0, true),
            (1000, false),
            (3000, true),
            (4000, false),
            (8000, true),
        ] {
            let active: &[&str] = if on { &["vehicle"] } else { &[] };
            starts += d
                .observe(t, active)
                .iter()
                .filter(|c| matches!(c, Change::Start { .. }))
                .count();
        }
        assert_eq!(starts, 1);
        assert_eq!(d.open_ms("vehicle"), Some(8000));
    }

    #[test]
    fn labels_are_independent() {
        let mut d = EdgeDetector::new(HOLD);
        d.observe(0, &["motion", "person"]);
        let c = d.observe(10_000, &["motion"]);
        assert!(c.contains(&Change::End {
            label: "person",
            at_ms: 0
        }));
        assert!(c.contains(&Change::Extend {
            label: "motion",
            to_ms: 10_000
        }));
        assert_eq!(
            d.close_all(),
            [Change::End {
                label: "motion",
                at_ms: 10_000
            }]
        );
    }

    fn events(index: &Arc<Mutex<Index>>) -> Vec<(i64, i64, String, String)> {
        index.lock().unwrap().events("door", 0, i64::MAX).unwrap()
    }

    #[tokio::test]
    async fn writes_events_while_they_happen() {
        let cam = Arc::new(FakeCam {
            password: "pw".into(),
            ..Default::default()
        });
        let base = serve(cam.clone()).await;
        let client = reolink::Client::new(&base, "admin", "pw").unwrap();
        let index = Arc::new(Mutex::new(Index::open_in_memory().unwrap()));
        let labels = ["person", "vehicle", "animal"].map(String::from).to_vec();
        let mut t = ReolinkTriggers::new("door", client, HOLD, labels, index.clone());

        t.step(1_000).await.unwrap();
        assert!(events(&index).is_empty());
        // Motion alone is not a configured label: no event.
        *cam.state.lock().unwrap() = (true, false, false, false);
        t.step(2_000).await.unwrap();
        assert!(events(&index).is_empty());
        // A person: the event is in the index straight away...
        *cam.state.lock().unwrap() = (true, true, false, false);
        t.step(3_000).await.unwrap();
        assert_eq!(
            events(&index),
            [(3_000, 3_000, "reolink".into(), "person".into())]
        );
        // ...and grows while it lasts, before it has ended.
        t.step(60_000).await.unwrap();
        assert_eq!(events(&index)[0].1, 60_000);
        *cam.state.lock().unwrap() = (false, false, false, false);
        t.step(61_000).await.unwrap();
        t.step(66_000).await.unwrap();
        assert_eq!(
            events(&index),
            [(3_000, 60_000, "reolink".into(), "person".into())]
        );
    }
}
