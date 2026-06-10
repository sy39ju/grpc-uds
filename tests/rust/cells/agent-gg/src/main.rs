// SPDX-License-Identifier: MIT OR Apache-2.0
//! AI agent — grpcuds server + grpcuds client (`cargo run -p agent-gg`).
use agent_domain::proto_grpcuds as pb;

fn main() {
    let sock = uds_harness::sock("agent-gg-demo");
    let (b, _active) = agent_domain::agent_builder(&sock);
    let running = b.build().unwrap().run().unwrap();

    let mut cli = pb::AgentClient::connect(&sock).unwrap();
    let models = cli.list_models(pb::ListModelsRequest {}).unwrap();
    println!("models: {}", models.models.len());

    {
        let mut st = cli
            .generate(pb::GenerateRequest {
                model: "echo-1".into(),
                prompt: "the quick brown fox".into(),
                max_tokens: 6,
            })
            .unwrap();
        while let Some(t) = st.message().unwrap() {
            println!("token[{}] {}", t.index, t.text);
        }
    }

    drop(cli);
    running.join().unwrap();
}
