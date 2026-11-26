//! The wire journal: every line that crossed the transport, in order.
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
//! - `dir` names the receiver's side of the stream, not the writer of the
//!   journal, so the value means the same thing whoever holds the journal.
//! - `line` is the line exactly as it was sent or received, never a
//!   re-serialised copy, so request ids and field order survive. The one
//!   exception is a record carrying `invalid`: the received bytes were not a
//!   usable line, so `line` is only a bounded, lossy preview of them and the
//!   record is never read as a message.
//! - `invalid` is absent for every ordinary record. When present it names why
//!   the framer rejected the bytes: `invalid_utf8`, `too_long` or
//!   `unterminated_at_eof`.

use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::schema::RequestId;

/// Which way a line travelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Client to agent.
    ToAgent,
    /// Agent to client.
    FromAgent,
}

/// One journalled line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRecord {
    pub seq: u64,
    pub ts_ms: u64,
    pub dir: Direction,
    pub line: String,
    /// Why the received bytes were unusable, when `line` is only a preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid: Option<String>,
}

/// The single ordered writer of a journal.
///
/// Appends from both directions go through one lock, so `seq` order is the
/// order the lines were handled in.
pub struct Journal {
    state: Mutex<State>,
}

struct State {
    next_seq: u64,
    sink: Box<dyn Write + Send>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal").finish_non_exhaustive()
    }
}

impl Journal {
    /// Creates (or truncates) the journal file, readable by its owner only:
    /// the lines hold prompts, local paths and whatever the agent printed.
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        Ok(Self::to_writer(file))
    }

    /// A journal over any sink. Each record is one `write_all`.
    pub fn to_writer(sink: impl Write + Send + 'static) -> Self {
        Self {
            state: Mutex::new(State {
                next_seq: 1,
                sink: Box::new(sink),
            }),
        }
    }

    /// Appends one line and returns the sequence number it was given.
    pub fn append(&self, dir: Direction, line: &str) -> io::Result<u64> {
        self.append_record(dir, line, None)
    }

    /// Appends a preview of bytes that were not a usable line, marked so a
    /// reader never mistakes it for a message that was received.
    pub fn append_invalid(&self, dir: Direction, preview: &str, kind: &str) -> io::Result<u64> {
        self.append_record(dir, preview, Some(kind.to_owned()))
    }

    fn append_record(&self, dir: Direction, line: &str, invalid: Option<String>) -> io::Result<u64> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let seq = state.next_seq;
        let record = WireRecord {
            seq,
            ts_ms: now_ms(),
            dir,
            line: line.to_owned(),
            invalid,
        };
        let mut encoded = serde_json::to_vec(&record).map_err(io::Error::other)?;
        encoded.push(b'\n');
        state.sink.write_all(&encoded)?;
        state.sink.flush()?;
        state.next_seq += 1;
        Ok(seq)
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
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<WireRecord>(&line) {
            Ok(record) => records.push(record),
            Err(_) if lines.peek().is_none() => break,
            Err(error) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("journal record after seq {}: {error}", records.len()),
                ));
            }
        }
    }
    Ok(records)
}

/// A request that the journal shows was published and never answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnansweredRequest {
    /// Who sent the request: [`Direction::ToAgent`] is one of ours.
    pub dir: Direction,
    pub id: RequestId,
    pub method: String,
    /// The journal record that carried the request.
    pub seq: u64,
}

