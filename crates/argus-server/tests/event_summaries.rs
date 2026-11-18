//! Event summaries with a scripted model: merging within the gap, one per
//! camera per gap, opt-outs, events without stills, the spending cap; and
//! search over them through the API, with and without the model.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use argus_agent::mock::MockProvider;
use argus_agent::runner::Agent;
use argus_agent::store::RunStore;
use argus_server::agent::AgentHandle;
use argus_server::devices::Devices;
use argus_server::http::{self, AppState, Recordings};
use argus_server::live::{Live, LiveStream};
use argus_server::plan::PlanStore;
use argus_server::stills::StillStore;
use argus_server::summaries::Worker;
use argus_store::Index;
use serde_json::{Value, json};

const TOKEN: &str = "main-secret";
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 0xFF, 0xD9];
const MIN: i64 = 60_000;

fn summary(text: &str, objects: &[&str], notable: bool) -> Value {
    json!({
        "tool_calls": [{"name": "submit_summary", "arguments": {
            "summary": text, "objects": objects, "notable": notable, "confidence": 0.9
        }}],
        "usage": {"input_tokens": 1500, "output_tokens": 60},
    })
}

fn handle(script: Value, cap: f64) -> (Arc<AgentHandle>, Arc<MockProvider>) {
    let mock = Arc::new(MockProvider::from_json(&script.to_string()).unwrap());
    let cfg = serde_json::from_value(json!({
        "provider": "mock", "model": "mock", "monthly_cap_usd": cap,
        "price_input_per_mtok": 1000.0, "price_output_per_mtok": 1000.0,
    }))
    .unwrap();
    let h = AgentHandle {
        agent: Ok(Agent {
            provider: mock.clone(),
            store: Arc::new(RunStore::in_memory().unwrap()),
            cfg,
        }),
        provider: "mock".into(),
        model: "mock".into(),
    };
    (Arc::new(h), mock)
}

struct Setup {
    dir: tempfile::TempDir,
    index: Arc<Mutex<Index>>,
    stills: StillStore,
    plan: Arc<PlanStore>,
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(Mutex::new(
        Index::open(&dir.path().join("index.sqlite")).unwrap(),
    ));
    let plan = Arc::new(PlanStore::open(dir.path()).unwrap());
    // The demo house, with its five cameras placed.
    let mut v = plan.get();
    v.plan = argus_plan::fixture::demo_house();
    plan.replace(v).unwrap();
    let stills = StillStore::new(dir.path());
    Setup {
        dir,
        index,
        stills,
        plan,
    }
}

impl Setup {
    fn event(&self, camera: &str, start: i64, label: &str, with_stills: bool) -> i64 {
        let id = self
            .index
            .lock()
            .unwrap()
            .add_event(camera, start, start + 6_000, "reolink", label)
            .unwrap();
        if with_stills {
            for n in 0..3 {
                self.stills.save(id, n, JPEG).unwrap();
            }
        }
        id
    }

    fn worker(&self, h: Arc<AgentHandle>, opted_out: &[&str]) -> Worker {
        Worker {
            agent: h,
            index: self.index.clone(),
            plan: Some(self.plan.clone()),
            stills: self.stills.clone(),
            opted_out: opted_out
                .iter()
                .map(|s| s.to_string())
                .collect::<HashSet<_>>(),
            gap_ms: MIN,
            camera_gap_ms: Default::default(),
            contexts: Default::default(),
            paused_until: 0,
        }
    }
}

/// A real JPEG of `w` x `h`.
fn big_jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
    });
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&img)
        .unwrap();
    out
}

fn jpeg_size(b: &[u8]) -> (u32, u32) {
    use image::ImageDecoder;
    image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(b))
        .unwrap()
        .dimensions()
}

