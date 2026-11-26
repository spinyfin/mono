//! The wire journal: every line that crossed the host, in order.
//!
//! One record per line, as one line of JSON:
//!
//! ```json
//! {"seq":7,"ts_ms":1795000000123,"dir":"from_agent","line":"{\"jsonrpc\":\"2.0\",...}"}
//! ```
//!
//! - `seq` starts at 1 and is the only ordering authority.
//! - `ts_ms` is wall-clock milliseconds since the Unix epoch, so a record can
//!   be lined up with other logs. Wall clocks step; never order by it.
//! - `dir` names the receiver's side of the stream, so the value means the
//!   same thing to whoever reads the journal.
//! - `line` is the line exactly as it was sent or received, without its line
//!   terminator, never a re-serialised copy.
//!
//! The file is append-only and the host keeps an index of where each record
//! starts, so a replay is a byte-range copy of the file: a client receives
//! the same bytes a journal reader sees.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Which way a line travelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Client to child.
    ToAgent,
    /// Child to client.
    FromAgent,
}

/// One journalled line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRecord {
    pub seq: u64,
    pub ts_ms: u64,
    pub dir: Direction,
    pub line: String,
}

/// A record just written: its decoded form and the exact bytes appended to
/// the file (record JSON plus the newline).
#[derive(Debug, Clone)]
pub struct Appended {
    pub record: WireRecord,
    pub encoded: Arc<[u8]>,
}

/// A byte range of the journal file holding a run of whole records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayRange {
    pub start: u64,
    pub end: u64,
}

/// The single ordered writer of a journal.
///
/// Appends from both directions go through the one owner of this value, so
/// `seq` order is the order the lines were handled in.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    /// Written sequentially by this one owner; readers use their own handle.
    file: File,
    /// Byte offset of the start of record `seq`, at index `seq - 1`.
    offsets: Vec<u64>,
    len: u64,
    failed: bool,
}

impl Journal {
    /// Creates (or truncates) the journal file, readable by its owner only:
    /// the lines hold prompts, local paths and whatever the child printed.
    ///
    /// A journal belongs to one host. A file left by an earlier host in the
    /// same place is that host's generation and is not appended to: its
    /// sequence numbers would collide with this one's.
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            offsets: Vec::new(),
            len: 0,
            failed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The sequence number the next record will get.
    pub fn next_seq(&self) -> u64 {
        self.offsets.len() as u64 + 1
    }

    /// Appends one line. Once an append has failed the file may end in a
    /// partial record, so every later append fails too rather than writing
    /// after it.
    pub fn append(&mut self, dir: Direction, line: &str) -> io::Result<Appended> {
        if self.failed {
            return Err(io::Error::other("journal is unusable after an earlier write failure"));
        }
        let record = WireRecord {
            seq: self.next_seq(),
            ts_ms: now_ms(),
            dir,
            line: line.to_owned(),
        };
        let mut encoded = serde_json::to_vec(&record).map_err(io::Error::other)?;
        encoded.push(b'\n');
        if let Err(err) = self.file.write_all(&encoded) {
            self.failed = true;
            return Err(err);
        }
        self.offsets.push(self.len);
        self.len += encoded.len() as u64;
        Ok(Appended {
            record,
            encoded: encoded.into(),
        })
    }

