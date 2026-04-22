use crate::tmux_glue_contract::{
    ProcessTmuxGlueExecutor, TmuxGlueBuildError, TmuxGlueBuildPlan, TmuxGlueBuildStatus,
    TmuxGlueTool, TMUX_GLUE_BUILD_DIR_NAME, VENDORED_TMUX_BIN_ENV, VENDORED_TMUX_BUILD_ROOT_ENV,
    VENDORED_TMUX_BUILD_STATUS_ENV, VENDORED_TMUX_MANIFEST_ENV, VENDORED_TMUX_SOURCE_ENV,
    VENDORED_TMUX_SUBMODULE_PATH, VENDORED_TMUX_VERSION_ENV,
};
use std::env;
use std::path::PathBuf;

const INSTALL_SCRIPT_PATH: &str = "./scripts/install-build-deps.sh";

fn execute_vendored_tmux_build(
    plan: &TmuxGlueBuildPlan,
) -> crate::tmux_glue_contract::TmuxGlueExecutionReport {
    let mut executor = ProcessTmuxGlueExecutor::from_current_env();
    match plan.execute_with(&mut executor) {
        Ok(report) => report,
        Err(TmuxGlueBuildError::ToolchainIncomplete(report)) => {
            let mut missing = report
                .missing
                .iter()
                .map(|tool| match tool {
                    TmuxGlueTool::Yacc => {
                        "yacc-compatible parser generator (`bison` or `yacc`)".to_string()
                    }
                    _ => tool.program_name().to_string(),
                })
                .collect::<Vec<_>>();
            missing.sort();
            missing.dedup();
            panic!(
                "failed to execute vendored tmux build plan: missing build tools: {}. Run `{INSTALL_SCRIPT_PATH}` first",
                missing.join(", ")
            )
        }
        Err(error) => panic!("failed to execute vendored tmux build plan: {error}"),
    }
}

pub fn run() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.gitmodules");
    println!("cargo:rerun-if-changed=src/infra/tmux_glue_contract.rs");
    println!("cargo:rerun-if-changed=src/infra/tmux_glue_build_script.rs");
    println!("cargo:rerun-if-changed={VENDORED_TMUX_SUBMODULE_PATH}");

    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must exist"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must exist"));

    let vendored_tmux_source = manifest_dir.join(VENDORED_TMUX_SUBMODULE_PATH);
    let glue_build_root = out_dir.join(TMUX_GLUE_BUILD_DIR_NAME);

    if !vendored_tmux_source.exists() {
        panic!(
            "vendored tmux source is missing at {}. Initialize the pinned submodule before building waitagent",
            vendored_tmux_source.display()
        );
    }

    let plan = TmuxGlueBuildPlan::new(&vendored_tmux_source, &glue_build_root);
    let metadata = plan
        .source_metadata()
        .expect("failed to discover vendored tmux source metadata");
    let report = execute_vendored_tmux_build(&plan);

    println!(
        "cargo:rustc-env={VENDORED_TMUX_SOURCE_ENV}={}",
        report.artifacts.source_path.display()
    );
    println!(
        "cargo:rustc-env={VENDORED_TMUX_BUILD_ROOT_ENV}={}",
        report.artifacts.build_root.display()
    );
    println!(
        "cargo:rustc-env={VENDORED_TMUX_BIN_ENV}={}",
        report.artifacts.tmux_binary_path.display()
    );
    println!(
        "cargo:rustc-env={VENDORED_TMUX_MANIFEST_ENV}={}",
        plan.manifest_path().display()
    );
    println!(
        "cargo:rustc-env={VENDORED_TMUX_VERSION_ENV}={}",
        metadata.version
    );
    println!(
        "cargo:rustc-env={VENDORED_TMUX_BUILD_STATUS_ENV}={}",
        TmuxGlueBuildStatus::Executed.as_str()
    );
}
