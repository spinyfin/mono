//! Newline-delimited framing.
//!
//! A frame ends at `\n`. Bytes are held until the terminator arrives, so a
//! read that ends in the middle of a line, or in the middle of a multi-byte
//! UTF-8 character, never produces a partial frame.

/// Upper bound on one line.
///
/// It exists only so a peer that never sends a terminator cannot grow the
/// buffer without limit. It is far above real traffic: the largest line the
/// coordinator ACP spike recorded was 27,587 bytes (the agent's list of
/// available commands), and an outgoing prompt that carries a full-screen
/// screenshot is a few megabytes of base64.
pub const DEFAULT_MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// How much of an unusable frame is kept for diagnostics.
const INVALID_PREVIEW_BYTES: usize = 256;

/// Why bytes on the stream could not be handed on as a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The line is not valid UTF-8.
    InvalidUtf8,
    /// The line exceeded the framer's limit before its terminator arrived.
    TooLong { limit: usize },
    /// The stream ended in the middle of a line.
    UnterminatedAtEof,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUtf8 => write!(f, "line is not valid UTF-8"),
            Self::TooLong { limit } => write!(f, "line is longer than {limit} bytes"),
            Self::UnterminatedAtEof => write!(f, "stream ended in the middle of a line"),
        }
    }
}

/// One unit read from the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A complete line, without its terminator.
    Line(String),
    /// Bytes that are not a usable line. `preview` is a bounded, lossy
    /// rendering for a log; the bytes themselves are dropped.
    Invalid { preview: String, error: FrameError },
}

/// Incremental line splitter. Feed it whatever each read returned.
#[derive(Debug)]
pub struct LineFramer {
    buffer: Vec<u8>,
    max_line_bytes: usize,
    /// Set after an over-long line was reported, until its terminator is seen.
    discarding: bool,
}

impl Default for LineFramer {
    fn default() -> Self {
        Self::new()
    }
}

impl LineFramer {
    pub fn new() -> Self {
        Self::with_max_line_bytes(DEFAULT_MAX_LINE_BYTES)
    }

    pub fn with_max_line_bytes(max_line_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_line_bytes,
            discarding: false,
        }
    }

    /// Bytes held for a line whose terminator has not arrived.
    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// Adds `bytes` and returns every frame they complete, in order.
    pub fn push(&mut self, mut bytes: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            let (head, rest) = bytes.split_at(newline);
            bytes = &rest[1..];
            if self.discarding {
                // The terminator of the over-long line that was already reported.
                self.discarding = false;
                continue;
            }
            if self.buffer.len() + head.len() > self.max_line_bytes {
                frames.push(self.too_long(head));
                continue;
            }
            self.buffer.extend_from_slice(head);
            frames.extend(self.take_line());
        }
        if self.discarding {
            return frames;
        }
        if self.buffer.len() + bytes.len() > self.max_line_bytes {
            frames.push(self.too_long(bytes));
            self.discarding = true;
        } else {
            self.buffer.extend_from_slice(bytes);
        }
        frames
    }

    /// Call once when the stream ends. Reports a line that was cut off.
    pub fn finish(&mut self) -> Option<Frame> {
        let rest = std::mem::take(&mut self.buffer);
        if self.discarding || rest.iter().all(u8::is_ascii_whitespace) {
            return None;
        }
        Some(Frame::Invalid {
            preview: preview(&rest),
            error: FrameError::UnterminatedAtEof,
        })
    }

    fn too_long(&mut self, tail: &[u8]) -> Frame {
        let mut seen = std::mem::take(&mut self.buffer);
        if seen.len() < INVALID_PREVIEW_BYTES {
            let room = INVALID_PREVIEW_BYTES - seen.len();
            seen.extend_from_slice(&tail[..tail.len().min(room)]);
        }
        Frame::Invalid {
            preview: preview(&seen),
            error: FrameError::TooLong {
                limit: self.max_line_bytes,
            },
        }
    }

    fn take_line(&mut self) -> Option<Frame> {
        let mut line = std::mem::take(&mut self.buffer);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            return None;
        }
        Some(match String::from_utf8(line) {
            Ok(line) => Frame::Line(line),
            Err(error) => Frame::Invalid {
                preview: preview(error.as_bytes()),
                error: FrameError::InvalidUtf8,
            },
        })
    }
}

