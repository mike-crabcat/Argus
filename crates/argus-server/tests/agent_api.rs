//! /api/agent/status with no [agent], with a working one, and a broken one.

use std::sync::Arc;

use argus_agent::mock::MockProvider;
use argus_agent::runner::Agent;
use argus_agent::store::RunStore;
use argus_server::agent::AgentHandle;
use argus_server::devices::Devices;
use argus_server::http::{self, AppState};
use argus_server::live::Live;
use serde_json::{Value, json};

const TOKEN: &str = "main-secret";

async fn server(agent: Option<AgentHandle>) -> (String, String, tempfile::TempDir) {
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
        plan: None,
        agent: agent.map(Arc::new),
        alerts: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(http::serve(listener, state));
    (base, phone, dir)
}

async fn status(base: &str, token: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{base}/api/agent/status"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn not_configured() {
    let (base, _, _d) = server(None).await;
    let v = status(&base, TOKEN).await;
    assert_eq!(v["configured"], false);
    assert!(v["message"].as_str().unwrap().contains("argus.toml"));
}

#[tokio::test]
async fn configured_shows_model_and_spend_to_the_admin_only() {
    let store = Arc::new(RunStore::in_memory().unwrap());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let id = store.start("plan-import", "mock", "x", now).unwrap();
    store.update(id, &json!([]), 1000, 100, 0.42).unwrap();
    let cfg = serde_json::from_value(
        json!({"provider": "mock", "model": "mock-1", "monthly_cap_usd": 5.0}),
    )
    .unwrap();
    let h = AgentHandle {
        agent: Ok(Agent {
            provider: Arc::new(MockProvider::new(vec![])),
            store,
            cfg,
        }),
        provider: "mock".into(),
        model: "mock-1".into(),
    };
    let (base, phone, _d) = server(Some(h)).await;
    let v = status(&base, TOKEN).await;
    assert_eq!(
        (v["configured"].as_bool(), v["model"].as_str()),
        (Some(true), Some("mock-1"))
    );
    assert_eq!(v["month_spend_usd"], 0.42);
    assert_eq!(v["monthly_cap_usd"], 5.0);
    assert_eq!(v["runs_this_month"], 1);
    let p = status(&base, &phone).await;
    assert_eq!(p["configured"], true);
    assert!(p.get("month_spend_usd").is_none(), "{p}");
}

#[tokio::test]
async fn a_broken_setup_is_reported_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = serde_json::from_value(json!({
        "provider": "openai", "model": "m", "api_key_file": dir.path().join("missing").to_str().unwrap()
    }))
    .unwrap();
    let h = AgentHandle::new(&cfg, dir.path());
    let (base, _, _d) = server(Some(h)).await;
    let v = status(&base, TOKEN).await;
    assert_eq!(v["configured"], false);
    assert!(
        v["message"].as_str().unwrap().contains("api_key_file"),
        "{v}"
    );
}
