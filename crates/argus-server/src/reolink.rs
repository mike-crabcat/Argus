//! Minimal Reolink HTTP API client: log in, then read the camera's own
//! motion and AI detection state. Read-only; it never changes settings.
//!
//! Protocol: `POST /api.cgi?cmd=Login` with the credentials in the JSON body
//! returns a token (lease ~1 h); later calls pass `?token=...` and may batch
//! several commands in one body. An expired or unknown token answers
//! `rspCode: -6` ("please login first").

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::time::Instant;
use url::Url;

/// What the camera's own analytics see right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Detections {
    pub motion: bool,
    pub person: bool,
    pub vehicle: bool,
    pub animal: bool,
    /// AI types this camera model supports, as event labels.
    pub supported: Vec<&'static str>,
}

impl Detections {
    /// Event labels currently active.
    pub fn active(&self) -> Vec<&'static str> {
        [
            ("motion", self.motion),
            ("person", self.person),
            ("vehicle", self.vehicle),
            ("animal", self.animal),
        ]
        .into_iter()
        .filter_map(|(l, on)| on.then_some(l))
        .collect()
    }
}

/// Reolink AI type keys and the event labels they map to.
const AI_TYPES: [(&str, &str); 3] = [
    ("people", "person"),
    ("vehicle", "vehicle"),
    ("dog_cat", "animal"),
];

const RSP_LOGIN_FIRST: i64 = -6;

pub struct Client {
    http: reqwest::Client,
    api: Url,
    user: String,
    password: String,
    token: Option<(String, Instant)>,
    /// Logins so far (a health signal, and for tests).
    pub logins: u32,
}

/// One command's reply was an error.
#[derive(Debug, thiserror::Error)]
#[error("{cmd}: {detail} (rspCode {code})")]
pub struct ApiError {
    pub cmd: String,
    pub code: i64,
    pub detail: String,
}

impl Client {
    /// `base` is the camera's HTTP address, e.g. `http://192.168.1.20`.
    pub fn new(base: &Url, user: &str, password: &str) -> Result<Self> {
        let mut api = base.clone();
        api.set_path("/api.cgi");
        api.set_query(None);
        let _ = api.set_username("");
        let _ = api.set_password(None);
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(5))
                .build()?,
            api,
            user: user.to_owned(),
            password: password.to_owned(),
            token: None,
            logins: 0,
        })
    }

    /// Reads motion and AI state, logging in (again) as needed.
    pub async fn detections(&mut self) -> Result<Detections> {
        let body = json!([
            {"cmd": "GetMdState", "param": {"channel": 0}},
            {"cmd": "GetAiState", "param": {"channel": 0}},
        ]);
        let reply = match self.call_authed("GetMdState", &body).await {
            Err(e) if is_login_first(&e) => {
                // Token expired or the camera rebooted: log in once more.
                self.token = None;
                self.call_authed("GetMdState", &body).await?
            }
            r => r?,
        };
        parse_detections(&reply)
    }

    /// Ends the session, if any (best effort; tokens also expire).
    pub async fn logout(&mut self) {
        if let Some((t, _)) = self.token.take() {
            let _ = self
                .post("Logout", Some(&t), &json!([{"cmd": "Logout", "param": {}}]))
                .await;
        }
    }

    async fn call_authed(&mut self, cmd: &str, body: &Value) -> Result<Vec<Value>> {
        let token = self.token().await?;
        let reply = self.post(cmd, Some(&token), body).await?;
        for r in &reply {
            check(r)?;
        }
        Ok(reply)
    }

    async fn token(&mut self) -> Result<String> {
        if let Some((t, expires)) = &self.token {
            if Instant::now() < *expires {
                return Ok(t.clone());
            }
        }
        let body = json!([{"cmd": "Login", "param": {"User": {
            "Version": "0", "userName": self.user, "password": self.password,
        }}}]);
        let reply = self.post("Login", None, &body).await?;
        let first = reply.first().ok_or_else(|| anyhow!("empty Login reply"))?;
        check(first)?;
        let tok = &first["value"]["Token"];
        let name = tok["name"]
            .as_str()
            .ok_or_else(|| anyhow!("Login reply has no token"))?
            .to_owned();
        // Renew a minute before the lease runs out.
        let lease = tok["leaseTime"].as_u64().unwrap_or(3600).max(120);
        self.token = Some((
            name.clone(),
            Instant::now() + Duration::from_secs(lease - 60),
        ));
        self.logins += 1;
        Ok(name)
    }

    async fn post(&self, cmd: &str, token: Option<&str>, body: &Value) -> Result<Vec<Value>> {
        let mut url = self.api.clone();
        url.query_pairs_mut().append_pair("cmd", cmd);
        if let Some(t) = token {
            url.query_pairs_mut().append_pair("token", t);
        }
        // The URL carries the token: strip it from any error.
        let resp = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(body)?)
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("Reolink {cmd}"))?
            .error_for_status()
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("Reolink {cmd}"))?;
        let bytes = resp
            .bytes()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("Reolink {cmd}"))?;
        serde_json::from_slice(&bytes).with_context(|| format!("Reolink {cmd}: reply is not JSON"))
    }
}

