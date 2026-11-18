//! The agent, if `[agent]` is configured: the model, its run log
//! (`agent.sqlite` in the storage root) and `/api/agent/status`.

use std::path::Path;
use std::sync::Arc;

use argus_agent::config::AgentConfig;
use argus_agent::runner::Agent;
use argus_agent::store::RunStore;
use axum::extract::State;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;

use crate::http::{AppState, Role};

/// The configured agent, or why it isn't usable.
pub struct AgentHandle {
    pub agent: Result<Agent, String>,
    pub provider: String,
    pub model: String,
}

/// Shown wherever agent features are offered but no model is set up.
pub const SET_UP: &str =
    "Set up a model in argus.toml: add an [agent] section (see argus.example.toml).";

impl AgentHandle {
    /// Builds the agent from config. A bad setup (missing key file...)
    /// doesn't stop Argus; it is reported by the status endpoint.
    pub fn new(cfg: &AgentConfig, root: &Path) -> Self {
        let agent = (|| {
            let provider = cfg.provider().map_err(|e| e.to_string())?;
            let store = RunStore::open(&root.join("agent.sqlite"))
                .map_err(|e| format!("agent log: {e}"))?;
            Ok(Agent {
                provider,
                store: Arc::new(store),
                cfg: cfg.clone(),
            })
        })();
        if let Err(e) = &agent {
            tracing::warn!("agent not available: {e}");
        }
        AgentHandle {
            agent,
            provider: cfg.provider.clone(),
            model: cfg.model.clone(),
        }
    }
}

/// `GET /api/agent/status`: whether a model is set up and, for the main
/// token, this month's spending.
pub async fn status(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
) -> Response {
    let Some(h) = &st.agent else {
        return Json(json!({ "configured": false, "message": SET_UP })).into_response();
    };
    let mut v = json!({
        "configured": h.agent.is_ok(),
        "provider": h.provider,
        "model": h.model,
    });
    match &h.agent {
        Err(e) => v["message"] = format!("The [agent] section has a problem: {e}").into(),
        Ok(a) if role == Role::Admin => {
            let (spent, cap) = a.spend();
            let month = argus_agent::store::month_start_ms(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as i64),
            );
            v["month_spend_usd"] = spent.into();
            v["monthly_cap_usd"] = cap.into();
            v["runs_this_month"] = a.store.runs_since(month).unwrap_or(0).into();
        }
        Ok(_) => {}
    }
    Json(v).into_response()
}
