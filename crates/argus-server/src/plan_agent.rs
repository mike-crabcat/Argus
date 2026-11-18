//! The agent drafting plan changes: from an uploaded sketch or photo
//! (`POST /api/plan/import`) or from a sentence (`POST /api/plan/ask`).
//!
//! The model gets the image or the current level, the plan conventions and
//! one tool whose arguments are a command batch (the plan-commands schema).
//! Argus applies the batch to a draft copy of the plan; if a command is
//! refused, the error goes back to the model to fix, within the step
//! limit. Nothing is saved here: the editor shows the batch as a draft and
//! the user accepts it (through `/api/plan/commands`) or discards it.

use argus_agent::runner::{RunSpec, Tool, ToolOutcome};
use argus_agent::{AgentError, Part, ToolSpec};
use argus_plan::{Background, Command, Id, Plan, pt};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::http::{AppState, Role};

const CONVENTIONS: &str = "\
Plan conventions: lengths in millimetres; +x is east (right), +y is north (up); \
angles in degrees counter-clockwise from +x. Walls run along their centreline \
from a to b; give ends as {\"at\": {\"x\": .., \"y\": ..}} and walls that share \
an end point join there. A wall end landing on another wall makes a T-junction \
(that wall is split; its doors and windows stay where they are). Openings \
(kind \"door\" or \"window\") sit in a wall: offset is the distance from the \
wall's start (a) to the opening's centre; doors may have swing {hinge: start|end, \
side: left|right}. Typical thicknesses: outside walls 230, inside walls 90. \
Rooms are a name and a point inside them. Give every new wall an explicit \
\"id\" so openings can refer to it.";

#[derive(Deserialize)]
pub struct ImportRequest {
    /// An uploaded image (`POST /api/plan/assets`).
    pub asset: String,
    pub level: Id,
    /// Anything the user wants to add ("the top is north").
    #[serde(default)]
    pub notes: String,
}

#[derive(Deserialize)]
pub struct AskRequest {
    pub text: String,
    pub level: Id,
}

/// The tool's parameters: a summary and a command batch, with the plan
/// command schema inlined.
fn submit_schema(with_scale: bool) -> Value {
    let mut cmd = argus_plan::command_schema();
    let defs = cmd
        .as_object_mut()
        .and_then(|o| o.remove("$defs"))
        .unwrap_or(json!({}));
    if let Some(o) = cmd.as_object_mut() {
        o.remove("$schema");
        o.remove("title");
    }
    let mut props = json!({
        "summary": {"type": "string", "description": "One or two sentences on what you drew or changed."},
        "commands": {"type": "array", "items": cmd},
    });
    let mut required = vec!["summary", "commands"];
    if with_scale {
        props["scale_mm_per_px"] = json!({
            "type": "number",
            "description": "Millimetres per image pixel, estimated from dimensions written on the image (or typical sizes such as an 820 mm door)."
        });
        required.push("scale_mm_per_px");
    }
    json!({"type": "object", "properties": props, "required": required, "$defs": defs})
}

/// Applies `cmds` to a copy of `plan`; the reason for the first refusal.
pub fn check_batch(plan: &Plan, cmds: &[Command]) -> Result<Plan, String> {
    // As one batch, as the editor and /api/plan/commands will apply it
    // (the batch's own ids are reserved throughout).
    let mut draft = plan.clone();
    match draft.apply_all_checked(cmds) {
        Ok(_) => Ok(draft),
        Err((i, e)) => {
            let op = serde_json::to_value(&cmds[i])
                .ok()
                .and_then(|v| v["op"].as_str().map(String::from))
                .unwrap_or_default();
            Err(format!("command {i} ({op}) was refused: {e}"))
        }
    }
}