fn check(reply: &Value) -> Result<()> {
    if reply["code"].as_i64() == Some(0) {
        return Ok(());
    }
    Err(ApiError {
        cmd: reply["cmd"].as_str().unwrap_or("?").to_owned(),
        code: reply["error"]["rspCode"].as_i64().unwrap_or(0),
        detail: reply["error"]["detail"]
            .as_str()
            .unwrap_or("error")
            .to_owned(),
    }
    .into())
}

fn is_login_first(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>()
        .is_some_and(|a| a.code == RSP_LOGIN_FIRST)
}

fn parse_detections(reply: &[Value]) -> Result<Detections> {
    let find = |cmd: &str| {
        reply
            .iter()
            .find(|r| r["cmd"] == cmd)
            .map(|r| &r["value"])
            .ok_or_else(|| anyhow!("no {cmd} in reply"))
    };
    let md = find("GetMdState")?;
    let ai = find("GetAiState")?;
    let mut d = Detections {
        motion: md["state"].as_i64().context("GetMdState: no state")? != 0,
        ..Default::default()
    };
    for (key, label) in AI_TYPES {
        let t = &ai[key];
        if t["support"].as_i64() != Some(1) {
            continue;
        }
        d.supported.push(label);
        let on = t["alarm_state"].as_i64().unwrap_or(0) != 0;
        match label {
            "person" => d.person = on,
            "vehicle" => d.vehicle = on,
            _ => d.animal = on,
        }
    }
    Ok(d)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A fake camera. `state` is (motion, person, vehicle, animal); tokens
    /// in `expired` answer -6.
    #[derive(Default)]
    pub struct FakeCam {
        pub state: Mutex<(bool, bool, bool, bool)>,
        pub logins: AtomicU32,
        pub expire_next: Mutex<bool>,
        pub password: String,
        pub requests: Mutex<Vec<String>>,
    }

    impl FakeCam {
        fn reply(&self, query: &str, body: &str) -> String {
            self.requests
                .lock()
                .unwrap()
                .push(format!("{query} {body}"));
            if query.contains("cmd=Login") {
                if !body.contains(&format!("\"password\":\"{}\"", self.password)) {
                    return r#"[{"cmd":"Login","code":1,"error":{"detail":"login failed","rspCode":-7}}]"#.into();
                }
                let n = self.logins.fetch_add(1, Ordering::SeqCst) + 1;
                return format!(
                    r#"[{{"cmd":"Login","code":0,"value":{{"Token":{{"leaseTime":3600,"name":"tok{n}"}}}}}}]"#
                );
            }
            let current = format!("token=tok{}", self.logins.load(Ordering::SeqCst));
            let mut expire = self.expire_next.lock().unwrap();
            if !query.contains(&current) || *expire {
                *expire = false;
                return r#"[{"cmd":"GetMdState","code":1,"error":{"detail":"please login first","rspCode":-6}}]"#.into();
            }
            let (m, p, v, a) = *self.state.lock().unwrap();
            let b = |x: bool| u8::from(x);
            format!(
                r#"[{{"cmd":"GetMdState","code":0,"value":{{"state":{}}}}},
                   {{"cmd":"GetAiState","code":0,"value":{{"channel":0,
                     "dog_cat":{{"alarm_state":{},"support":1}},
                     "face":{{"alarm_state":0,"support":0}},
                     "people":{{"alarm_state":{},"support":1}},
                     "vehicle":{{"alarm_state":{},"support":1}}}}}}]"#,
                b(m),
                b(a),
                b(p),
                b(v)
            )
        }
    }

    /// Serves `cam` over HTTP on a local port; returns its base URL.
    pub async fn serve(cam: Arc<FakeCam>) -> Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let cam = cam.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    // Read headers, then Content-Length bytes of body.
                    let (head_end, len) = loop {
                        let n = sock.read(&mut tmp).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .map_or(0, |v| v.trim().parse().unwrap());
                            break (i + 4, len);
                        }
                    };
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut tmp).await.unwrap();
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                    let query = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).into_owned();
                    let reply = cam.reply(&query, &body);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        Url::parse(&format!("http://{addr}")).unwrap()
    }

    fn cam() -> Arc<FakeCam> {
        Arc::new(FakeCam {
            password: "hunter2".into(),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn logs_in_once_and_reads_detections() {
        let cam = cam();
        let mut c = Client::new(&serve(cam.clone()).await, "admin", "hunter2").unwrap();
        let d = c.detections().await.unwrap();
        assert_eq!(d.active(), Vec::<&str>::new());
        assert_eq!(d.supported, ["person", "vehicle", "animal"]);
        *cam.state.lock().unwrap() = (true, true, false, true);
        let d = c.detections().await.unwrap();
        assert_eq!(d.active(), ["motion", "person", "animal"]);
        assert_eq!(c.logins, 1);
        // The password only ever travels in the Login body, never the URL.
        let reqs = cam.requests.lock().unwrap();
        assert!(
            reqs.iter()
                .all(|r| !r.split(' ').next().unwrap().contains("hunter2"))
        );
        assert!(reqs[1].starts_with("/api.cgi?cmd=GetMdState&token=tok1"));
    }

    #[tokio::test]
    async fn logs_in_again_when_the_token_expires() {
        let cam = cam();
        let mut c = Client::new(&serve(cam.clone()).await, "admin", "hunter2").unwrap();
        c.detections().await.unwrap();
        *cam.expire_next.lock().unwrap() = true;
        c.detections().await.unwrap();
        assert_eq!(c.logins, 2);
        assert_eq!(cam.logins.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn errors_carry_no_secrets() {
        let cam = cam();
        let base = serve(cam.clone()).await;
        let mut c = Client::new(&base, "admin", "wrong-password").unwrap();
        let e = format!("{:#}", c.detections().await.unwrap_err());
        assert!(e.contains("login failed") && e.contains("-7"), "{e}");
        assert!(!e.contains("wrong-password"), "{e}");
        // Network errors: the URL (with its token) is stripped.
        let mut c = Client::new(&Url::parse("http://127.0.0.1:1").unwrap(), "a", "p").unwrap();
        c.token = Some((
            "secret-token".into(),
            Instant::now() + Duration::from_secs(60),
        ));
        let err = c.detections().await.unwrap_err();
        let e = format!("{err:#} {err:?}");
        assert!(
            !e.contains("secret-token") && !e.contains("127.0.0.1:1/"),
            "{e}"
        );
    }

    #[test]
    fn parses_doorbell_reply_with_unsupported_types() {
        let reply: Vec<Value> = serde_json::from_str(
            r#"[{"cmd":"GetMdState","code":0,"value":{"state":1}},
                {"cmd":"GetAiState","code":0,"value":{"channel":0,
                 "dog_cat":{"alarm_state":0,"support":1},"face":{"alarm_state":0,"support":0},
                 "package":{"alarm_state":0,"support":0},"people":{"alarm_state":1,"support":1},
                 "vehicle":{"alarm_state":1,"support":0}}}]"#,
        )
        .unwrap();
        let d = parse_detections(&reply).unwrap();
        assert_eq!(d.supported, ["person", "animal"]);
        // vehicle is reported active but unsupported: ignored.
        assert_eq!(d.active(), ["motion", "person"]);
    }
}
