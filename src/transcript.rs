use crate::terminal::{ScreenSnapshot, TerminalEngine, TerminalSize};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalTranscript {
    output: Vec<u8>,
}

impl TerminalTranscript {
    pub fn record_output(&mut self, bytes: &[u8]) {
        self.output.extend_from_slice(bytes);
    }

    pub fn rebuild_engine(&self, size: TerminalSize) -> TerminalEngine {
        let mut engine = TerminalEngine::new(size);
        engine.feed(&self.output);
        engine
    }

    pub fn replay_active_snapshot(&self, size: TerminalSize) -> ScreenSnapshot {
        let engine = self.rebuild_engine(size);
        engine.state().active_snapshot().clone()
    }

    #[cfg(test)]
    pub fn replay_normal_screen_snapshot(&self, size: TerminalSize) -> ScreenSnapshot {
        let engine = self.rebuild_engine(size);
        engine.state().normal
    }
}

#[cfg(test)]
mod tests {
    use super::TerminalTranscript;
    use crate::terminal::TerminalSize;

    #[test]
    fn replay_rebuilds_normal_screen_for_new_width() {
        let mut transcript = TerminalTranscript::default();
        transcript.record_output(b"1234567890");

        let narrow = transcript.replay_normal_screen_snapshot(TerminalSize {
            rows: 3,
            cols: 8,
            pixel_width: 0,
            pixel_height: 0,
        });
        let wide = transcript.replay_normal_screen_snapshot(TerminalSize {
            rows: 3,
            cols: 12,
            pixel_width: 0,
            pixel_height: 0,
        });

        assert_eq!(narrow.lines[0], "12345678");
        assert_eq!(narrow.lines[1], "90      ");
        assert_eq!(wide.lines[0], "1234567890  ");
        assert_eq!(wide.lines[1], "            ");
    }

    #[test]
    fn replay_preserves_normal_screen_scrollback() {
        let mut transcript = TerminalTranscript::default();
        transcript.record_output(b"one\r\ntwo\r\nthree");

        let snapshot = transcript.replay_normal_screen_snapshot(TerminalSize {
            rows: 2,
            cols: 5,
            pixel_width: 0,
            pixel_height: 0,
        });

        assert_eq!(snapshot.scrollback, vec!["one  ".to_string()]);
        assert_eq!(snapshot.lines[0], "two  ");
        assert_eq!(snapshot.lines[1], "three");
    }

    #[test]
    fn rebuild_engine_preserves_active_screen_mode() {
        let mut transcript = TerminalTranscript::default();
        transcript.record_output(b"prompt$ \x1b[?1049hvim");

        let engine = transcript.rebuild_engine(TerminalSize {
            rows: 3,
            cols: 12,
            pixel_width: 0,
            pixel_height: 0,
        });

        assert!(engine.state().alternate_screen_active);
        assert_eq!(engine.state().active_snapshot().lines[0].trim_end(), "vim");
    }

    #[test]
    fn replay_active_snapshot_preserves_alternate_screen_scrollback() {
        let mut transcript = TerminalTranscript::default();
        transcript.record_output(b"prompt$ \x1b[?1049hone\r\ntwo\r\nthree");

        let snapshot = transcript.replay_active_snapshot(TerminalSize {
            rows: 2,
            cols: 8,
            pixel_width: 0,
            pixel_height: 0,
        });

        assert!(snapshot.alternate_screen);
        assert_eq!(snapshot.scrollback, vec!["one     ".to_string()]);
        assert_eq!(snapshot.lines[0], "two     ");
        assert_eq!(snapshot.lines[1], "three   ");
    }
}
