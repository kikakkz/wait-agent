use std::os::unix::io::RawFd;

use super::platform;
use super::types::{TerminalError, TerminalSize, TerminalSnapshot};

const ENTER_ALTERNATE_SCREEN: &str = "\x1b[?1049h\x1b[H";
const LEAVE_ALTERNATE_SCREEN: &str = "\x1b[?1049l\x1b[?25h";

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ResizeTracker {
    last_size: Option<TerminalSize>,
}

impl ResizeTracker {
    pub(crate) fn observe(&mut self, size: TerminalSize) -> Option<TerminalSize> {
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
            input_fd: platform::STDIN_FD,
            output_fd: platform::STDOUT_FD,
            resize: ResizeTracker::default(),
        }
    }

    pub fn input_is_tty(&self) -> bool {
        platform::is_tty(self.input_fd)
    }

    pub fn output_is_tty(&self) -> bool {
        platform::is_tty(self.output_fd)
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

        platform::current_terminal_size(self.output_fd)
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

        let original = platform::read_termios(self.input_fd)?;
        let raw = platform::make_raw(original);
        platform::write_termios(self.input_fd, &raw)?;

        Ok(RawModeGuard {
            fd: self.input_fd,
            original,
            active: true,
        })
    }

    pub fn enter_alternate_screen(&self) -> Result<AlternateScreenGuard, TerminalError> {
        if !self.output_is_tty() {
            return Err(TerminalError::NotTty("stdout".to_string()));
        }

        platform::write_escape(self.output_fd, ENTER_ALTERNATE_SCREEN)?;
        Ok(AlternateScreenGuard {
            fd: self.output_fd,
            active: true,
        })
    }
}

pub struct RawModeGuard {
    fd: RawFd,
    original: platform::Termios,
    active: bool,
}

impl RawModeGuard {
    pub fn restore(&mut self) -> Result<(), TerminalError> {
        if !self.active {
            return Ok(());
        }

        platform::write_termios(self.fd, &self.original)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub struct AlternateScreenGuard {
    fd: RawFd,
    active: bool,
}

impl AlternateScreenGuard {
    pub fn suspend(&mut self) -> Result<(), TerminalError> {
        self.restore()
    }

    pub fn resume(&mut self) -> Result<(), TerminalError> {
        if self.active {
            return Ok(());
        }

        platform::write_escape(self.fd, ENTER_ALTERNATE_SCREEN)?;
        self.active = true;
        Ok(())
    }

    pub fn restore(&mut self) -> Result<(), TerminalError> {
        if !self.active {
            return Ok(());
        }

        platform::write_escape(self.fd, LEAVE_ALTERNATE_SCREEN)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for AlternateScreenGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::ResizeTracker;
    use crate::terminal::types::TerminalSize;

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
}
