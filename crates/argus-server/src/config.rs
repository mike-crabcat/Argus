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
    /// The HTTP API and live view. Off without this section.
    pub http: Option<Http>,
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

/// `[http]`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Http {
    /// Address to listen on. The default only accepts local connections;
    /// put `tailscale serve` (which adds HTTPS) or another proxy in front.
    #[serde(default = "default_listen")]
    pub listen: std::net::SocketAddr,
    /// If set, every request must carry it (`Authorization: Bearer ...` or
    /// `?token=...`).
    pub token: Option<String>,
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http")
            .field("listen", &self.listen)
            .field("token", &self.token.as_ref().map(|_| "***"))
            .finish()
    }
}

fn default_listen() -> std::net::SocketAddr {
    ([127, 0, 0, 1], 8080).into()
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
    /// Where this camera's events come from. Reolink cameras (recognised by
    /// their stream URLs) default to their own detections.
    pub events: Option<EventsConfig>,
}

/// `[camera.events]`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsConfig {
    /// "reolink", or "none" to turn camera events off.
    #[serde(default = "default_events_source")]
    pub source: String,
    /// Camera API address, e.g. `http://192.168.1.20`. Default: the host of
    /// one of the camera's stream URLs.
    pub url: Option<String>,
    /// Default: the credentials in the stream URL.
    pub user: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    /// An event ends this long after its detection was last seen.
    #[serde(default = "default_hold_ms")]
    pub hold_ms: u64,
    /// Which detections become events: any of motion, person, vehicle,
    /// animal. Default: person, vehicle, animal.
    pub labels: Option<Vec<String>>,
}

impl std::fmt::Debug for EventsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventsConfig")
            .field("source", &self.source)
            .field("url", &self.url)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("poll_ms", &self.poll_ms)
            .field("hold_ms", &self.hold_ms)
            .field("labels", &self.labels)
            .finish()
    }
}

fn default_events_source() -> String {
    "reolink".into()
}
fn default_poll_ms() -> u64 {
    1000
}
fn default_hold_ms() -> u64 {
    5000
}

pub const EVENT_LABELS: [&str; 4] = ["motion", "person", "vehicle", "animal"];
const DEFAULT_EVENT_LABELS: [&str; 3] = ["person", "vehicle", "animal"];

/// A camera's resolved Reolink event settings.
#[derive(Clone, PartialEq, Eq)]
pub struct ReolinkEvents {
    pub base: url::Url,
    pub user: String,
    pub password: String,
    pub poll: std::time::Duration,
    pub hold: std::time::Duration,
    pub labels: Vec<String>,
}

impl std::fmt::Debug for ReolinkEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReolinkEvents")
            .field("base", &self.base.as_str())
            .field("user", &self.user)
            .field("password", &"***")
            .field("poll", &self.poll)
            .field("hold", &self.hold)
            .field("labels", &self.labels)
            .finish()
    }
}

/// Reolink stream URLs: HTTP-FLV `/flv?...app=bcs...`, or RTSP
/// `/h264Preview_01_main` / `/h265Preview_01_main` and friends.
fn looks_like_reolink(u: &url::Url) -> bool {
    match u.scheme() {
        "http" | "https" => {
            u.path() == "/flv" && u.query_pairs().any(|(k, v)| k == "app" && v == "bcs")
        }
        "rtsp" | "rtsps" => u.path().contains("Preview_"),
        _ => false,
    }
}

