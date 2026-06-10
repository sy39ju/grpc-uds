// SPDX-License-Identifier: MIT OR Apache-2.0
//! The agent proper: a model↔tools loop that streams its steps as
//! `AgentEvent`s. The model is behind [`ModelBackend`] so the same loop runs
//! against the real Claude API ([`ClaudeBackend`]), a local Ollama
//! ([`OllamaBackend`]), or a deterministic [`ScriptedBackend`] (tests,
//! keyless demo) — selected by [`backend_from_env`].

use std::sync::Arc;

use grpcuds::{MessageWriter, Status};
use serde_json::{json, Value};

use crate::proto_grpcuds::agent_event::Event;
use crate::proto_grpcuds::{AgentEvent, Assistant, Done, RunRequest, ToolResult, ToolUse};

/// One model turn: the assistant's content blocks + why it stopped.
pub struct ModelTurn {
    /// `(text)` and `(id, name, input)` blocks in model order.
    pub blocks: Vec<Block>,
    pub stop_reason: String,
}

pub enum Block {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

/// Anything that can play the model side of the loop. `messages` is the
/// Anthropic Messages-API conversation array (the scripted backend only
/// looks at the last user message).
pub trait ModelBackend: Send + Sync + 'static {
    fn turn(&self, model: &str, messages: &[Value]) -> Result<ModelTurn, String>;
}

// ---- Tools ------------------------------------------------------------------

/// Tool registry the loop executes locally. Deliberately harmless demo
/// tools; a real agent would gate these behind permissions.
fn tool_schemas() -> Value {
    json!([
        {
            "name": "get_time",
            "description": "Current time of the device, ISO-8601 UTC.",
            "input_schema": {"type": "object", "properties": {}}
        },
        {
            "name": "calc",
            "description": "Evaluate a simple binary arithmetic expression: '<a> <op> <b>' with op in + - * /.",
            "input_schema": {
                "type": "object",
                "properties": {"expression": {"type": "string"}},
                "required": ["expression"]
            }
        }
    ])
}

fn run_tool(name: &str, input: &Value) -> String {
    match name {
        "get_time" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("unix:{now}")
        }
        "calc" => {
            let expr = input
                .get("expression")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut it = expr.split_whitespace();
            let (a, op, b) = (it.next(), it.next(), it.next());
            match (
                a.and_then(|v| v.parse::<f64>().ok()),
                op,
                b.and_then(|v| v.parse::<f64>().ok()),
            ) {
                (Some(a), Some("+"), Some(b)) => format!("{}", a + b),
                (Some(a), Some("-"), Some(b)) => format!("{}", a - b),
                (Some(a), Some("*"), Some(b)) => format!("{}", a * b),
                (Some(a), Some("/"), Some(b)) if b != 0.0 => format!("{}", a / b),
                _ => format!("error: cannot evaluate {expr:?}"),
            }
        }
        other => format!("error: unknown tool {other:?}"),
    }
}

// ---- The agent loop ----------------------------------------------------------

/// How [`drive_loop`] ended: cleanly (the caller should finish OK) or with a
/// model-backend failure to surface as a non-OK status.
pub enum LoopEnd {
    /// Loop finished (a `Done` event was already emitted) or the client hung
    /// up mid-stream — either way, nothing more to send.
    Ok,
    /// The model backend errored; the string is the underlying message.
    Backend(String),
}

