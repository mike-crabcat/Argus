use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::gop_index::{self, GopEntry};

/// The metadata store. Answers "which bytes hold camera X at time T" without
/// touching the video itself.
pub struct Index {
    pub(crate) conn: Connection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkRecord {
    pub camera: String,
    pub stream: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub object_key: String,
    pub bytes: u64,
    pub init_id: i64,
    pub gops: Vec<GopEntry>,
}

/// Everything a client needs to fetch and decode from time T.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeekTarget {
    pub object_key: String,
    pub init_key: String,
    pub gop_start_ms: i64,
    pub byte_offset: u64,
    pub byte_len: u64,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS init_segment (
    id          INTEGER PRIMARY KEY,
    camera      TEXT NOT NULL,
    stream      TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    object_key  TEXT NOT NULL,
    codec       TEXT NOT NULL,
    rfc6381     TEXT NOT NULL,
    width       INTEGER NOT NULL,
    height      INTEGER NOT NULL,
    UNIQUE (camera, stream, fingerprint)
);
CREATE TABLE IF NOT EXISTS chunk (
    id          INTEGER PRIMARY KEY,
    camera      TEXT NOT NULL,
    stream      TEXT NOT NULL,
    start_ms    INTEGER NOT NULL,
    end_ms      INTEGER NOT NULL,
    object_key  TEXT NOT NULL UNIQUE,
    bytes       INTEGER NOT NULL,
    init_id     INTEGER NOT NULL REFERENCES init_segment(id),
    tier        TEXT NOT NULL DEFAULT 'local',
    gop_index   BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS chunk_by_time ON chunk (camera, stream, start_ms);
-- Things worth keeping at full resolution: manual marks now, camera motion
-- and AI triggers and Argus's own detections later.
CREATE TABLE IF NOT EXISTS event (
    id          INTEGER PRIMARY KEY,
    camera      TEXT NOT NULL,
    start_ms    INTEGER NOT NULL,
    end_ms      INTEGER NOT NULL,
    source      TEXT NOT NULL,
    label       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS event_by_time ON event (camera, start_ms);
"#;

/// Where a chunk's bytes live. `tier` on the chunk table.
pub mod tier {
    /// On local disk (and, if `uploaded_at` is set, also in object storage).
    pub const LOCAL: &str = "local";
    /// Evicted from local disk after upload; only in object storage.
    pub const REMOTE: &str = "remote";
    /// Evicted without ever being uploaded (by policy). The row stays as a
    /// record that the recording existed.
    pub const DISCARDED: &str = "discarded";
}

/// Columns added after the first release, as (table, column, type). Added
/// to existing databases on open.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    // RFC 6381 codec of the audio track, NULL for video-only init segments.
    ("init_segment", "audio_rfc6381", "TEXT"),
    // Upload state: when (ms since epoch) and under which object key.
    ("chunk", "uploaded_at", "INTEGER"),
    ("chunk", "remote", "TEXT"),
    ("init_segment", "uploaded_at", "INTEGER"),
    ("init_segment", "remote", "TEXT"),
];

/// A chunk replay can read, with where its bytes are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayChunk {
    pub chunk: ChunkRecord,
    pub init_key: String,
    /// The file is (still) on local disk.
    pub local: bool,
    /// Its key in object storage, if uploaded.
    pub remote: Option<String>,
    pub init_remote: Option<String>,
}

/// A closed chunk, as the uploader and eviction see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredChunk {
    pub id: i64,
    pub camera: String,
    pub stream: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub object_key: String,
    pub bytes: u64,
    pub init_id: i64,
    pub uploaded: bool,
}

const STORED_CHUNK_COLUMNS: &str = "c.id, c.camera, c.stream, c.start_ms, c.end_ms, c.object_key, c.bytes, c.init_id, \
     c.uploaded_at IS NOT NULL";

fn stored_chunk(r: &rusqlite::Row) -> rusqlite::Result<StoredChunk> {
    Ok(StoredChunk {
        id: r.get(0)?,
        camera: r.get(1)?,
        stream: r.get(2)?,
        start_ms: r.get(3)?,
        end_ms: r.get(4)?,
        object_key: r.get(5)?,
        bytes: r.get::<_, i64>(6)? as u64,
        init_id: r.get(7)?,
        uploaded: r.get(8)?,
    })
}

