//! Unix Domain Socket implementation of local IPC.

use super::LocalIpcAddr;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

#[derive(Debug)]
pub struct LocalListener {
    inner: UnixListener,
}

impl LocalListener {
    pub fn bind(addr: &LocalIpcAddr) -> io::Result<Self> {
        let path = addr.path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::infra::best_effort::remove_file(&path);
        let inner = UnixListener::bind(&path)?;
        Ok(Self { inner })
    }

    pub fn accept(&self) -> io::Result<LocalStream> {
        let (stream, _) = self.inner.accept()?;
        Ok(LocalStream { inner: stream })
    }

    #[allow(dead_code)]
    pub fn local_addr(&self) -> io::Result<LocalIpcAddr> {
        // UDS 本地地址不携带 port；返回传入地址更实用，但这里简化处理。
        Err(io::Error::other("Unix local_addr not supported"))
    }
}

#[derive(Debug)]
pub struct LocalStream {
    inner: UnixStream,
}

impl LocalStream {
    pub fn connect(addr: &LocalIpcAddr) -> io::Result<Self> {
        let inner = UnixStream::connect(addr.path())?;
        Ok(Self { inner })
    }

    #[cfg(test)]
    pub fn from_unix(inner: UnixStream) -> Self {
        Self { inner }
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        let inner = self.inner.try_clone()?;
        Ok(Self { inner })
    }

    #[allow(dead_code)]
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