/// Parses the tool's commands, naming any that don't fit the schema.
fn parse_commands(args: &Value) -> Result<Vec<Command>, String> {
    let Some(list) = args["commands"].as_array() else {
        return Err("\"commands\" must be an array of plan commands.".into());
    };
    let mut out = Vec::with_capacity(list.len());
    for (i, c) in list.iter().enumerate() {
        match serde_json::from_value::<Command>(c.clone()) {
            Ok(c) => out.push(c),
            Err(e) => return Err(format!("command {i} doesn't match the schema: {e}")),
        }
    }
    Ok(out)
}

fn level_summary(plan: &Plan, level: Id) -> String {
    let l = plan.level(level).cloned();
    format!(
        "The level as it is now (JSON), next free id {}: {}",
        plan.next_id,
        serde_json::to_string(&l).unwrap_or_default()
    )
}

fn admin(role: Role) -> Result<(), Box<Response>> {
    if role == Role::Admin {
        Ok(())
    } else {
        Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "only the main token can use the agent"})),
            )
                .into_response(),
        ))
    }
}

fn agent_of(st: &AppState) -> Result<&argus_agent::runner::Agent, Box<Response>> {
    let unavailable = |msg: String| {
        Box::new(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": msg, "setup": true})),
            )
                .into_response(),
        )
    };
    match &st.agent {
        None => Err(unavailable(crate::agent::SET_UP.into())),
        Some(h) => h
            .agent
            .as_ref()
            .map_err(|e| unavailable(format!("The [agent] section has a problem: {e}"))),
    }
}

fn agent_error(e: AgentError) -> Response {
    let code = match e {
        AgentError::CapReached { .. } => StatusCode::PAYMENT_REQUIRED,
        AgentError::NotConfigured => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    };
    (code, Json(json!({"error": e.to_string()}))).into_response()
}

/// Runs the model with one submit tool; returns the accepted batch.
async fn draft(
    st: &AppState,
    kind: &str,
    system: String,
    user: Vec<Part>,
    base: Plan,
    level: Id,
    background: Option<(String, u32)>,
) -> Response {
    let agent = match agent_of(st) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let with_scale = background.is_some();
    let tool = Tool {
        spec: ToolSpec {
            name: "submit_commands".into(),
            description: "Submit the plan commands. They are checked by applying them to a draft copy of the plan; if one is refused you get the reason and should submit the whole corrected batch again.".into(),
            parameters: submit_schema(with_scale),
        },
        run: Box::new(move |args: &Value| {
            let mut cmds = match parse_commands(args) {
                Ok(c) => c,
                Err(e) => return ToolOutcome::Reply(format!("{e} Fix it and submit the whole batch again.")),
            };
            if let Some((asset, height)) = &background {
                let s = args["scale_mm_per_px"].as_f64().unwrap_or(0.0);
                if !(s > 0.0 && s.is_finite()) {
                    return ToolOutcome::Reply("scale_mm_per_px must be a positive number.".into());
                }
                // The image under the level: its top-left at (0, height * s),
                // so image pixel (px, py) is plan point (px*s, (height-py)*s).
                cmds.insert(
                    0,
                    Command::SetBackground {
                        level,
                        background: Some(Background {
                            asset: asset.clone(),
                            mm_per_px: s,
                            offset: pt(0.0, *height as f64 * s),
                            rotation: 0.0,
                            opacity: 0.5,
                        }),
                    },
                );
            }
            match check_batch(&base, &cmds) {
                Ok(done) => {
                    let warnings: Vec<String> = argus_plan::validate::validate(&done)
                        .into_iter()
                        .filter(|i| i.level == level)
                        .map(|i| i.message)
                        .collect();
                    ToolOutcome::Finish(json!({
                        "commands": cmds,
                        "summary": args["summary"].as_str().unwrap_or(""),
                        "scale_mm_per_px": args["scale_mm_per_px"],
                        "warnings": warnings,
                    }))
                }
                Err(e) => ToolOutcome::Reply(format!(
                    "{e} Nothing was applied. Fix it and submit the whole batch again."
                )),
            }
        }),
    };
    let out = agent
        .run(RunSpec {
            kind: kind.into(),
            system,
            user,
            tools: vec![tool],
        })
        .await;
    match out {
        Ok(o) => match o.result {
            Some(mut v) => {
                v["run"] = json!({
                    "id": o.run_id, "steps": o.steps, "cost_usd": o.cost_usd,
                    "input_tokens": o.usage.input_tokens, "output_tokens": o.usage.output_tokens,
                });
                Json(v).into_response()
            }
            None => (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("The model answered without submitting commands: {}", o.text.chars().take(300).collect::<String>())})),
            )
                .into_response(),
        },
        Err(e) => agent_error(e),
    }
}

