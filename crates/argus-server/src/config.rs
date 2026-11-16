use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use argus_store::{Policy, StreamPolicy};
use serde::Deserialize;

/// `argus.toml`. See `argus.example.toml` at the repo root.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub storage: Storage,
    /// Object storage. Without it, recordings stay on local disk only.
    pub s3: Option<S3>,
    #[serde(default, rename = "camera")]
    pub cameras: Vec<Camera>,
}

/// An S3 bucket, on AWS or any S3-compatible store (MinIO, Backblaze B2,
/// Wasabi, ...). Credentials come from, in order: `access_key_id` and
/// `secret_access_key` here, the standard `AWS_*` environment variables, or
/// the shared credentials file (`~/.aws/credentials`, profile `profile`,
/// `$AWS_PROFILE` or `default`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3 {
    pub bucket: String,
    #[serde(default = "default_region")]
    pub region: String,
    /// For S3-compatible stores, e.g. `http://minio.lan:9000`.
    pub endpoint: Option<String>,
    /// Key prefix inside the bucket, e.g. `argus` for `argus/rec/...`.
    #[serde(default)]
    pub prefix: String,
    pub profile: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    /// Average upload rate cap in megabits per second; 0 = unlimited.
    #[serde(default = "default_max_upload_mbps")]
    pub max_upload_mbps: f64,
}

impl std::fmt::Debug for S3 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("prefix", &self.prefix)
            .field("profile", &self.profile)
            .field("access_key_id", &self.access_key_id.as_ref().map(|_| "***"))
            .field(
                "secret_access_key",
                &self.secret_access_key.as_ref().map(|_| "***"),
            )
            .field("max_upload_mbps", &self.max_upload_mbps)
            .finish()
    }
}

fn default_region() -> String {
    "us-east-1".into()
}

fn default_max_upload_mbps() -> f64 {
    50.0
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    /// Local hot tier: chunks, init segments and the index database.
    pub root: PathBuf,
    #[serde(default = "default_chunk_seconds")]
    pub chunk_seconds: u32,
    /// Cap on local recordings, in GB (10^9 bytes). Above it the oldest
    /// chunks that are safe to delete locally are removed. No cap if unset.
    pub local_budget_gb: Option<f64>,
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
    /// Stream name to upload policy: "always", "events" or "never".
    /// Defaults: `main` = events, anything else (e.g. `sub`) = always.
    #[serde(default)]
    pub upload: BTreeMap<String, String>,
}

impl Camera {
    pub fn policy(&self, stream: &str) -> Policy {
        self.upload
            .get(stream)
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| Policy::default_for(stream))
    }
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
            for (stream, policy) in &cam.upload {
                if !cam.streams.contains_key(stream) {
                    bail!(
                        "camera {:?} has upload policy for unknown stream {stream:?}",
                        cam.name
                    );
                }
                policy
                    .parse::<Policy>()
                    .with_context(|| format!("camera {:?} stream {stream:?}", cam.name))?;
            }
        }
        if let Some(s3) = &cfg.s3 {
            if s3.access_key_id.is_some() != s3.secret_access_key.is_some() {
                bail!("[s3] needs both access_key_id and secret_access_key, or neither");
            }
            if s3.max_upload_mbps.is_nan() || s3.max_upload_mbps < 0.0 {
                bail!("[s3] max_upload_mbps must be 0 (unlimited) or more");
            }
        }
        if cfg
            .storage
            .local_budget_gb
            .is_some_and(|gb| gb.is_nan() || gb <= 0.0)
        {
            bail!("[storage] local_budget_gb must be more than 0");
        }
        Ok(cfg)
    }

    /// Every configured stream's effective upload policy. Without `[s3]`,
    /// nothing uploads.
    pub fn stream_policies(&self) -> Vec<StreamPolicy> {
        self.cameras
            .iter()
            .flat_map(|cam| {
                cam.streams.keys().map(|stream| StreamPolicy {
                    camera: cam.name.clone(),
                    stream: stream.clone(),
                    policy: self.policy(&cam.name, stream),
                })
            })
            .collect()
    }

    /// Upload policy for any recorded stream, including ones since removed
    /// from the config (they get the default for their stream name).
    pub fn policy(&self, camera: &str, stream: &str) -> Policy {
        if self.s3.is_none() {
            return Policy::Never;
        }
        match self.cameras.iter().find(|c| c.name == camera) {
            Some(c) => c.policy(stream),
            None => Policy::default_for(stream),
        }
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
    fn upload_policies_and_s3_settings() {
        let cfg = Config::parse(
            r#"
            [storage]
            root = "/tmp"
            local_budget_gb = 500
            [s3]
            bucket = "nvr"
            endpoint = "http://127.0.0.1:9000"
            access_key_id = "AKIDEXAMPLE"
            secret_access_key = "very-secret"
            [[camera]]
            name = "door"
            streams = { main = "http://a", sub = "http://b", ext = "http://c" }
            upload = { ext = "never" }
            [[camera]]
            name = "yard"
            streams = { main = "http://d" }
            upload = { main = "always" }
            "#,
        )
        .unwrap();
        let s3 = cfg.s3.as_ref().unwrap();
        assert_eq!(
            (s3.region.as_str(), s3.max_upload_mbps),
            ("us-east-1", 50.0)
        );
        assert!(!format!("{cfg:?}").contains("very-secret"));
        let p: Vec<_> = cfg
            .stream_policies()
            .into_iter()
            .map(|s| format!("{}/{}={:?}", s.camera, s.stream, s.policy))
            .collect();
        assert_eq!(
            p,
            [
                "door/ext=Never",
                "door/main=Events",
                "door/sub=Always",
                "yard/main=Always"
            ]
        );
        assert_eq!(cfg.policy("gone", "main"), Policy::Events);

        let no_s3 = Config::parse("[storage]\nroot='/tmp'\n").unwrap();
        assert_eq!(no_s3.policy("door", "sub"), Policy::Never);
    }

    #[test]
    fn rejects_bad_upload_settings() {
        let base = "[storage]\nroot='/tmp'\n[[camera]]\nname='a'\nstreams={main='rtsp://a'}\n";
        assert!(Config::parse(&format!("{base}upload={{main='sometimes'}}\n")).is_err());
        assert!(Config::parse(&format!("{base}upload={{sub='always'}}\n")).is_err());
        let half_keys = "[storage]\nroot='/tmp'\n[s3]\nbucket='b'\naccess_key_id='x'\n";
        assert!(Config::parse(half_keys).is_err());
    }

    #[test]
    fn rejects_unsafe_names() {
        let bad = "[storage]\nroot='/tmp'\n[[camera]]\nname='../x'\nstreams={main='rtsp://a'}\n";
        assert!(Config::parse(bad).is_err());
    }
}