/// Finds the requests in `records` that have no response yet, in the order
/// they were sent.
///
/// This is what a client that attaches to a stream part-way through needs
/// before it reads on: its own unanswered requests (so the responses, when
/// they arrive, are recognised and not dropped) and the agent's unanswered
/// requests (a permission prompt that is still waiting).
pub fn unanswered_requests<'a>(records: impl IntoIterator<Item = &'a WireRecord>) -> Vec<UnansweredRequest> {
    let mut open: Vec<UnansweredRequest> = Vec::new();
    for record in records {
        if record.invalid.is_some() {
            continue;
        }
        let Ok(Value::Object(message)) = serde_json::from_str::<Value>(&record.line) else {
            continue;
        };
        let Some(id) = message
            .get("id")
            .and_then(|id| serde_json::from_value::<RequestId>(id.clone()).ok())
        else {
            continue;
        };
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            open.push(UnansweredRequest {
                dir: record.dir,
                id,
                method: method.to_owned(),
                seq: record.seq,
            });
        } else if message.contains_key("result") || message.contains_key("error") {
            // A response travels the opposite way to its request.
            open.retain(|request| request.id != id || request.dir == record.dir);
        }
    }
    open
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn record(seq: u64, dir: Direction, line: &str) -> WireRecord {
        WireRecord {
            seq,
            ts_ms: 0,
            dir,
            line: line.to_owned(),
            invalid: None,
        }
    }

    #[test]
    fn records_keep_the_line_verbatim_and_number_from_one() {
        let buffer = SharedBuffer::default();
        let journal = Journal::to_writer(buffer.clone());
        let sent = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"z":1,"a":"é\n"}}"#;
        assert_eq!(journal.append(Direction::ToAgent, sent).unwrap(), 1);
        assert_eq!(journal.append(Direction::FromAgent, "not json at all").unwrap(), 2);

        let bytes = buffer.0.lock().unwrap().clone();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.lines().count(), 2, "one record per line: {text}");
        assert!(
            text.starts_with(r#"{"seq":1,"ts_ms":"#),
            "field order is the documented shape: {text}"
        );
        assert!(text.contains(r#""dir":"to_agent""#) && text.contains(r#""dir":"from_agent""#));

        let records = read_records(text.as_bytes()).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].line, sent);
        assert_eq!(records[1].line, "not json at all");
        assert!(records[0].ts_ms > 1_700_000_000_000, "wall-clock milliseconds");
    }

    #[test]
    fn a_torn_final_record_is_ignored_but_a_torn_middle_record_is_an_error() {
        let whole = serde_json::to_string(&record(1, Direction::ToAgent, "a")).unwrap();
        let torn = &whole[..whole.len() - 4];
        assert_eq!(read_records(format!("{whole}\n{torn}").as_bytes()).unwrap().len(), 1);
        assert!(read_records(format!("{torn}\n{whole}\n").as_bytes()).is_err());
    }

    #[test]
    fn an_invalid_frame_preview_is_marked_and_never_answers_a_request() {
        let buffer = SharedBuffer::default();
        let journal = Journal::to_writer(buffer.clone());
        journal
            .append(
                Direction::ToAgent,
                r#"{"jsonrpc":"2.0","id":1,"method":"session/prompt"}"#,
            )
            .unwrap();
        // A lossy preview of a line with a bad byte inside a string parses as
        // a response, but the connection never delivered it to the waiter.
        journal
            .append_invalid(
                Direction::FromAgent,
                r#"{"jsonrpc":"2.0","id":1,"result":"\u{fffd}"}"#,
                "invalid_utf8",
            )
            .unwrap();
        let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert!(text.lines().next().unwrap().find("invalid").is_none());
        assert!(text.lines().nth(1).unwrap().contains(r#""invalid":"invalid_utf8""#));
        let records = read_records(text.as_bytes()).unwrap();
        assert_eq!(records[1].invalid.as_deref(), Some("invalid_utf8"));
        let open = unanswered_requests(&records);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].method, "session/prompt");
    }

    #[test]
    fn unanswered_requests_are_the_ones_without_a_response_in_either_direction() {
        let records = [
            record(
                1,
                Direction::ToAgent,
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            ),
            record(2, Direction::FromAgent, r#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            record(
                3,
                Direction::ToAgent,
                r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt"}"#,
            ),
            record(
                4,
                Direction::FromAgent,
                r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#,
            ),
            // The agent numbers its own requests independently: its id 1 is
            // not our id 1, and its id 2 is not our unanswered prompt.
            record(
                5,
                Direction::FromAgent,
                r#"{"jsonrpc":"2.0","id":1,"method":"session/request_permission"}"#,
            ),
            record(
                6,
                Direction::FromAgent,
                r#"{"jsonrpc":"2.0","id":2,"method":"session/request_permission"}"#,
            ),
            record(
                7,
                Direction::ToAgent,
                r#"{"jsonrpc":"2.0","id":1,"result":{"outcome":{"outcome":"cancelled"}}}"#,
            ),
            record(8, Direction::FromAgent, "garbage that is not JSON"),
        ];
        assert_eq!(
            unanswered_requests(&records),
            [
                UnansweredRequest {
                    dir: Direction::ToAgent,
                    id: RequestId::Number(2),
                    method: "session/prompt".into(),
                    seq: 3,
                },
                UnansweredRequest {
                    dir: Direction::FromAgent,
                    id: RequestId::Number(2),
                    method: "session/request_permission".into(),
                    seq: 6,
                },
            ]
        );
    }

    #[test]
    fn an_error_response_also_answers_its_request() {
        let records = [
            record(
                1,
                Direction::ToAgent,
                r#"{"jsonrpc":"2.0","id":"a","method":"session/prompt"}"#,
            ),
            record(
                2,
                Direction::FromAgent,
                r#"{"jsonrpc":"2.0","id":"a","error":{"code":-32603,"message":"x"}}"#,
            ),
        ];
        assert!(unanswered_requests(&records).is_empty());
    }
}
