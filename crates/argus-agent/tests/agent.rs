//! The tool loop with a scripted model, the spend cap, the run log, and the
//! OpenAI-compatible client against a local fake server.

use std::sync::{Arc, Mutex};

use argus_agent::config::AgentConfig;
use argus_agent::mock::MockProvider;
use argus_agent::openai::OpenAiCompat;
use argus_agent::runner::{Agent, RunSpec, Tool, ToolOutcome};
use argus_agent::store::RunStore;
use argus_agent::{AgentError, ChatRequest, Message, ModelProvider, Part, Role, ToolSpec};
use serde_json::{Value, json};

fn cfg(cap: f64, steps: usize) -> AgentConfig {
    serde_json::from_value(json!({
        "provider": "mock", "model": "mock", "monthly_cap_usd": cap, "max_steps": steps,
        "price_input_per_mtok": 1000.0, "price_output_per_mtok": 1000.0,
    }))
    .unwrap()
}

fn agent(script: Value, cap: f64, steps: usize) -> (Agent, Arc<MockProvider>) {
    let mock = Arc::new(MockProvider::from_json(&script.to_string()).unwrap());
    let a = Agent {
        provider: mock.clone(),
        store: Arc::new(RunStore::in_memory().unwrap()),
        cfg: cfg(cap, steps),
    };
    (a, mock)
}

fn adder<'a>(seen: &'a Mutex<Vec<i64>>) -> Tool<'a> {
    Tool {
        spec: ToolSpec {
            name: "add".into(),
            description: "Adds two numbers; call finish with the answer.".into(),
            parameters: json!({"type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}}}),
        },
        run: Box::new(move |args| {
            let s = args["a"].as_i64().unwrap_or(0) + args["b"].as_i64().unwrap_or(0);
            seen.lock().unwrap().push(s);
            ToolOutcome::Reply(s.to_string())
        }),
    }
}

fn finisher<'a>() -> Tool<'a> {
    Tool {
        spec: ToolSpec {
            name: "finish".into(),
            description: "Ends with the answer.".into(),
            parameters: json!({"type": "object", "properties": {"answer": {"type": "integer"}}}),
        },
        run: Box::new(|args| {
            if args["answer"].as_i64() == Some(5) {
                ToolOutcome::Finish(args.clone())
            } else {
                ToolOutcome::Reply("That's not right; add again.".into())
            }
        }),
    }
}

fn spec<'a>(tools: Vec<Tool<'a>>) -> RunSpec<'a> {
    RunSpec {
        kind: "test".into(),
        system: "You add.".into(),
        user: vec![
            Part::Text {
                text: "What is 2 + 3?".into(),
            },
            Part::Image {
                media_type: "image/png".into(),
                data: vec![1, 2, 3],
            },
        ],
        tools,
    }
}

#[tokio::test]
async fn tools_run_until_one_finishes_and_everything_is_logged() {
    let (a, mock) = agent(
        json!([
            {"tool_calls": [{"name": "add", "arguments": {"a": 2, "b": 3}}], "usage": {"input_tokens": 100, "output_tokens": 10}},
            {"tool_calls": [{"name": "finish", "arguments": {"answer": 4}}], "usage": {"input_tokens": 120, "output_tokens": 10}},
            {"tool_calls": [{"name": "nope", "arguments": {}}, {"name": "finish", "arguments": {"answer": 5}}], "usage": {"input_tokens": 140, "output_tokens": 10}}
        ]),
        10.0,
        6,
    );
    let seen = Mutex::new(Vec::new());
    let out = a.run(spec(vec![adder(&seen), finisher()])).await.unwrap();
    assert_eq!(out.result, Some(json!({"answer": 5})));
    assert_eq!(out.steps, 3);
    assert_eq!(*seen.lock().unwrap(), vec![5]);
    assert_eq!((out.usage.input_tokens, out.usage.output_tokens), (360, 30));
    assert!((out.cost_usd - 0.39).abs() < 1e-9, "{}", out.cost_usd);
    // The model saw the tool results, including the unknown tool.
    let reqs = mock.requests.lock().unwrap();
    let last = reqs.last().unwrap();
    let tool_texts: Vec<String> = last
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(Message::joined_text)
        .collect();
    assert_eq!(tool_texts, vec!["5", "That's not right; add again."]);
    assert_eq!(last.tools.len(), 2);
    // The log: prompt (image by size), steps, tokens, cost, outcome.
    let row = &a.store.recent(1).unwrap()[0];
    assert_eq!(row.outcome, "ok");
    assert!(
        row.prompt.contains("What is 2 + 3?") && row.prompt.contains("[image/png image, 3 bytes]")
    );
    assert_eq!(row.steps.as_array().unwrap().len(), 3);
    assert_eq!(
        row.steps[2]["calls"][0]["result"],
        "There is no tool called \"nope\"."
    );
    assert_eq!((row.input_tokens, row.output_tokens), (360, 30));
    assert!((a.spend().0 - 0.39).abs() < 1e-9);
}

