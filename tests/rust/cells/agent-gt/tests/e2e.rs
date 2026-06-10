// SPDX-License-Identifier: MIT OR Apache-2.0
//! AI agent — grpcuds server + tonic (stock gRPC) client.
use agent_domain::{expect, proto};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_grpcuds_to_tonic() {
    let sock = uds_harness::sock("agent-gt");
    let (b, _active) = agent_domain::agent_builder(&sock);
    let running = b.build().unwrap().run().unwrap();
    let mut c = agent_domain::tonic_client(sock.clone()).await;

    let models = c
        .list_models(proto::ListModelsRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(models.models.len(), 2);

    let mut stream = c
        .generate(proto::GenerateRequest {
            model: "echo-1".into(),
            prompt: "the quick brown fox".into(),
            max_tokens: 6,
        })
        .await
        .unwrap()
        .into_inner();
    let mut texts = Vec::new();
    while let Some(t) = stream.message().await.unwrap() {
        texts.push(t.text);
    }
    assert_eq!(texts, expect::agent_tokens("the quick brown fox", 6));

    running.join().unwrap();
}

/// Live forward path against a local ollama (grpcuds Assistant server + tonic
/// client). Ignored by default — run with `cargo test -- --ignored` and
/// `OLLAMA_HOST` set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a local ollama (set OLLAMA_HOST)"]
async fn assistant_ollama_forward() {
    use agent_domain::assistant::backend_from_env;
    use proto::agent_event::Event;
    let (backend, _kind) = backend_from_env();
    let model = std::env::var("AGENT_MODEL").unwrap_or_else(|_| "qwen2.5:7b-instruct".into());
    let sock = uds_harness::sock("agent-gt-ollama");
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
    let mut saw_done = false;
    while let Some(ev) = stream.message().await.unwrap() {
        if matches!(ev.event, Some(Event::Done(_))) {
            saw_done = true;
        }
    }
    assert!(saw_done, "loop must finish with Done");
    running.join().unwrap();
}
