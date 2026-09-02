//! Windows ConPTY implementation.
//!
//! `openpty` creates a pseudoconsole (`HPCON`) plus two anonymous pipes: the
//! parent writes user input to `ConPty::input` and reads terminal output from
//! `ConPty::output`. `spawn_shell` attaches a child process to the pseudoconsole
//! via `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` (the std
//! `Command` API cannot express this attribute on stable Rust), returning a
//! `ConPtyChild` that mirrors the `try_wait`/`id` surface of `std::process::Child`.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::process::ExitStatus;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, UpdateProcThreadAttribute, WaitForSingleObject,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTUPINFOEXW,
};

/// A Windows ConPTY session.
///
/// Owns the pseudo-console handle and the parent side of the two anonymous
/// pipes used to communicate with the hosted child. The child-side pipe ends
/// are held until `spawn_shell` succeeds and released there, so the output
/// pipe reports EOF once the pseudoconsole closes its own copy.
#[derive(Debug)]
pub struct ConPty {
    input: File,
    output: File,
    input_read: Option<File>,
    output_write: Option<File>,
    hpcon: HPCON,
}

impl ConPty {
    /// Pipe to write user input (keystrokes) into the pseudoconsole.
    pub fn input(&self) -> &File {
        &self.input
    }

    /// Pipe to read terminal output from the pseudoconsole.
    pub fn output(&self) -> &File {
        &self.output
    }

    /// Release the child-side pipe handles after the hosted process has been
    /// created. The pseudoconsole keeps its own copies; without this the
    /// output pipe would never reach EOF in the parent.
    fn close_child_side(&mut self) {
        self.input_read.take();
        self.output_write.take();
    }
}

impl Drop for ConPty {
    fn drop(&mut self) {
        // SAFETY: hpcon was returned by CreatePseudoConsole and is closed
        // exactly once here. Closing the pseudoconsole terminates the hosted
        // process tree; the reader thread drains the output pipe until the
        // broken channel unblocks it.
        unsafe {
            ClosePseudoConsole(self.hpcon);
        }
    }
}

/// Child process attached to a [`ConPty`].
///
/// Mirrors the parts of `std::process::Child` used by the authority-host IO
/// loop (`id` and `try_wait`). Constructed by [`spawn_shell`].
#[derive(Debug)]
pub struct ConPtyChild {
    process_handle: HANDLE,
    pid: u32,
    exit_status: Option<ExitStatus>,
}

// SAFETY: `process_handle` is an exclusively owned OS process handle. All Win32
// calls on it (`WaitForSingleObject`, `GetExitCodeProcess`, `CloseHandle`) are
// thread-safe, and the handle is only closed once, from `Drop`. Sharing the
// handle across threads cannot race with closure because `Drop` requires `&mut`.
unsafe impl Send for ConPtyChild {}
unsafe impl Sync for ConPtyChild {}

impl ConPtyChild {
    /// OS process id of the child.
    pub fn id(&self) -> u32 {
        self.pid
    }
    /// Non-blocking reap, matching `std::process::Child::try_wait`.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        use std::os::windows::process::ExitStatusExt;

        if let Some(status) = self.exit_status {
            return Ok(Some(status));
        }
        // SAFETY: process_handle is a valid open process handle owned by self.
        let wait_result = unsafe { WaitForSingleObject(self.process_handle, 0) };
        if wait_result == WAIT_TIMEOUT {
            return Ok(None);
        }
        if wait_result != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        let mut code = 0u32;
        // SAFETY: process_handle is a valid open process handle and code is a
        // writable local.
        let ok = unsafe { GetExitCodeProcess(self.process_handle, &mut code) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let status = ExitStatus::from_raw(code);
        self.exit_status = Some(status);
        Ok(Some(status))
    }
}

impl Drop for ConPtyChild {
    fn drop(&mut self) {
        // SAFETY: process_handle is an owned handle and is closed exactly once
        // here. The pid was already reaped by try_wait or the process is being
        // torn down.
        unsafe {
            CloseHandle(self.process_handle);
        }
    }
}

/// Create a new ConPTY with the given initial size.
pub fn openpty(cols: u16, rows: u16) -> io::Result<ConPty> {
    // ConPTY pipes are synchronous; the pseudoconsole reads/writes them with
    // blocking `ReadFile`/`WriteFile` calls.
    let (input_read, input_write) = create_pipe()?;
    let (output_read, output_write) = create_pipe()?;

    let size = COORD {
        X: cols as i16,
        Y: rows as i16,
    };
    let mut hpcon: HPCON = 0;
    // SAFETY: input_read/output_write are valid pipe handles; hpcon points to
    // a writable local. On success the pseudoconsole holds its own references
    // to both handles.
    let hr = unsafe {
        CreatePseudoConsole(
            size,
            input_read.as_raw_handle(),
            output_write.as_raw_handle(),
            0,
            &mut hpcon,
        )
    };
    if hr < 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("CreatePseudoConsole failed: 0x{hr:08x}"),
        ));
    }

    Ok(ConPty {
        input: input_write,
        output: output_read,
        input_read: Some(input_read),
        output_write: Some(output_write),
        hpcon,
    })
}

