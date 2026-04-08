#![allow(dead_code)]

use crate::session::SessionAddress;
use std::collections::HashMap;
use std::ffi::{CString, NulError};
use std::fmt;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::raw::{c_char, c_int, c_ulong};
use std::os::unix::io::{AsRawFd, FromRawFd};

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
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
    use super::{PtyManager, PtySize, SpawnRequest};
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
}
