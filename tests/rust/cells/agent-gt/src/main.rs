// SPDX-License-Identifier: MIT OR Apache-2.0
//! AI agent — grpcuds server + tonic (stock gRPC) client.
//!
//! `cargo run -p agent-gt` drives the **grpcuds** `Assistant` loop with a stock
//! tonic client. With a local ollama it runs a real model:
//!   `OLLAMA_HOST=localhost:11434 cargo run -p agent-gt`
//! Offline (no `OLLAMA_HOST`/`ANTHROPIC_API_KEY`) it falls back to the scripted
//! backend, so it still runs.
use agent_domain::assistant::backend_from_env;
use agent_domain::proto::{self, agent_event::Event};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let (backend, kind) = backend_from_env();
    let model = std::env::var("AGENT_MODEL").unwrap_or_else(|_| "qwen2.5:7b-instruct".into());
    eprintln!("grpcuds server backend: {kind}, model: {model}");

    let sock = uds_harness::sock("agent-gt-demo");
    let running = agent_domain::full_builder(&sock, backend, &model)
        .build()
        .unwrap()
        .run()
        .unwrap();

    let mut c = agent_domain::assistant_tonic_client(sock.clone()).await;
    let mut stream = c
        .run(proto::RunRequest {
            prompt: "What is 6 times 7? Use the calc tool, then state the answer.".into(),
            max_turns: 6,
            model: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    while let Some(ev) = stream.message().await.unwrap() {
        match ev.event.unwrap() {
            Event::Text(t) => println!("  [text] {t}"),
            Event::ToolUse(tu) => println!("  [tool_use] {}({})", tu.name, tu.input_json),
            Event::ToolResult(tr) => println!("  [tool_result] {}", tr.output),
            Event::Done(d) => println!("  [done] {} in {} turns", d.stop_reason, d.turns),
        }
    }

    running.join().unwrap();
}
