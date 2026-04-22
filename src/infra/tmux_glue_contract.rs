#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const VENDORED_TMUX_SUBMODULE_PATH: &str = "third_party/tmux";
pub const VENDORED_TMUX_SOURCE_ENV: &str = "WAITAGENT_VENDORED_TMUX_SOURCE_PATH";
pub const VENDORED_TMUX_BUILD_ROOT_ENV: &str = "WAITAGENT_VENDORED_TMUX_BUILD_ROOT";
pub const VENDORED_TMUX_BIN_ENV: &str = "WAITAGENT_VENDORED_TMUX_BIN_PATH";
pub const VENDORED_TMUX_MANIFEST_ENV: &str = "WAITAGENT_VENDORED_TMUX_MANIFEST_PATH";
pub const VENDORED_TMUX_VERSION_ENV: &str = "WAITAGENT_VENDORED_TMUX_VERSION";
pub const VENDORED_TMUX_BUILD_STATUS_ENV: &str = "WAITAGENT_VENDORED_TMUX_BUILD_STATUS";
pub const TMUX_GLUE_BUILD_DIR_NAME: &str = "vendored-tmux-glue";
pub const TMUX_GLUE_MANIFEST_FILE_NAME: &str = "tmux-glue-manifest.env";
pub const TMUX_GLUE_CONTRACT_VERSION: &str = "1";
pub const TMUX_CONFIGURE_AC_FILE_NAME: &str = "configure.ac";
pub const TMUX_AUTOGEN_SCRIPT_FILE_NAME: &str = "autogen.sh";
const YACC_CANDIDATE_PROGRAM_NAMES: &[&str] = &["yacc", "bison", "byacc"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueContractError {
    message: String,
}

impl TmuxGlueContractError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for TmuxGlueContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for TmuxGlueContractError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueLayout {
    pub source_path: PathBuf,
    pub build_root: PathBuf,
}

impl TmuxGlueLayout {
    pub fn new(source_path: impl Into<PathBuf>, build_root: impl Into<PathBuf>) -> Self {
        Self {
            source_path: source_path.into(),
            build_root: build_root.into(),
        }
    }

    pub fn bin_dir_path(&self) -> PathBuf {
        self.stage_dir_path().join("bin")
    }

    pub fn lib_dir_path(&self) -> PathBuf {
        self.build_root.join("lib")
    }

    pub fn include_dir_path(&self) -> PathBuf {
        self.stage_dir_path().join("include")
    }

    pub fn tmux_binary_path(&self) -> PathBuf {
        self.bin_dir_path().join("tmux")
    }

    pub fn static_lib_path(&self) -> PathBuf {
        self.lib_dir_path().join("libtmux-glue.a")
    }

    pub fn configure_stamp_path(&self) -> PathBuf {
        self.build_root.join("configure.stamp")
    }

    pub fn build_stamp_path(&self) -> PathBuf {
        self.build_root.join("build.stamp")
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.build_root.join(TMUX_GLUE_MANIFEST_FILE_NAME)
    }

    pub fn stage_dir_path(&self) -> PathBuf {
        self.build_root.join("stage")
    }

    pub fn artifacts(&self) -> TmuxGlueArtifacts {
        TmuxGlueArtifacts {
            source_path: self.source_path.clone(),
            build_root: self.build_root.clone(),
            tmux_binary_path: self.tmux_binary_path(),
            static_lib_path: self.static_lib_path(),
            include_dir_path: self.include_dir_path(),
            configure_stamp_path: self.configure_stamp_path(),
            build_stamp_path: self.build_stamp_path(),
        }
    }

    pub fn ensure_directories(&self) -> Result<(), TmuxGlueContractError> {
        fs::create_dir_all(self.bin_dir_path()).map_err(|error| {
            TmuxGlueContractError::new(format!(
                "failed to create tmux glue bin dir `{}`: {error}",
                self.bin_dir_path().display()
            ))
        })?;
        fs::create_dir_all(self.lib_dir_path()).map_err(|error| {
            TmuxGlueContractError::new(format!(
                "failed to create tmux glue lib dir `{}`: {error}",
                self.lib_dir_path().display()
            ))
        })?;
        fs::create_dir_all(self.include_dir_path()).map_err(|error| {
            TmuxGlueContractError::new(format!(
                "failed to create tmux glue include dir `{}`: {error}",
                self.include_dir_path().display()
            ))
        })?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueArtifacts {
    pub source_path: PathBuf,
    pub build_root: PathBuf,
    pub tmux_binary_path: PathBuf,
    pub static_lib_path: PathBuf,
    pub include_dir_path: PathBuf,
    pub configure_stamp_path: PathBuf,
    pub build_stamp_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueSourceMetadata {
    pub package_name: String,
    pub version: String,
    pub configure_ac_path: PathBuf,
    pub autogen_script_path: PathBuf,
}

impl TmuxGlueSourceMetadata {
    pub fn discover(source_path: impl Into<PathBuf>) -> Result<Self, TmuxGlueContractError> {
        let source_path = source_path.into();
        let configure_ac_path = source_path.join(TMUX_CONFIGURE_AC_FILE_NAME);
        let autogen_script_path = source_path.join(TMUX_AUTOGEN_SCRIPT_FILE_NAME);
        let configure_contents = fs::read_to_string(&configure_ac_path).map_err(|error| {
            TmuxGlueContractError::new(format!(
                "failed to read vendored tmux configure input `{}`: {error}",
                configure_ac_path.display()
            ))
        })?;
        let Some((package_name, version)) = parse_ac_init(&configure_contents) else {
            return Err(TmuxGlueContractError::new(format!(
                "failed to parse AC_INIT package metadata from `{}`",
                configure_ac_path.display()
            )));
        };
        if !autogen_script_path.exists() {
            return Err(TmuxGlueContractError::new(format!(
                "vendored tmux autogen script is missing at `{}`",
                autogen_script_path.display()
            )));
        }
        Ok(Self {
            package_name,
            version,
            configure_ac_path,
            autogen_script_path,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueBuildConfig {
    pub source_path: PathBuf,
    pub build_root: PathBuf,
    pub tmux_binary_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxGlueBuildStatus {
    Prepared,
    Executed,
}

impl TmuxGlueBuildStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Executed => "executed",
        }
    }

    pub fn parse(value: &str) -> Result<Self, TmuxGlueContractError> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "executed" => Ok(Self::Executed),
            other => Err(TmuxGlueContractError::new(format!(
                "unsupported tmux glue build status `{other}`"
            ))),
        }
    }
}

impl TmuxGlueBuildConfig {
    pub fn from_layout(layout: &TmuxGlueLayout) -> Self {
        Self {
            source_path: layout.source_path.clone(),
            build_root: layout.build_root.clone(),
            tmux_binary_path: layout.tmux_binary_path(),
        }
    }

    pub fn from_artifacts(artifacts: &TmuxGlueArtifacts) -> Self {
        Self {
            source_path: artifacts.source_path.clone(),
            build_root: artifacts.build_root.clone(),
            tmux_binary_path: artifacts.tmux_binary_path.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueBuildPlan {
    layout: TmuxGlueLayout,
}

impl TmuxGlueBuildPlan {
    pub fn new(source_path: impl Into<PathBuf>, build_root: impl Into<PathBuf>) -> Self {
        Self {
            layout: TmuxGlueLayout::new(source_path, build_root),
        }
    }

    pub fn layout(&self) -> &TmuxGlueLayout {
        &self.layout
    }

    pub fn validate_source(&self) -> Result<(), TmuxGlueContractError> {
        if !self.layout.source_path.exists() {
            return Err(TmuxGlueContractError::new(format!(
                "vendored tmux source is missing at {}",
                self.layout.source_path.display()
            )));
        }
        TmuxGlueSourceMetadata::discover(self.layout.source_path.clone())?;
        Ok(())
    }

    pub fn artifacts(&self) -> TmuxGlueArtifacts {
        self.layout.artifacts()
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.layout.manifest_path()
    }

    pub fn source_metadata(&self) -> Result<TmuxGlueSourceMetadata, TmuxGlueContractError> {
        TmuxGlueSourceMetadata::discover(self.layout.source_path.clone())
    }

    pub fn orchestration(&self) -> Result<TmuxGlueOrchestrationPlan, TmuxGlueContractError> {
        Ok(TmuxGlueOrchestrationPlan {
            metadata: self.source_metadata()?,
            required_tools: vec![
                TmuxGlueTool::Sh,
                TmuxGlueTool::Autoconf,
                TmuxGlueTool::Automake,
                TmuxGlueTool::PkgConfig,
                TmuxGlueTool::Cc,
                TmuxGlueTool::Make,
                TmuxGlueTool::Yacc,
            ],
            steps: vec![
                TmuxGlueBuildStep {
                    kind: TmuxGlueBuildStepKind::Autogen,
                    command: TmuxGlueCommand::new(
                        "sh",
                        vec![self
                            .layout
                            .source_path
                            .join(TMUX_AUTOGEN_SCRIPT_FILE_NAME)
                            .display()
                            .to_string()],
                        self.layout.source_path.clone(),
                    ),
                },
                TmuxGlueBuildStep {
                    kind: TmuxGlueBuildStepKind::Configure,
                    command: TmuxGlueCommand::new(
                        self.layout.source_path.join("configure"),
                        vec![
                            format!("--prefix={}", self.layout.stage_dir_path().display()),
                            "--enable-static".to_string(),
                        ],
                        self.layout.build_root.clone(),
                    ),
                },
                TmuxGlueBuildStep {
                    kind: TmuxGlueBuildStepKind::Build,
                    command: TmuxGlueCommand::new(
                        "make",
                        vec!["-j1".to_string(), "install".to_string()],
                        self.layout.build_root.clone(),
                    ),
                },
            ],
        })
    }

    pub fn prepare(&self) -> Result<TmuxGlueArtifacts, TmuxGlueContractError> {
        self.validate_source()?;
        self.layout.ensure_directories()?;
        let artifacts = self.artifacts();
        let metadata = self.source_metadata()?;
        TmuxGlueManifestWriter::write(self.manifest_path(), &artifacts, &metadata)?;
        Ok(artifacts)
    }

    pub fn execute_with<E>(
        &self,
        executor: &mut E,
    ) -> Result<TmuxGlueExecutionReport, TmuxGlueBuildError>
    where
        E: TmuxGlueStepExecutor,
    {
        let artifacts = self.prepare().map_err(TmuxGlueBuildError::from)?;
        let orchestration = self.orchestration().map_err(TmuxGlueBuildError::from)?;

        if let Some(toolchain_report) = executor.toolchain_report(&orchestration) {
            if !toolchain_report.is_complete() {
                return Err(TmuxGlueBuildError::ToolchainIncomplete(toolchain_report));
            }
        }

        let mut completed_steps = Vec::new();
        for step in &orchestration.steps {
            executor
                .execute_step(step)
                .map_err(TmuxGlueBuildError::Execution)?;
            self.write_step_stamp(step, &artifacts)?;
            completed_steps.push(step.kind.clone());
        }

        Ok(TmuxGlueExecutionReport {
            metadata: orchestration.metadata,
            artifacts,
            completed_steps,
        })
    }

    fn write_step_stamp(
        &self,
        step: &TmuxGlueBuildStep,
        artifacts: &TmuxGlueArtifacts,
    ) -> Result<(), TmuxGlueBuildError> {
        let stamp_path = match step.kind {
            TmuxGlueBuildStepKind::Autogen => return Ok(()),
            TmuxGlueBuildStepKind::Configure => artifacts.configure_stamp_path.clone(),
            TmuxGlueBuildStepKind::Build => artifacts.build_stamp_path.clone(),
        };
        fs::write(&stamp_path, step.command.render()).map_err(|error| {
            TmuxGlueBuildError::StampWrite {
                step_kind: step.kind.clone(),
                stamp_path,
                message: error.to_string(),
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueExecutionReport {
    pub metadata: TmuxGlueSourceMetadata,
    pub artifacts: TmuxGlueArtifacts,
    pub completed_steps: Vec<TmuxGlueBuildStepKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxGlueBuildError {
    Contract(TmuxGlueContractError),
    ToolchainIncomplete(TmuxGlueToolchainReport),
    Execution(TmuxGlueExecutionError),
    StampWrite {
        step_kind: TmuxGlueBuildStepKind,
        stamp_path: PathBuf,
        message: String,
    },
}

impl From<TmuxGlueContractError> for TmuxGlueBuildError {
    fn from(value: TmuxGlueContractError) -> Self {
        Self::Contract(value)
    }
}

impl fmt::Display for TmuxGlueBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contract(error) => write!(f, "{error}"),
            Self::ToolchainIncomplete(report) => {
                write!(f, "tmux glue toolchain is incomplete; missing tools: ")?;
                for (index, tool) in report.missing.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", tool.program_name())?;
                }
                Ok(())
            }
            Self::Execution(error) => write!(f, "{error}"),
            Self::StampWrite {
                step_kind,
                stamp_path,
                message,
            } => write!(
                f,
                "failed to write tmux glue {:?} stamp `{}`: {}",
                step_kind,
                stamp_path.display(),
                message
            ),
        }
    }
}

impl std::error::Error for TmuxGlueBuildError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxGlueTool {
    Sh,
    Autoconf,
    Automake,
    PkgConfig,
    Cc,
    Make,
    Yacc,
}

impl TmuxGlueTool {
    pub fn program_name(&self) -> &'static str {
        match self {
            Self::Sh => "sh",
            Self::Autoconf => "autoconf",
            Self::Automake => "automake",
            Self::PkgConfig => "pkg-config",
            Self::Cc => "cc",
            Self::Make => "make",
            Self::Yacc => "yacc",
        }
    }

    pub fn candidate_program_names(&self) -> &'static [&'static str] {
        match self {
            Self::Sh => &["sh"],
            Self::Autoconf => &["autoconf"],
            Self::Automake => &["automake"],
            Self::PkgConfig => &["pkg-config"],
            Self::Cc => &["cc"],
            Self::Make => &["make"],
            Self::Yacc => YACC_CANDIDATE_PROGRAM_NAMES,
        }
    }

    pub fn resolve_on_path(&self, path_env: Option<&OsStr>) -> Option<PathBuf> {
        let path_env = path_env?;
        for entry in std::env::split_paths(path_env) {
            for program_name in self.candidate_program_names() {
                let candidate = entry.join(program_name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxGlueBuildStepKind {
    Autogen,
    Configure,
    Build,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

impl TmuxGlueCommand {
    pub fn new(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<String>>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
        }
    }

    pub fn render(&self) -> String {
        let mut rendered = self.program.display().to_string();
        for arg in &self.args {
            rendered.push(' ');
            rendered.push_str(arg);
        }
        rendered
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueBuildStep {
    pub kind: TmuxGlueBuildStepKind,
    pub command: TmuxGlueCommand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueOrchestrationPlan {
    pub metadata: TmuxGlueSourceMetadata,
    pub required_tools: Vec<TmuxGlueTool>,
    pub steps: Vec<TmuxGlueBuildStep>,
}

impl TmuxGlueOrchestrationPlan {
    pub fn probe_toolchain(&self, path_env: Option<&OsStr>) -> TmuxGlueToolchainReport {
        let mut available = Vec::new();
        let mut missing = Vec::new();

        for tool in &self.required_tools {
            if let Some(program_path) = tool.resolve_on_path(path_env) {
                available.push(TmuxGlueResolvedTool {
                    tool: tool.clone(),
                    program_path,
                });
            } else {
                missing.push(tool.clone());
            }
        }

        TmuxGlueToolchainReport { available, missing }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueResolvedTool {
    pub tool: TmuxGlueTool,
    pub program_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueToolchainReport {
    pub available: Vec<TmuxGlueResolvedTool>,
    pub missing: Vec<TmuxGlueTool>,
}

impl TmuxGlueToolchainReport {
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxGlueExecutionFailure {
    MissingProgram { program: PathBuf },
    SpawnFailed { message: String },
    NonZeroExit { code: Option<i32> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueExecutionError {
    pub step_kind: TmuxGlueBuildStepKind,
    pub command: TmuxGlueCommand,
    pub failure: TmuxGlueExecutionFailure,
}

impl fmt::Display for TmuxGlueExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.failure {
            TmuxGlueExecutionFailure::MissingProgram { program } => write!(
                f,
                "tmux glue {:?} step could not resolve program `{}`",
                self.step_kind,
                program.display()
            ),
            TmuxGlueExecutionFailure::SpawnFailed { message } => write!(
                f,
                "tmux glue {:?} step failed to spawn `{}`: {}",
                self.step_kind,
                self.command.render(),
                message
            ),
            TmuxGlueExecutionFailure::NonZeroExit { code } => write!(
                f,
                "tmux glue {:?} step exited unsuccessfully with code {:?}: {}",
                self.step_kind,
                code,
                self.command.render()
            ),
        }
    }
}

impl std::error::Error for TmuxGlueExecutionError {}

pub trait TmuxGlueStepExecutor {
    fn toolchain_report(
        &self,
        _orchestration: &TmuxGlueOrchestrationPlan,
    ) -> Option<TmuxGlueToolchainReport> {
        None
    }

    fn execute_step(&mut self, step: &TmuxGlueBuildStep) -> Result<(), TmuxGlueExecutionError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessTmuxGlueExecutor {
    path_env: Option<std::ffi::OsString>,
}

impl ProcessTmuxGlueExecutor {
    pub fn from_current_env() -> Self {
        Self {
            path_env: std::env::var_os("PATH"),
        }
    }

    pub fn with_path_env(path_env: impl Into<std::ffi::OsString>) -> Self {
        Self {
            path_env: Some(path_env.into()),
        }
    }

    fn resolve_program(&self, program: &Path) -> Option<PathBuf> {
        if program.components().count() > 1 || program.is_absolute() {
            return program.is_file().then(|| program.to_path_buf());
        }
        std::env::split_paths(self.path_env.as_deref()?)
            .map(|entry| entry.join(program))
            .find(|candidate| candidate.is_file())
    }
}

impl TmuxGlueStepExecutor for ProcessTmuxGlueExecutor {
    fn toolchain_report(
        &self,
        orchestration: &TmuxGlueOrchestrationPlan,
    ) -> Option<TmuxGlueToolchainReport> {
        Some(orchestration.probe_toolchain(self.path_env.as_deref()))
    }

    fn execute_step(&mut self, step: &TmuxGlueBuildStep) -> Result<(), TmuxGlueExecutionError> {
        let resolved_program =
            self.resolve_program(&step.command.program)
                .ok_or_else(|| TmuxGlueExecutionError {
                    step_kind: step.kind.clone(),
                    command: step.command.clone(),
                    failure: TmuxGlueExecutionFailure::MissingProgram {
                        program: step.command.program.clone(),
                    },
                })?;

        let status = Command::new(&resolved_program)
            .args(&step.command.args)
            .current_dir(&step.command.cwd)
            .status()
            .map_err(|error| TmuxGlueExecutionError {
                step_kind: step.kind.clone(),
                command: step.command.clone(),
                failure: TmuxGlueExecutionFailure::SpawnFailed {
                    message: error.to_string(),
                },
            })?;

        if status.success() {
            Ok(())
        } else {
            Err(TmuxGlueExecutionError {
                step_kind: step.kind.clone(),
                command: step.command.clone(),
                failure: TmuxGlueExecutionFailure::NonZeroExit {
                    code: status.code(),
                },
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxGlueManifest {
    path: PathBuf,
    entries: HashMap<String, String>,
}

impl TmuxGlueManifest {
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, TmuxGlueContractError> {
        let path = path.into();
        let contents = fs::read_to_string(&path).map_err(|error| {
            TmuxGlueContractError::new(format!(
                "failed to read tmux glue manifest `{}`: {error}",
                path.display()
            ))
        })?;
        let mut entries = HashMap::new();
        for line in contents.lines().filter(|line| !line.trim().is_empty()) {
            let Some((key, value)) = line.split_once('=') else {
                return Err(TmuxGlueContractError::new(format!(
                    "invalid tmux glue manifest line `{line}`"
                )));
            };
            entries.insert(key.to_string(), value.to_string());
        }
        Ok(Self { path, entries })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn require_value(&self, key: &str) -> Result<&str, TmuxGlueContractError> {
        self.entries.get(key).map(String::as_str).ok_or_else(|| {
            TmuxGlueContractError::new(format!("tmux glue manifest missing key `{key}`"))
        })
    }

    pub fn require_path(&self, key: &str) -> Result<PathBuf, TmuxGlueContractError> {
        self.require_value(key).map(PathBuf::from)
    }
}

pub struct TmuxGlueManifestWriter;

impl TmuxGlueManifestWriter {
    pub fn render(artifacts: &TmuxGlueArtifacts, metadata: &TmuxGlueSourceMetadata) -> String {
        format!(
            concat!(
                "contract_version={}\n",
                "source_package_name={}\n",
                "source_version={}\n",
                "source_path={}\n",
                "build_root={}\n",
                "tmux_binary_path={}\n",
                "static_lib_path={}\n",
                "include_dir_path={}\n",
                "configure_stamp_path={}\n",
                "build_stamp_path={}\n"
            ),
            TMUX_GLUE_CONTRACT_VERSION,
            metadata.package_name,
            metadata.version,
            artifacts.source_path.display(),
            artifacts.build_root.display(),
            artifacts.tmux_binary_path.display(),
            artifacts.static_lib_path.display(),
            artifacts.include_dir_path.display(),
            artifacts.configure_stamp_path.display(),
            artifacts.build_stamp_path.display(),
        )
    }

    pub fn write(
        manifest_path: impl Into<PathBuf>,
        artifacts: &TmuxGlueArtifacts,
        metadata: &TmuxGlueSourceMetadata,
    ) -> Result<(), TmuxGlueContractError> {
        let manifest_path = manifest_path.into();
        fs::write(&manifest_path, Self::render(artifacts, metadata)).map_err(|error| {
            TmuxGlueContractError::new(format!(
                "failed to write tmux glue manifest `{}`: {error}",
                manifest_path.display()
            ))
        })
    }
}

fn parse_ac_init(contents: &str) -> Option<(String, String)> {
    for line in contents.lines() {
        let line = line.trim();
        if !line.starts_with("AC_INIT([") {
            continue;
        }
        let package_part = line.strip_prefix("AC_INIT([")?;
        let (package_name, remainder) = package_part.split_once("],")?;
        let remainder = remainder.trim_start();
        let version = if let Some(remainder) = remainder.strip_prefix('[') {
            let (version, _) = remainder.split_once(']')?;
            version
        } else {
            remainder
                .split([',', ')'])
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())?
        };
        return Some((package_name.to_string(), version.to_string()));
    }
    None
}

#[cfg(test)]
#[path = "tmux_glue_contract_tests.rs"]
mod tests;
