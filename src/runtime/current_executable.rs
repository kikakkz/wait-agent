use crate::lifecycle::LifecycleError;
use std::path::PathBuf;

pub(crate) fn current_waitagent_executable() -> Result<PathBuf, LifecycleError> {
    let current_exe = std::env::current_exe().map_err(|error| {
        LifecycleError::Io(
            "failed to locate current waitagent executable".to_string(),
            error,
        )
    })?;

    #[cfg(test)]
    {
        if current_exe
            .parent()
            .and_then(|parent| parent.file_name())
            .is_some_and(|name| name == "deps")
        {
            let candidate = current_exe
                .parent()
                .and_then(|parent| parent.parent())
                .map(|parent| parent.join(format!("waitagent{}", std::env::consts::EXE_SUFFIX)));
            if let Some(candidate) = candidate.filter(|candidate| candidate.exists()) {
                return Ok(candidate);
            }
        }
    }

    Ok(current_exe)
}

#[cfg(test)]
pub(crate) fn waitagent_test_executable() -> PathBuf {
    current_waitagent_executable().expect("waitagent test executable should resolve")
}
