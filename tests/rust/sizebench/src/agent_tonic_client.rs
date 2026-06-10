// SPDX-License-Identifier: MIT OR Apache-2.0
//! Agent client, tonic — the stock-gRPC side of the footprint comparison.
use agent_domain::proto;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let sock = std::env::args().nth(1).unwrap();
    let mut c = agent_domain::tonic_client(sock).await;
    let _ = c.list_models(proto::ListModelsRequest {}).await.unwrap();
    let mut s = c
        .generate(proto::GenerateRequest {
            model: "echo-1".into(),
            prompt: "the quick brown fox".into(),
            max_tokens: 6,
        })
        .await
        .unwrap()
        .into_inner();
    while s.message().await.unwrap().is_some() {}
    println!("READY");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
}
