//! Per-device access tokens, for phones added by QR code.
//!
//! The main `[http] token` stays the admin credential: only it can add or
//! revoke devices. Each device gets its own random token, shown once (in the
//! QR code) and stored only as a SHA-256 hash in `devices.json` in the
//! storage root, so a revoked phone stops working without changing anything
//! else. Last-used times are kept in memory and written out now and then.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A device as stored and as listed (never with its token).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub created_ms: i64,
    pub last_used_ms: Option<i64>,
    /// SHA-256 of the token, hex. Not sent to clients.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    hash: String,
}

impl Device {
    /// The same device without its hash, for API responses.
    pub fn public(&self) -> Device {
        Device {
            hash: String::new(),
            ..self.clone()
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    devices: Vec<Device>,
}

pub struct Devices {
    path: PathBuf,
    state: Mutex<(Vec<Device>, bool)>, // (devices, last-used changed since save)
    /// Bumped on every revoke, so open streams can check they still may.
    revocations: tokio::sync::watch::Sender<u64>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn random_hex(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(hex(&buf))
}

/// Compares two equal-length strings without an early exit.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

impl Devices {
    /// Loads `devices.json` in `root` (or starts empty).
    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join("devices.json");
        let devices = match std::fs::read(&path) {
            Ok(b) => {
                serde_json::from_slice::<File>(&b)
                    .with_context(|| format!("reading {}", path.display()))?
                    .devices
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self {
            path,
            state: Mutex::new((devices, false)),
            revocations: tokio::sync::watch::channel(0).0,
        })
    }

    fn save(&self, devices: &[Device]) -> Result<()> {
        let body = serde_json::to_vec_pretty(&File {
            devices: devices.to_vec(),
        })?;
        let tmp = self.path.with_extension("json.partial");
        std::fs::write(&tmp, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Adds a device; returns it and its token (the only time the token is
    /// available).
    pub fn create(&self, name: &str, now_ms: i64) -> Result<(Device, String)> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "a device needs a name");
        anyhow::ensure!(
            name.chars().count() <= 60,
            "device names are at most 60 characters"
        );
        let token = random_hex(32)?;
        let device = Device {
            id: random_hex(8)?,
            name: name.to_owned(),
            created_ms: now_ms,
            last_used_ms: None,
            hash: hash(&token),
        };
        let mut st = self.state.lock().unwrap();
        st.0.push(device.clone());
        self.save(&st.0)?;
        Ok((device.public(), token))
    }

    pub fn list(&self) -> Vec<Device> {
        self.state
            .lock()
            .unwrap()
            .0
            .iter()
            .map(Device::public)
            .collect()
    }

    /// Removes a device; false if there was none with that id.
    pub fn revoke(&self, id: &str) -> Result<bool> {
        let mut st = self.state.lock().unwrap();
        let before = st.0.len();
        st.0.retain(|d| d.id != id);
        if st.0.len() == before {
            return Ok(false);
        }
        self.save(&st.0)?;
        drop(st);
        self.revocations.send_modify(|n| *n += 1);
        Ok(true)
    }

    fn exists(&self, id: &str) -> bool {
        self.state.lock().unwrap().0.iter().any(|d| d.id == id)
    }

    /// Resolves once device `id` is revoked (at once if it already is), so
    /// a phone's open streams end with it.
    pub async fn revoked(&self, id: &str) {
        let mut rx = self.revocations.subscribe();
        while self.exists(id) {
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// The device this token belongs to, noting when it was used.
    pub fn verify(&self, token: &str, now_ms: i64) -> Option<String> {
        if token.is_empty() {
            return None;
        }
        let h = hash(token);
        let mut st = self.state.lock().unwrap();
        let mut found = None;
        for d in st.0.iter_mut() {
            if same(&d.hash, &h) {
                // Only record minute-level changes, to keep writes rare.
                if d.last_used_ms.is_none_or(|t| now_ms - t > 60_000) {
                    d.last_used_ms = Some(now_ms);
                    found = Some((d.id.clone(), true));
                } else {
                    found = Some((d.id.clone(), false));
                }
            }
        }
        let (id, changed) = found?;
        st.1 |= changed;
        Some(id)
    }

    /// Writes last-used times if they changed since the last write.
    pub fn flush(&self) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if st.1 {
            self.save(&st.0)?;
            st.1 = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_verify_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let d = Devices::open(dir.path()).unwrap();
        let (phone, token) = d.create(" Mike's iPhone ", 1_000).unwrap();
        assert_eq!(phone.name, "Mike's iPhone");
        assert_eq!(token.len(), 64);
        assert_eq!(d.verify(&token, 2_000), Some(phone.id.clone()));
        assert_eq!(d.verify("wrong", 2_000), None);
        assert_eq!(d.verify("", 2_000), None);
        let (_, other) = d.create("iPad", 1_000).unwrap();
        assert_ne!(token, other);
        assert!(d.revoke(&phone.id).unwrap());
        assert!(!d.revoke(&phone.id).unwrap());
        assert_eq!(d.verify(&token, 3_000), None, "revoked token refused");
        assert!(d.verify(&other, 3_000).is_some());
        assert!(d.create("  ", 1).is_err());
    }

    #[tokio::test]
    async fn revoked_wakes_waiters() {
        let dir = tempfile::tempdir().unwrap();
        let d = std::sync::Arc::new(Devices::open(dir.path()).unwrap());
        let (a, _) = d.create("a", 1).unwrap();
        let (b, _) = d.create("b", 1).unwrap();
        let waiter = {
            let (d, id) = (d.clone(), a.id.clone());
            tokio::spawn(async move { d.revoked(&id).await })
        };
        d.revoke(&b.id).unwrap();
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "another device's revoke");
        d.revoke(&a.id).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        d.revoked(&a.id).await; // already gone: returns at once
    }

    #[test]
    fn stores_only_hashes_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let d = Devices::open(dir.path()).unwrap();
        let (phone, token) = d.create("phone", 1_000).unwrap();
        let file = std::fs::read_to_string(dir.path().join("devices.json")).unwrap();
        assert!(!file.contains(&token), "plaintext token on disk");
        assert!(file.contains(&hash(&token)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("devices.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Listing never includes the hash.
        let listed = serde_json::to_string(&d.list()).unwrap();
        assert!(!listed.contains(&hash(&token)), "{listed}");
        // Last use is remembered after a flush and reopen.
        d.verify(&token, 500_000).unwrap();
        d.flush().unwrap();
        let again = Devices::open(dir.path()).unwrap();
        assert_eq!(again.verify(&token, 500_001), Some(phone.id));
        assert_eq!(again.list()[0].last_used_ms, Some(500_000));
    }
}
