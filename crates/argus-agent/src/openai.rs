//! An OpenAI-compatible chat-completions client (`POST {base}/chat/completions`).
//! Covers OpenAI, OpenRouter, Zhipu GLM, DeepSeek, Together, vLLM, Ollama
//! and most others.

use std::time::Duration;

use serde_json::{Value, json};

use crate::{
    AgentError, BoxFuture, ChatRequest, ChatResponse, Message, ModelProvider, Part, Role, ToolCall,
    Usage,
};

pub struct OpenAiCompat {
    http: reqwest::Client,
    url: String,
    model: String,
    key: Option<String>,
}

impl OpenAiCompat {
    /// `base_url` like `https://api.openai.com/v1`. The key may be absent
    /// for local servers.
    pub fn new(
        base_url: &str,
        model: &str,
        key: Option<String>,
        timeout: Duration,
    ) -> Result<Self, AgentError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| AgentError::Other(e.to_string()))?;
        Ok(OpenAiCompat {
            http,
            url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            model: model.to_owned(),
            key,
        })
    }

    /// The request body (public for tests).
    pub fn body(&self, req: &ChatRequest) -> Value {
        let messages: Vec<Value> = req.messages.iter().map(message_json).collect();
        let mut body = json!({ "model": self.model, "messages": messages });
        if !req.tools.is_empty() {
            body["tools"] = req
                .tools
                .iter()
                .map(|t| {
                    json!({"type": "function", "function": {
                        "name": t.name, "description": t.description, "parameters": t.parameters,
                    }})
                })
                .collect();
        }
        if let Some((name, schema)) = &req.json_schema {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": { "name": name, "schema": schema },
            });
        }
        if let Some(m) = req.max_tokens {
            body["max_tokens"] = m.into();
        }
        if let Some(t) = req.temperature {
            // As written (0.2, not f32's 0.20000000298...): some providers
            // (Z.AI's GLM) refuse more than two decimals.
            body["temperature"] = (((t as f64) * 100.0).round() / 100.0).into();
        }
        body
    }
}

fn message_json(m: &Message) -> Value {
    let role = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let only_text = m.parts.iter().all(|p| matches!(p, Part::Text { .. }));
    let content: Value = if only_text {
        m.joined_text().into()
    } else {
        m.parts
            .iter()
            .map(|p| match p {
                Part::Text { text } => json!({"type": "text", "text": text}),
                Part::Image { media_type, data } => json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:{media_type};base64,{}", argus_proto::base64_encode(data))},
                }),
            })
            .collect()
    };
    let mut v = json!({ "role": role, "content": content });
    if !m.tool_calls.is_empty() {
        v["tool_calls"] = m
            .tool_calls
            .iter()
            .map(|c| {
                json!({"id": c.id, "type": "function", "function": {
                    "name": c.name, "arguments": c.arguments.to_string(),
                }})
            })
            .collect();
    }
    if let Some(id) = &m.tool_call_id {
        v["tool_call_id"] = id.clone().into();
    }
    v
}

/// Parses a chat-completions answer.
pub fn parse_response(v: &Value) -> Result<ChatResponse, AgentError> {
    if let Some(e) = v.get("error") {
        let mut msg = e["message"].as_str().unwrap_or("unknown error").to_owned();
        // OpenRouter wraps the upstream provider's error; its own message
        // ("Provider returned error") says little, so add the provider's.
        if let Some(raw) = e["metadata"]["raw"].as_str() {
            let inner = serde_json::from_str::<Value>(raw)
                .ok()
                .and_then(|r| r["error"]["message"].as_str().map(String::from))
                .unwrap_or_else(|| raw.to_owned());
            let who = e["metadata"]["provider_name"]
                .as_str()
                .unwrap_or("provider");
            msg = format!("{msg} ({who}: {inner})");
        }
        return Err(AgentError::Provider(msg.chars().take(400).collect()));
    }
    let m = &v["choices"][0]["message"];
    if m.is_null() {
        return Err(AgentError::Provider("no choices in the answer".into()));
    }
    let mut parts = Vec::new();
    if let Some(t) = m["content"].as_str().filter(|t| !t.is_empty()) {
        parts.push(Part::Text { text: t.to_owned() });
    }
    let mut tool_calls = Vec::new();
    for (i, c) in m["tool_calls"].as_array().into_iter().flatten().enumerate() {
        let f = &c["function"];
        let raw = &f["arguments"];
        // Arguments come as a JSON string (sometimes already an object).
        let arguments = match raw.as_str() {
            Some(s) => serde_json::from_str(s).unwrap_or(Value::String(s.to_owned())),
            None => raw.clone(),
        };
        tool_calls.push(ToolCall {
            id: c["id"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| format!("call_{i}")),
            name: f["name"].as_str().unwrap_or_default().to_owned(),
            arguments,
        });
    }
    let u = &v["usage"];
    Ok(ChatResponse {
        message: Message {
            role: Role::Assistant,
            parts,
            tool_calls,
            tool_call_id: None,
        },
        usage: Usage {
            input_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
        },
    })
}

impl ModelProvider for OpenAiCompat {
    fn model(&self) -> &str {
        &self.model
    }

    fn chat<'a>(&'a self, req: &'a ChatRequest) -> BoxFuture<'a, Result<ChatResponse, AgentError>> {
        Box::pin(async move {
            let mut r = self
                .http
                .post(&self.url)
                .header("Content-Type", "application/json")
                .body(self.body(req).to_string());
            if let Some(k) = &self.key {
                r = r.bearer_auth(k);
            }
            // Errors never include the request (and so never the key).
            let resp = r
                .send()
                .await
                .map_err(|e| AgentError::Provider(e.without_url().to_string()))?;
            let status = resp.status();
            let text = resp
                .text()
                .await
                .map_err(|e| AgentError::Provider(e.without_url().to_string()))?;
            let v: Value = serde_json::from_str(&text).map_err(|_| {
                AgentError::Provider(format!(
                    "HTTP {status}: {}",
                    text.chars().take(200).collect::<String>()
                ))
            })?;
            if !status.is_success() && v.get("error").is_none() {
                return Err(AgentError::Provider(format!("HTTP {status}")));
            }
            parse_response(&v)
        })
    }
}
