//! Cross-platform intra-process wake pipe for event loops.
//!
//! Unix uses `UnixStream::pair()`. Windows uses a TCP loopback pair because
//! `UnixStream` is not available there.

use std::io::{self, Read, Write};

/// Read end of a wake pipe. Registered with a poller and read when woken.
pub struct WakeRead {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    inner: std::net::TcpStream,
}

/// Write end of a wake pipe. Used to wake the event loop thread.
pub struct WakeWrite {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    inner: std::net::TcpStream,
}

#[cfg(unix)]
impl std::os::fd::AsRawFd for WakeRead {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.inner)
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsRawSocket for WakeRead {
    fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
        std::os::windows::io::AsRawSocket::as_raw_socket(&self.inner)
    }
}

impl Read for WakeRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl WakeWrite {
    pub fn wake(&mut self) -> io::Result<()> {
        self.inner.write_all(&[1])
    }
}

/// Create a connected pair suitable for waking a polling event loop.
pub fn pair() -> io::Result<(WakeRead, WakeWrite)> {
    #[cfg(unix)]
    {
        let (read, write) = std::os::unix::net::UnixStream::pair()?;
        Ok((WakeRead { inner: read }, WakeWrite { inner: write }))
    }
    #[cfg(windows)]
    {
        use std::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let write = TcpStream::connect(listener.local_addr()?)?;
        let (read, _) = listener.accept()?;
        read.set_nonblocking(true)?;
        write.set_nonblocking(true)?;
        Ok((WakeRead { inner: read }, WakeWrite { inner: write }))
    }
}
