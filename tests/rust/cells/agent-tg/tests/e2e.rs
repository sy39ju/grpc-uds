// SPDX-License-Identifier: MIT OR Apache-2.0
//! AI agent — tonic (stock gRPC) server + grpcuds client (the generated
//! stubs).
use agent_domain::{expect, proto_grpcuds as pb};

#[test]
fn agent_tonic_to_grpcuds() {
    let sock = uds_harness::sock("agent-tg");
    let server = agent_domain::spawn_tonic(&sock);
    uds_harness::wait_for_sock(&sock);
    let mut cli = pb::AgentClient::connect(&sock).unwrap();

    let models = cli.list_models(pb::ListModelsRequest {}).unwrap();
    assert_eq!(models.models.len(), 2);

    let emb = cli
        .embed(pb::EmbedRequest {
            text: "hello world".into(),
        })
        .unwrap();
    assert_eq!(emb.values.len(), 8);

    let mut texts = Vec::new();
    {
        let mut st = cli
            .generate(pb::GenerateRequest {
                model: "echo-1".into(),
                prompt: "the quick brown fox".into(),
                max_tokens: 6,
            })
            .unwrap();
        while let Some(t) = st.message().unwrap() {
            texts.push(t.text);
        }
    }
    assert_eq!(texts, expect::agent_tokens("the quick brown fox", 6));

    drop(cli);
    server.stop();
}

/// Live reverse path against a local ollama (tonic Assistant server + grpcuds
/// client). Ignored by default — run with `cargo test -- --ignored` and
/// `OLLAMA_HOST` set.
#[test]
#[ignore = "requires a local ollama (set OLLAMA_HOST)"]
fn assistant_ollama_reverse() {
    use agent_domain::assistant::backend_from_env;
    use pb::agent_event::Event;
    let (backend, _kind) = backend_from_env();
    let model = std::env::var("AGENT_MODEL").unwrap_or_else(|_| "qwen2.5:7b-instruct".into());
    let sock = uds_harness::sock("agent-tg-ollama");
    let server = agent_domain::spawn_assistant_tonic(&sock, backend, &model);
    uds_harness::wait_for_sock(&sock);

    let mut saw_done = false;
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
            if matches!(ev.event, Some(Event::Done(_))) {
                saw_done = true;
            }
        }
    }
    assert!(saw_done, "loop must finish with Done");
    server.stop();
}
