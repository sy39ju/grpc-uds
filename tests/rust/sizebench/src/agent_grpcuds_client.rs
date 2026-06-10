// SPDX-License-Identifier: MIT OR Apache-2.0
//! Agent client, grpcuds (raw typed `Client`) — footprint comparison.
use agent_domain::{paths, proto};

fn main() {
    let sock = std::env::args().nth(1).unwrap();
    let mut cli = grpcuds::Client::connect(&sock).unwrap();
    let _: proto::ModelList = cli
        .unary_msg(paths::LIST_MODELS, &proto::ListModelsRequest {})
        .unwrap();
    {
        let mut st = cli
            .server_streaming_msg::<_, proto::Token>(
                paths::GENERATE,
                &proto::GenerateRequest {
                    model: "echo-1".into(),
                    prompt: "the quick brown fox".into(),
                    max_tokens: 6,
                },
            )
            .unwrap();
        while st.message().unwrap().is_some() {}
    }
    println!("READY");
    std::thread::sleep(std::time::Duration::from_secs(5));
}
