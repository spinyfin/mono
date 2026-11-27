//! The service's end of the agent host's socket.
//!
//! The host's wire contract (see the `boss_agent_host` crate docs) is: connect,
//! send one `{"replay_from":N}` line, then read the journal's records, one JSON
//! object per line, from record `N` on; every later line the client sends is a
//! line for the agent. Both directions come back as records, including the
//! client's own lines, so what the service reads is the journal and nothing
//! else.
//!
//! Before it says anything, the service checks who is listening, the way the
//! inbox client does: the socket's directory must be private and owned by the
//! engine's user, and the process at the other end must be the one the caller
//! expects.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use boss_acp_client::framing::{Frame, LineFramer};
use boss_agent_host::Handshake;
use boss_agent_host::journal::WireRecord;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::intent::REPLAY_READ_BUFFER_BYTES;

/// Where the agent host listens and who must be listening.
#[derive(Debug, Clone)]
pub struct HostEndpoint {
    /// The host's Unix socket.
    pub socket_path: PathBuf,
    /// The process that must be at the other end: the coordinator's pane
    /// process, which is the agent host. The service refuses any other peer.
    pub peer_pid: libc::pid_t,
}

/// Why the service could not attach to the host.
#[derive(Debug, thiserror::Error)]
pub(super) enum ConnectError {
    /// The thing at the socket is not the agent host the engine launched.
    /// Retrying cannot change that.
    #[error("{0}")]
    Refused(String),
    /// The host is not reachable right now.
    #[error("connecting to the agent host: {0}")]
    Io(#[from] io::Error),
}

/// Why a connection to the host ended.
#[derive(Debug, thiserror::Error)]
pub(super) enum LinkError {
    #[error("the agent host closed the connection")]
    Closed,
    #[error("reading from the agent host failed: {0}")]
    Read(io::Error),
    #[error("writing to the agent host failed: {0}")]
    Write(io::Error),
    /// The host sent something that is not a journal record.
    #[error("the agent host sent a line that is not a journal record: {0}")]
    Protocol(String),
}

/// One verified connection, past its handshake.
pub(super) struct HostConnection {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    framer: LineFramer,
    chunk: Vec<u8>,
}

impl HostConnection {
    /// Connects, verifies the peer, and asks for the journal from `replay_from`.
    pub(super) async fn connect(endpoint: &HostEndpoint, replay_from: u64) -> Result<Self, ConnectError> {
        if let Some(dir) = endpoint.socket_path.parent()
            && let Some(problem) = private_directory_problem(dir)?
        {
            return Err(ConnectError::Refused(format!(
                "the agent host's socket directory must be private and owned by the engine user, but {problem}"
            )));
        }
        let stream = UnixStream::connect(&endpoint.socket_path).await?;
        verify_peer(&stream, endpoint.peer_pid)?;
        let (reader, mut writer) = stream.into_split();
        writer
            .write_all(format!("{}\n", Handshake { replay_from }.line()).as_bytes())
            .await?;
        Ok(Self {
            reader,
            writer,
            framer: LineFramer::new(),
            chunk: vec![0; REPLAY_READ_BUFFER_BYTES],
        })
    }

    /// The next journal records, at least one. Cancel-safe: bytes read are
    /// kept in the connection, so a read that loses a `select!` loses nothing.
    pub(super) async fn read_records(&mut self) -> Result<Vec<WireRecord>, LinkError> {
        loop {
            let read = self.reader.read(&mut self.chunk).await.map_err(LinkError::Read)?;
            if read == 0 {
                // A final line without its terminator is the host dying
                // mid-write; the journal still has the whole record.
                return Err(LinkError::Closed);
            }
            let mut records = Vec::new();
            for frame in self.framer.push(&self.chunk[..read]) {
                match frame {
                    Frame::Line(line) => records.push(
                        serde_json::from_str::<WireRecord>(&line)
                            .map_err(|err| LinkError::Protocol(err.to_string()))?,
                    ),
                    Frame::Invalid { error, .. } => return Err(LinkError::Protocol(error.to_string())),
                }
            }
            if !records.is_empty() {
                return Ok(records);
            }
        }
    }

