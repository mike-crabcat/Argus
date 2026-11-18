//! Agent plumbing for Argus, independent of any model vendor.
//!
//! * [`ModelProvider`]: one chat turn with text and image inputs, tool
//!   (function) calling and optional structured JSON output.
//! * [`openai::OpenAiCompat`]: any OpenAI-compatible chat-completions API
//!   (OpenAI, OpenRouter, Zhipu GLM, local servers...).
//! * [`mock::MockProvider`]: scripted answers, for tests.
//! * [`runner::run`]: the tool loop: the model calls tools, Argus runs
//!   them and hands back the results, up to a step limit. Every run is
//!   logged in [`store::RunStore`] (prompt, tool calls, tokens, cost; never
//!   the API key) and checked against a monthly spend cap.
//! * [`config::AgentConfig`]: the `[agent]` section of argus.toml.
//!
//! Streaming isn't implemented: Argus's uses (plan import, plan edits) want
//! the whole answer before acting on it.

pub mod config;
pub mod mock;
pub mod openai;
pub mod runner;
pub mod store;

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Part of a message: text or an image.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
    },
    /// Image bytes with their media type (image/png, image/jpeg).
    Image {
        media_type: String,
        #[serde(with = "bytes_len")]
        data: Vec<u8>,
    },
}

/// Images are logged by size only.
mod bytes_len {
    pub fn serialize<S: serde::Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("[{} bytes]", v.len()))
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(_: D) -> Result<Vec<u8>, D::Error> {
        Ok(Vec::new())
    }
}

/// A call the model wants made.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub parts: Vec<Part>,
    /// Assistant messages: the tools it called.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Tool messages: which call this answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Message {
            role,
            parts: vec![Part::Text { text: text.into() }],
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn tool_result(call_id: &str, text: impl Into<String>) -> Self {
        Message {
            role: Role::Tool,
            parts: vec![Part::Text { text: text.into() }],
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.to_owned()),
        }
    }

    /// All text parts, joined.
    pub fn joined_text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Text { text } => Some(text.as_str()),
                Part::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A tool the model may call, with a JSON Schema for its arguments.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    /// Ask for a JSON answer matching this schema (name, schema).
    pub json_schema: Option<(String, serde_json::Value)>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatResponse {
    /// The assistant's message (text and/or tool calls).
    pub message: Message,
    pub usage: Usage,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("no model is set up (add an [agent] section to argus.toml)")]
    NotConfigured,
    #[error("this month's model spending cap (${cap:.2}) is reached (${spent:.2} spent)")]
    CapReached { spent: f64, cap: f64 },
    #[error("the model took more than {0} steps without finishing")]
    TooManySteps(usize),
    #[error("the model service failed: {0}")]
    Provider(String),
    #[error("{0}")]
    Other(String),
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A chat model.
pub trait ModelProvider: Send + Sync {
    /// Which model answers, for logs and status.
    fn model(&self) -> &str;
    /// One turn: the next assistant message for this conversation.
    fn chat<'a>(&'a self, req: &'a ChatRequest) -> BoxFuture<'a, Result<ChatResponse, AgentError>>;
}