/// Resize a ConPTY to the given dimensions.
pub fn resize(conpty: &ConPty, cols: u16, rows: u16) -> io::Result<()> {
    let size = COORD {
        X: cols as i16,
        Y: rows as i16,
    };
    // SAFETY: conpty.hpcon is a valid open pseudoconsole handle.
    let hr = unsafe { ResizePseudoConsole(conpty.hpcon, size) };
    if hr < 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("ResizePseudoConsole failed: 0x{hr:08x}"),
        ));
    }
    Ok(())
}

/// Spawn `program` attached to `conpty`'s pseudoconsole, inheriting the
/// caller's current directory and the environment in `env`.
pub fn spawn_shell(
    program: &OsStr,
    env: &HashMap<String, String>,
    conpty: &mut ConPty,
) -> io::Result<ConPtyChild> {
    let mut command_line = Vec::new();
    append_quoted_arg(&mut command_line, program);
    command_line.push(0);
    let env_block = build_env_block(env);
    let attributes = ProcThreadAttributeList::with_pseudoconsole(conpty.hpcon)?;

    let mut startup_info: STARTUPINFOEXW = unsafe {
        // SAFETY: STARTUPINFOEXW is plain data; zeroed is a valid init state
        // and cb/lpAttributeList are filled in below before use.
        std::mem::zeroed()
    };
    startup_info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup_info.lpAttributeList = attributes.buffer.as_ptr() as *mut core::ffi::c_void;
    let mut process_info: PROCESS_INFORMATION = unsafe {
        // SAFETY: PROCESS_INFORMATION is plain data; CreateProcessW fills it
        // on success.
        std::mem::zeroed()
    };

    // SAFETY: command_line is NUL-terminated wide text; env_block is a
    // correctly terminated environment block; startup_info points at a valid
    // attribute list containing the pseudoconsole handle; process_info is
    // writable. No handles are inherited: the child uses the pseudoconsole
    // for all stdio.
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            env_block.as_ptr() as *const core::ffi::c_void,
            std::ptr::null(),
            &startup_info.StartupInfo,
            &mut process_info,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: hThread is an owned handle returned by CreateProcessW; the IO
    // loop only needs the process handle.
    unsafe {
        CloseHandle(process_info.hThread);
    }
    conpty.close_child_side();

    Ok(ConPtyChild {
        process_handle: process_info.hProcess,
        pid: process_info.dwProcessId,
        exit_status: None,
    })
}

/// Create an anonymous pipe pair `(read, write)` backed by `File`s.
fn create_pipe() -> io::Result<(File, File)> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: read and write point to live stack locals that CreatePipe fills
    // with newly allocated pipe handles.
    let ok = unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreatePipe returned freshly allocated, exclusively owned
    // handles; wrapping them transfers ownership to the Files.
    let read_file = unsafe { File::from_raw_handle(read) };
    let write_file = unsafe { File::from_raw_handle(write) };
    Ok((read_file, write_file))
}

/// Owned `PROC_THREAD_ATTRIBUTE_LIST` carrying the pseudoconsole attribute.
struct ProcThreadAttributeList {
    buffer: Vec<u8>,
}

impl ProcThreadAttributeList {
    fn with_pseudoconsole(hpcon: HPCON) -> io::Result<Self> {
        let mut size = 0usize;
        // SAFETY: sizing call with a null list; failure (ERROR_INSUFFICIENT_BUFFER)
        // is expected and leaves the required byte count in size.
        unsafe {
            let _ = InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
        }
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u8; size];
        // SAFETY: buffer has the size reported by the first call.
        let ok = unsafe {
            InitializeProcThreadAttributeList(
                buffer.as_mut_ptr() as *mut core::ffi::c_void,
                1,
                0,
                &mut size,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: buffer holds an initialized list; hpcon is copied by value
        // into the attribute.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                buffer.as_mut_ptr() as *mut core::ffi::c_void,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpcon as *mut core::ffi::c_void,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: buffer still holds the initialized list.
            unsafe {
                DeleteProcThreadAttributeList(buffer.as_mut_ptr() as *mut core::ffi::c_void);
            }
            return Err(error);
        }
        Ok(Self { buffer })
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        // SAFETY: buffer holds the initialized attribute list owned by self.
        unsafe {
            DeleteProcThreadAttributeList(self.buffer.as_mut_ptr() as *mut core::ffi::c_void);
        }
    }
}

/// Append `arg` to a Windows command line following the std quoting rules
/// (quote when empty or containing space/tab; backslashes before a quote are
/// doubled).
fn append_quoted_arg(out: &mut Vec<u16>, arg: &OsStr) {
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = arg.encode_wide().collect();
    let need_quotes =
        wide.is_empty() || wide.iter().any(|&c| c == b' ' as u16 || c == b'\t' as u16);
    if need_quotes {
        out.push(b'"' as u16);
    }
    let mut backslashes = 0usize;
    for &c in &wide {
        if c == b'\\' as u16 {
            backslashes += 1;
        } else if c == b'"' as u16 {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            out.push(c);
            backslashes = 0;
        } else {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            backslashes = 0;
            out.push(c);
        }
    }
    if need_quotes {
        out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
        out.push(b'"' as u16);
    } else {
        out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
    }
}

/// Build a `VAR\0VAR\0...\0\0` wide environment block.
fn build_env_block(env: &HashMap<String, String>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    let mut block = Vec::new();
    for (key, value) in env {
        block.extend(OsStr::new(key).encode_wide());
        block.push(b'=' as u16);
        block.extend(OsStr::new(value).encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}
