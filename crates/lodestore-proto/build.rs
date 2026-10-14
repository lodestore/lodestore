fn main() {
    println!("cargo:rerun-if-changed=proto");
    let fds = protox::compile(["lodestore/agent/v1/agent.proto"], ["proto"])
        .expect("compile agent.proto");
    tonic_prost_build::configure()
        .build_transport(false)
        .compile_fds(fds)
        .expect("generate agent.proto bindings");
}
