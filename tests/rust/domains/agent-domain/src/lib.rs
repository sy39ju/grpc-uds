// SPDX-License-Identifier: MIT OR Apache-2.0
//! AI-agent domain logic shared by the `agent-gg` / `agent-gt` / `agent-tg`
//! cells.
//!
//! Two services, two transports:
//! - **`Agent`** (model runtime: ListModels / Generate / Embed) — deterministic
//!   echo runtime, implemented for both grpcuds ([`agent_builder`]) and tonic
//!   ([`spawn_tonic`]). This is what the cells' e2e tests exercise (offline).
//! - **`Assistant`** (the model↔tools loop) — backed by a real model via
//!   [`assistant::backend_from_env`] (ollama / claude / scripted). Served over
//!   grpcuds ([`full_builder`]) and tonic ([`spawn_assistant_tonic`]); the cells
//!   drive it from their `main` as a live (ollama) demo.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// grpcuds-build output: the generated `Agent` + `Assistant` traits.
pub mod proto_grpcuds {
    include!(concat!(env!("OUT_DIR"), "/grpcuds/agent.rs"));
}

/// Canonical prost messages + tonic client/server stubs.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/tonic/agent.rs"));
}

pub mod assistant;

/// gRPC method paths for raw (non-stub) grpcuds clients — the cells use the
/// generated `proto_grpcuds` stubs; sizebench drives these directly.
pub mod paths {
    pub const LIST_MODELS: &str = "/agent.Agent/ListModels";
    pub const GENERATE: &str = "/agent.Agent/Generate";
    pub const EMBED: &str = "/agent.Agent/Embed";
    pub const ASSISTANT_RUN: &str = "/agent.Assistant/Run";
}

/// Deterministic fixtures both Agent servers reproduce.
pub mod expect {
    /// Tokens produced for `prompt` capped at `max` — the echo "inference".
    pub fn agent_tokens(prompt: &str, max: u32) -> Vec<String> {
        let words: Vec<&str> = prompt.split_whitespace().collect();
        (0..max)
            .map(|i| {
                words
                    .get(i as usize % words.len().max(1))
                    .map(|w| w.to_string())
                    .unwrap_or_else(|| "…".to_string())
            })
            .collect()
    }
}

// ---- grpcuds Agent server (model runtime) -----------------------------------

use grpcuds::{MessageWriter, Server, ServerBuilder, Status};
use proto_grpcuds::{
    Agent, EmbedRequest, Embedding, GenerateRequest, ListModelsRequest, Model, ModelList, Token,
};

pub struct MockRuntime {
    /// Producers currently streaming — observable proof that cancellation
    /// actually stops generation.
    pub active_generations: Arc<AtomicUsize>,
}

impl Default for MockRuntime {
    fn default() -> Self {
        Self {
            active_generations: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Agent for MockRuntime {
    fn list_models(&self, _req: ListModelsRequest) -> Result<ModelList, Status> {
        Ok(ModelList {
            models: vec![
                Model {
                    name: "echo-1".into(),
                    context_len: 4096,
                },
                Model {
                    name: "echo-1-mini".into(),
                    context_len: 1024,
                },
            ],
        })
    }

    fn generate(&self, req: GenerateRequest, w: MessageWriter<Token>) -> Status {
        if req.model != "echo-1" && req.model != "echo-1-mini" {
            return Status::not_found(format!("unknown model {:?}", req.model));
        }
        let max = if req.max_tokens == 0 {
            32
        } else {
            req.max_tokens
        };
        let active = self.active_generations.clone();
        active.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            for (i, text) in expect::agent_tokens(&req.prompt, max)
                .into_iter()
                .enumerate()
            {
                std::thread::sleep(Duration::from_millis(5)); // token cadence
                let token = Token {
                    index: i as u32,
                    text,
                };
                if w.send(&token).is_err() {
                    break; // client cancelled / disconnected
                }
            }
            let _ = w.finish(Status::ok());
            active.fetch_sub(1, Ordering::SeqCst);
        });
        Status::ok()
    }

    fn embed(&self, req: EmbedRequest) -> Result<Embedding, Status> {
        let mut buckets = [0f32; 8];
        for b in req.text.bytes() {
            buckets[(b % 8) as usize] += 1.0;
        }
        let norm = buckets.iter().map(|v| v * v).sum::<f32>().sqrt().max(1.0);
        Ok(Embedding {
            values: buckets.iter().map(|v| v / norm).collect(),
        })
    }
}

/// grpcuds builder with a fresh mock runtime; also returns the active-producer
/// counter so tests can observe cancellation.
pub fn agent_builder(sock: &str) -> (ServerBuilder, Arc<AtomicUsize>) {
    let rt = MockRuntime::default();
    let active = rt.active_generations.clone();
    (
        proto_grpcuds::add_agent_service(Server::builder().bind(sock), Arc::new(rt)),
        active,
    )
}

/// grpcuds builder serving BOTH the model runtime (`Agent`) and the agent loop
/// (`Assistant`), the latter backed by `backend`/`default_model`.
pub fn full_builder(
    sock: &str,
    backend: Arc<dyn assistant::ModelBackend>,
    default_model: &str,
) -> ServerBuilder {
    let b = proto_grpcuds::add_agent_service(
        Server::builder().bind(sock),
        Arc::new(MockRuntime::default()),
    );
    proto_grpcuds::add_assistant_service(
        b,
        Arc::new(assistant::AgentLoop {
            backend,
            default_model: default_model.to_string(),
        }),
    )
}

// ---- tonic Agent server (model runtime) -------------------------------------

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response};

#[derive(Default)]
pub struct TonicAgent;

