// SPDX-License-Identifier: MIT OR Apache-2.0
//! AI agent — grpcuds server + grpcuds client (Agent runtime + scripted
//! Assistant loop, both offline/deterministic; the generated stubs).
use agent_domain::{expect, proto_grpcuds as pb};

#[test]
fn agent_grpcuds_to_grpcuds() {
    let sock = uds_harness::sock("agent-gg");
    let (b, _active) = agent_domain::agent_builder(&sock);
    let running = b.build().unwrap().run().unwrap();
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
    running.join().unwrap();
}

/// The `Assistant` model↔tools loop, driven by the deterministic scripted
/// backend (no network) over grpcuds⇄grpcuds.
#[test]
fn assistant_scripted_grpcuds_to_grpcuds() {
    use pb::agent_event::Event;
    let sock = uds_harness::sock("agent-gg-asst");
    let running = agent_domain::full_builder(
        &sock,
        std::sync::Arc::new(agent_domain::assistant::ScriptedBackend),
        "scripted-1",
    )
    .build()
    .unwrap()
    .run()
    .unwrap();
    let mut cli = pb::AssistantClient::connect(&sock).unwrap();

    let mut kinds = Vec::new();
    {
        let mut st = cli
            .run(pb::RunRequest {
                prompt: "what time is it, and what is 6*7?".into(),
                max_turns: 0,
                model: String::new(),
            })
            .unwrap();
        while let Some(ev) = st.message().unwrap() {
            match ev.event.expect("event set") {
                Event::Text(_) => kinds.push("text"),
                Event::ToolUse(_) => kinds.push("tool_use"),
                Event::ToolResult(_) => kinds.push("tool_result"),
                Event::Done(_) => kinds.push("done"),
            }
        }
    }
    assert!(kinds.contains(&"tool_use"), "loop must invoke a tool");
    assert!(kinds.contains(&"tool_result"));
    assert_eq!(kinds.last(), Some(&"done"), "loop must finish with Done");

    drop(cli);
    running.join().unwrap();
}
