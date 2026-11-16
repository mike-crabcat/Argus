//! Connects to the configured S3 bucket.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;

use crate::config::S3;

/// Credentials from a shared credentials file.
#[derive(Debug, PartialEq, Eq)]
pub struct ProfileCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

pub fn connect(cfg: &S3) -> Result<Arc<dyn ObjectStore>> {
    // AWS_* environment variables first (keys, session token, endpoint...).
    let mut b = AmazonS3Builder::from_env()
        .with_bucket_name(&cfg.bucket)
        .with_region(&cfg.region);
    if let Some(endpoint) = &cfg.endpoint {
        b = b
            .with_endpoint(endpoint)
            .with_allow_http(endpoint.starts_with("http://"));
    }
    if let (Some(id), Some(secret)) = (&cfg.access_key_id, &cfg.secret_access_key) {
        b = b.with_access_key_id(id).with_secret_access_key(secret);
    } else if std::env::var_os("AWS_ACCESS_KEY_ID").is_none() {
        let profile = cfg
            .profile
            .clone()
            .or_else(|| std::env::var("AWS_PROFILE").ok())
            .unwrap_or_else(|| "default".into());
        if let Some(path) = credentials_file() {
            if let Some(c) = read_profile(&path, &profile)? {
                b = b
                    .with_access_key_id(c.access_key_id)
                    .with_secret_access_key(c.secret_access_key);
                if let Some(t) = c.session_token {
                    b = b.with_token(t);
                }
            }
        }
        // Otherwise object_store falls back to instance/container roles.
    }
    Ok(Arc::new(b.build().context("configuring S3")?))
}

fn credentials_file() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AWS_SHARED_CREDENTIALS_FILE") {
        return Some(p.into());
    }
    std::env::var_os("HOME").map(|h| Path::new(&h).join(".aws/credentials"))
}

/// Reads one `[profile]` section of an AWS shared credentials file. A
/// missing file or profile is `None`, not an error.
pub fn read_profile(path: &Path, profile: &str) -> Result<Option<ProfileCredentials>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let (mut id, mut secret, mut token) = (None, None, None);
    let mut in_profile = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_profile = name.trim() == profile;
            continue;
        }
        if !in_profile {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().to_owned();
            match k.trim() {
                "aws_access_key_id" => id = Some(v),
                "aws_secret_access_key" => secret = Some(v),
                "aws_session_token" => token = Some(v),
                _ => {}
            }
        }
    }
    Ok(match (id, secret) {
        (Some(access_key_id), Some(secret_access_key)) => Some(ProfileCredentials {
            access_key_id,
            secret_access_key,
            session_token: token,
        }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_named_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        std::fs::write(
            &path,
            "[default]\naws_access_key_id = A\naws_secret_access_key = B\n\n\
             # comment\n[nvr]\naws_access_key_id=C\naws_secret_access_key = D\naws_session_token = E\n",
        )
        .unwrap();
        let nvr = read_profile(&path, "nvr").unwrap().unwrap();
        assert_eq!(
            nvr,
            ProfileCredentials {
                access_key_id: "C".into(),
                secret_access_key: "D".into(),
                session_token: Some("E".into()),
            }
        );
        assert_eq!(
            read_profile(&path, "default")
                .unwrap()
                .unwrap()
                .access_key_id,
            "A"
        );
        assert!(read_profile(&path, "missing").unwrap().is_none());
        assert!(
            read_profile(&dir.path().join("nope"), "default")
                .unwrap()
                .is_none()
        );
    }
}