#[tokio::test]
async fn stills_are_shrunk_for_the_model_only() {
    let s = setup();
    let t0 = 1_800_000_000_000i64;
    let id = s.event("doorbell", t0, "person", false);
    let full = big_jpeg(2560, 1440);
    s.stills.save(id, 0, &full).unwrap();
    let small = big_jpeg(640, 360);
    s.stills.save(id, 1, &small).unwrap();

    let (h, mock) = handle(
        json!([summary("Someone at the door.", &["person"], false)]),
        100.0,
    );
    let mut w = s.worker(h, &[]);
    w.stills = s.stills.clone().with_model_edge(1280);
    let t = w.tick(t0 + 2 * MIN).await;
    assert_eq!(t.summarized, 1, "{t:?}");
    let reqs = mock.requests.lock().unwrap().clone();
    let sizes: Vec<(u32, u32)> = reqs[0].messages[1]
        .parts
        .iter()
        .filter_map(|p| match p {
            argus_agent::Part::Image { data, .. } => Some(jpeg_size(data)),
            _ => None,
        })
        .collect();
    assert_eq!(
        sizes,
        [(1280, 720), (640, 360)],
        "big ones shrink, small ones stay"
    );
    // The file on disk keeps its full size, for the viewer.
    assert_eq!(s.stills.read(id, 0).unwrap(), full);
    // 0: as taken.
    let as_taken = s.stills.clone().with_model_edge(0);
    assert_eq!(as_taken.for_model(id, 0).await.unwrap(), full);
}

#[tokio::test]
async fn per_camera_gap_and_context() {
    let s = setup();
    let t0 = 1_800_000_000_000i64;
    // The driveway gets one summary per ten minutes; the doorbell keeps
    // the minute.
    let d1 = s.event("driveway", t0, "vehicle", true);
    let d2 = s.event("driveway", t0 + 5 * MIN, "vehicle", true);
    let f1 = s.event("doorbell", t0, "person", true);
    let f2 = s.event("doorbell", t0 + 5 * MIN, "person", true);
    let (h, mock) = handle(
        json!([
            summary("A visitor.", &["person"], false),
            summary("Another visitor.", &["person"], false),
            summary("Cars pass.", &["car"], false),
        ]),
        100.0,
    );
    let mut w = s.worker(h, &[]);
    w.camera_gap_ms.insert("driveway".into(), 10 * MIN);
    w.contexts.insert(
        "doorbell".into(),
        "the front porch and the path to the street".into(),
    );
    w.tick(t0 + 2 * MIN).await; // the doorbell's first
    w.tick(t0 + 7 * MIN).await; // the doorbell's second; the driveway waits
    {
        let i = s.index.lock().unwrap();
        assert!(i.event(f1).unwrap().unwrap().summary.is_some());
        assert!(i.event(f2).unwrap().unwrap().summary.is_some());
        assert!(i.event(d1).unwrap().unwrap().summary.is_none());
    }
    w.tick(t0 + 11 * MIN).await; // the driveway's window has closed
    {
        let i = s.index.lock().unwrap();
        for id in [d1, d2] {
            let e = i.event(id).unwrap().unwrap();
            assert_eq!(
                e.summary.unwrap().summary,
                "Cars pass.",
                "both in one summary"
            );
        }
    }
    let reqs = mock.requests.lock().unwrap();
    assert_eq!(reqs.len(), 3);
    let door = reqs[0].messages[1].joined_text();
    assert!(
        door.contains("What this camera looks at: the front porch"),
        "{door}"
    );
    let drive = reqs[2].messages[1].joined_text();
    assert!(
        drive.contains("Camera: driveway") && !drive.contains("looks at"),
        "{drive}"
    );
}

