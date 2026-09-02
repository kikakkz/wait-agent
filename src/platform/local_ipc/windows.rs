//! TCP loopback implementation of local IPC for Windows.

use super::LocalIpcAddr;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::time::Duration;

#[derive(Debug)]
pub struct LocalListener {
    inner: TcpListener,
    port: u16,
}

impl LocalListener {
    pub fn bind(addr: &LocalIpcAddr) -> io::Result<Self> {
        let inner = TcpListener::bind(addr.tcp_addr())?;
        let port = addr.port();
        Ok(Self { inner, port })
    }

    pub fn accept(&self) -> io::Result<LocalStream> {
        let (stream, _) = self.inner.accept()?;
        Ok(LocalStream { inner: stream })
    }

    pub fn local_addr(&self) -> io::Result<LocalIpcAddr> {
        Ok(LocalIpcAddr::node(self.port))
    }
}

#[derive(Debug)]
pub struct LocalStream {
    inner: TcpStream,
}

impl LocalStream {
    pub fn connect(addr: &LocalIpcAddr) -> io::Result<Self> {
        let inner = TcpStream::connect_timeout(&addr.tcp_addr(), Duration::from_secs(2))?;
        Ok(Self { inner })
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        let inner = self.inner.try_clone()?;
        Ok(Self { inner })
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(dur)
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.inner.shutdown(how)
    }
}

impl Read for LocalStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for LocalStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
