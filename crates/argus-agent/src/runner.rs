//! The tool loop: send the conversation, run the tools the model asks for,
//! send back their results, until the model answers without tools, a tool
//! finishes the run, or the step limit is hit. Spending is checked against
//! the monthly cap before every model call and logged after it.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::config::{AgentConfig, cost};
use crate::store::{RunStore, month_start_ms};
use crate::{AgentError, ChatRequest, Message, ModelProvider, Part, Role, ToolSpec, Usage};

/// What a tool hands back.
pub enum ToolOutcome {
    /// Text for the model (results, or what to fix); the loop goes on.
    Reply(String),
    /// The run's result; the loop ends.
    Finish(Value),
}

pub struct Tool<'a> {
    pub spec: ToolSpec,
    pub run: Box<dyn FnMut(&Value) -> ToolOutcome + Send + 'a>,
}

pub struct RunSpec<'a> {
    /// What the run is for ("plan-import", "plan-ask"...), for the log.
    pub kind: String,
    pub system: String,
    pub user: Vec<Part>,
    pub tools: Vec<Tool<'a>>,
}

#[derive(Debug)]
pub struct RunOutcome {
    pub run_id: i64,
    /// What a tool finished with, if one did.
    pub result: Option<Value>,
    /// The model's last text.
    pub text: String,
    pub usage: Usage,
    pub cost_usd: f64,
    /// Model calls made.
    pub steps: usize,
}

/// A configured model with its run log.
pub struct Agent {
    pub provider: Arc<dyn ModelProvider>,
    pub store: Arc<RunStore>,
    pub cfg: AgentConfig,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_owned()
    } else {
        let mut end = n;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}… ({} bytes)", &s[..end], s.len())
    }
}

impl Agent {
    /// (spent, cap) this month.
    pub fn spend(&self) -> (f64, f64) {
        let spent = self
            .store
            .spent_since(month_start_ms(now_ms()))
            .unwrap_or(0.0);
        (spent, self.cfg.monthly_cap_usd)
    }

    pub async fn run(&self, mut spec: RunSpec<'_>) -> Result<RunOutcome, AgentError> {
        let prompt = format!(
            "{}\n---\n{}",
            spec.system,
            spec.user
                .iter()
                .map(|p| match p {
                    Part::Text { text } => text.clone(),
                    Part::Image { media_type, data } =>
                        format!("[{media_type} image, {} bytes]", data.len()),
                })
                .collect::<Vec<_>>()
                .join("\n")
        );
        let store_err = |e: anyhow::Error| AgentError::Other(format!("agent log: {e}"));
        let id = self
            .store
            .start(
                &spec.kind,
                self.provider.model(),
                &clip(&prompt, 20_000),
                now_ms(),
            )
            .map_err(store_err)?;
        let mut messages = vec![
            Message::text(Role::System, spec.system.clone()),
            Message {
                role: Role::User,
                parts: std::mem::take(&mut spec.user),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
        ];
        let tool_specs: Vec<ToolSpec> = spec.tools.iter().map(|t| t.spec.clone()).collect();
        let mut usage = Usage::default();
        let mut log: Vec<Value> = Vec::new();
        let mut text = String::new();
        let result = 'run: {
            for step in 0..self.cfg.max_steps.max(1) {
                let (spent, cap) = self.spend();
                if spent >= cap {
                    break 'run Err(AgentError::CapReached { spent, cap });
                }
                let req = ChatRequest {
                    messages: messages.clone(),
                    tools: tool_specs.clone(),
                    json_schema: None,
                    max_tokens: None,
                    temperature: Some(0.2),
                };
                let resp = match self.provider.chat(&req).await {
                    Ok(r) => r,
                    Err(e) => break 'run Err(e),
                };
                usage.input_tokens += resp.usage.input_tokens;
                usage.output_tokens += resp.usage.output_tokens;
                text = resp.message.joined_text();
                let mut entry = json!({
                    "step": step + 1,
                    "text": clip(&text, 4000),
                    "usage": resp.usage,
                    "calls": [],
                });
                let calls = resp.message.tool_calls.clone();
                messages.push(resp.message);
                let mut finished = None;
                for call in &calls {
                    let outcome = match spec.tools.iter_mut().find(|t| t.spec.name == call.name) {
                        Some(t) => (t.run)(&call.arguments),
                        None => {
                            ToolOutcome::Reply(format!("There is no tool called {:?}.", call.name))
                        }
                    };
                    let (reply, done) = match outcome {
                        ToolOutcome::Reply(r) => (r, None),
                        ToolOutcome::Finish(v) => ("Done.".to_string(), Some(v)),
                    };
                    entry["calls"].as_array_mut().unwrap().push(json!({
                        "name": call.name,
                        "arguments": clip(&call.arguments.to_string(), 20_000),
                        "result": clip(&reply, 4000),
                    }));
                    messages.push(Message::tool_result(&call.id, reply));
                    if done.is_some() {
                        finished = done;
                    }
                }
                log.push(entry);
                let c = cost(&self.cfg, usage.input_tokens, usage.output_tokens);
                let _ = self.store.update(
                    id,
                    &Value::Array(log.clone()),
                    usage.input_tokens,
                    usage.output_tokens,
                    c,
                );
                if finished.is_some() || calls.is_empty() {
                    break 'run Ok((finished, step + 1));
                }
            }
            Err(AgentError::TooManySteps(self.cfg.max_steps))
        };
        let c = cost(&self.cfg, usage.input_tokens, usage.output_tokens);
        match result {
            Ok((value, steps)) => {
                let _ = self.store.finish(id, "ok", None, now_ms());
                tracing::info!(run = id, steps, cost_usd = c, "agent run finished");
                Ok(RunOutcome {
                    run_id: id,
                    result: value,
                    text,
                    usage,
                    cost_usd: c,
                    steps,
                })
            }
            Err(e) => {
                let outcome = match e {
                    AgentError::CapReached { .. } => "cap",
                    AgentError::TooManySteps(_) => "steps",
                    _ => "error",
                };
                let _ = self
                    .store
                    .finish(id, outcome, Some(&e.to_string()), now_ms());
                tracing::warn!(run = id, "agent run failed: {e}");
                Err(e)
            }
        }
    }
}
