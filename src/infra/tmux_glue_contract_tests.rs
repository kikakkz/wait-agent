use super::{
    ProcessTmuxGlueExecutor, TmuxGlueBuildConfig, TmuxGlueBuildError, TmuxGlueBuildPlan,
    TmuxGlueBuildStep, TmuxGlueBuildStepKind, TmuxGlueExecutionError, TmuxGlueLayout,
    TmuxGlueManifest, TmuxGlueManifestWriter, TmuxGlueSourceMetadata, TmuxGlueStepExecutor,
    TmuxGlueTool, TMUX_GLUE_CONTRACT_VERSION,
};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("waitagent-{name}-{}-{nonce}", std::process::id()))
}

#[test]
fn layout_derives_expected_artifact_paths() {
    let layout = TmuxGlueLayout::new("/tmp/src", "/tmp/build");

    assert_eq!(layout.bin_dir_path(), PathBuf::from("/tmp/build/stage/bin"));
    assert_eq!(
        layout.tmux_binary_path(),
        PathBuf::from("/tmp/build/stage/bin/tmux")
    );
    assert_eq!(
        layout.static_lib_path(),
        PathBuf::from("/tmp/build/lib/libtmux-glue.a")
    );
    assert_eq!(
        layout.manifest_path(),
        PathBuf::from("/tmp/build/tmux-glue-manifest.env")
    );
}

#[test]
fn build_plan_prepares_layout_and_manifest() {
    let source_path = temp_path("tmux-src");
    let build_root = temp_path("tmux-build");
    fs::create_dir_all(&source_path).expect("source path should be creatable");
    fs::write(
        source_path.join("configure.ac"),
        "AC_INIT([tmux], test-1.0)\n",
    )
    .expect("configure.ac should be writable");
    fs::write(source_path.join("autogen.sh"), "#!/bin/sh\n").expect("autogen should exist");

    let plan = TmuxGlueBuildPlan::new(&source_path, &build_root);
    let artifacts = plan.prepare().expect("build plan should prepare");
    let manifest =
        TmuxGlueManifest::from_path(plan.manifest_path()).expect("manifest should parse");

    assert_eq!(artifacts.source_path, source_path);
    assert!(build_root.join("stage").join("bin").exists());
    assert!(build_root.join("lib").exists());
    assert!(build_root.join("stage").join("include").exists());
    assert_eq!(
        manifest
            .require_value("contract_version")
            .expect("contract version should exist"),
        TMUX_GLUE_CONTRACT_VERSION
    );
    assert_eq!(
        manifest
            .require_value("source_version")
            .expect("source version should exist"),
        "test-1.0"
    );
}

#[test]
fn source_metadata_missing_configure_ac_points_to_submodule_init() {
    let source_path = temp_path("tmux-src-missing-configure");
    fs::create_dir_all(&source_path).expect("source path should be creatable");

    let error = TmuxGlueSourceMetadata::discover(&source_path)
        .expect_err("missing configure.ac should fail");

    assert!(error
        .to_string()
        .contains("git submodule update --init --recursive"));
    assert!(error.to_string().contains("missing or incomplete"));
}

#[test]
fn source_metadata_missing_autogen_points_to_submodule_init() {
    let source_path = temp_path("tmux-src-missing-autogen");
    fs::create_dir_all(&source_path).expect("source path should be creatable");
    fs::write(
        source_path.join("configure.ac"),
        "AC_INIT([tmux], test-1.0)\n",
    )
    .expect("configure.ac should be writable");

    let error =
        TmuxGlueSourceMetadata::discover(&source_path).expect_err("missing autogen.sh should fail");

    assert!(error
        .to_string()
        .contains("git submodule update --init --recursive"));
    assert!(error.to_string().contains("missing or incomplete"));
}

#[test]
fn manifest_writer_renders_all_required_fields() {
    let layout = TmuxGlueLayout::new("/tmp/src", "/tmp/build");
    let metadata = super::TmuxGlueSourceMetadata {
        package_name: "tmux".to_string(),
        version: "3.6a".to_string(),
        configure_ac_path: PathBuf::from("/tmp/src/configure.ac"),
        autogen_script_path: PathBuf::from("/tmp/src/autogen.sh"),
    };
    let rendered = TmuxGlueManifestWriter::render(&layout.artifacts(), &metadata);

    assert!(rendered.contains("contract_version=1"));
    assert!(rendered.contains("source_package_name=tmux"));
    assert!(rendered.contains("source_version=3.6a"));
    assert!(rendered.contains("source_path=/tmp/src"));
    assert!(rendered.contains("tmux_binary_path=/tmp/build/stage/bin/tmux"));
}

#[test]
fn build_config_tracks_layout_binary_path() {
    let layout = TmuxGlueLayout::new("/tmp/src", "/tmp/build");
    let config = TmuxGlueBuildConfig::from_layout(&layout);

    assert_eq!(config.source_path, PathBuf::from("/tmp/src"));
    assert_eq!(config.build_root, PathBuf::from("/tmp/build"));
    assert_eq!(
        config.tmux_binary_path,
        PathBuf::from("/tmp/build/stage/bin/tmux")
    );
}

