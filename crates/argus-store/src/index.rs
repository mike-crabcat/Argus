use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::gop_index::{self, GopEntry};

/// The metadata store. Answers "which bytes hold camera X at time T" without
/// touching the video itself.
pub struct Index {
    conn: Connection,
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
"#;

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
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
    ) -> Result<i64> {
        let fp = format!("{fingerprint:016x}");
        self.conn.execute(
            "INSERT OR IGNORE INTO init_segment
                 (camera, stream, fingerprint, object_key, codec, rfc6381, width, height)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                camera,
                stream,
                fp,
                object_key,
                cfg.codec.as_str(),
                cfg.rfc6381,
                cfg.width,
                cfg.height
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
              WHERE camera = ?1 AND stream = ?2 AND end_ms > ?3 AND start_ms < ?4 ORDER BY start_ms",
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_media::VideoConfig;

    #[test]
    fn seek_finds_gop_bytes() {
        let idx = Index::open_in_memory().unwrap();
        let cfg = VideoConfig::from_avcc(&[1, 0x4d, 0, 0x1f, 0xff, 0xe0, 0], 640, 360).unwrap();
        let init = idx
            .upsert_init("drive", "sub", 42, "init/drive/sub/x.mp4", &cfg)
            .unwrap();
        assert_eq!(
            idx.upsert_init("drive", "sub", 42, "init/drive/sub/x.mp4", &cfg)
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
