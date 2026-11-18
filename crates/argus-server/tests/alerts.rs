//! Alerts end to end with a scripted model and a fake push service:
//! compiling a rule, matching and cooldown, the agent check, trying a rule
//! on history, subscriptions per device, and the daily brief.

use std::sync::{Arc, Mutex};

use argus_agent::mock::MockProvider;
use argus_agent::runner::Agent;
use argus_agent::store::RunStore;
use argus_server::agent::AgentHandle;
use argus_server::alerts::{Engine, Filter, Rule, RuleStore};
use argus_server::alerts_api::AlertsState;
use argus_server::brief::BriefStore;
use argus_server::devices::Devices;
use argus_server::http::{self, AppState, Recordings};
use argus_server::live::{Live, LiveStream};
use argus_server::push::{Notifier, Subscription};
use argus_server::stills::StillStore;
use argus_store::Index;
use chrono::TimeZone;
use chrono_tz::Australia::Perth;
use serde_json::{Value, json};

const TOKEN: &str = "main-secret";
const P256DH: &str =
    "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";

fn perth(h: u32, m: u32) -> i64 {
    Perth
        .with_ymd_and_hms(2026, 11, 18, h, m, 0)
        .unwrap()
        .timestamp_millis()
}

/// A push service that counts what it gets.
async fn push_service() -> (String, Arc<Mutex<Vec<Vec<u8>>>>) {
    let got: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let g = got.clone();
    let app = axum::Router::new().route(
        "/push/{id}",
        axum::routing::post(move |b: axum::body::Bytes| {
            let g = g.clone();
            async move {
                g.lock().unwrap().push(b.to_vec());
                axum::http::StatusCode::CREATED
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, got)
}

fn mock(script: Value) -> (Arc<AgentHandle>, Arc<MockProvider>) {
    let m = Arc::new(MockProvider::from_json(&script.to_string()).unwrap());
    let cfg = serde_json::from_value(
        json!({"provider": "mock", "model": "mock", "monthly_cap_usd": 100.0}),
    )
    .unwrap();
    (
        Arc::new(AgentHandle {
            agent: Ok(Agent {
                provider: m.clone(),
                store: Arc::new(RunStore::in_memory().unwrap()),
                cfg,
            }),
            provider: "mock".into(),
            model: "mock".into(),
        }),
        m,
    )
}

struct World {
    dir: tempfile::TempDir,
    index: Arc<Mutex<Index>>,
    alerts: Arc<AlertsState>,
    pushes: Arc<Mutex<Vec<Vec<u8>>>>,
}

async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(Mutex::new(
        Index::open(&dir.path().join("index.sqlite")).unwrap(),
    ));
    let (base, pushes) = push_service().await;
    let notifier = Arc::new(Notifier::new(dir.path(), None, None, None).unwrap());
    notifier
        .subs
        .add(Subscription {
            endpoint: format!("{base}/push/main"),
            p256dh: P256DH.into(),
            auth: AUTH.into(),
            device: None,
            created_ms: 0,
        })
        .unwrap();
    let alerts = Arc::new(AlertsState {
        rules: Arc::new(RuleStore::open(dir.path()).unwrap()),
        notifier,
        briefs: Arc::new(BriefStore::open(dir.path()).unwrap()),
        tz: Perth,
        brief_time: "07:00".into(),
        brief_enabled: true,
    });
    World {
        dir,
        index,
        alerts,
        pushes,
    }
}

fn rule(id: &str, filter: Filter, cooldown_s: u64) -> Rule {
    Rule {
        id: id.into(),
        text: format!("rule {id}"),
        filter,
        cooldown_s,
        enabled: true,
        note: None,
        created_ms: 0,
        last_fired_ms: None,
    }
}

#[tokio::test]
async fn rules_fire_once_per_cooldown_and_ask_the_model_when_needed() {
    let w = world().await;
    let night = Filter {
        cameras: vec!["doorbell".into()],
        labels: vec!["person".into()],
        from: Some("22:00".into()),
        to: Some("06:00".into()),
        ..Default::default()
    };
    w.alerts.rules.put(rule("night", night, 600)).unwrap();
    let delivery = Filter {
        cameras: vec!["driveway".into()],
        check: Some("Is this a delivery driver?".into()),
        ..Default::default()
    };
    w.alerts.rules.put(rule("delivery", delivery, 0)).unwrap();
    let (h, m) = mock(json!([
        {"tool_calls": [{"name": "answer", "arguments": {"yes": true, "reason": "A courier in a hi-vis vest holds a parcel."}}]},
        {"tool_calls": [{"name": "answer", "arguments": {"yes": false}}]}
    ]));
    let mut e = Engine::new(
        w.alerts.rules.clone(),
        w.index.clone(),
        w.alerts.notifier.clone(),
        Some(h),
        StillStore::new(w.dir.path()),
        Perth,
    );
    let add = |cam: &str, label: &str, at: i64| {
        w.index
            .lock()
            .unwrap()
            .add_event(cam, at, at + 5000, "reolink", label)
            .unwrap()
    };

    add("doorbell", "person", perth(23, 0));
    add("doorbell", "person", perth(15, 0)); // daytime: no
    add("doorbell", "vehicle", perth(23, 1)); // a vehicle: no
    assert_eq!(e.tick(perth(23, 2)).await, 1);
    add("doorbell", "person", perth(23, 5)); // within the cooldown
    assert_eq!(e.tick(perth(23, 6)).await, 0);
    add("doorbell", "person", perth(23, 20)); // after it
    assert_eq!(e.tick(perth(23, 21)).await, 1);

    // The delivery rule waits for stills, then asks.
    let d1 = add("driveway", "person", perth(23, 30));
    assert_eq!(e.tick(perth(23, 30) + 1000).await, 0);
    assert_eq!(e.pending.len(), 1);
    assert_eq!(m.calls(), 0, "nothing asked before the stills are in");
    assert_eq!(e.tick(perth(23, 30) + 7000).await, 1);
    add("driveway", "person", perth(23, 40));
    assert_eq!(e.tick(perth(23, 40) + 7000).await, 0, "the model said no");
    assert_eq!(m.calls(), 2);

    let log = w.alerts.rules.log();
    assert_eq!(log.len(), 3);
    assert_eq!(log[2].event_id, d1);
    assert!(log[2].body.contains("courier"), "{}", log[2].body);
    assert_eq!(w.pushes.lock().unwrap().len(), 3, "each alert pushed");
}

#[tokio::test]
async fn without_a_model_checks_fire_on_the_filter_and_say_so() {
    let w = world().await;
    w.alerts
        .rules
        .put(rule(
            "d",
            Filter {
                check: Some("Is it a delivery?".into()),
                ..Default::default()
            },
            0,
        ))
        .unwrap();
    let mut e = Engine::new(
        w.alerts.rules.clone(),
        w.index.clone(),
        w.alerts.notifier.clone(),
        None,
        StillStore::new(w.dir.path()),
        Perth,
    );
    w.index
        .lock()
        .unwrap()
        .add_event("back", perth(9, 0), perth(9, 0) + 5000, "reolink", "person")
        .unwrap();
    e.tick(perth(9, 0)).await;
    assert_eq!(e.tick(perth(9, 1)).await, 1);
    assert!(w.alerts.rules.log()[0].body.contains("not checked"));
}

struct Api {
    base: String,
    phone: (String, String),
    w: World,
    mock: Arc<MockProvider>,
}

async fn api(script: Value) -> Api {
    let w = world().await;
    let devices = Arc::new(Devices::open(w.dir.path()).unwrap());
    let (dev, phone) = devices.create("phone", 0).unwrap();
    let mut live = Live::default();
    for c in ["doorbell", "driveway", "back"] {
        live.add(LiveStream::new(c, "sub", 4));
    }
    let (h, mock) = mock(script);
    let state = AppState {
        live: Arc::new(live),
        token: Some(Arc::from(TOKEN)),
        web_root: None,
        recordings: Some(Recordings {
            index: w.index.clone(),
            root: Arc::new(w.dir.path().to_owned()),
            remote: None,
        }),
        devices: Some(devices),
        public_url: None,
        plan: None,
        agent: Some(h),
        alerts: Some(w.alerts.clone()),
    };
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(http::serve(l, state));
    Api {
        base,
        phone: (dev.id, phone),
        w,
        mock,
    }
}

impl Api {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut r = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.unwrap();
        (
            resp.status().as_u16(),
            resp.json().await.unwrap_or(Value::Null),
        )
    }
}

#[tokio::test]
async fn compile_test_and_manage_rules_over_http() {
    let a = api(json!([
        {"tool_calls": [{"name": "set_filter", "arguments": {
            "cameras": ["doorbell"], "labels": ["person"], "from": "25:00"
        }}]},
        {"tool_calls": [{"name": "set_filter", "arguments": {
            "cameras": ["doorbell"], "labels": ["person"], "from": "22:00", "to": "06:00",
            "note": "Argus can't tell whether anyone is home."
        }}]},
        {"tool_calls": [{"name": "submit_brief", "arguments": {"text": "A quiet night: one person at the front door at 23:00."}}]}
    ]))
    .await;
    use reqwest::Method as M;
    let (phone_id, phone) = a.phone.clone();
    // Phones can't add rules.
    let (code, _) = a
        .call(
            M::POST,
            "/api/alerts/rules",
            &phone,
            Some(json!({"text": "x"})),
        )
        .await;
    assert_eq!(code, 403);
    // The model's first try has a bad time; it's told and fixes it.
    let (code, r) = a
        .call(
            M::POST,
            "/api/alerts/rules",
            TOKEN,
            Some(json!({"text": "a person at the front door after 10pm while we're away"})),
        )
        .await;
    assert_eq!(code, 201, "{r}");
    assert_eq!(r["filter"]["from"], "22:00");
    assert_eq!(r["describe"], "person on doorbell, 22:00–06:00");
    assert!(r["note"].as_str().unwrap().contains("home"));
    let id = r["id"].as_str().unwrap().to_owned();
    let asked = a.mock.requests.lock().unwrap()[0].messages[1].joined_text();
    assert!(
        asked.contains("after 10pm") && asked.contains("doorbell"),
        "{asked}"
    );
    // A rule with a filter given needs no model.
    let (code, r2) = a
        .call(
            M::POST,
            "/api/alerts/rules",
            TOKEN,
            Some(json!({"text": "cars", "filter": {"labels": ["vehicle"]}, "cooldown_s": 60})),
        )
        .await;
    assert_eq!(code, 201, "{r2}");

    // Try it on the last day: 3 matches, the middle one inside the cooldown.
    let now = argus_store::upload::now_ms();
    let local_ts = |back_min: i64| now - back_min * 60_000;
    {
        let i = a.w.index.lock().unwrap();
        // Times chosen in Perth's night so the window matches whatever "now" is:
        // just use the filter on any hour by editing it below.
        for (cam, label, back) in [
            ("doorbell", "person", 120),
            ("doorbell", "person", 115),
            ("doorbell", "person", 30),
            ("driveway", "vehicle", 50),
        ] {
            i.add_event(cam, local_ts(back), local_ts(back) + 5000, "reolink", label)
                .unwrap();
        }
    }
    let (code, _) = a
        .call(
            M::PUT,
            &format!("/api/alerts/rules/{id}"),
            TOKEN,
            Some(json!({"filter": {"cameras": ["doorbell"], "labels": ["person"]}})),
        )
        .await;
    assert_eq!(code, 200);
    let (code, t) = a
        .call(
            M::POST,
            &format!("/api/alerts/rules/{id}/test"),
            TOKEN,
            None,
        )
        .await;
    assert_eq!(code, 200, "{t}");
    assert_eq!(
        (t["matched"].as_u64(), t["would_fire"].as_u64()),
        (Some(3), Some(2)),
        "{t}"
    );
    // Bad edits are refused; disable and delete work.
    let (code, _) = a
        .call(
            M::PUT,
            &format!("/api/alerts/rules/{id}"),
            TOKEN,
            Some(json!({"filter": {"days": [9]}})),
        )
        .await;
    assert_eq!(code, 400);
    let (code, _) = a
        .call(
            M::PUT,
            &format!("/api/alerts/rules/{id}"),
            TOKEN,
            Some(json!({"enabled": false})),
        )
        .await;
    assert_eq!(code, 200);
    let (_, list) = a.call(M::GET, "/api/alerts/rules", &phone, None).await;
    assert_eq!(list["rules"].as_array().unwrap().len(), 2);
    assert_eq!(list["timezone"], "Australia/Perth");
    let id2 = r2["id"].as_str().unwrap();
    let (code, _) = a
        .call(M::DELETE, &format!("/api/alerts/rules/{id2}"), &phone, None)
        .await;
    assert_eq!(code, 403);
    let (code, _) = a
        .call(M::DELETE, &format!("/api/alerts/rules/{id2}"), TOKEN, None)
        .await;
    assert_eq!(code, 204);

    // Push: the key, a phone subscribing, revoking the phone drops it.
    let (_, k) = a.call(M::GET, "/api/push/key", &phone, None).await;
    assert_eq!(k["public_key"].as_str().unwrap().len(), 87);
    let (code, _) = a
        .call(M::POST, "/api/push/subscribe", &phone, Some(json!({"endpoint": "https://push.example/phone", "keys": {"p256dh": P256DH, "auth": AUTH}})))
        .await;
    assert_eq!(code, 204);
    let subs = a.w.alerts.notifier.subs.all();
    assert_eq!(subs.len(), 2);
    assert!(
        subs.iter()
            .any(|s| s.device.as_deref() == Some(phone_id.as_str()))
    );
    let (code, _) = a
        .call(M::DELETE, &format!("/api/devices/{phone_id}"), TOKEN, None)
        .await;
    assert_eq!(code, 204);
    assert_eq!(
        a.w.alerts.notifier.subs.all().len(),
        1,
        "the phone's subscription went with it"
    );

    // The brief: written by the model, stored, pushed.
    let pushes_before = a.w.pushes.lock().unwrap().len();
    let (code, b) = a.call(M::POST, "/api/brief/run", TOKEN, None).await;
    assert_eq!(code, 200, "{b}");
    assert!(b["text"].as_str().unwrap().contains("quiet night"));
    assert_eq!(b["events"], 4);
    let (_, g) = a.call(M::GET, "/api/brief", TOKEN, None).await;
    assert_eq!(g["latest"]["text"], b["text"]);
    assert!(
        g["next"]
            .as_str()
            .unwrap()
            .contains("07:00 Australia/Perth"),
        "{g}"
    );
    assert_eq!(a.w.pushes.lock().unwrap().len(), pushes_before + 1);
    let brief_prompt = a.mock.requests.lock().unwrap().last().unwrap().messages[1].joined_text();
    assert!(
        brief_prompt.contains("doorbell: 3 person") && brief_prompt.contains("driveway: 1 vehicle"),
        "{brief_prompt}"
    );
}
