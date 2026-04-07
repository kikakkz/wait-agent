#![allow(dead_code)]

use crate::pty::PtySize;
use std::fmt;
use std::io;
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_uchar, c_uint, c_ulong};
use std::os::unix::io::RawFd;

const STDIN_FD: RawFd = 0;
const STDOUT_FD: RawFd = 1;
const TIOCGWINSZ: c_ulong = 0x5413;
const TCSAFLUSH: c_int = 2;
const NCCS: usize = 32;

const BRKINT: c_uint = 0o000002;
const ICRNL: c_uint = 0o000400;
const INPCK: c_uint = 0o000020;
const ISTRIP: c_uint = 0o000040;
const IXON: c_uint = 0o002000;
const OPOST: c_uint = 0o000001;
const CS8: c_uint = 0o000060;
const ECHO: c_uint = 0o000010;
const ICANON: c_uint = 0o000002;
const IEXTEN: c_uint = 0o100000;
const ISIG: c_uint = 0o000001;
const VTIME: usize = 5;
const VMIN: usize = 6;

extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn isatty(fd: c_int) -> c_int;
    fn tcgetattr(fd: c_int, termios_p: *mut Termios) -> c_int;
    fn tcsetattr(fd: c_int, optional_actions: c_int, termios_p: *const Termios) -> c_int;
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Termios {
    c_iflag: c_uint,
    c_oflag: c_uint,
    c_cflag: c_uint,
    c_lflag: c_uint,
    c_line: c_uchar,
    c_cc: [c_uchar; NCCS],
    c_ispeed: c_uint,
    c_ospeed: c_uint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl From<TerminalSize> for PtySize {
    fn from(value: TerminalSize) -> Self {
        Self {
            rows: value.rows,
            cols: value.cols,
            pixel_width: value.pixel_width,
            pixel_height: value.pixel_height,
        }
    }
}

impl From<PtySize> for TerminalSize {
    fn from(value: PtySize) -> Self {
        Self {
            rows: value.rows,
            cols: value.cols,
            pixel_width: value.pixel_width,
            pixel_height: value.pixel_height,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub input_is_tty: bool,
    pub output_is_tty: bool,
    pub size: TerminalSize,
}

#[derive(Debug, Default, Clone, Copy)]
struct ResizeTracker {
    last_size: Option<TerminalSize>,
}

impl ResizeTracker {
    fn observe(&mut self, size: TerminalSize) -> Option<TerminalSize> {
        if self.last_size == Some(size) {
            None
        } else {
            self.last_size = Some(size);
            Some(size)
        }
    }
}

#[derive(Debug)]
pub struct TerminalRuntime {
    input_fd: RawFd,
    output_fd: RawFd,
    resize: ResizeTracker,
}

impl TerminalRuntime {
    pub fn stdio() -> Self {
        Self {
            input_fd: STDIN_FD,
            output_fd: STDOUT_FD,
            resize: ResizeTracker::default(),
        }
    }

    pub fn input_is_tty(&self) -> bool {
        is_tty(self.input_fd)
    }

    pub fn output_is_tty(&self) -> bool {
        is_tty(self.output_fd)
    }

    pub fn snapshot(&mut self) -> Result<TerminalSnapshot, TerminalError> {
        let size = self.current_size_or_default();
        self.resize.observe(size);

        Ok(TerminalSnapshot {
            input_is_tty: self.input_is_tty(),
            output_is_tty: self.output_is_tty(),
            size,
        })
    }

    pub fn current_size(&self) -> Result<TerminalSize, TerminalError> {
        if !self.output_is_tty() {
            return Err(TerminalError::NotTty("stdout".to_string()));
        }

        let mut winsize = MaybeUninit::<Winsize>::uninit();
        let result = unsafe { ioctl(self.output_fd, TIOCGWINSZ, winsize.as_mut_ptr()) };
        if result != 0 {
            return Err(TerminalError::Io(
                "failed to query terminal size".to_string(),
                io::Error::last_os_error(),
            ));
        }

        let winsize = unsafe { winsize.assume_init() };
        Ok(TerminalSize {
            rows: winsize.ws_row,
            cols: winsize.ws_col,
            pixel_width: winsize.ws_xpixel,
            pixel_height: winsize.ws_ypixel,
        })
    }

    pub fn current_size_or_default(&self) -> TerminalSize {
        self.current_size().unwrap_or_default()
    }

    pub fn capture_resize(&mut self) -> Result<Option<TerminalSize>, TerminalError> {
        let size = self.current_size()?;
        Ok(self.resize.observe(size))
    }

    pub fn enter_raw_mode(&self) -> Result<RawModeGuard, TerminalError> {
        if !self.input_is_tty() {
            return Err(TerminalError::NotTty("stdin".to_string()));
        }

        let original = read_termios(self.input_fd)?;
        let raw = make_raw(original);
        write_termios(self.input_fd, &raw)?;

        Ok(RawModeGuard {
            fd: self.input_fd,
            original,
            active: true,
        })
    }
}

pub struct RawModeGuard {
    fd: RawFd,
    original: Termios,
    active: bool,
}

impl RawModeGuard {
    pub fn restore(&mut self) -> Result<(), TerminalError> {
        if !self.active {
            return Ok(());
        }

        write_termios(self.fd, &self.original)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Debug)]
pub enum TerminalError {
    Io(String, io::Error),
    NotTty(String),
}

impl fmt::Display for TerminalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(context, error) => write!(f, "{context}: {error}"),
            Self::NotTty(name) => write!(f, "{name} is not attached to a terminal"),
        }
    }
}

