use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// `argus.toml`. See `argus.example.toml` at the repo root.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub storage: Storage,
    #[serde(default, rename = "camera")]
    pub cameras: Vec<Camera>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    /// Local hot tier: chunks, init segments and the index database.
    pub root: PathBuf,
    #[serde(default = "default_chunk_seconds")]
    pub chunk_seconds: u32,
}

fn default_chunk_seconds() -> u32 {
    60
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Camera {
    pub name: String,
    /// Stream name (e.g. `main`, `sub`) to source URL.
    pub streams: BTreeMap<String, String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text)?;
        let mut seen = std::collections::HashSet::new();
        for cam in &cfg.cameras {
            if !argus_store::keys::is_valid_name(&cam.name) {
                bail!(
                    "camera name {:?} must be 1-64 chars of a-z, 0-9, - or _",
                    cam.name
                );
            }
            if !seen.insert(&cam.name) {
                bail!("duplicate camera {:?}", cam.name);
            }
            if cam.streams.is_empty() {
                bail!("camera {:?} has no streams", cam.name);
            }
            for name in cam.streams.keys() {
                if !argus_store::keys::is_valid_name(name) {
                    bail!(
                        "stream name {name:?} on camera {:?} is not path-safe",
                        cam.name
                    );
                }
            }
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_config() {
        let cfg = Config::load(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../argus.example.toml"
        )))
        .unwrap();
        assert_eq!(cfg.storage.chunk_seconds, 60);
        assert!(cfg.cameras.iter().any(|c| c.streams.contains_key("sub")));
    }

    #[test]
    fn rejects_unsafe_names() {
        let bad = "[storage]\nroot='/tmp'\n[[camera]]\nname='../x'\nstreams={main='rtsp://a'}\n";
        assert!(Config::parse(bad).is_err());
    }
}