/// Credentials in a stream URL: `user`/`password` query parameters (FLV)
/// or userinfo (RTSP).
fn url_credentials(u: &url::Url) -> Option<(String, String)> {
    let q = |name: &str| {
        u.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    if let (Some(user), Some(pass)) = (q("user"), q("password")) {
        return Some((user, pass));
    }
    if !u.username().is_empty() {
        return Some((
            percent_decode(u.username()),
            percent_decode(u.password().unwrap_or("")),
        ));
    }
    None
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Camera {
    /// Reolink event settings, if this camera's events come from Reolink.
    pub fn reolink_events(&self) -> Result<Option<ReolinkEvents>> {
        let ev = self.events.as_ref();
        match ev.map(|e| e.source.as_str()) {
            Some("none") => return Ok(None),
            Some("reolink") | None => {}
            Some(other) => bail!(
                "camera {:?}: events source {other:?} must be \"reolink\" or \"none\"",
                self.name
            ),
        }
        // Prefer the main stream's URL, then any other.
        let urls: Vec<url::Url> = self
            .streams
            .get("main")
            .into_iter()
            .chain(
                self.streams
                    .iter()
                    .filter(|(k, _)| *k != "main")
                    .map(|(_, v)| v),
            )
            .filter_map(|s| url::Url::parse(s).ok())
            .collect();
        let reolink_url = urls.iter().find(|u| looks_like_reolink(u));
        if ev.is_none() && reolink_url.is_none() {
            return Ok(None); // not a Reolink camera and nothing configured
        }
        let source_url = reolink_url.or(urls.first());
        let base = match ev.and_then(|e| e.url.as_deref()) {
            Some(u) => {
                url::Url::parse(u).with_context(|| format!("camera {:?}: events url", self.name))?
            }
            None => {
                let u = source_url
                    .with_context(|| format!("camera {:?}: events need a url", self.name))?;
                let mut base = url::Url::parse("http://localhost/").unwrap();
                base.set_host(u.host_str())
                    .with_context(|| format!("camera {:?}: no host", self.name))?;
                if u.scheme() == "http" {
                    let _ = base.set_port(u.port());
                }
                base
            }
        };
        let from_url = source_url.and_then(url_credentials);
        let user = ev
            .and_then(|e| e.user.clone())
            .or_else(|| from_url.as_ref().map(|c| c.0.clone()));
        let password = ev
            .and_then(|e| e.password.clone())
            .or_else(|| from_url.as_ref().map(|c| c.1.clone()));
        let (Some(user), Some(password)) = (user, password) else {
            bail!(
                "camera {:?}: Reolink events need user and password (in [camera.events] or the stream URL)",
                self.name
            );
        };
        let labels = match ev.and_then(|e| e.labels.clone()) {
            Some(l) => l,
            None => DEFAULT_EVENT_LABELS.map(String::from).to_vec(),
        };
        for l in &labels {
            if !EVENT_LABELS.contains(&l.as_str()) {
                bail!(
                    "camera {:?}: event label {l:?} must be one of {EVENT_LABELS:?}",
                    self.name
                );
            }
        }
        Ok(Some(ReolinkEvents {
            base,
            user,
            password,
            poll: std::time::Duration::from_millis(ev.map_or(1000, |e| e.poll_ms).max(200)),
            hold: std::time::Duration::from_millis(ev.map_or(5000, |e| e.hold_ms)),
            labels,
        }))
    }

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
            cam.reolink_events()?;
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

    fn camera(toml_body: &str) -> Camera {
        let cfg = Config::parse(&format!(
            "[storage]\nroot='/tmp'\n[[camera]]\nname='c'\n{toml_body}"
        ))
        .unwrap();
        cfg.cameras.into_iter().next().unwrap()
    }

    #[test]
    fn reolink_events_default_from_flv_url() {
        let c = camera(
            "streams.main = 'http://192.168.1.20/flv?port=1935&app=bcs&stream=channel0_main.bcs&user=ha&password=p%26ss'\n",
        );
        let e = c.reolink_events().unwrap().unwrap();
        assert_eq!(e.base.as_str(), "http://192.168.1.20/");
        assert_eq!((e.user.as_str(), e.password.as_str()), ("ha", "p&ss"));
        assert_eq!(e.labels, ["person", "vehicle", "animal"]);
        assert_eq!(e.poll, std::time::Duration::from_secs(1));
        assert!(!format!("{e:?}").contains("p&ss"));
    }

    #[test]
    fn reolink_events_from_rtsp_and_overrides() {
        let c = camera(
            "streams.main = 'rtsp://admin:s%40cret@10.0.0.5:554/h265Preview_01_main'\n\
             [camera.events]\nlabels = ['motion', 'person']\npoll_ms = 500\n",
        );
        let e = c.reolink_events().unwrap().unwrap();
        assert_eq!(e.base.as_str(), "http://10.0.0.5/");
        assert_eq!((e.user.as_str(), e.password.as_str()), ("admin", "s@cret"));
        assert_eq!(e.labels, ["motion", "person"]);
        let c = camera(
            "streams.main = 'rtsp://cam/stream1'\n\
             [camera.events]\nurl = 'http://10.0.0.9:8080'\nuser = 'u'\npassword = 'p'\n",
        );
        let e = c.reolink_events().unwrap().unwrap();
        assert_eq!(e.base.as_str(), "http://10.0.0.9:8080/");
        assert!(!format!("{:?}", c.events).contains("\"p\""));
    }

    #[test]
    fn reolink_events_off() {
        // Not a Reolink URL and nothing configured.
        assert!(
            camera("streams.main = 'rtsp://u:p@cam/stream1'\n")
                .reolink_events()
                .unwrap()
                .is_none()
        );
        // Explicitly off.
        let c = camera(
            "streams.main = 'http://h/flv?app=bcs&user=u&password=p'\n[camera.events]\nsource = 'none'\n",
        );
        assert!(c.reolink_events().unwrap().is_none());
        // Bad settings fail at load.
        let base = "[storage]\nroot='/tmp'\n[[camera]]\nname='c'\nstreams.main = 'http://h/flv?app=bcs&user=u&password=p'\n[camera.events]\n";
        assert!(Config::parse(&format!("{base}labels = ['cat']\n")).is_err());
        assert!(Config::parse(&format!("{base}source = 'onvif'\n")).is_err());
        let no_creds =
            "[storage]\nroot='/tmp'\n[[camera]]\nname='c'\nstreams.main = 'http://h/flv?app=bcs'\n";
        assert!(Config::parse(no_creds).is_err());
    }

    #[test]
    fn http_section() {
        let cfg = Config::parse(
            "[storage]\nroot='/tmp'\n[http]\nlisten = '0.0.0.0:9090'\ntoken = 'sekrit'\n",
        )
        .unwrap();
        let h = cfg.http.as_ref().unwrap();
        assert_eq!(h.listen.port(), 9090);
        assert!(!format!("{cfg:?}").contains("sekrit"));
        let d = Config::parse("[storage]\nroot='/tmp'\n[http]\n").unwrap();
        assert_eq!(d.http.unwrap().listen.to_string(), "127.0.0.1:8080");
        assert!(
            Config::parse("[storage]\nroot='/tmp'\n")
                .unwrap()
                .http
                .is_none()
        );
    }

    #[test]
    fn rejects_unsafe_names() {
        let bad = "[storage]\nroot='/tmp'\n[[camera]]\nname='../x'\nstreams={main='rtsp://a'}\n";
        assert!(Config::parse(bad).is_err());
    }
}