#[tokio::test]
async fn summaries_merge_rate_limit_and_opt_out() {
    let s = setup();
    let t0 = 1_800_000_000_000i64;
    // The doorbell: three detections within a minute (one summary), then
    // one more two minutes later. The back camera: one. Laundry opted out.
    let d1 = s.event("doorbell", t0, "person", true);
    let d2 = s.event("doorbell", t0 + 20_000, "person", true);
    let d3 = s.event("doorbell", t0 + 40_000, "person", true);
    let d4 = s.event("doorbell", t0 + 2 * MIN, "person", true);
    let b1 = s.event("back", t0 + 5_000, "animal", true);
    let l1 = s.event("laundry", t0, "person", true);
    let g1 = s.event("garage", t0, "person", false);
    let (h, mock) = handle(
        json!([
            summary("A cat crosses the back lawn by the pool.", &["cat"], false),
            summary(
                "A courier leaves a parcel at the front door.",
                &["person", "parcel"],
                true
            ),
            summary(
                "A person in a cap waits at the front door, then leaves.",
                &["person"],
                true
            ),
        ]),
        100.0,
    );
    let mut w = s.worker(h, &["laundry"]);

    // Too early: the doorbell's window is still open.
    let t = w.tick(t0 + 30_000).await;
    assert_eq!((t.summarized, mock.calls()), (0, 0), "{t:?}");
    assert_eq!(t.skipped, 1, "the opted-out laundry is skipped at once");

    // After the window: back and doorbell (three merged); the garage has
    // no stills.
    let t = w.tick(t0 + MIN + 20_000).await;
    assert_eq!((t.summarized, t.merged, t.skipped), (2, 2, 1), "{t:?}");
    {
        let i = s.index.lock().unwrap();
        for id in [d1, d2, d3] {
            let e = i.event(id).unwrap().unwrap();
            assert!(e.summary.unwrap().summary.contains("parcel"));
        }
        assert!(
            i.event(b1)
                .unwrap()
                .unwrap()
                .summary
                .unwrap()
                .summary
                .contains("cat")
        );
        assert!(i.event(l1).unwrap().unwrap().summary.is_none());
        assert!(i.event(g1).unwrap().unwrap().summary.is_none());
    }
    // What the model got: stills, the time, the label, and the floor plan.
    {
        let reqs = mock.requests.lock().unwrap();
        let door = reqs
            .iter()
            .find(|r| r.messages[1].joined_text().contains("Camera: doorbell"))
            .unwrap();
        let text = door.messages[1].joined_text();
        assert!(
            text.contains("detection: person") && text.contains("3 detections in a row"),
            "{text}"
        );
        assert!(
            text.contains("outside the building") && text.contains("facing south"),
            "{text}"
        );
        let images = door.messages[1]
            .parts
            .iter()
            .filter(|p| matches!(p, argus_agent::Part::Image { .. }))
            .count();
        assert_eq!(
            images, 5,
            "three from the first detection, one from each other"
        );
        assert_eq!(door.tools[0].name, "submit_summary");
    }

    // d4 is ready, but the doorbell had a summary under a minute ago.
    let now = t0 + 3 * MIN + 10_000;
    let mut w2 = s.worker(w.agent.clone(), &["laundry"]);
    // Pretend the last summary was just now: it waits.
    {
        let i = s.index.lock().unwrap();
        let mut sum = i.event(d1).unwrap().unwrap().summary.unwrap();
        sum.summary.push('.');
        i.set_summary(d1, &[d2, d3], &sum, now - 10_000).unwrap();
    }
    let t = w2.tick(now).await;
    assert_eq!(t.summarized, 0, "rate limited");
    let t = w2.tick(now + MIN).await;
    assert_eq!(t.summarized, 1);
    let i = s.index.lock().unwrap();
    assert!(
        i.event(d4)
            .unwrap()
            .unwrap()
            .summary
            .unwrap()
            .summary
            .contains("cap")
    );
    drop(i);
    drop(s.dir);
}

#[tokio::test]
async fn the_cap_pauses_summaries() {
    let s = setup();
    let t0 = 1_800_000_000_000i64;
    s.event("doorbell", t0, "person", true);
    s.event("back", t0, "animal", true);
    // One call costs $1.56 at test prices; the cap is $1.
    let (h, mock) = handle(
        json!([summary("x", &[], false), summary("y", &[], false)]),
        1.0,
    );
    let mut w = s.worker(h, &[]);
    let t = w.tick(t0 + 2 * MIN).await;
    assert!(t.capped && t.summarized == 1, "{t:?}");
    assert_eq!(mock.calls(), 1);
    // Paused: no calls until later.
    let t = w.tick(t0 + 3 * MIN).await;
    assert!(t.capped);
    assert_eq!(mock.calls(), 1);
}

struct Api {
    base: String,
    phone: String,
    _s: Setup,
}

