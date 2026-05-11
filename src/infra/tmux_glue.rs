use crate::infra::tmux::TmuxError;
use crate::infra::tmux_glue_contract::TmuxGlueContractError;
use std::path::{Path, PathBuf};

/// The vendored tmux binary, embedded at compile time.
/// Extracted to a runtime directory on first use.
pub const VENDORED_TMUX_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/vendored_tmux.bin"));

#[allow(unused_imports)]
pub use crate::infra::tmux_glue_contract::{
    ProcessTmuxGlueExecutor, TmuxGlueArtifacts, TmuxGlueBuildConfig, TmuxGlueBuildError,
    TmuxGlueBuildPlan, TmuxGlueBuildStatus, TmuxGlueBuildStep, TmuxGlueBuildStepKind,
    TmuxGlueCommand, TmuxGlueExecutionError, TmuxGlueExecutionFailure, TmuxGlueExecutionReport,
    TmuxGlueLayout, TmuxGlueManifest, TmuxGlueManifestWriter, TmuxGlueOrchestrationPlan,
    TmuxGlueResolvedTool, TmuxGlueSourceMetadata, TmuxGlueStepExecutor, TmuxGlueTool,
    TmuxGlueToolchainReport, TMUX_GLUE_BUILD_DIR_NAME, TMUX_GLUE_CONTRACT_VERSION,
    TMUX_GLUE_MANIFEST_FILE_NAME, VENDORED_TMUX_BIN_ENV, VENDORED_TMUX_BUILD_ROOT_ENV,
    VENDORED_TMUX_BUILD_STATUS_ENV, VENDORED_TMUX_MANIFEST_ENV, VENDORED_TMUX_SOURCE_ENV,
    VENDORED_TMUX_SUBMODULE_PATH, VENDORED_TMUX_VERSION_ENV,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendoredTmuxSource {
    path: PathBuf,
}

/// Extract the embedded vendored tmux binary to a user-local data directory.
/// Returns the path to the extracted binary on success.
pub fn extract_embedded_tmux() -> Result<PathBuf, TmuxError> {
    let data_dir = data_local_dir().join("waitagent");
    let tmux_path = data_dir.join("tmux");
    let version_path = data_dir.join("tmux.version");
    let tmux_version = option_env!("WAITAGENT_VENDORED_TMUX_VERSION").unwrap_or("unknown");

    // Re-extract if missing, or if the embedded tmux version has changed
    // (e.g. after a waitagent upgrade via a new package).
    let needs_extract = !tmux_path.exists()
        || std::fs::read_to_string(&version_path)
            .map(|v| v != tmux_version)
            .unwrap_or(true);

    if needs_extract {
        std::fs::create_dir_all(&data_dir).map_err(|e| {
            TmuxError::new(format!(
                "failed to create waitagent data directory at {}: {e}",
                data_dir.display()
            ))
        })?;
        std::fs::write(&tmux_path, VENDORED_TMUX_BYTES).map_err(|e| {
            TmuxError::new(format!(
                "failed to write vendored tmux binary to {}: {e}",
                tmux_path.display()
            ))
        })?;
        #[cfg(unix)]
        std::fs::set_permissions(
            &tmux_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .map_err(|e| {
            TmuxError::new(format!(
                "failed to set permissions on vendored tmux binary at {}: {e}",
                tmux_path.display()
            ))
        })?;
        // Store the version so future runs can detect stale binaries
        // after a waitagent upgrade.
        let _ = std::fs::write(&version_path, tmux_version);
    }
    Ok(tmux_path)
}

fn data_local_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local").join("share");
    }
    // Last-resort fallback — should be writable on most systems
    PathBuf::from("/tmp")
}

