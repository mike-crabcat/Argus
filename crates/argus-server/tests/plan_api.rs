//! The floor plan over HTTP: anyone signed in reads it, only the main token
//! changes it, and stale versions are refused.

use std::sync::Arc;

use argus_plan::{Command, Versioned, pt};
use argus_server::devices::Devices;
use argus_server::http::{self, AppState};
use argus_server::live::Live;
use argus_server::plan::PlanStore;
use serde_json::{Value, json};

const TOKEN: &str = "main-secret";

struct Server {
    base: String,
    client: reqwest::Client,
    phone: String,
    _dir: tempfile::TempDir,
}

async fn server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let devices = Arc::new(Devices::open(dir.path()).unwrap());
    let (_, phone) = devices.create("phone", 0).unwrap();
    let state = AppState {
        live: Arc::new(Live::default()),
        token: Some(Arc::from(TOKEN)),
        web_root: None,
        recordings: None,
        devices: Some(devices),
        public_url: None,
        plan: Some(Arc::new(PlanStore::open(dir.path()).unwrap())),
        agent: None,
        alerts: None,
        snapshots: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(http::serve(listener, state));
    Server {
        base,
        client: reqwest::Client::new(),
        phone,
        _dir: dir,
    }
}

impl Server {
    fn req(&self, m: reqwest::Method, path: &str, token: &str) -> reqwest::RequestBuilder {
        self.client
            .request(m, format!("{}{path}", self.base))
            .bearer_auth(token)
    }
    async fn plan(&self, token: &str) -> Versioned {
        self.req(reqwest::Method::GET, "/api/plan", token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn commands(&self, token: &str, body: Value) -> reqwest::Response {
        self.req(reqwest::Method::POST, "/api/plan/commands", token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    b
}

#[tokio::test]
async fn read_with_any_token_change_with_the_main_one() {
    let s = server().await;
    let v = s.plan(&s.phone).await;
    assert_eq!(v.version, 0);
    let level = v.plan.levels[0].id;
    let wall = Command::AddWall {
        level,
        id: None,
        a: argus_plan::End::At(pt(0.0, 0.0)),
        b: argus_plan::End::At(pt(4000.0, 0.0)),
        thickness: 230.0,
    };

    // A phone can't change it.
    let r = s
        .commands(&s.phone, json!({ "version": 0, "commands": [wall] }))
        .await;
    assert_eq!(r.status(), 403);
    let r = s
        .req(reqwest::Method::PUT, "/api/plan", &s.phone)
        .json(&v)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = s
        .req(reqwest::Method::POST, "/api/plan/assets", &s.phone)
        .body(png(10, 10))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // The main token can.
    let r = s
        .commands(TOKEN, json!({ "version": 0, "commands": [wall] }))
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["version"], 1);
    let v1 = s.plan(&s.phone).await;
    assert_eq!(v1.version, 1);
    assert_eq!(v1.plan.levels[0].walls.len(), 1);

    // Stale: 409 with the current version, nothing applied.
    let r = s
        .commands(TOKEN, json!({ "version": 0, "commands": [wall] }))
        .await;
    assert_eq!(r.status(), 409);
    assert_eq!(r.json::<Value>().await.unwrap()["version"], 1);
    // A refused command: 400 with the reason.
    let r = s.commands(TOKEN, json!({ "commands": [wall] })).await;
    assert_eq!(r.status(), 400);
    let e = r.json::<Value>().await.unwrap();
    assert!(
        e["error"].as_str().unwrap().contains("already a wall"),
        "{e}"
    );
    // Not a command at all: rejected before reaching the plan.
    let r = s
        .commands(TOKEN, json!({ "commands": [{ "op": "explode" }] }))
        .await;
    assert!(r.status().is_client_error());
    assert_eq!(s.plan(TOKEN).await.version, 1);

    // PUT replaces the whole plan, with the same check.
    let mut whole = s.plan(TOKEN).await;
    whole.plan = argus_plan::fixture::demo_house();
    let put = |body: &Versioned| {
        s.req(reqwest::Method::PUT, "/api/plan", TOKEN)
            .json(body)
            .send()
    };
    assert_eq!(put(&whole).await.unwrap().status(), 200);
    assert_eq!(put(&whole).await.unwrap().status(), 409);
    let got = s.plan(&s.phone).await;
    assert_eq!(got.version, 2);
    assert_eq!(got.plan.levels[0].cameras.len(), 5);
}

#[tokio::test]
async fn background_images() {
    let s = server().await;
    let r = s
        .req(reqwest::Method::POST, "/api/plan/assets", TOKEN)
        .header("Content-Type", "image/png")
        .body(png(1200, 800))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let a: Value = r.json().await.unwrap();
    assert_eq!(
        (a["width"].as_u64(), a["height"].as_u64()),
        (Some(1200), Some(800))
    );
    let id = a["id"].as_str().unwrap();
    // Phones can load it.
    let r = s
        .req(
            reqwest::Method::GET,
            &format!("/api/plan/assets/{id}"),
            &s.phone,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "image/png");
    assert_eq!(r.bytes().await.unwrap().to_vec(), png(1200, 800));
    // Not an image, or too big.
    let r = s
        .req(reqwest::Method::POST, "/api/plan/assets", TOKEN)
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let mut big = png(10, 10);
    big.resize(21 * 1024 * 1024, 0);
    let r = s
        .req(reqwest::Method::POST, "/api/plan/assets", TOKEN)
        .body(big)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error(), "{}", r.status());
    let r = s
        .req(reqwest::Method::GET, "/api/plan/assets/nope.png", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    // Without a token: nothing.
    let r = s
        .client
        .get(format!("{}/api/plan", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn calibration_preview_and_saving() {
    use argus_plan::calib::{CalPair, pinhole, project};
    let s = server().await;
    // A preview: the fit, nothing saved.
    let mut plan = argus_plan::fixture::demo_house();
    let cam = plan.levels[0].cameras[0].clone();
    let h = pinhole(&cam, 4.0 / 3.0).unwrap();
    let pairs: Vec<CalPair> = [
        [0.2, 0.6],
        [0.8, 0.6],
        [0.1, 0.95],
        [0.9, 0.95],
        [0.5, 0.75],
    ]
    .iter()
    .map(|&i| CalPair {
        image: i,
        plan: project(&h, i).unwrap(),
    })
    .collect();
    let r = s
        .req(reqwest::Method::POST, "/api/plan/calibrate", &s.phone)
        .json(&json!({ "pairs": pairs }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let fit: Value = r.json().await.unwrap();
    assert!(fit["rms_mm"].as_f64().unwrap() < 1.0);
    assert_eq!(fit["errors"].as_array().unwrap().len(), 5);
    let r = s
        .req(reqwest::Method::POST, "/api/plan/calibrate", TOKEN)
        .json(&json!({ "pairs": &pairs[..3] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("at least 4"));
    // Saving is a plan command: the main token only.
    let v = s.plan(TOKEN).await;
    plan.next_id = plan.next_id.max(v.plan.next_id);
    let r = s
        .req(reqwest::Method::PUT, "/api/plan", TOKEN)
        .json(&json!({"version": v.version, "plan": plan}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v = s.plan(TOKEN).await;
    let cmd = Command::SetCalibration {
        level: v.plan.levels[0].id,
        camera: cam.id,
        pairs: Some(pairs),
    };
    assert_eq!(
        s.commands(&s.phone, json!({"commands": [cmd]}))
            .await
            .status(),
        403
    );
    assert_eq!(
        s.commands(TOKEN, json!({"commands": [cmd]})).await.status(),
        200
    );
    let v = s.plan(&s.phone).await;
    assert!(v.plan.levels[0].cameras[0].calibration.is_some());
    // No snapshot source here: 404, not a hang.
    let r = s
        .req(
            reqwest::Method::GET,
            "/api/cameras/doorbell/snapshot",
            TOKEN,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}
