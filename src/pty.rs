#![allow(dead_code)]

use crate::agent::live_agent_label;
use crate::session::SessionAddress;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CString, NulError};
use std::fmt;
use std::fs;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::raw::{c_char, c_int, c_ulong};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::Path;

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
const TIOCGPGRP: c_ulong = 0x540f;
const TIOCSWINSZ: c_ulong = 0x5414;
pub const PTY_EOF_ERRNO: i32 = 5;
const SIGKILL: c_int = 9;

extern "C" {
    fn forkpty(
        amaster: *mut c_int,
        name: *mut c_char,
        termp: *const core::ffi::c_void,
        winp: *const Winsize,
    ) -> c_int;
    fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int;
    fn kill(pid: c_int, sig: c_int) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn _exit(status: c_int) -> !;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PtyId(u64);

impl PtyId {
    pub fn new(value: u64) -> Self {
        Self(value)
    }
}

impl fmt::Display for PtyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pty-{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self {
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl From<PtySize> for Winsize {
    fn from(value: PtySize) -> Self {
        Self {
            ws_row: value.rows,
            ws_col: value.cols,
            ws_xpixel: value.pixel_width,
            ws_ypixel: value.pixel_height,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub program: String,
    pub args: Vec<String>,
    pub size: PtySize,
}

impl SpawnRequest {
    pub fn command_line(&self) -> String {
        let mut parts = Vec::with_capacity(self.args.len() + 1);
        parts.push(self.program.clone());
        parts.extend(self.args.iter().cloned());
        parts.join(" ")
    }
}

#[derive(Debug, Clone)]
pub struct PtyOwnershipRecord {
    pub pty_id: PtyId,
    pub session: SessionAddress,
}

#[derive(Debug)]
pub struct PtyHandle {
    ownership: PtyOwnershipRecord,
    size: PtySize,
    process_id: Option<u32>,
    master: File,
}

impl PtyHandle {
    pub fn ownership(&self) -> &PtyOwnershipRecord {
        &self.ownership
    }

    pub fn pty_id(&self) -> PtyId {
        self.ownership.pty_id
    }

    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    pub fn size(&self) -> PtySize {
        self.size
    }

    pub fn foreground_process_name(&self) -> Result<Option<String>, PtyError> {
        let Some(process_group) = foreground_process_group(self.master.as_raw_fd())? else {
            return Ok(None);
        };

        let processes = read_process_snapshots()?;
        Ok(select_foreground_process_name(process_group, &processes))
    }

    pub fn resize(&mut self, size: PtySize) -> Result<(), PtyError> {
        let winsize: Winsize = size.into();
        let result = unsafe { ioctl(self.master.as_raw_fd(), TIOCSWINSZ, &winsize) };
        if result != 0 {
            return Err(PtyError::Resize(io::Error::last_os_error()));
        }
        self.size = size;
        Ok(())
    }

    pub fn read_to_end(&mut self) -> Result<Vec<u8>, PtyError> {
        let mut buffer = Vec::new();
        let mut reader = self.try_clone_reader()?;
        let mut chunk = [0_u8; 4096];

        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => buffer.extend_from_slice(&chunk[..count]),
                Err(error) if error.raw_os_error() == Some(PTY_EOF_ERRNO) => break,
                Err(error) => return Err(PtyError::Read(error)),
            }
        }

        Ok(buffer)
    }

    pub fn try_clone_reader(&self) -> Result<File, PtyError> {
        self.master.try_clone().map_err(PtyError::CloneReader)
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyError> {
        let mut writer = self.master.try_clone().map_err(PtyError::TakeWriter)?;
        writer.write_all(bytes).map_err(PtyError::Write)?;
        writer.flush().map_err(PtyError::Write)?;
        Ok(())
    }

    pub fn wait(&mut self) -> Result<ExitStatus, PtyError> {
        let process_id = self.process_id.ok_or_else(|| {
            PtyError::Wait(io::Error::new(ErrorKind::Other, "missing process id"))
        })?;
        let mut status = 0;
        let result = unsafe { waitpid(process_id as c_int, &mut status, 0) };
        if result < 0 {
            return Err(PtyError::Wait(io::Error::last_os_error()));
        }

        Ok(ExitStatus::from_wait_status(status))
    }

    pub fn terminate(&self) -> Result<(), PtyError> {
        let process_id = self.process_id.ok_or_else(|| {
            PtyError::Terminate(io::Error::new(ErrorKind::Other, "missing process id"))
        })?;
        let process_group = -(process_id as c_int);
        let result = unsafe { kill(process_group, SIGKILL) };
        if result == 0 {
            return Ok(());
        }

        let fallback = unsafe { kill(process_id as c_int, SIGKILL) };
        if fallback != 0 {
            return Err(PtyError::Terminate(io::Error::last_os_error()));
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct PtyManager {
    next_id: u64,
    ownership: HashMap<SessionAddress, PtyId>,
}

impl PtyManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn(
        &mut self,
        session: SessionAddress,
        request: SpawnRequest,
    ) -> Result<PtyHandle, PtyError> {
        let argv = build_argv(&request.program, &request.args)?;
        let argv_ptrs = argv
            .iter()
            .map(|arg| arg.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect::<Vec<_>>();
        let winsize: Winsize = request.size.into();
        let mut master_fd = -1;
        let pid = unsafe {
            forkpty(
                &mut master_fd,
                std::ptr::null_mut(),
                std::ptr::null(),
                &winsize,
            )
        };

        if pid < 0 {
            return Err(PtyError::Open(io::Error::last_os_error()));
        }

        if pid == 0 {
            unsafe {
                execvp(argv[0].as_ptr(), argv_ptrs.as_ptr());
                _exit(127);
            }
        }

        self.next_id += 1;
        let pty_id = PtyId::new(self.next_id);
        self.ownership.insert(session.clone(), pty_id);

        Ok(PtyHandle {
            ownership: PtyOwnershipRecord { pty_id, session },
            size: request.size,
            process_id: Some(pid as u32),
            master: unsafe { File::from_raw_fd(master_fd) },
        })
    }

    pub fn owner_of(&self, session: &SessionAddress) -> Option<PtyId> {
        self.ownership.get(session).copied()
    }

    pub fn release(&mut self, session: &SessionAddress) -> Option<PtyId> {
        self.ownership.remove(session)
    }
}

#[derive(Debug)]
pub enum PtyError {
    Open(io::Error),
    Spawn(NulError),
    Inspect(io::Error),
    CloneReader(std::io::Error),
    TakeWriter(std::io::Error),
    Read(std::io::Error),
    Write(std::io::Error),
    Resize(std::io::Error),
    Wait(std::io::Error),
    Terminate(std::io::Error),
}

impl fmt::Display for PtyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(error) => write!(f, "failed to open PTY: {error}"),
            Self::Spawn(error) => write!(f, "failed to build PTY command: {error}"),
            Self::Inspect(error) => write!(f, "failed to inspect PTY foreground process: {error}"),
            Self::CloneReader(error) => write!(f, "failed to clone PTY reader: {error}"),
            Self::TakeWriter(error) => write!(f, "failed to take PTY writer: {error}"),
            Self::Read(error) => write!(f, "failed to read PTY output: {error}"),
            Self::Write(error) => write!(f, "failed to write PTY input: {error}"),
            Self::Resize(error) => write!(f, "failed to resize PTY: {error}"),
            Self::Wait(error) => write!(f, "failed to wait for PTY child: {error}"),
            Self::Terminate(error) => write!(f, "failed to terminate PTY child: {error}"),
        }
    }
}

impl std::error::Error for PtyError {}

fn foreground_process_group(fd: c_int) -> Result<Option<u32>, PtyError> {
    let mut process_group = 0_i32;
    let result = unsafe { ioctl(fd, TIOCGPGRP, &mut process_group) };
    if result != 0 {
        return Err(PtyError::Inspect(io::Error::last_os_error()));
    }

    Ok((process_group > 0).then_some(process_group as u32))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessSnapshot {
    pid: u32,
    parent_id: u32,
    process_group: u32,
    cmdline_name: Option<String>,
    comm_name: Option<String>,
}

fn read_process_snapshots() -> Result<Vec<ProcessSnapshot>, PtyError> {
    let mut processes = Vec::new();
    let entries = fs::read_dir("/proc").map_err(PtyError::Inspect)?;
    for entry in entries {
        let entry = entry.map_err(PtyError::Inspect)?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };

        let Some((parent_id, process_group)) = read_process_stat(pid)? else {
            continue;
        };

        processes.push(ProcessSnapshot {
            pid,
            parent_id,
            process_group,
            cmdline_name: read_process_name_from_cmdline(pid)?,
            comm_name: read_process_name_from_comm(pid)?,
        });
    }

    Ok(processes)
}

fn read_process_stat(process_id: u32) -> Result<Option<(u32, u32)>, PtyError> {
    let path = format!("/proc/{process_id}/stat");
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(PtyError::Inspect(error)),
    };

    let Some(close_paren) = contents.rfind(')') else {
        return Ok(None);
    };
    let Some(fields) = contents.get(close_paren + 2..) else {
        return Ok(None);
    };
    let mut parts = fields.split_whitespace();
    let _state = parts.next();
    let Some(parent_id) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
        return Ok(None);
    };
    let Some(process_group) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
        return Ok(None);
    };

    Ok(Some((parent_id, process_group)))
}

fn select_foreground_process_name(
    process_group: u32,
    processes: &[ProcessSnapshot],
) -> Option<String> {
    let by_pid = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect::<HashMap<_, _>>();

    if let Some(leader) = by_pid.get(&process_group) {
        if let Some(live_name) = live_agent_name(leader) {
            return Some(live_name);
        }

        let mut children_by_parent = HashMap::<u32, Vec<&ProcessSnapshot>>::new();
        for process in processes {
            children_by_parent
                .entry(process.parent_id)
                .or_default()
                .push(process);
        }
        for children in children_by_parent.values_mut() {
            children.sort_by_key(|process| process.pid);
        }

        let mut visited = HashSet::from([leader.pid]);
        let mut queue = children_by_parent
            .get(&leader.pid)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect::<VecDeque<_>>();

        while let Some(process) = queue.pop_front() {
            if !visited.insert(process.pid) {
                continue;
            }
            if let Some(live_name) = live_agent_name(process) {
                return Some(live_name);
            }
            if let Some(children) = children_by_parent.get(&process.pid) {
                queue.extend(children.iter().copied());
            }
        }

        if let Some(name) = display_process_name(leader) {
            return Some(name);
        }
    }

    let mut group_members = processes
        .iter()
        .filter(|process| process.process_group == process_group)
        .collect::<Vec<_>>();
    group_members.sort_by_key(|process| process.pid);

    for process in &group_members {
        if let Some(live_name) = live_agent_name(process) {
            return Some(live_name);
        }
    }

    group_members.into_iter().find_map(display_process_name)
}

fn live_agent_name(process: &ProcessSnapshot) -> Option<String> {
    process
        .cmdline_name
        .as_deref()
        .and_then(live_agent_label)
        .or_else(|| process.comm_name.as_deref().and_then(live_agent_label))
}

fn display_process_name(process: &ProcessSnapshot) -> Option<String> {
    process
        .cmdline_name
        .clone()
        .or_else(|| process.comm_name.clone())
}

fn read_process_name_from_cmdline(process_id: u32) -> Result<Option<String>, PtyError> {
    let path = format!("/proc/{process_id}/cmdline");
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(PtyError::Inspect(error)),
    };
    Ok(process_name_from_cmdline_bytes(&bytes))
}

