//! The floor plan: one JSON file in the storage root (`plan.json`), with a
//! version that goes up with every change, and background images in
//! `plan-assets/`. Reading needs any valid token; changing needs the main
//! token. See docs/plan-commands.md.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use argus_plan::{CommandBatch, Plan, Versioned};
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::info;

use crate::http::{AppState, Role};

/// The largest background image accepted.
pub const MAX_ASSET_BYTES: usize = 20 * 1024 * 1024;

pub struct PlanStore {
    path: PathBuf,
    assets: PathBuf,
    current: Mutex<Versioned>,
}

/// Why a change was refused.
#[derive(Debug, PartialEq)]
pub enum Refused {
    /// The plan moved on since the version given.
    Stale {
        current: u64,
    },
    Invalid(String),
}

impl PlanStore {
    /// Loads `plan.json` from `root`, or starts with an empty plan.
    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join("plan.json");
        let current = match std::fs::read(&path) {
            Ok(b) => {
                let mut v: Versioned = serde_json::from_slice(&b)
                    .with_context(|| format!("reading {}", path.display()))?;
                v.plan
                    .check()
                    .with_context(|| format!("checking {}", path.display()))?;
                v
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Versioned {
                version: 0,
                plan: Plan::default(),
            },
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(PlanStore {
            path,
            assets: root.join("plan-assets"),
            current: Mutex::new(current),
        })
    }

    pub fn get(&self) -> Versioned {
        self.current.lock().unwrap().clone()
    }

    /// The plan's version, without copying the plan.
    pub fn version(&self) -> u64 {
        self.current.lock().unwrap().version
    }

    fn save(&self, v: &Versioned) -> Result<()> {
        let tmp = self.path.with_extension("json.partial");
        std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Applies a batch atomically; returns the new version.
    pub fn apply(&self, batch: &CommandBatch) -> Result<u64, Refused> {
        let mut cur = self.current.lock().unwrap();
        if let Some(v) = batch.version
            && v != cur.version
        {
            return Err(Refused::Stale {
                current: cur.version,
            });
        }
        let mut next = cur.clone();
        next.plan
            .apply_all(&batch.commands)
            .map_err(|e| Refused::Invalid(e.to_string()))?;
        next.version += 1;
        self.save(&next)
            .map_err(|e| Refused::Invalid(format!("saving the plan: {e}")))?;
        *cur = next;
        Ok(cur.version)
    }

    /// Replaces the whole plan; returns the new version.
    pub fn replace(&self, mut v: Versioned) -> Result<u64, Refused> {
        let mut cur = self.current.lock().unwrap();
        if v.version != cur.version {
            return Err(Refused::Stale {
                current: cur.version,
            });
        }
        v.plan
            .check()
            .map_err(|e| Refused::Invalid(e.to_string()))?;
        v.version = cur.version + 1;
        self.save(&v)
            .map_err(|e| Refused::Invalid(format!("saving the plan: {e}")))?;
        *cur = v;
        Ok(cur.version)
    }

    /// Stores an image; returns (id, width, height). The id is derived
    /// from the content, so the same image uploads to the same id.
    pub fn add_asset(&self, body: &[u8]) -> Result<(String, u32, u32), String> {
        if body.len() > MAX_ASSET_BYTES {
            return Err(format!(
                "images are at most {} MB",
                MAX_ASSET_BYTES / 1024 / 1024
            ));
        }
        let (ext, (w, h)) = image_info(body).ok_or("only PNG and JPEG images can be used")?;
        let hash = Sha256::digest(body);
        let id = format!(
            "{}.{ext}",
            hash[..16]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        std::fs::create_dir_all(&self.assets).map_err(|e| e.to_string())?;
        let path = self.assets.join(&id);
        if !path.exists() {
            let tmp = self.assets.join(format!("{id}.partial"));
            std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        }
        Ok((id, w, h))
    }

    /// An asset's bytes and content type.
    pub fn asset(&self, id: &str) -> Option<(Vec<u8>, &'static str)> {
        let (stem, ext) = id.split_once('.')?;
        if stem.len() != 32 || !stem.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let ct = match ext {
            "png" => "image/png",
            "jpg" => "image/jpeg",
            _ => return None,
        };
        Some((std::fs::read(self.assets.join(id)).ok()?, ct))
    }
}

/// ("png" or "jpg", (width, height)) for a PNG or JPEG image.
pub fn image_info(b: &[u8]) -> Option<(&'static str, (u32, u32))> {
    if b.len() >= 24 && b.starts_with(b"\x89PNG\r\n\x1a\n") && &b[12..16] == b"IHDR" {
        let w = u32::from_be_bytes(b[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(b[20..24].try_into().ok()?);
        return (w > 0 && h > 0).then_some(("png", (w, h)));
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                return None;
            }
            let m = b[i + 1];
            if m == 0xFF {
                i += 1;
                continue;
            }
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            // Start-of-frame markers carry the size (not DHT/JPG/DAC).
            if (0xC0..=0xCF).contains(&m) && ![0xC4, 0xC8, 0xCC].contains(&m) {
                let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
                let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
                return (w > 0 && h > 0).then_some(("jpg", (w, h)));
            }
            i += 2 + len;
        }
    }
    None
}

type Denied = (StatusCode, &'static str);

fn store(st: &AppState) -> Result<&Arc<PlanStore>, Denied> {
    st.plan
        .as_ref()
        .ok_or((StatusCode::NOT_FOUND, "no floor plan here"))
}

fn admin(role: Role) -> Result<(), Denied> {
    if role == Role::Admin {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "only the main token can change the plan",
        ))
    }
}

fn refused(r: Refused) -> Response {
    match r {
        Refused::Stale { current } => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "the plan has changed; reload it", "version": current })),
        )
            .into_response(),
        Refused::Invalid(e) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response()
        }
    }
}

