//! HTTP for notifications, alert rules and the daily brief.
//!
//! * `GET /api/push/key`: the VAPID public key (any token).
//! * `POST /api/push/subscribe`, `DELETE /api/push/subscribe`: this
//!   browser's Web Push subscription, kept per token (phones: per device).
//! * `POST /api/push/test`: a test notification to the caller's own
//!   subscriptions.
//! * `GET /api/alerts/rules`, `GET /api/alerts/log` (any token).
//! * `POST /api/alerts/rules` (`{text}`, or `{text, filter}` to skip the
//!   model), `PUT /api/alerts/rules/{id}`, `DELETE /api/alerts/rules/{id}`,
//!   `POST /api/alerts/rules/{id}/test` (main token): edit, and try a rule
//!   on the last 24 h of events.
//! * `GET /api/brief`, `POST /api/brief/run` (main token).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::json;

use crate::alerts::{Filter, Rule, RuleStore};
use crate::brief::BriefStore;
use crate::http::{AppState, DeviceId, Role};
use crate::push::{Notification, Notifier, Subscription};

pub struct AlertsState {
    pub rules: Arc<RuleStore>,
    pub notifier: Arc<Notifier>,
    pub briefs: Arc<BriefStore>,
    pub tz: chrono_tz::Tz,
    pub brief_time: String,
    pub brief_enabled: bool,
}

type Denied = Box<Response>;

fn state(st: &AppState) -> Result<&Arc<AlertsState>, Denied> {
    st.alerts
        .as_ref()
        .ok_or_else(|| Box::new(StatusCode::NOT_FOUND.into_response()))
}

fn admin(role: Role) -> Result<(), Denied> {
    if role == Role::Admin {
        Ok(())
    } else {
        Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "only the main token can change alerts"})),
            )
                .into_response(),
        ))
    }
}

fn bad(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": msg.into()}))).into_response()
}

fn now_ms() -> i64 {
    argus_store::upload::now_ms()
}

pub async fn push_key(State(st): State<AppState>) -> Response {
    match state(&st) {
        Ok(a) => Json(json!({
            "public_key": a.notifier.vapid.public_key(),
            "ntfy": a.notifier.has_ntfy(),
        }))
        .into_response(),
        Err(r) => *r,
    }
}

#[derive(Deserialize)]
pub struct SubscribeBody {
    endpoint: String,
    keys: Keys,
}
#[derive(Deserialize)]
struct Keys {
    p256dh: String,
    auth: String,
}

