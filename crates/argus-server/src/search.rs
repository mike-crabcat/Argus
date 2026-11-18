//! `GET /api/events/search?q=...&from=MS&to=MS&cameras=a,b&limit=N&smart=1`:
//! full-text search over event summaries, labels and camera names. With
//! `smart=1` and a model configured, the model first turns a question
//! ("anyone at the back gate last night?") into search words, a time
//! window and cameras; without a model (or if that fails) the words are
//! searched as typed.

use std::collections::HashMap;

use argus_agent::runner::{RunSpec, Tool, ToolOutcome};
use argus_agent::{Part, ToolSpec};
use argus_store::summaries::{EventRow, SearchQuery};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{Value, json};

use crate::http::AppState;
use crate::stills::StillStore;

fn parse_time(v: &Value) -> Option<i64> {
    let s = v.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// Asks the model to turn `question` into a search. None if it can't.
async fn rewrite(
    st: &AppState,
    question: &str,
    cameras: &[String],
    now_ms: i64,
) -> Option<SearchQuery> {
    let agent = st.agent.as_ref()?.agent.as_ref().ok()?;
    let plan = st.plan.as_ref().map(|p| p.get().plan);
    let places: Vec<String> = cameras
        .iter()
        .map(|c| {
            plan.as_ref()
                .and_then(|p| argus_plan::context::camera_context(p, c))
                .map(|x| x.describe(c))
                .unwrap_or_else(|| format!("Camera \"{c}\"."))
        })
        .collect();
    let tool = Tool {
        spec: ToolSpec {
            name: "search_events".into(),
            description: "Search the event log.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "A few plain words likely to appear in event summaries (person, van, parcel, dog, gate...). Empty to list everything in the window."},
                    "from": {"type": "string", "description": "Window start, RFC 3339 with offset, or omit."},
                    "to": {"type": "string", "description": "Window end, RFC 3339 with offset, or omit."},
                    "cameras": {"type": "array", "items": {"type": "string"}, "description": "Only these cameras (names from the list), or omit for all."}
                },
                "required": ["text"]
            }),
        },
        run: Box::new(|args: &Value| ToolOutcome::Finish(args.clone())),
    };
    let out = agent
        .run(RunSpec {
            kind: "event-search".into(),
            system: "You turn questions about a home's security camera events into a search. \
                     Call search_events once."
                .into(),
            user: vec![Part::Text {
                text: format!(
                    "Now: {} ({}).\nCameras:\n{}\n\nQuestion: {question}",
                    crate::summaries::local_time(now_ms),
                    chrono::DateTime::from_timestamp_millis(now_ms)
                        .map(|t| t.with_timezone(&chrono::Local).to_rfc3339())
                        .unwrap_or_default(),
                    places.join("\n")
                ),
            }],
            tools: vec![tool],
        })
        .await
        .ok()?;
    let v = out.result?;
    Some(SearchQuery {
        text: v["text"].as_str().unwrap_or("").to_owned(),
        from_ms: parse_time(&v["from"]),
        to_ms: parse_time(&v["to"]),
        cameras: v["cameras"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.as_str())
                    .filter(|c| cameras.iter().any(|k| k == c))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        limit: 0,
    })
}

/// An event for the API: its summary and the media keys of its stills.
pub fn event_json(e: &EventRow, stills: &StillStore) -> Value {
    json!({
        "id": e.id,
        "camera": e.camera,
        "start_ms": e.start_ms,
        "end_ms": e.end_ms,
        "source": e.source,
        "label": e.label,
        "summary": e.summary.as_ref().map(|s| s.summary.clone()),
        "objects": e.summary.as_ref().map(|s| s.objects.clone()),
        "notable": e.summary.as_ref().map(|s| s.notable),
        "confidence": e.summary.as_ref().map(|s| s.confidence),
        "stills": stills.list(e.id).into_iter().map(|n| StillStore::key(e.id, n)).collect::<Vec<_>>(),
    })
}

pub async fn search(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(rec) = st.recordings.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let int = |k: &str| q.get(k).and_then(|v| v.parse::<i64>().ok());
    let all_cameras: Vec<String> = {
        let mut v: Vec<String> = st.live.all().map(|s| s.camera.clone()).collect();
        v.dedup();
        v
    };
    let text = q.get("q").cloned().unwrap_or_default();
    let limit = int("limit").unwrap_or(50).clamp(1, 200) as usize;
    let mut query = SearchQuery {
        text: text.clone(),
        from_ms: int("from"),
        to_ms: int("to"),
        cameras: q
            .get("cameras")
            .map(|c| {
                c.split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        limit,
    };
    let mut rewritten = false;
    if q.get("smart").is_some_and(|v| v == "1") && !text.trim().is_empty() {
        let now = crate::recorder::now_us() / 1000;
        if let Some(mut r) = rewrite(&st, &text, &all_cameras, now).await {
            r.limit = limit;
            // The user's own filters win.
            r.from_ms = query.from_ms.or(r.from_ms);
            r.to_ms = query.to_ms.or(r.to_ms);
            if !query.cameras.is_empty() {
                r.cameras = query.cameras.clone();
            }
            query = r;
            rewritten = true;
        }
    }
    let stills = StillStore::new(&rec.root);
    let rows = {
        let index = rec.index.lock().unwrap();
        let _ = index.index_new_events();
        index.search_events(&query)
    };
    match rows {
        Ok(rows) => Json(json!({
            "query": {
                "text": query.text, "from_ms": query.from_ms, "to_ms": query.to_ms,
                "cameras": query.cameras, "rewritten": rewritten,
            },
            "results": rows.iter().map(|e| event_json(e, &stills)).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