/// `POST /api/plan/import`: draft a level from an uploaded image.
pub async fn import(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
    Json(req): Json<ImportRequest>,
) -> Response {
    if let Err(r) = admin(role) {
        return *r;
    }
    let Some(store) = st.plan.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((bytes, media_type)) = store.asset(&req.asset) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no such image; upload it first"})),
        )
            .into_response();
    };
    let Some((_, (w, h))) = crate::plan::image_info(&bytes) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "not a PNG or JPEG"})),
        )
            .into_response();
    };
    let base = store.get().plan;
    if base.level(req.level).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no such level"})),
        )
            .into_response();
    }
    let system = format!(
        "You turn floor plan sketches, photos and exported plans into Argus plan commands. \
         Read the walls, doors, windows and room names from the image and submit them with \
         submit_commands. {CONVENTIONS} The image is {w} x {h} pixels. Estimate the scale \
         (scale_mm_per_px) from dimensions written on it, or from typical sizes. Place \
         everything so image pixel (px, py) becomes plan point (px * scale, (H - py) * scale) \
         with H = {h}: the image's bottom-left corner is the plan origin. Order: outside \
         walls, their doors and windows, inside walls, their openings, then rooms. Use level \
         id {}. New ids must be at least {}.",
        req.level.0, base.next_id
    );
    let mut text = level_summary(&base, req.level);
    if !req.notes.trim().is_empty() {
        text.push_str(&format!("\nNotes from the user: {}", req.notes.trim()));
    }
    let user = vec![
        Part::Text { text },
        Part::Image {
            media_type: media_type.into(),
            data: bytes,
        },
    ];
    draft(
        &st,
        "plan-import",
        system,
        user,
        base,
        req.level,
        Some((req.asset, h)),
    )
    .await
}

/// `POST /api/plan/ask`: draft a change described in words.
pub async fn ask(
    State(st): State<AppState>,
    axum::Extension(role): axum::Extension<Role>,
    Json(req): Json<AskRequest>,
) -> Response {
    if let Err(r) = admin(role) {
        return *r;
    }
    let Some(store) = st.plan.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if req.text.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "say what to change"})),
        )
            .into_response();
    }
    let base = store.get().plan;
    if base.level(req.level).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no such level"})),
        )
            .into_response();
    }
    let system = format!(
        "You change an Argus floor plan as the user asks, by submitting plan commands with \
         submit_commands. {CONVENTIONS} Change only what is asked: prefer move_node, \
         update_entity, set_room_name and small additions over rebuilding. Use level id {}.",
        req.level.0
    );
    let mut text = level_summary(&base, req.level);
    // The level's background (a scanned plan, a sketch) goes along, with
    // where its pixels are on the plan, so "draw the outside walls" can be
    // done from it.
    let mut image = None;
    if let Some(bg) = base.level(req.level).and_then(|l| l.background.clone())
        && let Some((bytes, media_type)) = store.asset(&bg.asset)
        && let Some((_, (w, h))) = crate::plan::image_info(&bytes)
    {
        let (wm, hm) = (
            w as f64 * bg.mm_per_px / 1000.0,
            h as f64 * bg.mm_per_px / 1000.0,
        );
        text.push_str(&format!(
            "\n\nThe level's background image is attached ({w} x {h} pixels). Image pixel \
             (px, py) is plan point (x, y) = ({ox:.0} + u*cos(r) - v*sin(r), {oy:.0} + \
             u*sin(r) + v*cos(r)) with u = px * {s}, v = -py * {s} and r = {rot} degrees: \
             the image covers {wm:.1} m by {hm:.1} m on the plan.",
            ox = bg.offset.x,
            oy = bg.offset.y,
            s = bg.mm_per_px,
            rot = bg.rotation,
        ));
        if !(2.0..=500.0).contains(&wm.max(hm)) {
            text.push_str(
                " That size looks wrong for a building. Estimate the real scale from the \
                 drawing (dimensions written on it, or typical door and room sizes) and put a \
                 set_background command first with the corrected mm_per_px and offset (keep \
                 the asset, rotation and opacity); place everything else to match it.",
            );
        }
        image = Some(Part::Image {
            media_type: media_type.to_owned(),
            data: bytes,
        });
    }
    text.push_str(&format!("\n\nThe change: {}", req.text.trim()));
    let mut user = vec![Part::Text { text }];
    user.extend(image);
    draft(&st, "plan-ask", system, user, base, req.level, None).await
}

