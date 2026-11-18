//! Argus's own detections: each *moving spell* of a tracked object (a
//! person walking through, a car arriving) with its boxes over time, and
//! how they compare with the camera's own events.
//!
//! Boxes are in the detection picture's pixels (`width` x `height`, the
//! sub stream's size), so they can be mapped to the floor plan once the
//! camera is calibrated.

use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

use crate::index::Index;

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS track (
    id          INTEGER PRIMARY KEY,
    camera      TEXT NOT NULL,
    label       TEXT NOT NULL,
    class       TEXT NOT NULL,
    start_ms    INTEGER NOT NULL,
    end_ms      INTEGER NOT NULL,
    score       REAL NOT NULL,
    width       INTEGER NOT NULL,
    height      INTEGER NOT NULL,
    -- The event made from it, when the camera's events come from Argus.
    event_id    INTEGER
);
CREATE INDEX IF NOT EXISTS track_by_time ON track (camera, start_ms);
CREATE TABLE IF NOT EXISTS track_box (
    track_id    INTEGER NOT NULL,
    t_ms        INTEGER NOT NULL,
    x1 REAL NOT NULL, y1 REAL NOT NULL, x2 REAL NOT NULL, y2 REAL NOT NULL,
    score       REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS track_box_by_track ON track_box (track_id, t_ms);
"#;

/// A stored moving spell.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TrackRow {
    pub id: i64,
    pub camera: String,
    pub label: String,
    pub class: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub score: f64,
    pub width: u32,
    pub height: u32,
    pub event_id: Option<i64>,
}

/// One box of a track.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TrackBox {
    pub t_ms: i64,
    pub bbox: [f32; 4],
    pub score: f32,
}

/// One camera's events and tracks over a window, and how many of each
/// overlap the other (within a couple of seconds).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Comparison {
    pub camera: String,
    pub camera_events: u64,
    pub camera_events_matched: u64,
    pub tracks: u64,
    pub tracks_matched: u64,
    /// Tracks by label.
    pub track_labels: std::collections::BTreeMap<String, u64>,
}

/// Times closer than this count as overlapping in comparisons.
const SLACK_MS: i64 = 2_000;