/// The transport-agnostic agent loop: model ↔ tools until the model stops,
/// emitting each step through `send`. `send` returns `Err(())` when the sink
/// is gone (client hung up), and the loop then stops promptly. This is the
/// shared core behind both the grpcuds [`AgentLoop`] server and any other
/// transport (e.g. a stock tonic server) that wants to host the same loop.
pub fn drive_loop(
    backend: &dyn ModelBackend,
    model: &str,
    prompt: String,
    max_turns: u32,
    mut send: impl FnMut(Event) -> Result<(), ()>,
) -> LoopEnd {
    let max_turns = if max_turns == 0 { 8 } else { max_turns };
    let mut messages = vec![json!({"role": "user", "content": prompt})];
    let mut turns = 0u32;

    let stop_reason = loop {
        turns += 1;
        let turn = match backend.turn(model, &messages) {
            Ok(t) => t,
            Err(e) => return LoopEnd::Backend(e),
        };

        // Stream the assistant blocks; collect tool calls + the raw assistant
        // content for the history.
        let mut tool_calls = Vec::new();
        let mut assistant_content = Vec::new();
        for block in turn.blocks {
            match block {
                Block::Text(t) => {
                    assistant_content.push(json!({"type": "text", "text": t}));
                    if send(Event::Text(t)).is_err() {
                        return LoopEnd::Ok; // client hung up — stop the loop
                    }
                }
                Block::ToolUse { id, name, input } => {
                    assistant_content.push(json!({
                        "type": "tool_use", "id": id, "name": name, "input": input
                    }));
                    if send(Event::ToolUse(ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input_json: input.to_string(),
                    }))
                    .is_err()
                    {
                        return LoopEnd::Ok;
                    }
                    tool_calls.push((id, name, input));
                }
            }
        }

        if turn.stop_reason != "tool_use" {
            break turn.stop_reason;
        }
        if turns >= max_turns {
            break "max_turns".to_string();
        }

        // Execute the tools, stream the results, extend the history.
        messages.push(json!({"role": "assistant", "content": assistant_content}));
        let mut results = Vec::new();
        for (id, name, input) in tool_calls {
            let output = run_tool(&name, &input);
            if send(Event::ToolResult(ToolResult {
                tool_use_id: id.clone(),
                output: output.clone(),
            }))
            .is_err()
            {
                return LoopEnd::Ok;
            }
            results.push(json!({
                "type": "tool_result", "tool_use_id": id, "content": output
            }));
        }
        messages.push(json!({"role": "user", "content": results}));
    };

    let _ = send(Event::Done(Done { stop_reason, turns }));
    LoopEnd::Ok
}

/// `Assistant` service: owns the loop; the RPC client is a thin renderer.
pub struct AgentLoop {
    pub backend: Arc<dyn ModelBackend>,
    pub default_model: String,
}

impl Assistant for AgentLoop {
    fn run(&self, req: RunRequest, w: MessageWriter<AgentEvent>) -> Status {
        let backend = self.backend.clone();
        let model = if req.model.is_empty() {
            self.default_model.clone()
        } else {
            req.model
        };

        std::thread::spawn(move || {
            let send = |ev: Event| w.send(&AgentEvent { event: Some(ev) }).map_err(|_| ());
            match drive_loop(backend.as_ref(), &model, req.prompt, req.max_turns, send) {
                LoopEnd::Ok => {
                    let _ = w.finish(Status::ok());
                }
                LoopEnd::Backend(e) => {
                    let _ = w.finish(Status::unavailable(format!("model backend: {e}")));
                }
            }
        });
        Status::ok()
    }
}

// ---- Backends ----------------------------------------------------------------

/// Real Anthropic Messages API over HTTPS (blocking — runs on the producer
/// thread, which is exactly where grpcuds wants blocking work).
pub struct ClaudeBackend {
    pub api_key: String,
}

impl ModelBackend for ClaudeBackend {
    fn turn(&self, model: &str, messages: &[Value]) -> Result<ModelTurn, String> {
        let body = json!({
            "model": model,
            "max_tokens": 1024,
            "messages": messages,
            "tools": tool_schemas(),
        });
        let resp: Value = ureq::post("https://api.anthropic.com/v1/messages")
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", "2023-06-01")
            .set("content-type", "application/json")
            .send_json(body)
            .map_err(|e| e.to_string())?
            .into_json()
            .map_err(|e| e.to_string())?;

        let stop_reason = resp
            .get("stop_reason")
            .and_then(Value::as_str)
            .unwrap_or("end_turn")
            .to_string();
        let mut blocks = Vec::new();
        for b in resp
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => blocks.push(Block::Text(
                    b.get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                )),
                Some("tool_use") => blocks.push(Block::ToolUse {
                    id: b
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    name: b
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    input: b.get("input").cloned().unwrap_or(Value::Null),
                }),
                _ => {}
            }
        }
        Ok(ModelTurn {
            blocks,
            stop_reason,
        })
    }
}

/// Deterministic keyless backend: turn 1 asks for both tools, turn 2
/// summarizes their results — enough to exercise the whole loop shape.
pub struct ScriptedBackend;

impl ModelBackend for ScriptedBackend {
    fn turn(&self, _model: &str, messages: &[Value]) -> Result<ModelTurn, String> {
        let has_tool_results = messages
            .last()
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .is_some_and(|c| {
                c.iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            });
        if !has_tool_results {
            Ok(ModelTurn {
                blocks: vec![
                    Block::Text("Let me check.".into()),
                    Block::ToolUse {
                        id: "tu_1".into(),
                        name: "get_time".into(),
                        input: json!({}),
                    },
                    Block::ToolUse {
                        id: "tu_2".into(),
                        name: "calc".into(),
                        input: json!({"expression": "6 * 7"}),
                    },
                ],
                stop_reason: "tool_use".into(),
            })
        } else {
            let results: Vec<String> = messages
                .last()
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|b| b.get("content").and_then(Value::as_str))
                .map(str::to_owned)
                .collect();
            Ok(ModelTurn {
                blocks: vec![Block::Text(format!("Tools said: {}", results.join(", ")))],
                stop_reason: "end_turn".into(),
            })
        }
    }
}