fn preview(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(INVALID_PREVIEW_BYTES)]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(frames: Vec<Frame>) -> Vec<String> {
        frames
            .into_iter()
            .map(|frame| match frame {
                Frame::Line(line) => line,
                other => panic!("expected a line, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_line_split_across_reads_is_one_frame() {
        let mut framer = LineFramer::new();
        assert!(framer.push(br#"{"jsonrpc":"2.0","#).is_empty());
        assert!(framer.push(br#""id":1,"#).is_empty());
        assert_eq!(
            lines(framer.push(b"\"result\":{}}\n")),
            [r#"{"jsonrpc":"2.0","id":1,"result":{}}"#]
        );
        assert_eq!(framer.buffered_len(), 0);
    }

    #[test]
    fn several_lines_in_one_read_keep_their_order() {
        let mut framer = LineFramer::new();
        assert_eq!(lines(framer.push(b"one\ntwo\nthr")), ["one", "two"]);
        assert_eq!(lines(framer.push(b"ee\n")), ["three"]);
    }

    #[test]
    fn a_multi_byte_character_split_across_reads_is_reassembled() {
        let text = "snow ☃ and 🎉";
        let bytes = format!("{text}\n").into_bytes();
        for split in 1..bytes.len() - 1 {
            let mut framer = LineFramer::new();
            let mut frames = framer.push(&bytes[..split]);
            frames.extend(framer.push(&bytes[split..]));
            assert_eq!(lines(frames), [text], "split at byte {split}");
        }
    }

    #[test]
    fn carriage_returns_and_blank_lines_are_not_frames() {
        let mut framer = LineFramer::new();
        assert_eq!(lines(framer.push(b"one\r\n\r\n   \n\ntwo\n")), ["one", "two"]);
    }

    #[test]
    fn invalid_utf8_is_reported_and_does_not_poison_the_next_line() {
        let mut framer = LineFramer::new();
        let frames = framer.push(b"ok\n\xff\xfe broken\nstill ok\n");
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0], Frame::Line("ok".into()));
        assert!(matches!(
            &frames[1],
            Frame::Invalid { error: FrameError::InvalidUtf8, preview } if preview.ends_with(" broken")
        ));
        assert_eq!(frames[2], Frame::Line("still ok".into()));
    }

    #[test]
    fn an_over_long_line_is_reported_once_and_skipped_to_its_terminator() {
        let mut framer = LineFramer::with_max_line_bytes(8);
        let mut frames = framer.push(b"0123456");
        assert!(frames.is_empty());
        frames.extend(framer.push(b"789abcdef"));
        frames.extend(framer.push(b"more of the same line"));
        frames.extend(framer.push(b" end\nshort\n"));
        assert_eq!(
            frames,
            [
                Frame::Invalid {
                    preview: "0123456789abcdef".into(),
                    error: FrameError::TooLong { limit: 8 },
                },
                Frame::Line("short".into()),
            ]
        );
    }

    #[test]
    fn an_over_long_line_inside_one_read_is_reported_and_skipped() {
        let mut framer = LineFramer::with_max_line_bytes(4);
        assert_eq!(
            framer.push(b"ok\ntoo long for it\nfine\n"),
            [
                Frame::Line("ok".into()),
                Frame::Invalid {
                    preview: "too long for it".into(),
                    error: FrameError::TooLong { limit: 4 },
                },
                Frame::Line("fine".into()),
            ]
        );
    }

    #[test]
    fn end_of_stream_reports_a_cut_off_line_but_not_trailing_whitespace() {
        let mut framer = LineFramer::new();
        assert_eq!(lines(framer.push(b"whole\n{\"id\":")), ["whole"]);
        assert_eq!(
            framer.finish(),
            Some(Frame::Invalid {
                preview: "{\"id\":".into(),
                error: FrameError::UnterminatedAtEof,
            })
        );

        let mut framer = LineFramer::new();
        assert_eq!(lines(framer.push(b"whole\n  ")), ["whole"]);
        assert_eq!(framer.finish(), None);
    }
}
