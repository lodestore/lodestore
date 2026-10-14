fn main() {
    println!("cargo:rerun-if-changed=proto");
    let fds = protox::compile(["lodestore/format/v1/format.proto"], ["proto"])
        .expect("compile format.proto");
    prost_build::Config::new()
        .compile_fds(fds)
        .expect("generate format.proto bindings");
}