/// Local [Ollama](https://ollama.com) via its OpenAI-compatible
/// `/v1/chat/completions` endpoint — the same loop, no cloud, no key. The
/// loop keeps history in Anthropic shape, so this backend translates each
/// way (Anthropic messages -> OpenAI on the request, OpenAI choice ->
/// `ModelTurn` on the response).
pub struct OllamaBackend {
    /// e.g. `http://localhost:11434`.
    pub base_url: String,
}

impl OllamaBackend {
    /// Convert the loop's Anthropic-shaped history to OpenAI chat messages.
    fn to_openai(messages: &[Value]) -> Vec<Value> {
        let mut out = Vec::new();
        for m in messages {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
            match m.get("content") {
                // Plain string user/assistant content.
                Some(Value::String(s)) => {
                    out.push(json!({"role": role, "content": s}));
                }
                // Block array: assistant text+tool_use, or user tool_result.
                Some(Value::Array(blocks)) => {
                    let mut text = String::new();
                    let mut tool_calls = Vec::new();
                    for b in blocks {
                        match b.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                text.push_str(b.get("text").and_then(Value::as_str).unwrap_or(""))
                            }
                            Some("tool_use") => tool_calls.push(json!({
                                "id": b.get("id"),
                                "type": "function",
                                "function": {
                                    "name": b.get("name"),
                                    "arguments": b.get("input").unwrap_or(&Value::Null).to_string(),
                                }
                            })),
                            // tool_result -> a separate role:tool message.
                            Some("tool_result") => out.push(json!({
                                "role": "tool",
                                "tool_call_id": b.get("tool_use_id"),
                                "content": b.get("content"),
                            })),
                            _ => {}
                        }
                    }
                    if !text.is_empty() || !tool_calls.is_empty() {
                        let mut msg = json!({"role": role, "content": text});
                        if !tool_calls.is_empty() {
                            msg["tool_calls"] = Value::Array(tool_calls);
                        }
                        out.push(msg);
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// OpenAI function-tool schema (different envelope than Anthropic's).
    fn openai_tools() -> Value {
        let anthropic = tool_schemas();
        let tools: Vec<Value> = anthropic
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name"),
                        "description": t.get("description"),
                        "parameters": t.get("input_schema"),
                    }
                })
            })
            .collect();
        Value::Array(tools)
    }
}

impl ModelBackend for OllamaBackend {
    fn turn(&self, model: &str, messages: &[Value]) -> Result<ModelTurn, String> {
        let body = json!({
            "model": model,
            "messages": Self::to_openai(messages),
            "tools": Self::openai_tools(),
            "stream": false,
        });
        let resp: Value = ureq::post(&format!("{}/v1/chat/completions", self.base_url))
            .set("content-type", "application/json")
            .send_json(body)
            .map_err(|e| e.to_string())?
            .into_json()
            .map_err(|e| e.to_string())?;

        let msg = resp
            .pointer("/choices/0/message")
            .ok_or_else(|| format!("unexpected ollama response: {resp}"))?;
        let finish = resp
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("stop");

        let mut blocks = Vec::new();
        if let Some(text) = msg.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                blocks.push(Block::Text(text.to_string()));
            }
        }
        for tc in msg
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let f = tc.get("function");
            // OpenAI tool arguments are a JSON *string*; parse to a value.
            let input = f
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null);
            blocks.push(Block::ToolUse {
                id: tc
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                name: f
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                input,
            });
        }
        let stop_reason = if finish == "tool_calls" {
            "tool_use"
        } else {
            "end_turn"
        };
        Ok(ModelTurn {
            blocks,
            stop_reason: stop_reason.to_string(),
        })
    }
}

/// Pick a backend from the environment, in priority order:
/// `ANTHROPIC_API_KEY` (Claude) → `OLLAMA_HOST` (local ollama) → scripted.
pub fn backend_from_env() -> (Arc<dyn ModelBackend>, &'static str) {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.is_empty() {
            return (Arc::new(ClaudeBackend { api_key: key }), "claude");
        }
    }
    if let Ok(host) = std::env::var("OLLAMA_HOST") {
        if !host.is_empty() {
            let base = if host.starts_with("http") {
                host
            } else {
                format!("http://{host}")
            };
            return (Arc::new(OllamaBackend { base_url: base }), "ollama");
        }
    }
    (Arc::new(ScriptedBackend), "scripted")
}
