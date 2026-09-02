//! Windows process monitoring via Toolhelp32 snapshot diffing.
//!
//! Windows has no proc connector equivalent, so the event source periodically
//! takes a `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS)` snapshot and diffs it
//! against the previous one: new pids become [`ProcessEvent::Fork`], pids whose
//! executable path changed become [`ProcessEvent::Exec`], and vanished pids
//! become [`ProcessEvent::Exit`]. A full snapshot is typically a few hundred
//! entries, so diffing every 500 ms (see the event loop in `mod.rs`) is cheap.

use crate::lifecycle::LifecycleError;
use crate::process_monitor::event::ProcessEvent;
use std::collections::HashMap;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

/// Snapshot entry for a single process.
struct ProcessInfo {
    parent_pid: u32,
    exe_path: String,
}

/// Windows process event source backed by Toolhelp32 snapshot diffs.
///
/// The previous snapshot is kept behind a private `Mutex` that never interacts
/// with the monitor's `pid_to_session`/`sessions` locks; the source is only
/// touched from the monitor's single event-loop thread.
pub(crate) struct WindowsProcessEventSource {
    snapshot: Mutex<HashMap<u32, ProcessInfo>>,
}

impl WindowsProcessEventSource {
    /// Create the source and capture a baseline snapshot so all pre-existing
    /// processes are not misreported as `Fork` on the first diff.
    pub fn new() -> Result<Self, LifecycleError> {
        let baseline = take_process_snapshot()?;
        Ok(Self {
            snapshot: Mutex::new(baseline),
        })
    }

    /// Diff a fresh snapshot against the previous one and append the resulting
    /// events to `out`. Returns the number of events appended.
    pub fn read_events(&self, out: &mut Vec<ProcessEvent>) -> Result<usize, LifecycleError> {
        let current = take_process_snapshot()?;
        let mut previous = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        let mut count = 0usize;

        for (pid, info) in &current {
            match previous.get(pid) {
                None => {
                    out.push(ProcessEvent::Fork {
                        parent_pid: info.parent_pid,
                        child_pid: *pid,
                    });
                    count += 1;
                }
                Some(old) if old.exe_path != info.exe_path => {
                    out.push(ProcessEvent::Exec {
                        pid: *pid,
                        argv0: info.exe_path.clone(),
                        argv: Vec::new(),
                    });
                    count += 1;
                }
                Some(_) => {}
            }
        }

        for pid in previous.keys() {
            if !current.contains_key(pid) {
                out.push(ProcessEvent::Exit {
                    pid: *pid,
                    exit_code: None,
                });
                count += 1;
            }
        }

        *previous = current;
        Ok(count)
    }
}

/// Look up the executable path of `pid` via a one-shot Toolhelp32 snapshot.
///
/// Retrieving the full command line on Windows requires reading the child PEB
/// (e.g. via `NtQueryInformationProcess`), which is deliberately out of scope;
/// callers therefore receive `(Some(exe_path), None)` and rely on argv0 alone.
pub(crate) fn read_proc_cmdline(pid: u32) -> (Option<String>, Option<Vec<String>>) {
    match take_process_snapshot() {
        Ok(processes) => (processes.get(&pid).map(|info| info.exe_path.clone()), None),
        Err(_) => (None, None),
    }
}

/// Capture a snapshot of all running processes as a pid -> info map.
fn take_process_snapshot() -> Result<HashMap<u32, ProcessInfo>, LifecycleError> {
    // SAFETY: `CreateToolhelp32Snapshot` with `TH32CS_SNAPPROCESS` and pid 0 is
    // a standard, well-defined call; the returned handle is closed below.
    let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(LifecycleError::Io(
            "CreateToolhelp32Snapshot failed".to_string(),
            std::io::Error::last_os_error(),
        ));
    }

    // SAFETY: `PROCESSENTRY32W` is a plain C struct of integers and a fixed-size
    // array; zero initialization is valid, and `dwSize` must be set before the
    // entry is passed to Process32FirstW/Process32NextW.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    let mut processes = HashMap::new();
    // SAFETY: `handle` is a valid process snapshot and `entry.dwSize` is set,
    // which Process32FirstW requires.
    let mut has_entry = unsafe { Process32FirstW(handle, &mut entry) };
    while has_entry != 0 {
        let pid = entry.th32ProcessID;
        let exe_len = entry
            .szExeFile
            .iter()
            .position(|&ch| ch == 0)
            .unwrap_or(entry.szExeFile.len());
        let exe_path = String::from_utf16_lossy(&entry.szExeFile[..exe_len]);
        processes.insert(
            pid,
            ProcessInfo {
                parent_pid: entry.th32ParentProcessID,
                exe_path,
            },
        );
        // SAFETY: same valid snapshot handle and sized entry as above.
        has_entry = unsafe { Process32NextW(handle, &mut entry) };
    }

    // SAFETY: `handle` is the non-null Toolhelp snapshot handle returned above.
    unsafe {
        CloseHandle(handle);
    }

    Ok(processes)
}