pub async fn get_plan(State(st): State<AppState>) -> Response {
    match store(&st) {
        Ok(s) => ([(header::CACHE_CONTROL, "no-store")], Json(s.get())).into_response(),
        Err(r) => r.into_response(),
    }
}

pub async fn put_plan(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
    Json(body): Json<Versioned>,
) -> Response {
    let s = match store(&st).and_then(|s| admin(role).map(|_| s)) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    match s.replace(body) {
        Ok(version) => {
            info!(version, "plan replaced");
            Json(json!({ "version": version })).into_response()
        }
        Err(r) => refused(r),
    }
}

pub async fn post_commands(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
    Json(body): Json<CommandBatch>,
) -> Response {
    let s = match store(&st).and_then(|s| admin(role).map(|_| s)) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    match s.apply(&body) {
        Ok(version) => {
            info!(version, commands = body.commands.len(), "plan changed");
            Json(json!({ "version": version })).into_response()
        }
        Err(r) => refused(r),
    }
}

pub async fn post_asset(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
    body: Bytes,
) -> Response {
    let s = match store(&st).and_then(|s| admin(role).map(|_| s)) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    match s.add_asset(&body) {
        Ok((id, width, height)) => {
            info!(%id, width, height, bytes = body.len(), "plan image added");
            (
                StatusCode::CREATED,
                Json(json!({ "id": id, "width": width, "height": height })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

pub async fn get_asset(State(st): State<AppState>, UrlPath(id): UrlPath<String>) -> Response {
    let s = match store(&st) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    match s.asset(&id) {
        Some((bytes, ct)) => (
            [
                (header::CONTENT_TYPE, ct),
                (
                    header::CACHE_CONTROL,
                    "private, max-age=31536000, immutable",
                ),
            ],
            bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_plan::{Command, pt};

    #[test]
    fn versions_and_atomic_batches() {
        let dir = tempfile::tempdir().unwrap();
        let s = PlanStore::open(dir.path()).unwrap();
        assert_eq!(s.get().version, 0);
        let level = s.get().plan.levels[0].id;
        let room = |name: &str| Command::AddRoom {
            level,
            id: None,
            name: name.into(),
            at: pt(0.0, 0.0),
            polygon: None,
        };
        let v = s
            .apply(&CommandBatch {
                version: Some(0),
                commands: vec![room("Study")],
            })
            .unwrap();
        assert_eq!(v, 1);
        // Stale version: refused.
        assert_eq!(
            s.apply(&CommandBatch {
                version: Some(0),
                commands: vec![room("x")],
            }),
            Err(Refused::Stale { current: 1 })
        );
        // A bad command anywhere in a batch: nothing applied.
        let bad = s.apply(&CommandBatch {
            version: None,
            commands: vec![
                room("Ok"),
                Command::DeleteLevel {
                    level: argus_plan::Id(4242),
                },
            ],
        });
        assert!(matches!(bad, Err(Refused::Invalid(_))));
        assert_eq!(s.get().version, 1);
        assert_eq!(s.get().plan.levels[0].rooms.len(), 1);
        // Survives a restart.
        let again = PlanStore::open(dir.path()).unwrap();
        assert_eq!(again.get(), s.get());
        // Whole-plan replace with a version check.
        let mut whole = again.get();
        whole.plan = argus_plan::fixture::demo_house();
        assert_eq!(again.replace(whole.clone()), Ok(2));
        assert!(matches!(again.replace(whole), Err(Refused::Stale { .. })));
    }

    #[test]
    fn image_sizes() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(640u32.to_be_bytes());
        png.extend(480u32.to_be_bytes());
        png.extend([8, 6, 0, 0, 0]);
        assert_eq!(image_info(&png), Some(("png", (640, 480))));
        // SOI, an APP0 segment, then SOF0 with 1080 x 1920.
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0, 0];
        jpg.extend([
            0xFF, 0xC0, 0x00, 0x11, 8, 0x04, 0x38, 0x07, 0x80, 3, 0, 0, 0,
        ]);
        assert_eq!(image_info(&jpg), Some(("jpg", (1920, 1080))));
        assert_eq!(image_info(b"GIF89a....................."), None);

        let dir = tempfile::tempdir().unwrap();
        let s = PlanStore::open(dir.path()).unwrap();
        let (id, w, h) = s.add_asset(&png).unwrap();
        assert_eq!((w, h), (640, 480));
        assert!(id.ends_with(".png") && id.len() == 36);
        assert_eq!(s.asset(&id).unwrap().0, png);
        assert!(s.asset("../plan.json").is_none());
        assert!(s.add_asset(b"not an image").is_err());
    }
}