/// For tests: the schema the model gets.
pub fn tool_schema() -> Value {
    submit_schema(true)
}

/// A scripted conversation for the mock provider (tests and demos only):
/// an import whose first batch has a mistake (an opening too wide for its
/// wall), the corrected batch (the demo house), then an answer to "the
/// garage is 6.2 m deep". For a fresh plan whose level `level` is empty.
#[doc(hidden)]
pub mod demo {
    use argus_plan::{Command, Id, Plan, fixture, pt};
    use serde_json::{Value, json};

    const FIRST_ID: u64 = 1000;

    fn submit(cmds: &[Command], summary: &str, scale: Option<f64>, tokens: (u64, u64)) -> Value {
        let mut args = json!({"summary": summary, "commands": cmds});
        if let Some(s) = scale {
            args["scale_mm_per_px"] = s.into();
        }
        json!({
            "tool_calls": [{"name": "submit_commands", "arguments": args}],
            "usage": {"input_tokens": tokens.0, "output_tokens": tokens.1},
        })
    }

    pub fn mock_script(level: Id) -> Value {
        let house = fixture::demo_house_commands(level, FIRST_ID);
        // First try: one window far too wide for its wall.
        let mut wrong = house.clone();
        let at = wrong
            .iter()
            .position(|c| matches!(c, Command::AddOpening { .. }))
            .unwrap();
        if let Command::AddOpening { width, .. } = &mut wrong[at] {
            *width = 30_000.0;
        }
        // The garage's back wall nodes, as the import leaves them.
        let mut p = Plan::default();
        p.levels[0].id = level;
        p.next_id = p.next_id.max(level.0 + 1);
        p.apply_all(&house).unwrap();
        let l = p.level(level).unwrap();
        let moves: Vec<Command> = [10_000.0, 11_500.0, 13_000.0, 16_000.0]
            .iter()
            .filter_map(|&x| l.node_near(pt(x, 6500.0), 1.0))
            .map(|n| Command::MoveNode {
                level,
                node: n.id,
                to: pt(n.at.x, 6200.0),
            })
            .collect();
        json!([
            submit(
                &wrong,
                "A single-storey house, 16 x 11 m, with a double garage.",
                Some(20.0),
                (5200, 4100)
            ),
            submit(
                &house,
                "A single-storey house, 16 x 11 m, with a double garage; the front window fixed.",
                Some(20.0),
                (9800, 4100)
            ),
            submit(
                &moves,
                "Moved the garage's back wall in so it is 6.2 m deep.",
                None,
                (2600, 300)
            ),
        ])
    }
}