/// SQL condition: an event on the chunk's camera overlaps the chunk,
/// widened by `?pad` ms on both sides.
const EVENT_OVERLAP: &str = "EXISTS (SELECT 1 FROM event e WHERE e.camera = c.camera \
     AND e.start_ms < c.end_ms + :pad AND e.end_ms > c.start_ms - :pad)";

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // The CLI writes events while the server runs; wait for its locks.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(crate::summaries::SCHEMA)?;
        conn.execute_batch(crate::tracks::SCHEMA)?;
        for (table, column, ty) in ADDED_COLUMNS {
            let exists: bool = conn.query_row(
                &format!("SELECT COUNT(*) > 0 FROM pragma_table_info('{table}') WHERE name = ?1"),
                [column],
                |r| r.get(0),
            )?;
            if !exists {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ty}"))?;
            }
        }
        Ok(Self { conn })
    }

    /// Records an init segment version, returning its id (existing or new).
    pub fn upsert_init(
        &self,
        camera: &str,
        stream: &str,
        fingerprint: u64,
        object_key: &str,
        cfg: &argus_media::VideoConfig,
        audio: Option<&argus_media::AudioConfig>,
    ) -> Result<i64> {
        let fp = format!("{fingerprint:016x}");
        self.conn.execute(
            "INSERT OR IGNORE INTO init_segment
                 (camera, stream, fingerprint, object_key, codec, rfc6381, width, height,
                  audio_rfc6381)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                camera,
                stream,
                fp,
                object_key,
                cfg.codec.as_str(),
                cfg.rfc6381,
                cfg.width,
                cfg.height,
                audio.map(|a| a.rfc6381.as_str())
            ],
        )?;
        Ok(self.conn.query_row(
            "SELECT id FROM init_segment WHERE camera = ?1 AND stream = ?2 AND fingerprint = ?3",
            params![camera, stream, fp],
            |r| r.get(0),
        )?)
    }

    pub fn insert_chunk(&self, c: &ChunkRecord) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO chunk (camera, stream, start_ms, end_ms, object_key, bytes, init_id, gop_index)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                c.camera,
                c.stream,
                c.start_ms,
                c.end_ms,
                c.object_key,
                c.bytes as i64,
                c.init_id,
                gop_index::encode(&c.gops)
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Resolves (camera, stream, t) to the GOP that contains t.
    pub fn seek(&self, camera: &str, stream: &str, t_ms: i64) -> Result<Option<SeekTarget>> {
        let row = self
            .conn
            .query_row(
                "SELECT c.object_key, i.object_key, c.start_ms, c.gop_index
                   FROM chunk c JOIN init_segment i ON i.id = c.init_id
                  WHERE c.camera = ?1 AND c.stream = ?2 AND c.start_ms <= ?3 AND c.end_ms > ?3
                    AND c.tier != 'discarded'
                  ORDER BY c.start_ms DESC LIMIT 1",
                params![camera, stream, t_ms],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((object_key, init_key, start_ms, blob)) = row else {
            return Ok(None);
        };
        let gops = gop_index::decode(&blob);
        let offset = u32::try_from(t_ms - start_ms).unwrap_or(u32::MAX);
        Ok(gop_index::find(&gops, offset).map(|i| {
            let g = gops[i];
            SeekTarget {
                object_key,
                init_key,
                gop_start_ms: start_ms + i64::from(g.start_offset_ms),
                byte_offset: u64::from(g.byte_offset),
                byte_len: u64::from(g.byte_len),
            }
        }))
    }

    /// Records an event; returns its id.
    pub fn add_event(
        &self,
        camera: &str,
        start_ms: i64,
        end_ms: i64,
        source: &str,
        label: &str,
    ) -> Result<i64> {
        anyhow::ensure!(end_ms >= start_ms, "event ends before it starts");
        self.conn.execute(
            "INSERT INTO event (camera, start_ms, end_ms, source, label) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![camera, start_ms, end_ms, source, label],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Moves an event's end (events are written as they happen and grow).
    pub fn set_event_end(&self, id: i64, end_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE event SET end_ms = MAX(start_ms, ?2) WHERE id = ?1",
            params![id, end_ms],
        )?;
        Ok(())
    }

    /// Events on `camera` overlapping [from_ms, to_ms), oldest first, as
    /// (start_ms, end_ms, source, label).
    pub fn events(
        &self,
        camera: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<(i64, i64, String, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT start_ms, end_ms, source, label FROM event
              WHERE camera = ?1 AND end_ms >= ?2 AND start_ms < ?3 ORDER BY start_ms, id",
        )?;
        let rows = st
            .query_map(params![camera, from_ms, to_ms], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Local chunks of one stream not yet uploaded, oldest first. With
    /// `event_pad_ms`, only those overlapping an event (widened by that much).
    pub fn pending_uploads(
        &self,
        camera: &str,
        stream: &str,
        event_pad_ms: Option<i64>,
        limit: usize,
    ) -> Result<Vec<StoredChunk>> {
        let filter = if event_pad_ms.is_some() {
            format!("AND {EVENT_OVERLAP}")
        } else {
            String::new()
        };
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT {STORED_CHUNK_COLUMNS} FROM chunk c
              WHERE c.camera = :camera AND c.stream = :stream AND c.tier = 'local'
                AND c.uploaded_at IS NULL {filter}
              ORDER BY c.start_ms LIMIT :limit"
        ))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = if let Some(pad) = event_pad_ms {
            st.query_map(
                rusqlite::named_params! {":camera": camera, ":stream": stream, ":pad": pad, ":limit": limit},
                stored_chunk,
            )?
            .collect::<Result<Vec<_>, _>>()?
        } else {
            st.query_map(
                rusqlite::named_params! {":camera": camera, ":stream": stream, ":limit": limit},
                stored_chunk,
            )?
            .collect::<Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    /// Whether an event on `camera` overlaps the chunk, widened by `pad_ms`.
    pub fn chunk_has_event(&self, chunk: &StoredChunk, pad_ms: i64) -> Result<bool> {
        Ok(self.conn.query_row(
            &format!("SELECT {EVENT_OVERLAP} FROM chunk c WHERE c.id = :id"),
            rusqlite::named_params! {":id": chunk.id, ":pad": pad_ms},
            |r| r.get(0),
        )?)
    }

    /// An init segment's object key and whether it has been uploaded.
    pub fn init_upload_state(&self, init_id: i64) -> Result<(String, bool)> {
        Ok(self.conn.query_row(
            "SELECT object_key, uploaded_at IS NOT NULL FROM init_segment WHERE id = ?1",
            [init_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    pub fn mark_init_uploaded(&self, init_id: i64, remote: &str, at_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE init_segment SET uploaded_at = ?2, remote = ?3 WHERE id = ?1",
            params![init_id, at_ms, remote],
        )?;
        Ok(())
    }

    pub fn mark_chunk_uploaded(&self, chunk_id: i64, remote: &str, at_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE chunk SET uploaded_at = ?2, remote = ?3 WHERE id = ?1",
            params![chunk_id, at_ms, remote],
        )?;
        Ok(())
    }

    pub fn set_tier(&self, chunk_id: i64, tier: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE chunk SET tier = ?2 WHERE id = ?1",
            params![chunk_id, tier],
        )?;
        Ok(())
    }

    /// Bytes of chunk files on local disk.
    pub fn local_bytes(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(bytes), 0) FROM chunk WHERE tier = 'local'",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Local chunks, oldest first, starting after `after` (start_ms, id).
    pub fn local_chunks_oldest_first(
        &self,
        after: Option<(i64, i64)>,
        limit: usize,
    ) -> Result<Vec<StoredChunk>> {
        let (t, id) = after.unwrap_or((i64::MIN, i64::MIN));
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT {STORED_CHUNK_COLUMNS} FROM chunk c
              WHERE c.tier = 'local' AND (c.start_ms, c.id) > (?1, ?2)
              ORDER BY c.start_ms, c.id LIMIT ?3"
        ))?;
        let rows = st
            .query_map(
                params![t, id, i64::try_from(limit).unwrap_or(i64::MAX)],
                stored_chunk,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Chunks overlapping [from_ms, to_ms), oldest first.
    pub fn chunks_between(
        &self,
        camera: &str,
        stream: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<ChunkRecord>> {
        let mut st = self.conn.prepare(
            "SELECT camera, stream, start_ms, end_ms, object_key, bytes, init_id, gop_index FROM chunk
              WHERE camera = ?1 AND stream = ?2 AND end_ms > ?3 AND start_ms < ?4
                AND tier != 'discarded' ORDER BY start_ms",
        )?;
        let rows = st.query_map(params![camera, stream, from_ms, to_ms], |r| {
            Ok(ChunkRecord {
                camera: r.get(0)?,
                stream: r.get(1)?,
                start_ms: r.get(2)?,
                end_ms: r.get(3)?,
                object_key: r.get(4)?,
                bytes: r.get::<_, i64>(5)? as u64,
                init_id: r.get(6)?,
                gops: gop_index::decode(&r.get::<_, Vec<u8>>(7)?),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Codec string and size recorded for an init segment, by object key.
    pub fn init_info(&self, object_key: &str) -> Result<Option<(String, u16, u16)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT rfc6381, width, height FROM init_segment WHERE object_key = ?1",
                [object_key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    /// Chunks ending after `from_ms` that can be played: on local disk, or
    /// only in object storage (with their remote keys). Oldest first.
    pub fn replay_chunks_from(
        &self,
        camera: &str,
        stream: &str,
        from_ms: i64,
        limit: usize,
    ) -> Result<Vec<ReplayChunk>> {
        let mut st = self.conn.prepare_cached(
            "SELECT c.camera, c.stream, c.start_ms, c.end_ms, c.object_key, c.bytes, c.init_id,
                    c.gop_index, i.object_key, c.tier, c.remote, i.remote
               FROM chunk c JOIN init_segment i ON i.id = c.init_id
              WHERE c.camera = ?1 AND c.stream = ?2 AND c.end_ms > ?3
                AND (c.tier = 'local' OR (c.tier = 'remote' AND c.remote IS NOT NULL))
              ORDER BY c.start_ms LIMIT ?4",
        )?;
        let rows = st.query_map(
            params![
                camera,
                stream,
                from_ms,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |r| {
                Ok(ReplayChunk {
                    chunk: ChunkRecord {
                        camera: r.get(0)?,
                        stream: r.get(1)?,
                        start_ms: r.get(2)?,
                        end_ms: r.get(3)?,
                        object_key: r.get(4)?,
                        bytes: r.get::<_, i64>(5)? as u64,
                        init_id: r.get(6)?,
                        gops: gop_index::decode(&r.get::<_, Vec<u8>>(7)?),
                    },
                    init_key: r.get(8)?,
                    local: r.get::<_, String>(9)? == tier::LOCAL,
                    remote: r.get(10)?,
                    init_remote: r.get(11)?,
                })
            },
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The object-storage key of a chunk or init segment, if uploaded.
    pub fn remote_key_of(&self, object_key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT remote FROM chunk WHERE object_key = ?1 AND remote IS NOT NULL
                 UNION ALL
                 SELECT remote FROM init_segment WHERE object_key = ?1 AND remote IS NOT NULL
                 LIMIT 1",
                [object_key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Chunks on local disk ending after `from_ms`, oldest first, with their
    /// init segment keys: what replay reads from.
    pub fn local_chunks_from(
        &self,
        camera: &str,
        stream: &str,
        from_ms: i64,
        limit: usize,
    ) -> Result<Vec<(ChunkRecord, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT c.camera, c.stream, c.start_ms, c.end_ms, c.object_key, c.bytes, c.init_id,
                    c.gop_index, i.object_key
               FROM chunk c JOIN init_segment i ON i.id = c.init_id
              WHERE c.camera = ?1 AND c.stream = ?2 AND c.end_ms > ?3 AND c.tier = 'local'
              ORDER BY c.start_ms LIMIT ?4",
        )?;
        let rows = st.query_map(
            params![
                camera,
                stream,
                from_ms,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |r| {
                Ok((
                    ChunkRecord {
                        camera: r.get(0)?,
                        stream: r.get(1)?,
                        start_ms: r.get(2)?,
                        end_ms: r.get(3)?,
                        object_key: r.get(4)?,
                        bytes: r.get::<_, i64>(5)? as u64,
                        init_id: r.get(6)?,
                        gops: gop_index::decode(&r.get::<_, Vec<u8>>(7)?),
                    },
                    r.get(8)?,
                ))
            },
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Recorded spans of a stream within [from_ms, to_ms), as (start, end),
    /// merging chunks less than `gap_ms` apart. For drawing a timeline.
    pub fn coverage(
        &self,
        camera: &str,
        stream: &str,
        from_ms: i64,
        to_ms: i64,
        gap_ms: i64,
    ) -> Result<Vec<(i64, i64)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT start_ms, end_ms FROM chunk
              WHERE camera = ?1 AND stream = ?2 AND end_ms > ?3 AND start_ms < ?4
                AND tier != 'discarded' ORDER BY start_ms",
        )?;
        let mut spans: Vec<(i64, i64)> = Vec::new();
        for row in st.query_map(params![camera, stream, from_ms, to_ms], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })? {
            let (a, b) = row?;
            match spans.last_mut() {
                Some(last) if a - last.1 < gap_ms => last.1 = last.1.max(b),
                _ => spans.push((a, b)),
            }
        }
        Ok(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_media::VideoConfig;

    #[test]
    fn adds_audio_column_to_existing_databases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.sqlite");
        // SCHEMA is the table layout as first released; the audio column
        // only exists through ADDED_COLUMNS.
        Connection::open(&path)
            .unwrap()
            .execute_batch(SCHEMA)
            .unwrap();
        let has_column = |idx: &Index| -> bool {
            idx.conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('init_segment') WHERE name = 'audio_rfc6381'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert!(!has_column(&Index {
            conn: Connection::open(&path).unwrap()
        }));
        let idx = Index::open(&path).unwrap();
        assert!(has_column(&idx));
        let cfg = VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360).unwrap();
        let audio = argus_media::AudioConfig::from_aac_asc(&[0x14, 0x08]).unwrap();
        idx.upsert_init("c", "s", 1, "k", &cfg, Some(&audio))
            .unwrap();
        drop(idx);
        Index::open(&path).unwrap(); // reopening is a no-op
    }

    #[test]
    fn coverage_merges_adjacent_chunks_and_replay_reads_local_ones() {
        let idx = Index::open_in_memory().unwrap();
        let cfg = VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360).unwrap();
        let init = idx
            .upsert_init("c", "s", 1, "init/c/s/1.mp4", &cfg, None)
            .unwrap();
        for (i, (a, b)) in [(0, 60_000), (60_500, 120_000), (300_000, 360_000)]
            .into_iter()
            .enumerate()
        {
            idx.insert_chunk(&ChunkRecord {
                camera: "c".into(),
                stream: "s".into(),
                start_ms: a,
                end_ms: b,
                object_key: format!("rec/{i}"),
                bytes: 1,
                init_id: init,
                gops: vec![],
            })
            .unwrap();
        }
        assert_eq!(
            idx.coverage("c", "s", 0, i64::MAX, 2000).unwrap(),
            [(0, 120_000), (300_000, 360_000)]
        );
        let ids = idx.local_chunks_oldest_first(None, 10).unwrap();
        idx.set_tier(ids[1].id, tier::REMOTE).unwrap();
        let local = idx.local_chunks_from("c", "s", 30_000, 10).unwrap();
        assert_eq!(
            local
                .iter()
                .map(|(c, init)| (c.object_key.as_str(), init.as_str()))
                .collect::<Vec<_>>(),
            [("rec/0", "init/c/s/1.mp4"), ("rec/2", "init/c/s/1.mp4")]
        );
    }

    #[test]
    fn seek_finds_gop_bytes() {
        let idx = Index::open_in_memory().unwrap();
        let cfg = VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360).unwrap();
        let init = idx
            .upsert_init("drive", "sub", 42, "init/drive/sub/x.mp4", &cfg, None)
            .unwrap();
        assert_eq!(
            idx.upsert_init("drive", "sub", 42, "init/drive/sub/x.mp4", &cfg, None)
                .unwrap(),
            init
        );
        let gops = (0..3)
            .map(|i| GopEntry {
                start_offset_ms: i * 1000,
                byte_offset: i * 100,
                byte_len: 100,
                frame_count: 10,
            })
            .collect();
        idx.insert_chunk(&ChunkRecord {
            camera: "drive".into(),
            stream: "sub".into(),
            start_ms: 10_000,
            end_ms: 13_000,
            object_key: "rec/a.m4s".into(),
            bytes: 300,
            init_id: init,
            gops,
        })
        .unwrap();
        let t = idx.seek("drive", "sub", 11_500).unwrap().unwrap();
        assert_eq!(
            (t.gop_start_ms, t.byte_offset, t.byte_len),
            (11_000, 100, 100)
        );
        assert_eq!(t.init_key, "init/drive/sub/x.mp4");
        assert!(idx.seek("drive", "sub", 13_000).unwrap().is_none());
        assert_eq!(
            idx.chunks_between("drive", "sub", 0, 20_000).unwrap().len(),
            1
        );
    }
}
