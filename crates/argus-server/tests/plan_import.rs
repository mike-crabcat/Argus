//! Plan import and "ask" with a scripted model: the fixture house drawn
//! from an image after one corrected mistake, a change in words, the token
//! rules, the spending cap, and no model configured.

use std::sync::Arc;

use argus_agent::Role as MsgRole;
use argus_agent::mock::MockProvider;
use argus_agent::runner::Agent;
use argus_agent::store::RunStore;
use argus_plan::Versioned;
use argus_server::agent::AgentHandle;
use argus_server::devices::Devices;
use argus_server::http::{self, AppState};
use argus_server::live::Live;
use argus_server::plan::PlanStore;
use serde_json::{Value, json};

const TOKEN: &str = "main-secret";

struct Server {
    base: String,
    phone: String,
    mock: Option<Arc<MockProvider>>,
    _dir: tempfile::TempDir,
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    b
}

async fn server(cap: Option<f64>) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let devices = Arc::new(Devices::open(dir.path()).unwrap());
    let (_, phone) = devices.create("phone", 0).unwrap();
    let plan = Arc::new(PlanStore::open(dir.path()).unwrap());
    let level = plan.get().plan.levels[0].id;
    let mock = cap.map(|_| {
        Arc::new(
            MockProvider::from_json(
                &argus_server::plan_agent::demo::mock_script(level).to_string(),
            )
            .unwrap(),
        )
    });
    let agent = mock.clone().map(|m| {
        let cfg = serde_json::from_value(json!({
            "provider": "mock", "model": "mock", "monthly_cap_usd": cap.unwrap(),
            "price_input_per_mtok": 1000.0, "price_output_per_mtok": 1000.0,
        }))
        .unwrap();
        Arc::new(AgentHandle {
            agent: Ok(Agent {
                provider: m,
                store: Arc::new(RunStore::in_memory().unwrap()),
                cfg,
            }),
            provider: "mock".into(),
            model: "mock".into(),
        })
    });
    let state = AppState {
        live: Arc::new(Live::default()),
        token: Some(Arc::from(TOKEN)),
        web_root: None,
        recordings: None,
        devices: Some(devices),
        public_url: None,
        plan: Some(plan),
        agent,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(http::serve(listener, state));
    Server {
        base,
        phone,
        mock,
        _dir: dir,
    }
}

