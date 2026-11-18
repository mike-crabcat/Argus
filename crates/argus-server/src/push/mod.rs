//! Notifications: Web Push to subscribed browsers (and iPhones with Argus
//! on the Home Screen), and optionally ntfy.
//!
//! Each subscription belongs to the token that made it: a phone's device
//! id, or the main token. Revoking a phone drops its subscriptions.

pub mod webpush;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use webpush::Vapid;

/// What a notification says and opens.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Notification {
    pub title: String,
    pub body: String,
    /// Path in the viewer to open, e.g. `/?t=1700000000000&camera=door`.
    pub url: String,
    /// Same tag replaces an earlier notification.
    pub tag: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Subscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    /// The phone that subscribed (None: the main token).
    pub device: Option<String>,
    pub created_ms: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NtfyConfig {
    /// The topic URL, e.g. `https://ntfy.sh/my-argus-topic`.
    pub url: String,
    /// A file holding an access token, for protected topics.
    pub token_file: Option<PathBuf>,
}

/// The subscription list, `push_subscriptions.json` in the storage root.
pub struct Subs {
    path: PathBuf,
    list: Mutex<Vec<Subscription>>,
}

impl Subs {
    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join("push_subscriptions.json");
        let list = match std::fs::read(&path) {
            Ok(b) => {
                serde_json::from_slice(&b).with_context(|| format!("reading {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Subs {
            path,
            list: Mutex::new(list),
        })
    }

    fn save(&self, list: &[Subscription]) -> Result<()> {
        let tmp = self.path.with_extension("json.partial");
        std::fs::write(&tmp, serde_json::to_vec_pretty(list)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Adds (or refreshes) a subscription.
    pub fn add(&self, s: Subscription) -> Result<()> {
        anyhow::ensure!(
            s.endpoint.starts_with("https://") || s.endpoint.starts_with("http://127.0.0.1"),
            "push endpoints must be https"
        );
        webpush::unb64(&s.p256dh)?;
        anyhow::ensure!(webpush::unb64(&s.auth)?.len() == 16, "bad auth secret");
        let mut l = self.list.lock().unwrap();
        l.retain(|x| x.endpoint != s.endpoint);
        l.push(s);
        self.save(&l)
    }

    pub fn remove_endpoint(&self, endpoint: &str) -> Result<bool> {
        let mut l = self.list.lock().unwrap();
        let n = l.len();
        l.retain(|x| x.endpoint != endpoint);
        if l.len() != n {
            self.save(&l)?;
        }
        Ok(l.len() != n)
    }

    /// Drops a revoked phone's subscriptions.
    pub fn remove_device(&self, device: &str) -> Result<usize> {
        let mut l = self.list.lock().unwrap();
        let n = l.len();
        l.retain(|x| x.device.as_deref() != Some(device));
        let gone = n - l.len();
        if gone > 0 {
            self.save(&l)?;
        }
        Ok(gone)
    }

    pub fn all(&self) -> Vec<Subscription> {
        self.list.lock().unwrap().clone()
    }
}

/// What a send reached.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Sent {
    pub web_push: usize,
    pub failed: usize,
    pub expired: usize,
    pub ntfy: bool,
}

pub struct Notifier {
    pub vapid: Vapid,
    pub subs: Subs,
    http: reqwest::Client,
    /// VAPID contact (a URL or mailto:).
    subject: String,
    ntfy: Option<(String, Option<String>)>,
    /// For ntfy's click-through: where the viewer is.
    public_url: Option<String>,
}

impl Notifier {
    pub fn new(
        root: &Path,
        ntfy: Option<&NtfyConfig>,
        public_url: Option<String>,
        subject: Option<String>,
    ) -> Result<Self> {
        let ntfy = match ntfy {
            Some(n) => {
                let token = match &n.token_file {
                    Some(p) => Some(
                        std::fs::read_to_string(p)
                            .with_context(|| format!("reading ntfy token_file {}", p.display()))?
                            .trim()
                            .to_owned(),
                    ),
                    None => None,
                };
                Some((n.url.clone(), token))
            }
            None => None,
        };
        Ok(Notifier {
            vapid: Vapid::load_or_create(root)?,
            subs: Subs::open(root)?,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?,
            subject: subject.unwrap_or_else(|| "https://github.com/mike-crabcat/argus".into()),
            ntfy,
            public_url,
        })
    }

    pub fn has_ntfy(&self) -> bool {
        self.ntfy.is_some()
    }

    async fn web_push(&self, s: &Subscription, payload: &[u8]) -> Result<u16> {
        let body = webpush::encrypt(
            payload,
            &webpush::unb64(&s.p256dh)?,
            &webpush::unb64(&s.auth)?,
            None,
            None,
        )?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        let resp = self
            .http
            .post(&s.endpoint)
            .header(
                "Authorization",
                self.vapid.authorization(&s.endpoint, &self.subject, now)?,
            )
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", "86400")
            .header("Urgency", "high")
            .body(body)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        Ok(resp.status().as_u16())
    }

    /// Sends to every subscription (or only `device`'s: Some(None) for
    /// the main token's) and to ntfy. Gone subscriptions are dropped.
    pub async fn send(&self, n: &Notification, only: Option<Option<&str>>) -> Sent {
        let mut sent = Sent::default();
        let payload = serde_json::to_vec(n).unwrap_or_default();
        for s in self.subs.all() {
            if let Some(dev) = only
                && s.device.as_deref() != dev
            {
                continue;
            }
            match self.web_push(&s, &payload).await {
                Ok(c) if (200..300).contains(&c) => sent.web_push += 1,
                Ok(404 | 410) => {
                    // The browser unsubscribed or the subscription expired.
                    let _ = self.subs.remove_endpoint(&s.endpoint);
                    sent.expired += 1;
                }
                Ok(c) => {
                    warn!(status = c, "a push service refused a notification");
                    sent.failed += 1;
                }
                Err(e) => {
                    warn!("push failed: {e:#}");
                    sent.failed += 1;
                }
            }
        }
        if only.is_none() {
            sent.ntfy = self.ntfy(n).await;
        }
        info!(web_push = sent.web_push, ntfy = sent.ntfy, title = %n.title, "notification sent");
        sent
    }

    async fn ntfy(&self, n: &Notification) -> bool {
        let Some((url, token)) = &self.ntfy else {
            return false;
        };
        let mut r = self
            .http
            .post(url)
            .header("Title", n.title.as_str())
            .header("Tags", "rotating_light")
            .body(n.body.clone());
        if let Some(base) = &self.public_url {
            r = r.header("Click", format!("{}{}", base.trim_end_matches('/'), n.url));
        }
        if let Some(t) = token {
            r = r.bearer_auth(t);
        }
        match r.send().await.map_err(reqwest::Error::without_url) {
            Ok(resp) if resp.status().is_success() => true,
            Ok(resp) => {
                warn!(
                    status = resp.status().as_u16(),
                    "ntfy refused a notification"
                );
                false
            }
            Err(e) => {
                warn!("ntfy failed: {e}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn sub(endpoint: &str, device: Option<&str>) -> Subscription {
        // A real P-256 public key (RFC 8291's) and a 16-byte secret.
        Subscription {
            endpoint: endpoint.into(),
            p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4".into(),
            auth: "BTBZMqHH6r4Tts7J_aSIgg".into(),
            device: device.map(String::from),
            created_ms: 0,
        }
    }

    #[test]
    fn subscriptions_per_device() {
        let dir = tempfile::tempdir().unwrap();
        let s = Subs::open(dir.path()).unwrap();
        s.add(sub("https://push.example/a", Some("phone1")))
            .unwrap();
        s.add(sub("https://push.example/b", Some("phone1")))
            .unwrap();
        s.add(sub("https://push.example/c", None)).unwrap();
        s.add(sub("https://push.example/c", None)).unwrap(); // refresh, not a duplicate
        assert!(s.add(sub("http://evil.example/x", None)).is_err());
        assert_eq!(s.all().len(), 3);
        assert_eq!(s.remove_device("phone1").unwrap(), 2);
        let again = Subs::open(dir.path()).unwrap();
        assert_eq!(again.all().len(), 1);
    }

    /// A fake push service and a fake ntfy: what arrives, and 410 handling.
    #[tokio::test]
    async fn sends_encrypted_pushes_and_ntfy() {
        use axum::routing::post;
        type Seen = Arc<Mutex<Vec<(String, axum::http::HeaderMap, Vec<u8>)>>>;
        let seen: Seen = Arc::default();
        let s2 = seen.clone();
        let app = axum::Router::new()
            .route(
                "/push/{id}",
                post(
                    move |axum::extract::Path(id): axum::extract::Path<String>,
                          h: axum::http::HeaderMap,
                          b: axum::body::Bytes| {
                        let s2 = s2.clone();
                        async move {
                            s2.lock().unwrap().push((id.clone(), h, b.to_vec()));
                            if id == "gone" {
                                axum::http::StatusCode::GONE
                            } else {
                                axum::http::StatusCode::CREATED
                            }
                        }
                    },
                ),
            )
            .route(
                "/topic",
                post({
                    let s3 = seen.clone();
                    move |h: axum::http::HeaderMap, b: axum::body::Bytes| {
                        let s3 = s3.clone();
                        async move {
                            s3.lock().unwrap().push(("ntfy".into(), h, b.to_vec()));
                            "ok"
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let dir = tempfile::tempdir().unwrap();
        let tokf = dir.path().join("ntfy-token");
        std::fs::write(&tokf, "tk_secret\n").unwrap();
        let n = Notifier::new(
            dir.path(),
            Some(&NtfyConfig {
                url: format!("{base}/topic"),
                token_file: Some(tokf),
            }),
            Some("https://nvr.example:8444".into()),
            None,
        )
        .unwrap();
        n.subs
            .add(sub(&format!("{base}/push/ok"), Some("phone1")))
            .unwrap();
        n.subs.add(sub(&format!("{base}/push/gone"), None)).unwrap();
        let note = Notification {
            title: "Person at the front door".into(),
            body: "A courier leaves a parcel.".into(),
            url: "/?t=1700000000000&camera=doorbell".into(),
            tag: "rule-1".into(),
        };
        let sent = n.send(&note, None).await;
        assert_eq!(
            sent,
            Sent {
                web_push: 1,
                failed: 0,
                expired: 1,
                ntfy: true
            }
        );
        assert_eq!(n.subs.all().len(), 1, "the gone subscription is dropped");
        let seen = seen.lock().unwrap();
        let (_, h, body) = seen.iter().find(|x| x.0 == "ok").unwrap();
        assert_eq!(h["content-encoding"], "aes128gcm");
        assert!(h["authorization"].to_str().unwrap().starts_with("vapid t="));
        assert_eq!(&body[16..20], &4096u32.to_be_bytes(), "aes128gcm header");
        assert!(
            !String::from_utf8_lossy(body).contains("courier"),
            "encrypted"
        );
        let (_, h, body) = seen.iter().find(|x| x.0 == "ntfy").unwrap();
        assert_eq!(h["title"], "Person at the front door");
        assert_eq!(
            h["click"],
            "https://nvr.example:8444/?t=1700000000000&camera=doorbell"
        );
        assert_eq!(h["authorization"], "Bearer tk_secret");
        assert_eq!(body.as_slice(), b"A courier leaves a parcel.");
    }
}
