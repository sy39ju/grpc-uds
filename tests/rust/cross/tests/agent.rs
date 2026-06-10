// SPDX-License-Identifier: MIT OR Apache-2.0
//! AI-agent cross-language: Rust tonic peer ⇄ C++ grpcuds binary (Agent
//! service only — the C++ side never serves the Assistant loop).
use agent_domain::{expect, proto};

/// gt: C++ grpcuds **server** ← Rust tonic **client**.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_gt_cpp_server() {
    let Some(bin) = cross::cpp_bin("AGENT_GT_SERVER_BIN", "agent/agent-gt-server") else {
        eprintln!("skipping agent_gt: C++ agent-gt-server not built");
        return;
    };
    let sock = uds_harness::sock("agent-gt-cross");
    let _guard = uds_harness::cpp::spawn_server(&bin, &sock);

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
}

/// tg: Rust tonic **server** ← C++ grpcuds **client** (self-checking, exit 0).
#[test]
fn agent_tg_cpp_client() {
    let Some(bin) = cross::cpp_bin("AGENT_TG_CLIENT_BIN", "agent/agent-tg-client") else {
        eprintln!("skipping agent_tg: C++ agent-tg-client not built");
        return;
    };
    let sock = uds_harness::sock("agent-tg-cross");
    let server = agent_domain::spawn_tonic(&sock);
    uds_harness::wait_for_sock(&sock);

    let ok = uds_harness::cpp::run_client(&bin, &sock);
    assert!(ok, "C++ agent-tg-client self-check failed");

    server.stop();
}