#[tokio::test]
async fn a_plain_answer_ends_the_run() {
    let (a, _) = agent(json!([{"text": "It's 5."}]), 10.0, 6);
    let out = a.run(spec(vec![])).await.unwrap();
    assert_eq!(
        (out.result, out.text.as_str(), out.steps),
        (None, "It's 5.", 1)
    );
}

#[tokio::test]
async fn step_limit() {
    let call = json!({"tool_calls": [{"name": "add", "arguments": {"a": 1, "b": 1}}]});
    let (a, mock) = agent(json!([call, call, call, call]), 10.0, 3);
    let seen = Mutex::new(Vec::new());
    let err = a.run(spec(vec![adder(&seen)])).await.unwrap_err();
    assert!(matches!(err, AgentError::TooManySteps(3)), "{err}");
    assert_eq!(mock.calls(), 3);
    assert_eq!(a.store.recent(1).unwrap()[0].outcome, "steps");
}

#[tokio::test]
async fn the_monthly_cap_stops_further_calls() {
    // Each call costs $0.30 at these prices; the cap is $0.50.
    let call = json!({"tool_calls": [{"name": "add", "arguments": {"a": 1, "b": 1}}], "usage": {"input_tokens": 200, "output_tokens": 100}});
    let (a, mock) = agent(json!([call, call, call]), 0.5, 6);
    let seen = Mutex::new(Vec::new());
    let err = a.run(spec(vec![adder(&seen)])).await.unwrap_err();
    match err {
        AgentError::CapReached { spent, cap } => {
            assert!((spent - 0.6).abs() < 1e-9 && cap == 0.5, "{spent} {cap}");
        }
        e => panic!("{e}"),
    }
    assert_eq!(mock.calls(), 2, "stopped before the third call");
    assert_eq!(a.store.recent(1).unwrap()[0].outcome, "cap");
    // And the next run doesn't call the model at all.
    let err = a.run(spec(vec![adder(&seen)])).await.unwrap_err();
    assert!(matches!(err, AgentError::CapReached { .. }));
    assert_eq!(mock.calls(), 2);
}

#[tokio::test]
async fn mock_needs_the_test_flag() {
    // Without ARGUS_TEST_MOCK_AGENT=1 the mock provider refuses to start.
    if std::env::var(argus_agent::config::MOCK_ENV).is_ok() {
        return;
    }
    assert!(cfg(1.0, 1).provider().is_err());
}

/// A fake chat-completions server: records the request, answers with a
/// tool call.
#[tokio::test]
async fn openai_compatible_client() {
    use axum::routing::post;
    type Seen = Option<(Option<String>, Value)>;
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let s2 = seen.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        post(move |headers: axum::http::HeaderMap, body: String| {
            let s2 = s2.clone();
            async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                *s2.lock().unwrap() = Some((auth, serde_json::from_str(&body).unwrap()));
                axum::Json(json!({
                    "choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "add", "arguments": "{\"a\":2,\"b\":3}"}}
                    ]}}],
                    "usage": {"prompt_tokens": 50, "completion_tokens": 7}
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });

    let client = OpenAiCompat::new(
        &base,
        "test-model",
        Some("sk-secret".into()),
        std::time::Duration::from_secs(5),
    )
    .unwrap();
    let req = ChatRequest {
        messages: vec![
            Message::text(Role::System, "sys"),
            Message {
                role: Role::User,
                parts: vec![
                    Part::Text {
                        text: "look".into(),
                    },
                    Part::Image {
                        media_type: "image/png".into(),
                        data: vec![0x89, b'P'],
                    },
                ],
                tool_calls: vec![],
                tool_call_id: None,
            },
        ],
        tools: vec![ToolSpec {
            name: "add".into(),
            description: "adds".into(),
            parameters: json!({"type": "object"}),
        }],
        json_schema: Some(("answer".into(), json!({"type": "object"}))),
        max_tokens: Some(100),
        temperature: None,
    };
    let resp = client.chat(&req).await.unwrap();
    assert_eq!(resp.message.tool_calls[0].name, "add");
    assert_eq!(
        resp.message.tool_calls[0].arguments,
        json!({"a": 2, "b": 3})
    );
    assert_eq!((resp.usage.input_tokens, resp.usage.output_tokens), (50, 7));
    let (auth, body) = seen.lock().unwrap().clone().unwrap();
    assert_eq!(auth.as_deref(), Some("Bearer sk-secret"));
    assert_eq!(body["model"], "test-model");
    assert_eq!(
        body["messages"][0],
        json!({"role": "system", "content": "sys"})
    );
    let parts = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,iVA=");
    assert_eq!(body["tools"][0]["function"]["name"], "add");
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["max_tokens"], 100);

    // Errors from the service come back without the request (or key).
    let bad = OpenAiCompat::new(
        "http://127.0.0.1:1/v1",
        "m",
        Some("sk-secret".into()),
        std::time::Duration::from_secs(2),
    )
    .unwrap();
    let e = bad.chat(&req).await.unwrap_err().to_string();
    assert!(!e.contains("sk-secret"), "{e}");
}
