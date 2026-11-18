//! What a model made of an event (a one-line summary, objects seen,
//! notable or routine) and full-text search over events: summaries,
//! labels and camera names, in an FTS5 index next to the event table.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::index::Index;

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS event_summary (
    event_id    INTEGER PRIMARY KEY,
    -- 'done', 'skipped' (opted out, no stills...) or 'failed'.
    status      TEXT NOT NULL,
    summary     TEXT NOT NULL DEFAULT '',
    objects     TEXT NOT NULL DEFAULT '[]',
    notable     INTEGER NOT NULL DEFAULT 0,
    confidence  REAL NOT NULL DEFAULT 0,
    model       TEXT NOT NULL DEFAULT '',
    -- Other events this summary also covers (merged), JSON ids.
    merged      TEXT NOT NULL DEFAULT '[]',
    created_ms  INTEGER NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS event_fts USING fts5(
    camera, label, summary, objects, event_id UNINDEXED,
    tokenize = 'porter unicode61'
);
"#;

/// A model's reading of an event.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub summary: String,
    #[serde(default)]
    pub objects: Vec<String>,
    #[serde(default)]
    pub notable: bool,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub model: String,
}

/// An event with its summary, if any.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EventRow {
    pub id: i64,
    pub camera: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub source: String,
    pub label: String,
    pub summary: Option<Summary>,
}

/// What a search asks for.
#[derive(Clone, Debug, Default)]
pub struct SearchQuery {
    /// Free text; empty lists the window's events, newest first.
    pub text: String,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub cameras: Vec<String>,
    pub limit: usize,
}

/// Turns user text into an FTS5 query: each word as a quoted prefix term
/// (so nothing the user types is FTS syntax), joined with `joiner`.
pub fn fts_query(text: &str, joiner: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{}\"*", w.to_lowercase()))
        .collect();
    (!terms.is_empty()).then(|| terms.join(joiner))
}

const SELECT: &str = "SELECT e.id, e.camera, e.start_ms, e.end_ms, e.source, e.label,
        s.status, s.summary, s.objects, s.notable, s.confidence, s.model
   FROM event e LEFT JOIN event_summary s ON s.event_id = e.id";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    let status: Option<String> = r.get(6)?;
    let summary = match status.as_deref() {
        Some("done") => {
            let objects: String = r.get(8)?;
            Some(Summary {
                summary: r.get(7)?,
                objects: serde_json::from_str(&objects).unwrap_or_default(),
                notable: r.get::<_, i64>(9)? != 0,
                confidence: r.get(10)?,
                model: r.get(11)?,
            })
        }
        _ => None,
    };
    Ok(EventRow {
        id: r.get(0)?,
        camera: r.get(1)?,
        start_ms: r.get(2)?,
        end_ms: r.get(3)?,
        source: r.get(4)?,
        label: r.get(5)?,
        summary,
    })
}

