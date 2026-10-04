//! Bounded stderr tail for a child process.
//!
//! A chatty `codex app-server` can fill a pipe and deadlock if nobody is
//! reading. Drain on a dedicated thread into a fixed-size ring so the child
//! cannot block, and keep only the tail for refusal text.

use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

/// Bytes of stderr retained for a refusal. Older output is dropped.
pub const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// How long the caller waits for the drain thread after killing the child.
/// A descendant that inherited the stderr pipe can hold it open past the
/// child's death, so the wait is bounded and the captured tail is used as-is.
pub const STDERR_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Read `reader` to EOF, publishing the last `cap` bytes into `shared` as
/// they arrive. A writer that outruns the cap cannot fill the pipe: this loop
/// never stops consuming. Because the tail is published incrementally, a
/// caller that gives up waiting for EOF still sees everything read so far.
pub fn drain_into(mut reader: impl Read, shared: &Mutex<Vec<u8>>, cap: usize) {
    let cap = cap.max(1);
    let mut chunk = [0u8; 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut tail = shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > cap {
                    let excess = tail.len() - cap;
                    tail.drain(..excess);
                }
            }
        }
    }
}

/// Lossy UTF-8 snapshot of what [`drain_into`] has published so far.
pub fn snapshot(shared: &Mutex<Vec<u8>>) -> String {
    let tail = shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    String::from_utf8_lossy(&tail).into_owned()
}

#[cfg(test)]
/// Read `reader` to EOF and return the last `cap` bytes (lossy UTF-8).
pub fn drain_bounded_tail(reader: impl Read, cap: usize) -> String {
    let shared = Mutex::new(Vec::new());
    drain_into(reader, &shared, cap);
    snapshot(&shared)
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
        // Distinct, ASCII-only bytes (0..128 repeated) so a scrambled ring
        // cannot pass; the tail must equal the last `cap` input bytes.
        let input: Vec<u8> = (0..200usize).map(|i| (i % 128) as u8).collect();
        let tail = drain_bounded_tail(Cursor::new(input.clone()), 64);
        assert_eq!(tail.as_bytes(), &input[input.len() - 64..]);
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
            .arg("i=0; while [ $i -lt 256 ]; do printf '%01024d' 0 >&2; i=$((i+1)); done; printf 'TAIL-END-MARKER' >&2")
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
        // The 256 KiB of '0' bytes really reached the pipe: the tail is exactly
        // the cap, a '0' prefix followed by the marker.
        let marker = "TAIL-END-MARKER";
        assert_eq!(tail.len(), 64, "tail should be exactly the cap: {tail:?}");
        assert!(
            tail[..64 - marker.len()].bytes().all(|b| b == b'0'),
            "expected a '0' prefix, got {tail:?}"
        );
    }
}