fn read_process_name_from_comm(process_id: u32) -> Result<Option<String>, PtyError> {
    let path = format!("/proc/{process_id}/comm");
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(PtyError::Inspect(error)),
    };
    let trimmed = contents.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

fn process_name_from_cmdline_bytes(bytes: &[u8]) -> Option<String> {
    let first = bytes
        .split(|byte| *byte == 0)
        .find(|segment| !segment.is_empty())?;
    let command = std::str::from_utf8(first).ok()?;
    let name = Path::new(command)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(command);
    Some(name.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    code: Option<i32>,
    signal: Option<i32>,
}

impl ExitStatus {
    pub fn success(&self) -> bool {
        self.code == Some(0) && self.signal.is_none()
    }

    pub fn code(&self) -> Option<i32> {
        self.code
    }

    pub fn signal(&self) -> Option<i32> {
        self.signal
    }

    fn from_wait_status(status: i32) -> Self {
        if status & 0x7f == 0 {
            Self {
                code: Some((status >> 8) & 0xff),
                signal: None,
            }
        } else {
            Self {
                code: None,
                signal: Some(status & 0x7f),
            }
        }
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(code) = self.code {
            write!(f, "exit({code})")
        } else if let Some(signal) = self.signal {
            write!(f, "signal({signal})")
        } else {
            write!(f, "unknown")
        }
    }
}

fn build_argv(program: &str, args: &[String]) -> Result<Vec<CString>, PtyError> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(CString::new(program).map_err(PtyError::Spawn)?);
    for arg in args {
        argv.push(CString::new(arg.as_str()).map_err(PtyError::Spawn)?);
    }
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::{
        process_name_from_cmdline_bytes, select_foreground_process_name, ProcessSnapshot,
        PtyManager, PtySize, SpawnRequest,
    };
    use crate::session::SessionAddress;

    #[test]
    fn spawns_process_and_tracks_ownership() {
        let mut manager = PtyManager::new();
        let session = SessionAddress::new("local", "session-1");
        let request = SpawnRequest {
            program: "sh".to_string(),
            args: vec!["-lc".to_string(), "printf ready".to_string()],
            size: PtySize::default(),
        };

        let mut handle = manager
            .spawn(session.clone(), request)
            .expect("spawn should succeed");

        let output = handle.read_to_end().expect("read should succeed");
        let status = handle.wait().expect("wait should succeed");

        assert_eq!(manager.owner_of(&session), Some(handle.pty_id()));
        assert!(status.success());
        assert!(String::from_utf8_lossy(&output).contains("ready"));
    }

    #[test]
    fn resizes_spawned_pty() {
        let mut manager = PtyManager::new();
        let session = SessionAddress::new("local", "session-2");
        let request = SpawnRequest {
            program: "sh".to_string(),
            args: vec!["-lc".to_string(), "sleep 0.01".to_string()],
            size: PtySize::default(),
        };

        let mut handle = manager
            .spawn(session, request)
            .expect("spawn should succeed");

        let new_size = PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };

        handle.resize(new_size).expect("resize should succeed");
        assert_eq!(handle.size(), new_size);
    }

    #[test]
    fn writes_input_into_spawned_process_and_reads_response() {
        let mut manager = PtyManager::new();
        let session = SessionAddress::new("local", "session-3");
        let request = SpawnRequest {
            program: "sh".to_string(),
            args: vec![
                "-lc".to_string(),
                "read line; printf 'ack:%s' \"$line\"".to_string(),
            ],
            size: PtySize::default(),
        };

        let mut handle = manager
            .spawn(session, request)
            .expect("spawn should succeed");

        handle
            .write_all(b"hello from test\n")
            .expect("write should succeed");
        let output = handle.read_to_end().expect("read should succeed");
        let status = handle.wait().expect("wait should succeed");

        assert!(status.success());
        assert!(String::from_utf8_lossy(&output).contains("ack:hello from test"));
    }

    #[test]
    fn derives_process_name_from_proc_cmdline_bytes() {
        assert_eq!(
            process_name_from_cmdline_bytes(b"/usr/bin/codex\0--model\0gpt-5.4\0"),
            Some("codex".to_string())
        );
        assert_eq!(
            process_name_from_cmdline_bytes(b"claude-code\0--dangerously-skip-permissions\0"),
            Some("claude-code".to_string())
        );
        assert_eq!(process_name_from_cmdline_bytes(b"\0"), None);
    }

    #[test]
    fn foreground_process_selection_prefers_live_agent_descendant_of_wrapper() {
        let processes = vec![
            ProcessSnapshot {
                pid: 400,
                parent_id: 1,
                process_group: 400,
                cmdline_name: Some("node".to_string()),
                comm_name: Some("node".to_string()),
            },
            ProcessSnapshot {
                pid: 401,
                parent_id: 400,
                process_group: 400,
                cmdline_name: Some("codex".to_string()),
                comm_name: Some("codex".to_string()),
            },
        ];

        assert_eq!(
            select_foreground_process_name(400, &processes),
            Some("codex".to_string())
        );
    }

    #[test]
    fn foreground_process_selection_falls_back_to_group_leader_name() {
        let processes = vec![ProcessSnapshot {
            pid: 900,
            parent_id: 1,
            process_group: 900,
            cmdline_name: Some("bash".to_string()),
            comm_name: Some("bash".to_string()),
        }];

        assert_eq!(
            select_foreground_process_name(900, &processes),
            Some("bash".to_string())
        );
    }
}
