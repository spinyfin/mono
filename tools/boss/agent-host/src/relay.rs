//! The relay: one child, one journal, at most one attached client.
//!
//! # Ordering and the replay boundary
//!
//! Every line, in either direction, is appended to the journal under one
//! lock, and under that same lock the encoded record is queued for the
//! attached client. Attaching a client takes the lock too: it reads the
//! journal's end offset, then registers the client's queue. So a record is
//! either before the offset (the client gets it from the file) or after it
//! (the client gets it from the queue), never both and never neither. The
//! replay itself runs after the lock is released, so a slow client never
//! holds up the child.
//!
//! Lines for the child take the same route. The journal append and the
//! hand-off to the host's stdin writer task happen under the same lock, so
//! the child reads lines in journal order. The write itself runs in that
//! task, not in a connection's task: a connection that is replaced or fails
//! part-way through a large line must not leave half of it in the child's
//! pipe. The hand-off queue is unbounded for the reason below.
//!
//! The queue is unbounded on purpose. Bounding it would mean dropping or
//! blocking, and the journal already holds everything: a client that cannot
//! keep up costs memory in the host, not correctness. The client fixes a
//! stalled connection by reconnecting and asking for the records it missed.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot};

use crate::journal::{Direction, Journal};

/// How long a new connection has to send its handshake line. A connection
/// that stays silent never displaces the attached client.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// The first line a client sends: the first sequence number it wants.
///
/// A client that has seen everything up to `n` asks for `n + 1`. `0` and `1`
/// both mean the whole journal; a number past the end means nothing is
/// replayed and the client gets only what happens next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct Handshake {
    pub replay_from: u64,
}

impl Handshake {
    /// The line to send, without its terminator.
    pub fn line(self) -> String {
        serde_json::to_string(&self).expect("a handshake always encodes")
    }
}

struct Client {
    id: u64,
    /// Encoded journal records queued after the replay range.
    queue: mpsc::UnboundedSender<Arc<[u8]>>,
    /// Tells the connection to close at once: another client replaced it.
    kick: oneshot::Sender<()>,
    /// Resolves (as an error) once the connection task has finished.
    closed: oneshot::Receiver<()>,
}

struct Inner {
    journal: Journal,
    client: Option<Client>,
    next_client_id: u64,
}

/// State shared by the child pump, the accept loop and every connection.
pub(crate) struct Relay {
    inner: Mutex<Inner>,
    /// Lines for the child, written by the task started in [`Relay::new`].
    to_child: mpsc::UnboundedSender<String>,
    fatal: mpsc::UnboundedSender<io::Error>,
    journal_path: PathBuf,
}

