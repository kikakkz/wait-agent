#[path = "src/infra/tmux_glue_build_script.rs"]
mod tmux_glue_build_script;
#[path = "src/infra/tmux_glue_contract.rs"]
mod tmux_glue_contract;

fn main() {
    tmux_glue_build_script::run();
    compile_remote_grpc_proto();
}

fn compile_remote_grpc_proto() {
    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("vendored protoc binary should be available for remote gRPC proto generation");
    std::env::set_var("PROTOC", protoc);

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(
            &["proto/waitagent/remote/v1/node_session.proto"],
            &["proto"],
        )
        .expect("remote gRPC proto generation should succeed");
}