impl Index {
    #[allow(clippy::too_many_arguments)]
    pub fn add_track(
        &self,
        camera: &str,
        label: &str,
        class: &str,
        start_ms: i64,
        score: f32,
        width: u32,
        height: u32,
        event_id: Option<i64>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO track (camera, label, class, start_ms, end_ms, score, width, height, event_id)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8)",
            params![camera, label, class, start_ms, score, width, height, event_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Moves a track's end and adds its boxes since the last call.
    pub fn extend_track(
        &self,
        id: i64,
        end_ms: i64,
        class: &str,
        score: f32,
        boxes: &[TrackBox],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE track SET end_ms = MAX(start_ms, ?2), class = ?3, score = MAX(score, ?4)
             WHERE id = ?1",
            params![id, end_ms, class, score],
        )?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO track_box (track_id, t_ms, x1, y1, x2, y2, score)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for b in boxes {
                st.execute(params![
                    id, b.t_ms, b.bbox[0], b.bbox[1], b.bbox[2], b.bbox[3], b.score
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Tracks overlapping [from_ms, to_ms), oldest first; one camera or all.
    pub fn tracks(&self, camera: Option<&str>, from_ms: i64, to_ms: i64) -> Result<Vec<TrackRow>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, camera, label, class, start_ms, end_ms, score, width, height, event_id
               FROM track WHERE (?1 IS NULL OR camera = ?1) AND end_ms >= ?2 AND start_ms < ?3
              ORDER BY start_ms",
        )?;
        let rows = st
            .query_map(params![camera, from_ms, to_ms], |r| {
                Ok(TrackRow {
                    id: r.get(0)?,
                    camera: r.get(1)?,
                    label: r.get(2)?,
                    class: r.get(3)?,
                    start_ms: r.get(4)?,
                    end_ms: r.get(5)?,
                    score: r.get(6)?,
                    width: r.get(7)?,
                    height: r.get(8)?,
                    event_id: r.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn track_boxes(&self, id: i64) -> Result<Vec<TrackBox>> {
        let mut st = self.conn.prepare_cached(
            "SELECT t_ms, x1, y1, x2, y2, score FROM track_box WHERE track_id = ?1 ORDER BY t_ms",
        )?;
        let rows = st
            .query_map([id], |r| {
                Ok(TrackBox {
                    t_ms: r.get(0)?,
                    bbox: [r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?],
                    score: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The camera's own (Reolink) events against Argus's tracks, per
    /// camera, over [from_ms, to_ms). Cameras with neither are left out.
    pub fn compare_tracks(&self, from_ms: i64, to_ms: i64) -> Result<Vec<Comparison>> {
        let mut out: std::collections::BTreeMap<String, Comparison> = Default::default();
        let mut st = self.conn.prepare_cached(
            "SELECT e.camera, COUNT(*),
                    SUM(EXISTS (SELECT 1 FROM track t WHERE t.camera = e.camera
                        AND t.start_ms <= e.end_ms + ?3 AND t.end_ms >= e.start_ms - ?3))
               FROM event e WHERE e.source = 'reolink' AND e.start_ms >= ?1 AND e.start_ms < ?2
              GROUP BY e.camera",
        )?;
        for r in st.query_map(params![from_ms, to_ms, SLACK_MS], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })? {
            let (camera, n, matched) = r?;
            let c = out.entry(camera.clone()).or_insert_with(|| Comparison {
                camera,
                ..Default::default()
            });
            c.camera_events = n as u64;
            c.camera_events_matched = matched as u64;
        }
        let mut st = self.conn.prepare_cached(
            "SELECT t.camera, t.label, COUNT(*),
                    SUM(EXISTS (SELECT 1 FROM event e WHERE e.camera = t.camera
                        AND e.source = 'reolink'
                        AND e.start_ms <= t.end_ms + ?3 AND e.end_ms >= t.start_ms - ?3))
               FROM track t WHERE t.start_ms >= ?1 AND t.start_ms < ?2
              GROUP BY t.camera, t.label",
        )?;
        for r in st.query_map(params![from_ms, to_ms, SLACK_MS], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })? {
            let (camera, label, n, matched) = r?;
            let c = out.entry(camera.clone()).or_insert_with(|| Comparison {
                camera,
                ..Default::default()
            });
            c.tracks += n as u64;
            c.tracks_matched += matched as u64;
            c.track_labels.insert(label, n as u64);
        }
        Ok(out.into_values().collect())
    }

    /// Deletes tracks (and their boxes) that ended before `before_ms`.
    pub fn expire_tracks(&self, before_ms: i64) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM track_box WHERE track_id IN (SELECT id FROM track WHERE end_ms < ?1)",
            [before_ms],
        )?;
        let n = tx.execute("DELETE FROM track WHERE end_ms < ?1", [before_ms])?;
        tx.commit()?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_boxes_and_comparison() {
        let i = Index::open_in_memory().unwrap();
        // The camera saw a person at 10 s and a vehicle at 100 s.
        i.add_event("door", 10_000, 15_000, "reolink", "person")
            .unwrap();
        i.add_event("door", 100_000, 104_000, "reolink", "vehicle")
            .unwrap();
        // Argus: the person (starting a second later) and a cat nobody else saw.
        let t1 = i
            .add_track("door", "person", "person", 11_000, 0.8, 896, 672, None)
            .unwrap();
        let b = |t, x| TrackBox {
            t_ms: t,
            bbox: [x, 10.0, x + 50.0, 130.0],
            score: 0.9,
        };
        i.extend_track(
            t1,
            13_000,
            "person",
            0.9,
            &[b(11_000, 100.0), b(12_000, 140.0)],
        )
        .unwrap();
        i.extend_track(t1, 16_500, "person", 0.85, &[b(16_500, 200.0)])
            .unwrap();
        let t2 = i
            .add_track("door", "animal", "cat", 50_000, 0.7, 896, 672, Some(9))
            .unwrap();
        i.extend_track(t2, 52_000, "cat", 0.7, &[]).unwrap();

        let all = i.tracks(Some("door"), 0, i64::MAX).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(
            (all[0].start_ms, all[0].end_ms, all[0].score),
            (11_000, 16_500, 0.8999999761581421)
        );
        assert_eq!(all[1].event_id, Some(9));
        assert!(i.tracks(Some("yard"), 0, i64::MAX).unwrap().is_empty());
        assert_eq!(i.tracks(None, 51_000, 60_000).unwrap().len(), 1);
        let boxes = i.track_boxes(t1).unwrap();
        assert_eq!(
            boxes.iter().map(|b| b.t_ms).collect::<Vec<_>>(),
            [11_000, 12_000, 16_500]
        );
        assert_eq!(boxes[1].bbox, [140.0, 10.0, 190.0, 130.0]);

        let c = i.compare_tracks(0, 200_000).unwrap();
        assert_eq!(c.len(), 1);
        let c = &c[0];
        assert_eq!((c.camera_events, c.camera_events_matched), (2, 1));
        assert_eq!((c.tracks, c.tracks_matched), (2, 1));
        assert_eq!(c.track_labels["animal"], 1);

        assert_eq!(i.expire_tracks(20_000).unwrap(), 1);
        assert!(i.track_boxes(t1).unwrap().is_empty());
        assert_eq!(i.tracks(None, 0, i64::MAX).unwrap().len(), 1);
    }
}