async fn api(agent: Option<Arc<AgentHandle>>) -> (Api, Vec<i64>) {
    let s = setup();
    let t0 = argus_store::upload::now_ms() - 3_600_000;
    let ids = vec![
        s.event("doorbell", t0, "person", true),
        s.event("driveway", t0 + MIN, "vehicle", true),
        s.event("back", t0 + 2 * MIN, "person", true),
    ];
    {
        let i = s.index.lock().unwrap();
        let sum = |t: &str, o: &[&str], n| argus_store::summaries::Summary {
            summary: t.into(),
            objects: o.iter().map(|x| x.to_string()).collect(),
            notable: n,
            confidence: 0.9,
            model: "mock".into(),
        };
        i.set_summary(
            ids[0],
            &[],
            &sum(
                "A courier leaves a parcel at the front door.",
                &["person", "parcel"],
                true,
            ),
            t0,
        )
        .unwrap();
        i.set_summary(
            ids[1],
            &[],
            &sum("A white van reverses into the driveway.", &["van"], false),
            t0,
        )
        .unwrap();
        i.set_summary(
            ids[2],
            &[],
            &sum(
                "Someone opens the back gate and walks to the shed.",
                &["person", "gate"],
                true,
            ),
            t0,
        )
        .unwrap();
    }
    let devices = Arc::new(Devices::open(s.dir.path()).unwrap());
    let (_, phone) = devices.create("phone", 0).unwrap();
    let mut live = Live::default();
    for c in ["doorbell", "driveway", "back"] {
        live.add(LiveStream::new(c, "sub", 4));
    }
    let state = AppState {
        live: Arc::new(live),
        token: Some(Arc::from(TOKEN)),
        web_root: None,
        recordings: Some(Recordings {
            index: s.index.clone(),
            root: Arc::new(s.dir.path().to_owned()),
            remote: None,
        }),
        devices: Some(devices),
        public_url: None,
        plan: Some(s.plan.clone()),
        agent,
        alerts: None,
        snapshots: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(http::serve(listener, state));
    (Api { base, phone, _s: s }, ids)
}

impl Api {
    async fn get(&self, path: &str, token: Option<&str>) -> reqwest::Response {
        let mut r = reqwest::Client::new().get(format!("{}{path}", self.base));
        if let Some(t) = token {
            r = r.bearer_auth(t);
        }
        r.send().await.unwrap()
    }
    async fn search(&self, q: &str) -> Value {
        self.get(&format!("/api/events/search?{q}"), Some(&self.phone))
            .await
            .json()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn search_api_with_stills_and_auth() {
    let (a, ids) = api(None).await;
    assert_eq!(a.get("/api/events/search?q=van", None).await.status(), 401);
    let v = a.search("q=parcels").await;
    let r = v["results"].as_array().unwrap();
    assert_eq!(r[0]["id"], ids[0]);
    assert_eq!(r[0]["notable"], true);
    assert_eq!(r[0]["stills"][0], format!("events/{}/0.jpg", ids[0]));
    // Camera names and labels match too.
    assert_eq!(a.search("q=driveway").await["results"][0]["id"], ids[1]);
    // smart=1 without a model: the words as typed.
    let v = a.search("q=gate&smart=1").await;
    assert_eq!(v["query"]["rewritten"], false);
    assert_eq!(v["results"][0]["id"], ids[2]);
    // Stills come through the media API, with a token (phones too).
    let key = format!("/api/media/events/{}/1.jpg", ids[0]);
    let r = a.get(&key, Some(&a.phone)).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "image/jpeg");
    assert_eq!(r.bytes().await.unwrap().as_ref(), JPEG);
    assert_eq!(a.get(&key, None).await.status(), 401);
    assert_eq!(
        a.get("/api/media/events/../index.sqlite", Some(TOKEN))
            .await
            .status(),
        404
    );
    // The map's event list carries the summaries.
    let v: Value = a
        .get(
            &format!(
                "/api/events?from={}",
                argus_store::upload::now_ms() - 7_200_000
            ),
            Some(&a.phone),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        v["events"][1]["summary"],
        "A white van reverses into the driveway."
    );
}

#[tokio::test]
async fn smart_search_rewrites_the_question() {
    let now = chrono::Local::now();
    let from = (now - chrono::Duration::hours(12)).to_rfc3339();
    let (h, mock) = handle(
        json!([{"tool_calls": [{"name": "search_events", "arguments": {
            "text": "person gate", "from": from, "cameras": ["back", "nonexistent"]
        }}]}]),
        100.0,
    );
    let (a, ids) = api(Some(h)).await;
    let v = a
        .search("q=anyone%20at%20the%20back%20gate%20last%20night&smart=1")
        .await;
    assert_eq!(v["query"]["rewritten"], true, "{v}");
    assert_eq!(v["query"]["text"], "person gate");
    assert_eq!(v["query"]["cameras"], json!(["back"]));
    assert_eq!(v["results"].as_array().unwrap().len(), 1);
    assert_eq!(v["results"][0]["id"], ids[2]);
    let asked = mock.requests.lock().unwrap()[0].messages[1].joined_text();
    assert!(
        asked.contains("anyone at the back gate last night") && asked.contains("Camera \"back\""),
        "{asked}"
    );
    // The model failing (script ran out): plain search still answers.
    let v = a.search("q=van&smart=1").await;
    assert_eq!(v["query"]["rewritten"], false);
    assert_eq!(v["results"][0]["id"], ids[1]);
}
