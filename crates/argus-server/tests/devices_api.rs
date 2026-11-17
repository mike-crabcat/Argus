//! Device tokens over HTTP: who may add, list and revoke devices, and that a
//! device token views like the main token until it is revoked.

use std::sync::Arc;

use argus_server::devices::Devices;
use argus_server::http::{self, AppState};
use argus_server::live::{Live, LiveStream};
use serde_json::{Value, json};

const TOKEN: &str = "main-secret";

struct Server {
    base: String,
    client: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn server(public_url: Option<&str>) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let mut live = Live::default();
    live.add(LiveStream::new("door", "sub", 16));
    let state = AppState {
        live: Arc::new(live),
        token: Some(Arc::from(TOKEN)),
        web_root: None,
        recordings: None,
        devices: Some(Arc::new(Devices::open(dir.path()).unwrap())),
        public_url: public_url.map(Arc::from),
        plan: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(http::serve(listener, state));
    Server {
        base,
        client: reqwest::Client::new(),
        _dir: dir,
    }
}

impl Server {
    async fn get(&self, path: &str, token: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }

    async fn add(&self, name: &str, token: &str) -> reqwest::Response {
        self.client
            .post(format!("{}/api/devices", self.base))
            .bearer_auth(token)
            .json(&json!({ "name": name }))
            .send()
            .await
            .unwrap()
    }

    async fn revoke(&self, id: &str, token: &str) -> reqwest::Response {
        self.client
            .delete(format!("{}/api/devices/{id}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn device_tokens_view_but_cannot_manage() {
    let s = server(Some("https://nvr.example.ts.net:8444")).await;

    // The main token adds a phone; the token comes back once.
    let r = s.add("Mike's iPhone", TOKEN).await;
    assert_eq!(r.status(), 201);
    let created: Value = r.json().await.unwrap();
    let phone = created["token"].as_str().unwrap().to_owned();
    let id = created["device"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["device"]["name"], "Mike's iPhone");
    assert!(created["device"].get("hash").is_none());

    // The phone can view, by header or query parameter.
    assert_eq!(s.get("/api/cameras", &phone).await.status(), 200);
    let r = s
        .client
        .get(format!("{}/api/cameras?token={phone}", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let info: Value = s.get("/api/info", &phone).await.json().await.unwrap();
    assert_eq!(info["role"], "device");
    let info: Value = s.get("/api/info", TOKEN).await.json().await.unwrap();
    assert_eq!(info["role"], "admin");
    assert_eq!(info["public_url"], "https://nvr.example.ts.net:8444");

    // ...but cannot list, add or revoke devices.
    assert_eq!(s.get("/api/devices", &phone).await.status(), 403);
    assert_eq!(s.add("sneaky", &phone).await.status(), 403);
    assert_eq!(s.revoke(&id, &phone).await.status(), 403);

    // The list never carries tokens or hashes, and shows last use.
    let list: Value = s.get("/api/devices", TOKEN).await.json().await.unwrap();
    let text = list.to_string();
    assert!(!text.contains(&phone));
    assert!(!text.contains("hash"));
    assert_eq!(list["devices"].as_array().unwrap().len(), 1);
    assert!(list["devices"][0]["last_used_ms"].is_i64());

    // Wrong or missing tokens are refused.
    assert_eq!(s.get("/api/cameras", "nope").await.status(), 401);
    let r = s
        .client
        .get(format!("{}/api/cameras", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // Revoked: the phone is shut out; the main token is unaffected.
    assert_eq!(s.revoke(&id, TOKEN).await.status(), 204);
    assert_eq!(s.revoke(&id, TOKEN).await.status(), 404);
    assert_eq!(s.get("/api/cameras", &phone).await.status(), 401);
    assert_eq!(s.get("/api/cameras", TOKEN).await.status(), 200);

    // Names are required.
    assert_eq!(s.add("  ", TOKEN).await.status(), 400);
}

#[tokio::test]
async fn live_websocket_accepts_device_token() {
    let s = server(None).await;
    let created: Value = s.add("phone", TOKEN).await.json().await.unwrap();
    let phone = created["token"].as_str().unwrap();
    let addr = s.base.trim_start_matches("http://");
    let url = format!("ws://{addr}/api/live/door/sub?token={phone}");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let url = format!("ws://{addr}/api/live/door/sub?token=wrong");
    assert!(tokio_tungstenite::connect_async(url).await.is_err());

    // Revoking the phone closes the stream it already has open.
    let list: Value = s.get("/api/devices", TOKEN).await.json().await.unwrap();
    let id = list["devices"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(s.revoke(&id, TOKEN).await.status(), 204);
    let end = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        use futures::StreamExt;
        loop {
            match ws.next().await {
                None | Some(Err(_)) => break,
                Some(Ok(m)) if m.is_close() => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(end.is_ok(), "stream still open after revoke");
    let info: Value = s.get("/api/info", TOKEN).await.json().await.unwrap();
    assert!(info["public_url"].is_null());
}
