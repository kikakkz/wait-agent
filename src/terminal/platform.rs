use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_uchar, c_uint, c_ulong};
use std::os::unix::io::RawFd;

use super::types::{TerminalError, TerminalSize};

pub(crate) const STDIN_FD: RawFd = 0;
pub(crate) const STDOUT_FD: RawFd = 1;
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

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Termios {
    c_iflag: c_uint,
    c_oflag: c_uint,
    c_cflag: c_uint,
    c_lflag: c_uint,
    c_line: c_uchar,
    c_cc: [c_uchar; NCCS],
    c_ispeed: c_uint,
    c_ospeed: c_uint,
}

extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn isatty(fd: c_int) -> c_int;
    fn tcgetattr(fd: c_int, termios_p: *mut Termios) -> c_int;
    fn tcsetattr(fd: c_int, optional_actions: c_int, termios_p: *const Termios) -> c_int;
}

pub(crate) fn is_tty(fd: RawFd) -> bool {
    unsafe { isatty(fd as c_int) == 1 }
}

pub(crate) fn current_terminal_size(fd: RawFd) -> Result<TerminalSize, TerminalError> {
    let mut winsize = MaybeUninit::<Winsize>::uninit();
    let result = unsafe { ioctl(fd as c_int, TIOCGWINSZ, winsize.as_mut_ptr()) };
    if result != 0 {
        return Err(TerminalError::Io(
            "failed to query terminal size".to_string(),
            io::Error::last_os_error(),
        ));
    }

    let winsize = unsafe { winsize.assume_init() };
    Ok(TerminalSize {
        rows: winsize.ws_row.max(1),
        cols: winsize.ws_col.max(1),
        pixel_width: winsize.ws_xpixel,
        pixel_height: winsize.ws_ypixel,
    })
}

pub(crate) fn read_termios(fd: RawFd) -> Result<Termios, TerminalError> {
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

pub(crate) fn write_termios(fd: RawFd, termios: &Termios) -> Result<(), TerminalError> {
    let result = unsafe { tcsetattr(fd as c_int, TCSAFLUSH, termios as *const Termios) };
    if result != 0 {
        return Err(TerminalError::Io(
            "failed to write terminal mode".to_string(),
            io::Error::last_os_error(),
        ));
    }

    Ok(())
}

pub(crate) fn write_escape(fd: RawFd, value: &str) -> Result<(), TerminalError> {
    let path = format!("/proc/self/fd/{fd}");
    let mut handle = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| {
            TerminalError::Io("failed to open terminal output stream".to_string(), error)
        })?;
    handle
        .write_all(value.as_bytes())
        .map_err(|error| TerminalError::Io("failed to write terminal escape".to_string(), error))?;
    handle
        .flush()
        .map_err(|error| TerminalError::Io("failed to flush terminal escape".to_string(), error))
}

pub(crate) fn make_raw(mut termios: Termios) -> Termios {
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
    use super::*;

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