impl VendoredTmuxSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[allow(dead_code)]
    pub fn discover_from_repo_root(repo_root: impl AsRef<Path>) -> Result<Self, TmuxError> {
        let path = repo_root.as_ref().join(VENDORED_TMUX_SUBMODULE_PATH);
        if !path.exists() {
            return Err(TmuxError::new(format!(
                "vendored tmux source is missing at {}",
                path.display()
            )));
        }
        Ok(Self { path })
    }

    pub fn discover_from_build_env() -> Result<Self, TmuxError> {
        let Some(path) = option_env!("WAITAGENT_VENDORED_TMUX_SOURCE_PATH") else {
            return Err(TmuxError::new(format!(
                "vendored tmux build env `{VENDORED_TMUX_SOURCE_ENV}` is missing"
            )));
        };
        Ok(Self {
            path: PathBuf::from(path),
        })
    }

    pub fn system_default() -> Self {
        Self {
            path: PathBuf::from("tmux"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl VendoredTmuxSource {
    #[allow(dead_code)]
    pub fn build_plan(&self, build_root: impl Into<PathBuf>) -> TmuxGlueBuildPlan {
        TmuxGlueBuildPlan::new(self.path.clone(), build_root.into())
    }

    #[allow(dead_code)]
    pub fn metadata(&self) -> Result<TmuxGlueSourceMetadata, TmuxError> {
        TmuxGlueSourceMetadata::discover(self.path.clone()).map_err(TmuxError::from)
    }
}

impl From<TmuxGlueContractError> for TmuxError {
    fn from(value: TmuxGlueContractError) -> Self {
        TmuxError::new(value.to_string())
    }
}

impl TmuxGlueArtifacts {
    pub fn from_manifest(manifest: &TmuxGlueManifest) -> Result<Self, TmuxError> {
        let contract_version = manifest
            .require_value("contract_version")
            .map_err(TmuxError::from)?;
        if contract_version != TMUX_GLUE_CONTRACT_VERSION {
            return Err(TmuxError::new(format!(
                "unsupported tmux glue contract version `{contract_version}`"
            )));
        }
        Ok(Self {
            source_path: manifest
                .require_path("source_path")
                .map_err(TmuxError::from)?,
            build_root: manifest
                .require_path("build_root")
                .map_err(TmuxError::from)?,
            tmux_binary_path: manifest
                .require_path("tmux_binary_path")
                .map_err(TmuxError::from)?,
            static_lib_path: manifest
                .require_path("static_lib_path")
                .map_err(TmuxError::from)?,
            include_dir_path: manifest
                .require_path("include_dir_path")
                .map_err(TmuxError::from)?,
            configure_stamp_path: manifest
                .require_path("configure_stamp_path")
                .map_err(TmuxError::from)?,
            build_stamp_path: manifest
                .require_path("build_stamp_path")
                .map_err(TmuxError::from)?,
        })
    }

    pub fn from_build_env() -> Result<Self, TmuxError> {
        let manifest = TmuxGlueManifest::from_build_env()?;
        Self::from_manifest(&manifest)
    }

    pub fn system_default() -> Self {
        Self {
            source_path: PathBuf::new(),
            build_root: PathBuf::new(),
            tmux_binary_path: PathBuf::from("tmux"),
            static_lib_path: PathBuf::new(),
            include_dir_path: PathBuf::new(),
            configure_stamp_path: PathBuf::new(),
            build_stamp_path: PathBuf::new(),
        }
    }
}

impl TmuxGlueManifest {
    pub fn from_build_env() -> Result<Self, TmuxError> {
        let Some(path) = option_env!("WAITAGENT_VENDORED_TMUX_MANIFEST_PATH") else {
            return Err(TmuxError::new(format!(
                "vendored tmux build env `{VENDORED_TMUX_MANIFEST_ENV}` is missing"
            )));
        };
        Self::from_path(path).map_err(TmuxError::from)
    }
}

impl TmuxGlueBuildStatus {
    pub fn from_build_env() -> Result<Self, TmuxError> {
        let Some(value) = option_env!("WAITAGENT_VENDORED_TMUX_BUILD_STATUS") else {
            return Err(TmuxError::new(format!(
                "vendored tmux build env `{VENDORED_TMUX_BUILD_STATUS_ENV}` is missing"
            )));
        };
        Self::parse(value).map_err(TmuxError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        TmuxGlueArtifacts, TmuxGlueBuildConfig, TmuxGlueBuildStatus, TmuxGlueManifest,
        VendoredTmuxSource, VENDORED_TMUX_BIN_ENV, VENDORED_TMUX_BUILD_ROOT_ENV,
        VENDORED_TMUX_MANIFEST_ENV, VENDORED_TMUX_SOURCE_ENV, VENDORED_TMUX_SUBMODULE_PATH,
    };
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn vendored_source_discovers_submodule_path() {
        let source = VendoredTmuxSource::discover_from_repo_root(".")
            .expect("vendored tmux source should exist");
        assert!(source.path().ends_with(VENDORED_TMUX_SUBMODULE_PATH));
    }

    #[test]
    fn glue_build_config_uses_source_path_and_build_root() {
        let source = VendoredTmuxSource::discover_from_repo_root(".")
            .expect("vendored tmux source should exist");
        let config =
            TmuxGlueBuildConfig::from_layout(source.build_plan("target/tmux-glue").layout());

        assert_eq!(config.source_path, source.path().to_path_buf());
        assert_eq!(config.build_root, PathBuf::from("target/tmux-glue"));
        assert_eq!(
            config.tmux_binary_path,
            PathBuf::from("target/tmux-glue")
                .join("stage")
                .join("bin")
                .join("tmux")
        );
    }

    #[test]
    fn vendored_source_reads_configure_metadata() {
        let source = VendoredTmuxSource::discover_from_repo_root(".")
            .expect("vendored tmux source should exist");
        let metadata = source.metadata().expect("metadata should parse");

        assert_eq!(metadata.package_name, "tmux");
        assert!(metadata.version.contains("3."));
    }

    #[test]
    fn glue_build_status_is_available_from_build_env() {
        let status =
            TmuxGlueBuildStatus::from_build_env().expect("vendored tmux build status should exist");

        assert_eq!(status, TmuxGlueBuildStatus::Executed);
        assert_eq!(status.as_str(), "executed");
    }

    #[test]
    fn glue_artifacts_discover_paths_from_build_env() {
        let artifacts =
            TmuxGlueArtifacts::from_build_env().expect("vendored tmux build env should exist");

        assert!(artifacts
            .source_path
            .to_string_lossy()
            .contains(VENDORED_TMUX_SUBMODULE_PATH));
        assert!(artifacts
            .build_root
            .to_string_lossy()
            .contains("vendored-tmux-glue"));
        assert!(artifacts
            .tmux_binary_path
            .to_string_lossy()
            .ends_with("/stage/bin/tmux"));
        assert!(artifacts
            .static_lib_path
            .to_string_lossy()
            .ends_with("/lib/libtmux-glue.a"));
        assert!(artifacts
            .include_dir_path
            .to_string_lossy()
            .ends_with("/include"));
        assert_ne!(VENDORED_TMUX_SOURCE_ENV, VENDORED_TMUX_BUILD_ROOT_ENV);
        assert_ne!(VENDORED_TMUX_BUILD_ROOT_ENV, VENDORED_TMUX_BIN_ENV);
        assert_ne!(VENDORED_TMUX_BIN_ENV, VENDORED_TMUX_MANIFEST_ENV);
    }

    #[test]
    fn glue_manifest_parses_key_value_file() {
        let temp_dir = std::env::temp_dir().join("waitagent-tmux-glue-test");
        let _ = fs::create_dir_all(&temp_dir);
        let manifest_path = temp_dir.join("tmux-glue-manifest.env");
        fs::write(
            &manifest_path,
            "contract_version=1\nsource_path=/tmp/src\nbuild_root=/tmp/build\ntmux_binary_path=/tmp/build/stage/bin/tmux\nstatic_lib_path=/tmp/build/lib/libtmux-glue.a\ninclude_dir_path=/tmp/build/stage/include\nconfigure_stamp_path=/tmp/build/configure.stamp\nbuild_stamp_path=/tmp/build/build.stamp\n",
        )
        .expect("manifest write should succeed");

        let manifest =
            TmuxGlueManifest::from_path(&manifest_path).expect("manifest parse should succeed");
        let artifacts =
            TmuxGlueArtifacts::from_manifest(&manifest).expect("artifacts should parse");

        assert_eq!(manifest.path(), manifest_path.as_path());
        assert_eq!(artifacts.source_path, PathBuf::from("/tmp/src"));
        assert_eq!(
            artifacts.static_lib_path,
            PathBuf::from("/tmp/build/lib/libtmux-glue.a")
        );
    }
}
