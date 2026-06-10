// SPDX-License-Identifier: MIT OR Apache-2.0
//! Agent server, grpcuds — one side of the footprint comparison.
fn main() {
    let sock = std::env::args().nth(1).unwrap();
    let _ = std::fs::remove_file(&sock);
    let (b, _active) = agent_domain::agent_builder(&sock);
    let _running = b.build().unwrap().run().unwrap();
    println!("READY");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