impl Relay {
    pub(crate) fn new(journal: Journal, stdin: ChildStdin) -> (Arc<Self>, mpsc::UnboundedReceiver<io::Error>) {
        let (fatal, fatal_rx) = mpsc::unbounded_channel();
        let journal_path = journal.path().to_path_buf();
        let (to_child, to_child_rx) = mpsc::unbounded_channel();
        tokio::spawn(write_to_child(stdin, to_child_rx));
        let relay = Arc::new(Self {
            inner: Mutex::new(Inner {
                journal,
                client: None,
                next_client_id: 1,
            }),
            to_child,
            fatal,
            journal_path,
        });
        (relay, fatal_rx)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Journals one line and queues it for the attached client.
    ///
    /// A journal that cannot be written is fatal: a line delivered but not
    /// journalled could never be replayed, so the host stops instead.
    fn record(&self, dir: Direction, line: &str) -> io::Result<()> {
        self.record_with(dir, line, |_| {})
    }

    /// [`Relay::record`], running `then` under the same lock once the line is
    /// journalled, so whatever it does is ordered with the journal.
    fn record_with(&self, dir: Direction, line: &str, then: impl FnOnce(&str)) -> io::Result<()> {
        let mut inner = self.lock();
        let appended = match inner.journal.append(dir, line) {
            Ok(appended) => appended,
            Err(err) => {
                let _ = self
                    .fatal
                    .send(io::Error::new(err.kind(), format!("journal write failed: {err}")));
                return Err(err);
            }
        };
        if let Some(client) = &inner.client
            && client.queue.send(appended.encoded).is_err()
        {
            inner.client = None;
        }
        then(line);
        Ok(())
    }

    /// Reads the child's stdout to its end, journalling each line. An error
    /// means the journal could not be written, which the host treats as
    /// fatal.
    pub(crate) async fn pump_child_output(self: Arc<Self>, stdout: tokio::process::ChildStdout) -> io::Result<()> {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                // A final line the child never terminated is still a line
                // the child sent.
                Ok(_) => {
                    self.record(Direction::FromAgent, &decode_line(&buf))?;
                }
                Err(err) => {
                    eprintln!("boss-agent-host: reading the child's output failed: {err}");
                    break;
                }
            }
        }
        Ok(())
    }

    /// Journals a line from the client and queues it for the child.
    ///
    /// Nothing here waits on the child's pipe, so the caller being dropped
    /// (its connection was replaced or died) cannot cut a line short.
    fn send_to_child(&self, line: &str) -> io::Result<()> {
        self.record_with(Direction::ToAgent, line, |line| {
            // The writer task only ends once the child's stdin is gone; the
            // child is exiting and the line has nowhere to go.
            let _ = self.to_child.send(line.to_owned());
        })
    }

    /// Serves one accepted connection until it ends, is replaced, or the
    /// host shuts it down.
    pub(crate) async fn serve(self: Arc<Self>, stream: UnixStream) {
        let (read_half, write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        let handshake = match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_handshake(&mut reader)).await {
            Ok(Ok(handshake)) => handshake,
            Ok(Err(err)) => {
                eprintln!("boss-agent-host: dropped a connection: {err}");
                return;
            }
            Err(_) => {
                eprintln!("boss-agent-host: dropped a connection that sent no handshake");
                return;
            }
        };

        let (queue, queue_rx) = mpsc::unbounded_channel();
        let (kick, kick_rx) = oneshot::channel();
        let (closed_tx, closed) = oneshot::channel::<()>();
        let (id, range, displaced) = {
            let mut inner = self.lock();
            let id = inner.next_client_id;
            inner.next_client_id += 1;
            let range = inner.journal.replay_range(handshake.replay_from);
            let displaced = inner.client.replace(Client {
                id,
                queue,
                kick,
                closed,
            });
            (id, range, displaced)
        };
        if let Some(older) = displaced {
            // The older connection may be dead (an engine restart leaves one
            // behind) or alive; either way there is one client now.
            let _ = older.kick.send(());
        }

        let writer = write_to_client(write_half, self.journal_path.clone(), range, queue_rx);
        let reader_loop = self.read_from_client(reader);
        // Only an explicit kick ends the connection. A dropped sender means
        // the host is draining, which the writer finishes on its own.
        let kicked = async move {
            if kick_rx.await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            result = writer => {
                if let Err(err) = result {
                    eprintln!("boss-agent-host: client {id} write failed: {err}");
                }
            }
            () = reader_loop => {}
            () = kicked => {}
        }

        let mut inner = self.lock();
        if inner.client.as_ref().is_some_and(|client| client.id == id) {
            inner.client = None;
        }
        drop(inner);
        drop(closed_tx);
    }

    /// Reads whole lines from the client and forwards them to the child.
    /// A line cut off by the client dying mid-write is dropped: only a line
    /// the client finished is a line it sent.
    async fn read_from_client(&self, mut reader: BufReader<OwnedReadHalf>) {
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) if buf.last() != Some(&b'\n') => return,
                Ok(_) => {
                    if self.send_to_child(&decode_line(&buf)).is_err() {
                        return;
                    }
                }
            }
        }
    }

    /// Ends the live client's queue so its connection drains and closes,
    /// and waits (bounded) for that to finish.
    pub(crate) async fn finish_client(&self, grace: Duration) {
        let client = self.lock().client.take();
        if let Some(Client {
            queue, kick: _, closed, ..
        }) = client
        {
            drop(queue);
            let _ = tokio::time::timeout(grace, closed).await;
        }
    }
}

/// The only writer of the child's stdin: whole lines, in the order they were
/// queued, regardless of which connection (if any) queued them.
async fn write_to_child(mut stdin: ChildStdin, mut lines: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = lines.recv().await {
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
            return;
        }
    }
}

async fn read_handshake(reader: &mut BufReader<OwnedReadHalf>) -> io::Result<Handshake> {
    let mut buf = Vec::new();
    reader.read_until(b'\n', &mut buf).await?;
    if buf.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the connection closed before a handshake line",
        ));
    }
    serde_json::from_slice(&buf)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("bad handshake: {err}")))
}

/// Writes the replay range of the journal file, then every queued record.
async fn write_to_client(
    mut stream: OwnedWriteHalf,
    journal_path: PathBuf,
    range: crate::journal::ReplayRange,
    mut queue: mpsc::UnboundedReceiver<Arc<[u8]>>,
) -> io::Result<()> {
    if range.end > range.start {
        let mut file = tokio::fs::File::open(&journal_path).await?;
        file.seek(io::SeekFrom::Start(range.start)).await?;
        let mut replay = file.take(range.end - range.start);
        tokio::io::copy(&mut replay, &mut stream).await?;
    }
    while let Some(record) = queue.recv().await {
        stream.write_all(&record).await?;
    }
    stream.shutdown().await
}

/// A line without its terminator: `\n`, or `\r\n`. Bytes that are not UTF-8
/// are replaced, so what the child or client sent, what is journalled and
/// what is replayed are the same string.
fn decode_line(raw: &[u8]) -> String {
    let raw = raw.strip_suffix(b"\n").unwrap_or(raw);
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    String::from_utf8_lossy(raw).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_line_strips_one_terminator() {
        assert_eq!(decode_line(b"abc\n"), "abc");
        assert_eq!(decode_line(b"abc\r\n"), "abc");
        assert_eq!(decode_line(b"abc"), "abc");
        assert_eq!(decode_line(b"\n"), "");
        assert_eq!(decode_line(b"a\r\r\n"), "a\r");
    }

    #[test]
    fn decode_line_replaces_invalid_utf8() {
        assert_eq!(decode_line(b"a\xffb\n"), "a\u{fffd}b");
    }

    #[test]
    fn handshake_line_round_trips() {
        let handshake = Handshake { replay_from: 42 };
        assert_eq!(handshake.line(), r#"{"replay_from":42}"#);
        let parsed: Handshake = serde_json::from_str(&handshake.line()).unwrap();
        assert_eq!(parsed, handshake);
    }
}
