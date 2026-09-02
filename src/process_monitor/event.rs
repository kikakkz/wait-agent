/// Normalized process lifecycle events emitted by platform-specific sources.
///
/// The Linux netlink connector, Windows Toolhelp32 snapshot diffs, and macOS
/// libproc sources all translate their native events into this enum so the
/// rest of waitagent can reason about sessions in a platform-agnostic way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProcessEvent {
    /// A new process was created.
    Fork {
        /// PID of the parent process.
        parent_pid: u32,
        /// PID of the newly created child process.
        child_pid: u32,
    },
    /// An existing process replaced its image via execve.
    Exec {
        /// PID of the process that executed a new image.
        pid: u32,
        /// argv[0] as reported by the kernel.
        argv0: String,
        /// Full argument vector, when available.
        argv: Vec<String>,
    },
    /// A process terminated.
    Exit {
        /// PID of the process that exited.
        pid: u32,
        /// Exit code, if available.
        exit_code: Option<i32>,
    },
}