    /// Writes one line for the agent. The host journals it and echoes it back
    /// as a record, which is how the reducer sees what the service sent.
    pub(super) async fn write_line(&mut self, line: &str) -> Result<(), LinkError> {
        let mut framed = Vec::with_capacity(line.len() + 1);
        framed.extend_from_slice(line.as_bytes());
        framed.push(b'\n');
        // One write: the host drops a line cut short, never half-delivers it.
        self.writer.write_all(&framed).await.map_err(LinkError::Write)?;
        self.writer.flush().await.map_err(LinkError::Write)
    }
}

/// Why `path` is not a directory owned by the engine's user that no one else
/// can enter or list, or `None` if it is one. Does not follow a symbolic link
/// at `path`.
pub(super) fn private_directory_problem(path: &Path) -> io::Result<Option<String>> {
    let metadata = std::fs::symlink_metadata(path)?;
    let shown = path.display();
    Ok(if !metadata.is_dir() {
        Some(format!("{shown} is not a directory"))
    } else if metadata.uid() != effective_uid() {
        Some(format!("{shown} is owned by uid {}", metadata.uid()))
    } else if metadata.mode() & 0o077 != 0 {
        Some(format!("{shown} has mode {:04o}", metadata.mode() & 0o7777))
    } else {
        None
    })
}

fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn verify_peer(stream: &UnixStream, expected_pid: libc::pid_t) -> Result<(), ConnectError> {
    let credentials = stream.peer_cred()?;
    if credentials.uid() != effective_uid() {
        return Err(ConnectError::Refused(
            "the agent host socket is served by another user".to_owned(),
        ));
    }
    if credentials.pid() != Some(expected_pid) {
        return Err(ConnectError::Refused(format!(
            "the agent host socket is served by process {}, not the coordinator pane process {expected_pid}",
            credentials
                .pid()
                .map_or_else(|| "(unknown)".to_owned(), |pid| pid.to_string()),
        )));
    }
    Ok(())
}

/// The sequence number of the last whole record in the host's journal file,
/// or 0 if the file does not exist or holds none.
///
/// A record being written when this runs is not counted: the host appends a
/// record with one write, and a reader can see its first bytes before its
/// last. The service uses this as the replay boundary, the point it has
/// caught up to after reconnecting; it reads from the end so that finding the
/// boundary of a large journal does not cost a replay of it.
pub(super) fn last_journal_seq(path: &Path) -> io::Result<u64> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let len = file.metadata()?.len();
    // The file's bytes from `start` to `len`, grown from the end in chunks
    // until they hold one whole line.
    let mut tail: Vec<u8> = Vec::new();
    let mut start = len;
    loop {
        if let Some(line) = last_whole_line(&tail, start == 0) {
            return serde_json::from_slice::<WireRecord>(line)
                .map(|record| record.seq)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("journal tail record: {err}")));
        }
        if start == 0 {
            return Ok(0);
        }
        let step = start.min(REPLAY_READ_BUFFER_BYTES as u64);
        start -= step;
        let mut chunk = vec![0; step as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&tail);
        tail = chunk;
    }
}

/// The last newline-terminated line in `bytes`, if `bytes` holds all of it.
/// `from_file_start` says `bytes` begins at the start of the file, so the
/// first line needs no earlier newline to be whole.
fn last_whole_line(bytes: &[u8], from_file_start: bool) -> Option<&[u8]> {
    let end = bytes.iter().rposition(|byte| *byte == b'\n')?;
    let head = &bytes[..end];
    match head.iter().rposition(|byte| *byte == b'\n') {
        Some(previous) => Some(&head[previous + 1..]),
        None if from_file_start => Some(head),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use boss_agent_host::journal::{Direction, Journal};

    use super::*;

    fn journal(dir: &Path, lines: &[String]) -> PathBuf {
        let path = dir.join("wire.jsonl");
        let mut journal = Journal::create(&path).unwrap();
        for line in lines {
            journal.append(Direction::FromAgent, line).unwrap();
        }
        path
    }

    #[test]
    fn a_missing_or_empty_journal_has_no_last_seq() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(last_journal_seq(&dir.path().join("none.jsonl")).unwrap(), 0);
        let empty = journal(dir.path(), &[]);
        assert_eq!(last_journal_seq(&empty).unwrap(), 0);
    }

    #[test]
    fn the_last_seq_is_the_last_whole_record() {
        let dir = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (0..7).map(|n| format!("line {n}")).collect();
        let path = journal(dir.path(), &lines);
        assert_eq!(last_journal_seq(&path).unwrap(), 7);

        // A record cut off mid-write does not count.
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"seq":8,"ts_ms":1,"dir":"from_ag"#).unwrap();
        assert_eq!(last_journal_seq(&path).unwrap(), 7);
    }

    #[test]
    fn a_last_record_longer_than_the_read_chunk_is_found() {
        // Prompts that carry a screenshot are megabytes of base64.
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(REPLAY_READ_BUFFER_BYTES * 3 + 17);
        let path = journal(dir.path(), &["small".to_owned(), big.clone(), big]);
        assert_eq!(last_journal_seq(&path).unwrap(), 3);
    }

    #[test]
    fn a_single_record_that_is_the_whole_file_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal(dir.path(), &["only".to_owned()]);
        assert_eq!(last_journal_seq(&path).unwrap(), 1);
    }

    #[test]
    fn a_private_directory_is_owned_by_the_engine_user_and_closed_to_others() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(private_directory_problem(dir.path()).unwrap(), None);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        let problem = private_directory_problem(dir.path())
            .unwrap()
            .expect("a group-readable directory");
        assert!(problem.contains("mode 0750"), "{problem}");
        let file = dir.path().join("f");
        std::fs::write(&file, b"").unwrap();
        let problem = private_directory_problem(&file)
            .unwrap()
            .expect("a file is not a directory");
        assert!(problem.contains("not a directory"), "{problem}");
    }

    #[test]
    fn a_symlink_to_a_private_directory_is_not_one() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(private_directory_problem(&link).unwrap().is_some());
    }
}