    /// The bytes holding every record with `seq >= from_seq` written so far.
    /// A `from_seq` of 0 or 1 is the whole journal; one past the last record
    /// is empty.
    pub fn replay_range(&self, from_seq: u64) -> ReplayRange {
        let index = from_seq.max(1) - 1;
        match self.offsets.get(usize::try_from(index).unwrap_or(usize::MAX)) {
            Some(start) => ReplayRange {
                start: *start,
                end: self.len,
            },
            None => ReplayRange {
                start: self.len,
                end: self.len,
            },
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Reads a journal back. A final line that was cut off mid-write is ignored;
/// any other unreadable line is an error.
pub fn read_records(reader: impl BufRead) -> io::Result<Vec<WireRecord>> {
    let mut records = Vec::new();
    let mut lines = reader.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<WireRecord>(&line) {
            Ok(record) => records.push(record),
            Err(_) if lines.peek().is_none() => break,
            Err(err) => return Err(io::Error::new(io::ErrorKind::InvalidData, err)),
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal() -> (tempfile::TempDir, Journal) {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::create(&dir.path().join("wire.jsonl")).unwrap();
        (dir, journal)
    }

    #[test]
    fn sequence_numbers_start_at_one_and_follow_append_order() {
        let (_dir, mut journal) = journal();
        let a = journal.append(Direction::ToAgent, "a").unwrap();
        let b = journal.append(Direction::FromAgent, "b").unwrap();
        assert_eq!((a.record.seq, b.record.seq), (1, 2));
        assert_eq!(journal.next_seq(), 3);
    }

    #[test]
    fn the_file_holds_the_appended_bytes_and_reads_back() {
        let (_dir, mut journal) = journal();
        let a = journal.append(Direction::ToAgent, "{\"id\":1}").unwrap();
        let b = journal.append(Direction::FromAgent, "tab\there \"quoted\"").unwrap();
        let on_disk = std::fs::read(journal.path()).unwrap();
        let mut expected = a.encoded.to_vec();
        expected.extend_from_slice(&b.encoded);
        assert_eq!(on_disk, expected);
        let records = read_records(on_disk.as_slice()).unwrap();
        assert_eq!(records, vec![a.record, b.record]);
    }

    #[test]
    fn replay_range_starts_at_the_requested_record() {
        let (_dir, mut journal) = journal();
        let mut appended = Vec::new();
        for n in 0..4 {
            appended.push(journal.append(Direction::FromAgent, &format!("line {n}")).unwrap());
        }
        let on_disk = std::fs::read(journal.path()).unwrap();
        let slice = |range: ReplayRange| on_disk[range.start as usize..range.end as usize].to_vec();

        let from_third = slice(journal.replay_range(3));
        let mut expected = appended[2].encoded.to_vec();
        expected.extend_from_slice(&appended[3].encoded);
        assert_eq!(from_third, expected);

        assert_eq!(slice(journal.replay_range(0)), on_disk);
        assert_eq!(slice(journal.replay_range(1)), on_disk);
        assert!(slice(journal.replay_range(5)).is_empty());
        assert!(slice(journal.replay_range(u64::MAX)).is_empty());
    }

    #[test]
    fn a_truncated_final_record_is_ignored_on_read() {
        let (_dir, mut journal) = journal();
        let a = journal.append(Direction::ToAgent, "a").unwrap();
        let mut bytes = a.encoded.to_vec();
        bytes.extend_from_slice(b"{\"seq\":2,\"ts_ms\":1,\"dir\":\"to_a");
        assert_eq!(read_records(bytes.as_slice()).unwrap(), vec![a.record]);
    }

    #[test]
    fn a_corrupt_record_before_the_end_is_an_error() {
        let bytes = b"not json\n{\"seq\":1,\"ts_ms\":1,\"dir\":\"to_agent\",\"line\":\"x\"}\n";
        assert!(read_records(bytes.as_slice()).is_err());
    }

    #[test]
    fn the_file_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, journal) = journal();
        let mode = std::fs::metadata(journal.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn creating_over_an_old_journal_starts_a_new_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");
        let mut first = Journal::create(&path).unwrap();
        first.append(Direction::ToAgent, "old").unwrap();
        let mut second = Journal::create(&path).unwrap();
        let appended = second.append(Direction::ToAgent, "new").unwrap();
        assert_eq!(appended.record.seq, 1);
        assert_eq!(std::fs::read(&path).unwrap(), appended.encoded.to_vec());
    }
}
