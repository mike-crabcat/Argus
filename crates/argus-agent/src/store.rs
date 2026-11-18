//! The log of agent runs (`agent.sqlite` in the storage root): what was
//! asked, which tools were called, tokens and cost. API keys are never
//! written here; images are logged by size only.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_runs (
    id            INTEGER PRIMARY KEY,
    kind          TEXT NOT NULL,
    model         TEXT NOT NULL,
    started_ms    INTEGER NOT NULL,
    finished_ms   INTEGER,
    prompt        TEXT NOT NULL,
    steps         TEXT NOT NULL DEFAULT '[]',
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd      REAL NOT NULL DEFAULT 0,
    outcome       TEXT NOT NULL DEFAULT 'running',
    error         TEXT
);
CREATE INDEX IF NOT EXISTS agent_runs_by_time ON agent_runs (started_ms);
"#;

pub struct RunStore {
    db: Mutex<Connection>,
}

/// One logged run, for listing.
#[derive(Clone, Debug, Serialize)]
pub struct RunRow {
    pub id: i64,
    pub kind: String,
    pub model: String,
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
    pub prompt: String,
    pub steps: serde_json::Value,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    pub outcome: String,
    pub error: Option<String>,
}

/// Midnight UTC at the start of the month holding `now_ms`.
pub fn month_start_ms(now_ms: i64) -> i64 {
    // Days since 1970-01-01 to the first of this month (proleptic
    // Gregorian), without a date library.
    let days = now_ms.div_euclid(86_400_000);
    let (y, m, _) = civil_from_days(days);
    days_from_civil(y, m, 1) * 86_400_000
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl RunStore {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(SCHEMA)?;
        Ok(RunStore { db: Mutex::new(db) })
    }

    pub fn in_memory() -> Result<Self> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(SCHEMA)?;
        Ok(RunStore { db: Mutex::new(db) })
    }

    pub fn start(&self, kind: &str, model: &str, prompt: &str, now_ms: i64) -> Result<i64> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO agent_runs (kind, model, started_ms, prompt) VALUES (?1, ?2, ?3, ?4)",
            params![kind, model, now_ms, prompt],
        )?;
        Ok(db.last_insert_rowid())
    }

    /// Updates a run's steps and totals so far.
    pub fn update(
        &self,
        id: i64,
        steps: &serde_json::Value,
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: f64,
    ) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE agent_runs SET steps = ?2, input_tokens = ?3, output_tokens = ?4, cost_usd = ?5 WHERE id = ?1",
            params![id, steps.to_string(), input_tokens as i64, output_tokens as i64, cost_usd],
        )?;
        Ok(())
    }

    pub fn finish(&self, id: i64, outcome: &str, error: Option<&str>, now_ms: i64) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE agent_runs SET outcome = ?2, error = ?3, finished_ms = ?4 WHERE id = ?1",
            params![id, outcome, error, now_ms],
        )?;
        Ok(())
    }

    /// Dollars spent since `from_ms` (the month's start).
    pub fn spent_since(&self, from_ms: i64) -> Result<f64> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM agent_runs WHERE started_ms >= ?1",
            params![from_ms],
            |r| r.get(0),
        )?)
    }

    pub fn runs_since(&self, from_ms: i64) -> Result<i64> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM agent_runs WHERE started_ms >= ?1",
            params![from_ms],
            |r| r.get(0),
        )?)
    }

    /// The most recent runs, newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<RunRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT id, kind, model, started_ms, finished_ms, prompt, steps, input_tokens,
                    output_tokens, cost_usd, outcome, error
               FROM agent_runs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = st
            .query_map(params![limit as i64], |r| {
                let steps: String = r.get(6)?;
                Ok(RunRow {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    model: r.get(2)?,
                    started_ms: r.get(3)?,
                    finished_ms: r.get(4)?,
                    prompt: r.get(5)?,
                    steps: serde_json::from_str(&steps).unwrap_or_default(),
                    input_tokens: r.get(7)?,
                    output_tokens: r.get(8)?,
                    cost_usd: r.get(9)?,
                    outcome: r.get(10)?,
                    error: r.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_start() {
        // 2026-11-18T04:56:46Z -> 2026-11-01T00:00:00Z.
        assert_eq!(month_start_ms(1_794_891_406_000), 1_793_491_200_000);
        // 2024-02-29 -> 2024-02-01; 1970-01-01 -> 0.
        assert_eq!(month_start_ms(1_709_164_800_000), 1_706_745_600_000);
        assert_eq!(month_start_ms(0), 0);
    }

    #[test]
    fn spend_is_summed_per_month() {
        let s = RunStore::in_memory().unwrap();
        let a = s.start("import", "m", "draw", 1_000).unwrap();
        s.update(a, &serde_json::json!([]), 10, 5, 0.25).unwrap();
        s.finish(a, "ok", None, 2_000).unwrap();
        let b = s.start("ask", "m", "x", 5_000).unwrap();
        s.update(b, &serde_json::json!([]), 1, 1, 0.5).unwrap();
        assert_eq!(s.spent_since(0).unwrap(), 0.75);
        assert_eq!(s.spent_since(3_000).unwrap(), 0.5);
        assert_eq!(s.runs_since(0).unwrap(), 2);
        let r = s.recent(10).unwrap();
        assert_eq!(r[0].kind, "ask");
        assert_eq!(r[1].outcome, "ok");
    }
}
