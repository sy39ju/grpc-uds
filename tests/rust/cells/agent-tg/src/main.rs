// SPDX-License-Identifier: MIT OR Apache-2.0
//! AI agent — tonic (stock gRPC) server + grpcuds client.
//!
//! The mirror of `agent-gt`: a **stock tonic server** hosts the ollama-backed
//! `Assistant` loop, driven by the **grpcuds** generated stub over UDS.
//!   `OLLAMA_HOST=localhost:11434 cargo run -p agent-tg`
//! Offline it falls back to the scripted backend.
use agent_domain::assistant::backend_from_env;
use agent_domain::proto_grpcuds as pb;
use pb::agent_event::Event;

fn main() {
    let (backend, kind) = backend_from_env();
    let model = std::env::var("AGENT_MODEL").unwrap_or_else(|_| "qwen2.5:7b-instruct".into());
    eprintln!("tonic server backend: {kind}, model: {model}");

    let sock = uds_harness::sock("agent-tg-demo");
    let server = agent_domain::spawn_assistant_tonic(&sock, backend, &model);
    uds_harness::wait_for_sock(&sock);

    {
        let mut cli = pb::AssistantClient::connect(&sock).unwrap();
        let mut st = cli
            .run(pb::RunRequest {
                prompt: "What is 6 times 7? Use the calc tool, then state the answer.".into(),
                max_turns: 6,
                model: String::new(),
            })
            .unwrap();
        while let Some(ev) = st.message().unwrap() {
            match ev.event {
                Some(Event::Text(t)) => println!("  [text] {t}"),
                Some(Event::ToolUse(tu)) => println!("  [tool_use] {}({})", tu.name, tu.input_json),
                Some(Event::ToolResult(tr)) => println!("  [tool_result] {}", tr.output),
                Some(Event::Done(d)) => println!("  [done] {} in {} turns", d.stop_reason, d.turns),
                None => {}
            }
        }
    }

    server.stop();
}