impl std::error::Error for TerminalError {}

fn is_tty(fd: RawFd) -> bool {
    unsafe { isatty(fd as c_int) == 1 }
}

fn read_termios(fd: RawFd) -> Result<Termios, TerminalError> {
    let mut termios = MaybeUninit::<Termios>::uninit();
    let result = unsafe { tcgetattr(fd as c_int, termios.as_mut_ptr()) };
    if result != 0 {
        return Err(TerminalError::Io(
            "failed to read terminal mode".to_string(),
            io::Error::last_os_error(),
        ));
    }

    Ok(unsafe { termios.assume_init() })
}

fn write_termios(fd: RawFd, termios: &Termios) -> Result<(), TerminalError> {
    let result = unsafe { tcsetattr(fd as c_int, TCSAFLUSH, termios as *const Termios) };
    if result != 0 {
        return Err(TerminalError::Io(
            "failed to write terminal mode".to_string(),
            io::Error::last_os_error(),
        ));
    }

    Ok(())
}

fn make_raw(mut termios: Termios) -> Termios {
    termios.c_iflag &= !(BRKINT | ICRNL | INPCK | ISTRIP | IXON);
    termios.c_oflag &= !OPOST;
    termios.c_cflag |= CS8;
    termios.c_lflag &= !(ECHO | ICANON | IEXTEN | ISIG);
    termios.c_cc[VMIN] = 1;
    termios.c_cc[VTIME] = 0;
    termios
}

#[cfg(test)]
mod tests {
    use super::{
        make_raw, ResizeTracker, TerminalSize, Termios, BRKINT, ECHO, ICANON, ICRNL, IEXTEN, INPCK,
        ISIG, ISTRIP, IXON, OPOST, VMIN, VTIME,
    };

    #[test]
    fn resize_tracker_reports_only_real_changes() {
        let mut tracker = ResizeTracker::default();
        let initial = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let updated = TerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };

        assert_eq!(tracker.observe(initial), Some(initial));
        assert_eq!(tracker.observe(initial), None);
        assert_eq!(tracker.observe(updated), Some(updated));
    }

    #[test]
    fn make_raw_disables_canonical_input_flags() {
        let mut termios = Termios {
            c_iflag: BRKINT | ICRNL | INPCK | ISTRIP | IXON,
            c_oflag: OPOST,
            c_cflag: 0,
            c_lflag: ECHO | ICANON | IEXTEN | ISIG,
            c_line: 0,
            c_cc: [0; 32],
            c_ispeed: 0,
            c_ospeed: 0,
        };
        termios.c_cc[VMIN] = 0;
        termios.c_cc[VTIME] = 10;

        let raw = make_raw(termios);

        assert_eq!(raw.c_iflag & (BRKINT | ICRNL | INPCK | ISTRIP | IXON), 0);
        assert_eq!(raw.c_oflag & OPOST, 0);
        assert_eq!(raw.c_lflag & (ECHO | ICANON | IEXTEN | ISIG), 0);
        assert_eq!(raw.c_cc[VMIN], 1);
        assert_eq!(raw.c_cc[VTIME], 0);
    }
}