#[tonic::async_trait]
impl proto::agent_server::Agent for TonicAgent {
    async fn list_models(
        &self,
        _req: Request<proto::ListModelsRequest>,
    ) -> Result<Response<proto::ModelList>, tonic::Status> {
        Ok(Response::new(proto::ModelList {
            models: vec![
                proto::Model {
                    name: "echo-1".into(),
                    context_len: 4096,
                },
                proto::Model {
                    name: "echo-1-mini".into(),
                    context_len: 1024,
                },
            ],
        }))
    }

    type GenerateStream = ReceiverStream<Result<proto::Token, tonic::Status>>;
    async fn generate(
        &self,
        req: Request<proto::GenerateRequest>,
    ) -> Result<Response<Self::GenerateStream>, tonic::Status> {
        let req = req.into_inner();
        if req.model != "echo-1" && req.model != "echo-1-mini" {
            return Err(tonic::Status::not_found(format!(
                "unknown model {:?}",
                req.model
            )));
        }
        let max = if req.max_tokens == 0 {
            32
        } else {
            req.max_tokens
        };
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            for (i, text) in expect::agent_tokens(&req.prompt, max)
                .into_iter()
                .enumerate()
            {
                let tok = proto::Token {
                    index: i as u32,
                    text,
                };
                if tx.send(Ok(tok)).await.is_err() {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn embed(
        &self,
        req: Request<proto::EmbedRequest>,
    ) -> Result<Response<proto::Embedding>, tonic::Status> {
        let mut buckets = [0f32; 8];
        for b in req.into_inner().text.bytes() {
            buckets[(b % 8) as usize] += 1.0;
        }
        let norm = buckets.iter().map(|v| v * v).sum::<f32>().sqrt().max(1.0);
        Ok(Response::new(proto::Embedding {
            values: buckets.iter().map(|v| v / norm).collect(),
        }))
    }
}

/// Start the stock-gRPC `Agent` (model runtime) server on `sock`.
pub fn spawn_tonic(sock: &str) -> uds_harness::TonicServer {
    let routes = tonic::service::Routes::new(proto::agent_server::AgentServer::new(TonicAgent));
    uds_harness::serve_routes(sock.to_string(), routes)
}

/// A tonic `Agent` client over the UDS at `path`.
pub async fn tonic_client(
    path: String,
) -> proto::agent_client::AgentClient<tonic::transport::Channel> {
    proto::agent_client::AgentClient::new(uds_harness::connect_uds(path).await)
}

// ---- tonic Assistant server (the model↔tools loop) --------------------------

use assistant::{drive_loop, LoopEnd, ModelBackend};
use proto::agent_event::Event as DstEvent;
use proto_grpcuds::agent_event::Event as SrcEvent;

/// The loop emits grpcuds-build event types; the tonic server streams the
/// tonic-side types. Same proto, distinct Rust types — translate at the edge.
fn translate(ev: SrcEvent) -> DstEvent {
    match ev {
        SrcEvent::Text(t) => DstEvent::Text(t),
        SrcEvent::ToolUse(tu) => DstEvent::ToolUse(proto::ToolUse {
            id: tu.id,
            name: tu.name,
            input_json: tu.input_json,
        }),
        SrcEvent::ToolResult(tr) => DstEvent::ToolResult(proto::ToolResult {
            tool_use_id: tr.tool_use_id,
            output: tr.output,
        }),
        SrcEvent::Done(d) => DstEvent::Done(proto::Done {
            stop_reason: d.stop_reason,
            turns: d.turns,
        }),
    }
}

/// The `Assistant` loop as a stock tonic server, backed by a model.
pub struct TonicAssistant {
    pub backend: Arc<dyn ModelBackend>,
    pub default_model: String,
}

#[tonic::async_trait]
impl proto::assistant_server::Assistant for TonicAssistant {
    type RunStream = ReceiverStream<Result<proto::AgentEvent, tonic::Status>>;

    async fn run(
        &self,
        req: Request<proto::RunRequest>,
    ) -> Result<Response<Self::RunStream>, tonic::Status> {
        let req = req.into_inner();
        let backend = self.backend.clone();
        let model = if req.model.is_empty() {
            self.default_model.clone()
        } else {
            req.model.clone()
        };
        let (tx, rx) = mpsc::channel(16);
        // The loop blocks (ureq) — run it off the executor, as grpcuds does.
        std::thread::spawn(move || {
            let send = |ev: SrcEvent| {
                tx.blocking_send(Ok(proto::AgentEvent {
                    event: Some(translate(ev)),
                }))
                .map_err(|_| ())
            };
            match drive_loop(backend.as_ref(), &model, req.prompt, req.max_turns, send) {
                LoopEnd::Ok => {}
                LoopEnd::Backend(e) => {
                    let _ = tx.blocking_send(Err(tonic::Status::unavailable(format!(
                        "model backend: {e}"
                    ))));
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// Start the stock-gRPC `Assistant` (agent loop) server on `sock`.
pub fn spawn_assistant_tonic(
    sock: &str,
    backend: Arc<dyn ModelBackend>,
    default_model: &str,
) -> uds_harness::TonicServer {
    let routes = tonic::service::Routes::new(proto::assistant_server::AssistantServer::new(
        TonicAssistant {
            backend,
            default_model: default_model.to_string(),
        },
    ));
    uds_harness::serve_routes(sock.to_string(), routes)
}

/// A tonic `Assistant` client over the UDS at `path`.
pub async fn assistant_tonic_client(
    path: String,
) -> proto::assistant_client::AssistantClient<tonic::transport::Channel> {
    proto::assistant_client::AssistantClient::new(uds_harness::connect_uds(path).await)
}