#[test]
fn build_plan_exposes_autotools_orchestration() {
    let source_path = temp_path("tmux-src-orchestration");
    let build_root = temp_path("tmux-build-orchestration");
    fs::create_dir_all(&source_path).expect("source path should be creatable");
    fs::write(
        source_path.join("configure.ac"),
        "AC_INIT([tmux], next-3.7)\n",
    )
    .expect("configure.ac should be writable");
    fs::write(source_path.join("autogen.sh"), "#!/bin/sh\n").expect("autogen should exist");

    let plan = TmuxGlueBuildPlan::new(&source_path, &build_root);
    let orchestration = plan
        .orchestration()
        .expect("orchestration should be derivable");

    assert_eq!(orchestration.metadata.package_name, "tmux");
    assert_eq!(orchestration.metadata.version, "next-3.7");
    assert_eq!(orchestration.required_tools[0], TmuxGlueTool::Sh);
    assert_eq!(orchestration.steps[0].kind, TmuxGlueBuildStepKind::Autogen);
    assert_eq!(
        orchestration.steps[1].kind,
        TmuxGlueBuildStepKind::Configure
    );
    assert_eq!(orchestration.steps[2].kind, TmuxGlueBuildStepKind::Build);
    assert_eq!(
        orchestration.steps[2].command.program,
        PathBuf::from("make")
    );
    assert_eq!(orchestration.steps[2].command.args, vec!["-j1", "install"]);
}

#[test]
fn toolchain_probe_reports_missing_tools() {
    let source_path = temp_path("tmux-src-toolchain");
    let build_root = temp_path("tmux-build-toolchain");
    let fake_bin = temp_path("tmux-fake-bin");
    fs::create_dir_all(&source_path).expect("source path should be creatable");
    fs::create_dir_all(&fake_bin).expect("fake bin dir should be creatable");
    fs::write(
        source_path.join("configure.ac"),
        "AC_INIT([tmux], next-3.7)\n",
    )
    .expect("configure.ac should be writable");
    fs::write(source_path.join("autogen.sh"), "#!/bin/sh\n").expect("autogen should exist");
    fs::write(fake_bin.join("sh"), "").expect("fake sh should exist");
    fs::write(fake_bin.join("make"), "").expect("fake make should exist");

    let plan = TmuxGlueBuildPlan::new(&source_path, &build_root);
    let orchestration = plan
        .orchestration()
        .expect("orchestration should be derivable");
    let report = orchestration.probe_toolchain(Some(fake_bin.as_os_str()));

    assert!(!report.is_complete());
    assert_eq!(report.available.len(), 2);
    assert!(report.missing.contains(&TmuxGlueTool::Autoconf));
    assert!(report.missing.contains(&TmuxGlueTool::PkgConfig));
}

#[derive(Default)]
struct RecordingExecutor {
    executed_steps: Vec<TmuxGlueBuildStepKind>,
}

impl TmuxGlueStepExecutor for RecordingExecutor {
    fn execute_step(&mut self, step: &TmuxGlueBuildStep) -> Result<(), TmuxGlueExecutionError> {
        self.executed_steps.push(step.kind.clone());
        Ok(())
    }
}

#[test]
fn build_plan_executes_with_recording_executor_and_writes_stamps() {
    let source_path = temp_path("tmux-src-execute");
    let build_root = temp_path("tmux-build-execute");
    fs::create_dir_all(&source_path).expect("source path should be creatable");
    fs::write(
        source_path.join("configure.ac"),
        "AC_INIT([tmux], next-3.7)\n",
    )
    .expect("configure.ac should be writable");
    fs::write(source_path.join("autogen.sh"), "#!/bin/sh\n").expect("autogen should exist");

    let plan = TmuxGlueBuildPlan::new(&source_path, &build_root);
    let mut executor = RecordingExecutor::default();
    let report = plan
        .execute_with(&mut executor)
        .expect("execution should succeed");

    assert_eq!(
        report.completed_steps,
        vec![
            TmuxGlueBuildStepKind::Autogen,
            TmuxGlueBuildStepKind::Configure,
            TmuxGlueBuildStepKind::Build
        ]
    );
    assert!(report.artifacts.configure_stamp_path.exists());
    assert!(report.artifacts.build_stamp_path.exists());
    assert_eq!(executor.executed_steps, report.completed_steps);
}

#[test]
fn build_plan_reports_incomplete_toolchain_before_execution() {
    let source_path = temp_path("tmux-src-incomplete");
    let build_root = temp_path("tmux-build-incomplete");
    let fake_bin = temp_path("tmux-fake-bin-incomplete");
    fs::create_dir_all(&source_path).expect("source path should be creatable");
    fs::create_dir_all(&fake_bin).expect("fake bin dir should be creatable");
    fs::write(
        source_path.join("configure.ac"),
        "AC_INIT([tmux], next-3.7)\n",
    )
    .expect("configure.ac should be writable");
    fs::write(source_path.join("autogen.sh"), "#!/bin/sh\n").expect("autogen should exist");
    fs::write(fake_bin.join("sh"), "").expect("fake sh should exist");
    fs::write(fake_bin.join("make"), "").expect("fake make should exist");

    let plan = TmuxGlueBuildPlan::new(&source_path, &build_root);
    let mut executor = ProcessTmuxGlueExecutor::with_path_env(fake_bin.as_os_str());
    let error = plan
        .execute_with(&mut executor)
        .expect_err("execution should fail on missing toolchain");

    match error {
        TmuxGlueBuildError::ToolchainIncomplete(report) => {
            assert!(report.missing.contains(&TmuxGlueTool::Autoconf));
            assert!(report.missing.contains(&TmuxGlueTool::PkgConfig));
        }
        other => panic!("unexpected error: {other}"),
    }
}
