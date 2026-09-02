//! Unix PTY implementation using `rustix_openpty` and `libc` ioctls.

use super::PtyPair;
use std::fs::File;
use std::io;

/// Open a new PTY pair with the given initial size.
pub fn openpty(cols: u16, rows: u16) -> io::Result<PtyPair> {
    let window_size = rustix_openpty::rustix::termios::Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = rustix_openpty::openpty(None, Some(&window_size))
        .map_err(|error| io::Error::other(format!("failed to open pty: {error}")))?;

    let master = File::from(pty.controller);
    let slave = File::from(pty.user);
    Ok(PtyPair { master, slave })
}

/// Resize a PTY master to the given dimensions.
pub fn resize(pty_master: &File, cols: u16, rows: u16) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `ioctl` on a valid PTY master fd with `TIOCSWINSZ` is the standard
    // way to resize a pseudo-terminal.
    let result = unsafe { libc::ioctl(pty_master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Set a PTY master into non-blocking mode.
pub fn set_nonblocking(pty_master: &mut File) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let fd = pty_master.as_raw_fd();
    // SAFETY: `fcntl` on a valid fd returned by `std::fs::File` is safe.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags >= 0 {
            let result = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            if result == -1 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}