impl Index {
    /// Puts events missing from the search index into it (new events, and
    /// all of them the first time).
    pub fn index_new_events(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "INSERT INTO event_fts (camera, label, summary, objects, event_id)
             SELECT camera, label, '', '', id FROM event
              WHERE id NOT IN (SELECT event_id FROM event_fts)",
            [],
        )?)
    }

    pub fn event(&self, id: i64) -> Result<Option<EventRow>> {
        Ok(self
            .conn
            .query_row(&format!("{SELECT} WHERE e.id = ?1"), params![id], row)
            .optional()?)
    }

    /// Stores a summary for `event_id` and the events merged into it.
    pub fn set_summary(
        &self,
        event_id: i64,
        merged: &[i64],
        s: &Summary,
        now_ms: i64,
    ) -> Result<()> {
        let objects = serde_json::to_string(&s.objects)?;
        for &id in std::iter::once(&event_id).chain(merged) {
            let others: Vec<i64> = std::iter::once(event_id)
                .chain(merged.iter().copied())
                .filter(|o| *o != id)
                .collect();
            self.conn.execute(
                "INSERT OR REPLACE INTO event_summary
                   (event_id, status, summary, objects, notable, confidence, model, merged, created_ms)
                 VALUES (?1, 'done', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id,
                    s.summary,
                    objects,
                    s.notable as i64,
                    s.confidence,
                    s.model,
                    serde_json::to_string(&others)?,
                    now_ms
                ],
            )?;
            self.conn
                .execute("DELETE FROM event_fts WHERE event_id = ?1", params![id])?;
            self.conn.execute(
                "INSERT INTO event_fts (camera, label, summary, objects, event_id)
                 SELECT camera, label, ?2, ?3, id FROM event WHERE id = ?1",
                params![id, s.summary, s.objects.join(" ")],
            )?;
        }
        Ok(())
    }

    /// Marks events as not getting a summary (opted out, no stills, the
    /// model failed), so the worker moves on.
    pub fn skip_summary(&self, ids: &[i64], status: &str, now_ms: i64) -> Result<()> {
        for id in ids {
            self.conn.execute(
                "INSERT OR IGNORE INTO event_summary (event_id, status, created_ms) VALUES (?1, ?2, ?3)",
                params![id, status, now_ms],
            )?;
        }
        Ok(())
    }

    /// Finished events without a summary yet (ended before `ended_before`,
    /// started after `since`), oldest first.
    pub fn events_to_summarize(
        &self,
        since: i64,
        ended_before: i64,
        limit: usize,
    ) -> Result<Vec<EventRow>> {
        let mut st = self.conn.prepare_cached(&format!(
            "{SELECT} WHERE s.event_id IS NULL AND e.start_ms >= ?1 AND e.end_ms < ?2
              ORDER BY e.start_ms LIMIT ?3"
        ))?;
        let rows = st
            .query_map(params![since, ended_before, limit as i64], row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// When this camera last got a summary (ms), if ever.
    pub fn last_summary_ms(&self, camera: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT MAX(s.created_ms) FROM event_summary s JOIN event e ON e.id = s.event_id
              WHERE e.camera = ?1 AND s.status = 'done'",
            params![camera],
            |r| r.get(0),
        )?)
    }

    /// Searches events: ranked by text match (all words first, then any),
    /// newer first among equals; or the window's newest events for no text.
    pub fn search_events(&self, q: &SearchQuery) -> Result<Vec<EventRow>> {
        let limit = q.limit.clamp(1, 5000) as i64;
        let (from, to) = (q.from_ms.unwrap_or(i64::MIN), q.to_ms.unwrap_or(i64::MAX));
        let cams = serde_json::to_string(&q.cameras)?;
        let cam_filter =
            "(json_array_length(?4) = 0 OR e.camera IN (SELECT value FROM json_each(?4)))";
        let Some(all) = fts_query(&q.text, " AND ") else {
            let mut st = self.conn.prepare(&format!(
                "{SELECT} WHERE e.end_ms >= ?1 AND e.start_ms < ?2 AND {cam_filter}
                  ORDER BY e.start_ms DESC LIMIT ?3"
            ))?;
            let rows = st
                .query_map(params![from, to, limit, cams], row)?
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(rows);
        };
        let any = fts_query(&q.text, " OR ").unwrap();
        let sql = format!(
            "{SELECT} JOIN event_fts f ON f.event_id = e.id
              WHERE event_fts MATCH ?5 AND e.end_ms >= ?1 AND e.start_ms < ?2 AND {cam_filter}
              ORDER BY bm25(event_fts, 2.0, 3.0, 1.0, 1.5), e.start_ms DESC LIMIT ?3"
        );
        let mut out: Vec<EventRow> = Vec::new();
        for m in [all, any] {
            let mut st = self.conn.prepare(&sql)?;
            for r in st.query_map(params![from, to, limit, cams, m], row)? {
                let r = r?;
                if !out.iter().any(|o| o.id == r.id) {
                    out.push(r);
                }
            }
            if out.len() as i64 >= limit {
                break;
            }
        }
        out.truncate(limit as usize);
        Ok(out)
    }

    /// The oldest recording still on local disk for this camera, ms.
    pub fn oldest_local_ms(&self, camera: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT MIN(start_ms) FROM chunk WHERE camera = ?1 AND tier = 'local'",
            params![camera],
            |r| r.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str, objects: &[&str], notable: bool) -> Summary {
        Summary {
            summary: text.into(),
            objects: objects.iter().map(|o| o.to_string()).collect(),
            notable,
            confidence: 0.8,
            model: "m".into(),
        }
    }

    #[test]
    fn summaries_and_search() {
        let i = Index::open_in_memory().unwrap();
        let a = i
            .add_event("doorbell", 1_000, 5_000, "reolink", "person")
            .unwrap();
        let b = i
            .add_event("driveway", 2_000, 6_000, "reolink", "vehicle")
            .unwrap();
        let c = i
            .add_event("back", 3_000, 9_000, "reolink", "animal")
            .unwrap();
        let d = i
            .add_event("doorbell", 4_000, 7_000, "reolink", "person")
            .unwrap();
        assert_eq!(i.index_new_events().unwrap(), 4);
        assert_eq!(i.index_new_events().unwrap(), 0);

        // Finished, unsummarised, oldest first.
        let todo = i.events_to_summarize(0, 8_000, 10).unwrap();
        assert_eq!(todo.iter().map(|e| e.id).collect::<Vec<_>>(), vec![a, b, d]);

        i.set_summary(
            a,
            &[d],
            &s(
                "A courier leaves a parcel at the front door.",
                &["person", "parcel"],
                true,
            ),
            10_000,
        )
        .unwrap();
        i.set_summary(
            b,
            &[],
            &s("A white van parks in the driveway.", &["van"], false),
            11_000,
        )
        .unwrap();
        i.skip_summary(&[c], "skipped", 12_000).unwrap();
        assert!(i.events_to_summarize(0, 99_000, 10).unwrap().is_empty());
        assert_eq!(i.last_summary_ms("doorbell").unwrap(), Some(10_000));
        assert_eq!(i.last_summary_ms("back").unwrap(), None);
        let ev = i.event(d).unwrap().unwrap();
        assert!(
            ev.summary.unwrap().summary.contains("parcel"),
            "merged event shares it"
        );

        let q = |text: &str| SearchQuery {
            text: text.into(),
            limit: 10,
            ..Default::default()
        };
        // Stemming and prefixes: "parcels" finds "parcel", "deliv" nothing.
        let r = i.search_events(&q("parcels at the door")).unwrap();
        assert_eq!(r[0].camera, "doorbell");
        // Camera names and labels are searchable too; summary words rank first.
        let r = i.search_events(&q("van")).unwrap();
        assert_eq!(r.iter().map(|e| e.id).collect::<Vec<_>>(), vec![b]);
        let r = i.search_events(&q("driveway")).unwrap();
        assert_eq!(r[0].id, b);
        let r = i.search_events(&q("animal")).unwrap();
        assert_eq!(r[0].id, c);
        // All words beat any word.
        let r = i.search_events(&q("parcel van")).unwrap();
        assert_eq!(r.len(), 3, "any-word matches follow");
        // Time window and cameras.
        let r = i
            .search_events(&SearchQuery {
                text: "person".into(),
                from_ms: Some(6_500),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.iter().map(|e| e.id).collect::<Vec<_>>(), vec![d]);
        let r = i
            .search_events(&SearchQuery {
                cameras: vec!["back".into()],
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.iter().map(|e| e.id).collect::<Vec<_>>(), vec![c]);
        // FTS syntax in the query is just text.
        assert!(i.search_events(&q("\"NEAR( OR *")).unwrap().is_empty());
    }

    #[test]
    fn queries_are_quoted() {
        assert_eq!(
            fts_query("Van, at gate!", " AND ").unwrap(),
            "\"van\"* AND \"at\"* AND \"gate\"*"
        );
        assert_eq!(fts_query("  ", " AND "), None);
    }
}
