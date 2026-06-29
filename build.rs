#[path = "src/infra/tmux_glue_build_script.rs"]
mod tmux_glue_build_script;
#[path = "src/infra/tmux_glue_contract.rs"]
mod tmux_glue_contract;

fn main() {
    tmux_glue_build_script::run();
    compile_agent_signal_sender();
    compile_remote_grpc_proto();
    emit_version_info();
}

fn compile_agent_signal_sender() {
    println!("cargo:rerun-if-changed=src/runtime/agent_signal_sender_bundle.c");
    let out_dir =
        std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR should be set by cargo"));
    let object = out_dir.join("waitagent-agent-signal-send");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = std::process::Command::new(cc)
        .args([
            "-O2",
            "-std=c99",
            "-Wall",
            "-Wextra",
            "-o",
            object.to_str().expect("OUT_DIR path should be UTF-8"),
            "src/runtime/agent_signal_sender_bundle.c",
        ])
        .status()
        .expect("failed to invoke C compiler for agent signal sender");
    assert!(
        status.success(),
        "failed to compile bundled agent signal sender"
    );
}

fn emit_version_info() {
    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into());

    let git_hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    let git_dirty = std::process::Command::new("git")
        .args(["diff-index", "--quiet", "HEAD", "--"])
        .status()
        .ok()
        .map(|s| !s.success())
        .unwrap_or(false);

    let build_time = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    let dirty_flag = if git_dirty { "-dirty" } else { "" };
    println!("cargo:rustc-env=WAITAGENT_VERSION_FULL={pkg_version} ({git_hash}{dirty_flag}) {build_time}");
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
