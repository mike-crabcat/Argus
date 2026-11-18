//! A provider that plays back scripted answers, for tests and demos. It
//! records each request it gets, so tests can check what the model saw.

use std::sync::Mutex;

use serde::Deserialize;

use crate::{
    AgentError, BoxFuture, ChatRequest, ChatResponse, Message, ModelProvider, Part, Role, ToolCall,
    Usage,
};

/// One scripted answer.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct MockTurn {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<MockCall>,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MockCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

pub struct MockProvider {
    turns: Vec<MockTurn>,
    next: Mutex<usize>,
    /// Every request received, in order.
    pub requests: Mutex<Vec<ChatRequest>>,
    /// Loop the script from the start when it runs out (for demos).
    pub cycle: bool,
}

impl MockProvider {
    pub fn new(turns: Vec<MockTurn>) -> Self {
        MockProvider {
            turns,
            next: Mutex::new(0),
            requests: Mutex::new(Vec::new()),
            cycle: false,
        }
    }

    /// A script from JSON: `[{"text": ...} | {"tool_calls": [...]}, ...]`.
    pub fn from_json(json: &str) -> Result<Self, AgentError> {
        let turns: Vec<MockTurn> = serde_json::from_str(json)
            .map_err(|e| AgentError::Other(format!("mock script: {e}")))?;
        Ok(MockProvider::new(turns))
    }

    pub fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl ModelProvider for MockProvider {
    fn model(&self) -> &str {
        "mock"
    }

    fn chat<'a>(&'a self, req: &'a ChatRequest) -> BoxFuture<'a, Result<ChatResponse, AgentError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(req.clone());
            let turn = {
                let mut n = self.next.lock().unwrap();
                if *n >= self.turns.len() {
                    if self.cycle && !self.turns.is_empty() {
                        *n = 0;
                    } else {
                        return Err(AgentError::Provider("the mock script ran out".into()));
                    }
                }
                *n += 1;
                self.turns[*n - 1].clone()
            };
            Ok(ChatResponse {
                message: Message {
                    role: Role::Assistant,
                    parts: turn
                        .text
                        .into_iter()
                        .map(|text| Part::Text { text })
                        .collect(),
                    tool_calls: turn
                        .tool_calls
                        .into_iter()
                        .enumerate()
                        .map(|(i, c)| ToolCall {
                            id: format!("mock_{i}"),
                            name: c.name,
                            arguments: c.arguments,
                        })
                        .collect(),
                    tool_call_id: None,
                },
                usage: turn.usage,
            })
        })
    }
}
