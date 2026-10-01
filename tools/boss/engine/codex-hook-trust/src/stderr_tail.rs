//! Bounded stderr tail for a child process.
//!
//! A chatty `codex app-server` can fill a pipe and deadlock if nobody is
//! reading. Drain on a dedicated thread into a fixed-size ring so the child
//! cannot block, and keep only the tail for refusal text.

use std::io::Read;

/// Bytes of stderr retained for a refusal. Older output is dropped.
pub const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// Read `reader` to EOF, keeping only the last [`STDERR_TAIL_BYTES`] (or
/// `cap`) bytes. Lossy UTF-8. A writer that outruns the cap cannot fill the
/// pipe: this loop never stops consuming.
pub fn drain_bounded_tail(mut reader: impl Read, cap: usize) -> String {
    let cap = cap.max(1);
    let mut tail = vec![0; cap];
    let mut len = 0usize;
    let mut wrapped = false;
    let mut chunk = [0u8; 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                for &byte in &chunk[..n] {
                    if len == cap {
                        wrapped = true;
                    }
                    tail[len % cap] = byte;
                    len = len.saturating_add(1);
                }
            }
            Err(_) => break,
        }
    }
    let kept = if wrapped { cap } else { len.min(cap) };
    if kept == 0 {
        return String::new();
    }
    let mut bytes = Vec::with_capacity(kept);
    if wrapped {
        let start = len % cap;
        bytes.extend_from_slice(&tail[start..]);
        bytes.extend_from_slice(&tail[..start]);
    } else {
        bytes.extend_from_slice(&tail[..kept]);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn drain_bounded_tail_keeps_the_last_bytes() {
        let input = [b'a'; 200];
        let tail = drain_bounded_tail(Cursor::new(input), 64);
        assert_eq!(tail.len(), 64);
        assert!(tail.bytes().all(|b| b == b'a'), "{tail:?}");
    }

    #[test]
    fn drain_bounded_tail_keeps_a_short_input_in_full() {
        let tail = drain_bounded_tail(Cursor::new(b"hello"), 64);
        assert_eq!(tail, "hello");
    }

    #[test]
    fn drain_bounded_tail_does_not_block_a_chatty_child() {
        // 256 KiB exceeds a typical pipe buffer (16–64 KiB). If nobody drains,
        // the child blocks on write. The drain thread must keep it moving and
        // retain the marker at the end.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("dd if=/dev/zero bs=1024 count=256 2>/dev/null >&2; printf 'TAIL-END-MARKER' >&2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn chatty stderr child");
        let stderr = child.stderr.take().expect("piped stderr");
        let handle = thread::spawn(move || drain_bounded_tail(stderr, 64));

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let status = child.wait();
            let _ = tx.send(status);
        });
        let status = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("child blocked on a full stderr pipe")
            .expect("wait chatty child");
        assert!(status.success(), "chatty child did not exit cleanly: {status:?}");

        let tail = handle.join().expect("stderr drain thread");
        assert!(
            tail.ends_with("TAIL-END-MARKER"),
            "expected the retained tail to end with the marker, got {tail:?}"
        );
        assert!(tail.len() <= 64, "tail exceeded cap: {} bytes", tail.len());
    }
}