impl Server {
    async fn post(&self, path: &str, token: &str, body: Value) -> (u16, Value) {
        let r = reqwest::Client::new()
            .post(format!("{}{path}", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
    async fn upload(&self) -> String {
        let r = reqwest::Client::new()
            .post(format!("{}/api/plan/assets", self.base))
            .bearer_auth(TOKEN)
            .body(png(800, 550))
            .send()
            .await
            .unwrap();
        r.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    async fn plan(&self) -> Versioned {
        reqwest::Client::new()
            .get(format!("{}/api/plan", self.base))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn import_corrects_a_mistake_then_ask_changes_it() {
    let s = server(Some(100.0)).await;
    let asset = s.upload().await;
    let level = s.plan().await.plan.levels[0].id.0;

    let (code, d) = s
        .post(
            "/api/plan/import",
            TOKEN,
            json!({"asset": asset, "level": level}),
        )
        .await;
    assert_eq!(code, 200, "{d}");
    assert_eq!(d["run"]["steps"], 2, "one refusal, then accepted");
    assert!(d["summary"].as_str().unwrap().contains("16 x 11 m"));
    assert_eq!(d["warnings"], json!([]));
    let cmds = d["commands"].as_array().unwrap();
    assert_eq!(cmds[0]["op"], "set_background");
    assert_eq!(cmds[0]["background"]["mm_per_px"], 20.0);
    // The image's top-left sits at (0, 550 px * 20 mm).
    assert_eq!(cmds[0]["background"]["offset"]["y"], 11_000.0);
    assert!((d["run"]["cost_usd"].as_f64().unwrap() - 23.2).abs() < 1e-6);

    // What the model saw: the image, the schema, then the refusal.
    let mock = s.mock.as_ref().unwrap();
    {
        let reqs = mock.requests.lock().unwrap();
        let first = &reqs[0];
        assert!(
            first.messages[1]
                .parts
                .iter()
                .any(|p| matches!(p, argus_agent::Part::Image { data, .. } if data.len() > 20))
        );
        let params = &first.tools[0].parameters;
        assert!(
            params["$defs"].is_object()
                && params["properties"]["commands"]["items"]["oneOf"].is_array()
        );
        let feedback = reqs[1]
            .messages
            .iter()
            .find(|m| m.role == MsgRole::Tool)
            .unwrap()
            .joined_text();
        assert!(
            feedback.contains("(add_opening) was refused") && feedback.contains("doesn't fit"),
            "{feedback}"
        );
    }

    // Nothing is saved until the editor accepts.
    assert_eq!(s.plan().await.version, 0);
    let (code, _) = s
        .post(
            "/api/plan/commands",
            TOKEN,
            json!({"version": 0, "commands": cmds}),
        )
        .await;
    assert_eq!(code, 200);
    let v = s.plan().await;
    let house = argus_plan::fixture::demo_house();
    let l = &v.plan.levels[0];
    assert_eq!(l.walls.len(), house.levels[0].walls.len());
    assert_eq!(l.openings.len(), 17);
    assert_eq!(l.rooms.len(), 10);
    assert!(l.background.is_some());

    // "The garage is 6.2 m deep."
    let (code, d) = s
        .post(
            "/api/plan/ask",
            TOKEN,
            json!({"text": "the garage is 6.2 m deep", "level": level}),
        )
        .await;
    assert_eq!(code, 200, "{d}");
    assert!(
        d["commands"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["op"] == "move_node")
    );
    let reqs = mock.requests.lock().unwrap().clone();
    let asked = reqs.last().unwrap().messages[1].joined_text();
    assert!(asked.contains("the garage is 6.2 m deep") && asked.contains("\"walls\""));
    let (code, _) = s
        .post(
            "/api/plan/commands",
            TOKEN,
            json!({"version": 1, "commands": d["commands"]}),
        )
        .await;
    assert_eq!(code, 200);
    let v = s.plan().await;
    let l = &v.plan.levels[0];
    let garage = l.rooms.iter().find(|r| r.name == "Garage").unwrap();
    let area = argus_plan::rooms::area_m2(&argus_plan::rooms::room_polygon(l, garage).unwrap());
    assert!((area - 37.2).abs() < 1e-6, "{area}");
}

#[tokio::test]
async fn only_the_main_token_and_only_with_a_model() {
    let s = server(Some(100.0)).await;
    let asset = s.upload().await;
    let (code, _) = s
        .post(
            "/api/plan/import",
            &s.phone,
            json!({"asset": asset, "level": 1}),
        )
        .await;
    assert_eq!(code, 403);
    let (code, _) = s
        .post("/api/plan/ask", &s.phone, json!({"text": "x", "level": 1}))
        .await;
    assert_eq!(code, 403);
    assert_eq!(s.mock.as_ref().unwrap().calls(), 0);

    let none = server(None).await;
    let asset = none.upload().await;
    let (code, v) = none
        .post(
            "/api/plan/import",
            TOKEN,
            json!({"asset": asset, "level": 1}),
        )
        .await;
    assert_eq!(code, 503);
    assert_eq!(v["setup"], true);
    assert!(v["error"].as_str().unwrap().contains("argus.toml"));
}

#[tokio::test]
async fn the_spending_cap_stops_an_import() {
    // The first answer costs $9.30 at test prices; the cap is $5.
    let s = server(Some(5.0)).await;
    let asset = s.upload().await;
    let (code, v) = s
        .post(
            "/api/plan/import",
            TOKEN,
            json!({"asset": asset, "level": 1}),
        )
        .await;
    assert_eq!(code, 402, "{v}");
    assert!(v["error"].as_str().unwrap().contains("cap"), "{v}");
    assert_eq!(s.mock.as_ref().unwrap().calls(), 1);
    assert_eq!(s.plan().await.version, 0);
}