pub async fn subscribe(
    State(st): State<AppState>,
    device: Option<axum::Extension<DeviceId>>,
    Json(b): Json<SubscribeBody>,
) -> Response {
    let a = match state(&st) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let sub = Subscription {
        endpoint: b.endpoint,
        p256dh: b.keys.p256dh,
        auth: b.keys.auth,
        device: device.map(|d| d.0.0),
        created_ms: now_ms(),
    };
    match a.notifier.subs.add(sub) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => bad(e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct UnsubscribeBody {
    endpoint: String,
}

pub async fn unsubscribe(State(st): State<AppState>, Json(b): Json<UnsubscribeBody>) -> Response {
    match state(&st) {
        Ok(a) => {
            let _ = a.notifier.subs.remove_endpoint(&b.endpoint);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(r) => *r,
    }
}

pub async fn push_test(
    State(st): State<AppState>,
    device: Option<axum::Extension<DeviceId>>,
) -> Response {
    let a = match state(&st) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let dev = device.map(|d| d.0.0);
    let sent = a
        .notifier
        .send(
            &Notification {
                title: "Argus".into(),
                body: "Notifications work on this device.".into(),
                url: "/".into(),
                tag: "test".into(),
            },
            Some(dev.as_deref()),
        )
        .await;
    Json(json!({"sent": sent})).into_response()
}

pub async fn list_rules(State(st): State<AppState>) -> Response {
    match state(&st) {
        Ok(a) => Json(json!({
            "rules": a.rules.list().iter().map(|r| {
                let mut v = serde_json::to_value(r).unwrap_or_default();
                v["describe"] = r.filter.describe().into();
                v
            }).collect::<Vec<_>>(),
            "timezone": a.tz.name(),
        }))
        .into_response(),
        Err(r) => *r,
    }
}

pub async fn alert_log(State(st): State<AppState>) -> Response {
    match state(&st) {
        Ok(a) => {
            let mut log = a.rules.log();
            log.reverse();
            log.truncate(50);
            Json(json!({"log": log})).into_response()
        }
        Err(r) => *r,
    }
}

#[derive(Deserialize)]
pub struct NewRule {
    text: String,
    #[serde(default)]
    filter: Option<Filter>,
    #[serde(default)]
    cooldown_s: Option<u64>,
}

fn cameras(st: &AppState) -> Vec<String> {
    let mut v: Vec<String> = st.live.all().map(|s| s.camera.clone()).collect();
    v.dedup();
    v
}

pub async fn add_rule(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
    Json(b): Json<NewRule>,
) -> Response {
    let a = match state(&st).and_then(|a| admin(role).map(|_| a)) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let text = b.text.trim().to_owned();
    if text.is_empty() {
        return bad("say what the rule is");
    }
    let (filter, note) = match b.filter {
        Some(f) => (f, None),
        None => {
            let Some(agent) = st.agent.as_ref().and_then(|h| h.agent.as_ref().ok()) else {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"error": crate::agent::SET_UP, "setup": true})),
                )
                    .into_response();
            };
            let cams = cameras(&st);
            let plan = st.plan.as_ref().map(|p| p.get().plan);
            let places = crate::alerts::places(&cams, plan.as_ref());
            match crate::alerts::compile(agent, &text, &cams, &places).await {
                Ok(x) => x,
                Err(e) => {
                    let code = match e {
                        argus_agent::AgentError::CapReached { .. } => StatusCode::PAYMENT_REQUIRED,
                        _ => StatusCode::BAD_GATEWAY,
                    };
                    return (code, Json(json!({"error": e.to_string()}))).into_response();
                }
            }
        }
    };
    let rule = Rule {
        id: crate::alerts::new_id(),
        text,
        filter,
        cooldown_s: b.cooldown_s.unwrap_or(600),
        enabled: true,
        note,
        created_ms: now_ms(),
        last_fired_ms: None,
    };
    match a.rules.put(rule.clone()) {
        Ok(()) => {
            let mut v = serde_json::to_value(&rule).unwrap_or_default();
            v["describe"] = rule.filter.describe().into();
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(e) => bad(e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct RuleEdit {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    filter: Option<Filter>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    cooldown_s: Option<u64>,
}

pub async fn edit_rule(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
    Path(id): Path<String>,
    Json(b): Json<RuleEdit>,
) -> Response {
    let a = match state(&st).and_then(|a| admin(role).map(|_| a)) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let Some(mut r) = a.rules.get(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(t) = b.text {
        r.text = t;
    }
    if let Some(f) = b.filter {
        r.filter = f;
        r.note = None;
    }
    if let Some(e) = b.enabled {
        r.enabled = e;
    }
    if let Some(c) = b.cooldown_s {
        r.cooldown_s = c;
    }
    match a.rules.put(r.clone()) {
        Ok(()) => Json(r).into_response(),
        Err(e) => bad(e.to_string()),
    }
}

pub async fn delete_rule(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
    Path(id): Path<String>,
) -> Response {
    let a = match state(&st).and_then(|a| admin(role).map(|_| a)) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    match a.rules.delete(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// What the rule would have done over the last 24 h.
pub async fn test_rule(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
    Path(id): Path<String>,
) -> Response {
    let a = match state(&st).and_then(|a| admin(role).map(|_| a)) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let Some(r) = a.rules.get(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(rec) = &st.recordings else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let events = crate::alerts::last_day(&rec.index.lock().unwrap(), now_ms());
    let tried = crate::alerts::try_history(&r, &events, a.tz);
    let fires = tried.iter().filter(|t| t.fires).count();
    Json(json!({
        "events": events.len(),
        "matched": tried.len(),
        "would_fire": fires,
        "asks_model": r.filter.check.is_some(),
        "results": tried,
    }))
    .into_response()
}

pub async fn get_brief(State(st): State<AppState>) -> Response {
    let a = match state(&st) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let recent = a.briefs.recent();
    Json(json!({
        "latest": recent.first(),
        "recent": recent.iter().take(7).collect::<Vec<_>>(),
        "enabled": a.brief_enabled,
        "next": a.brief_enabled.then(|| crate::brief::describe_next(now_ms(), &a.brief_time, a.tz)).flatten(),
        "configured": st.agent.as_ref().is_some_and(|h| h.agent.is_ok()),
    }))
    .into_response()
}

pub async fn run_brief(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
) -> Response {
    let a = match state(&st).and_then(|a| admin(role).map(|_| a)) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let (Some(h), Some(rec)) = (&st.agent, &st.recordings) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": crate::agent::SET_UP, "setup": true})),
        )
            .into_response();
    };
    match crate::brief::run_once(h, &rec.index, &a.briefs, &a.notifier, a.tz, now_ms()).await {
        Ok(b) => Json(b).into_response(),
        Err(argus_agent::AgentError::NotConfigured) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": crate::agent::SET_UP, "setup": true})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
