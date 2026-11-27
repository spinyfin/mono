//! Byte-delimited, torn-tail-tolerant JSONL scanning shared by the intent log
//! and the wire journal readers.
//!
//! A crash or power loss can cut the final record off anywhere, including in
//! the middle of a multi-byte UTF-8 character, so lines are read as raw bytes
//! and validated afterwards. A damaged *final* record is dropped; damage
//! anywhere else is a hard error.

use std::io::{self, BufRead};

use serde::de::DeserializeOwned;

/// Records parsed from a JSONL stream plus where the valid prefix ends.
#[derive(Debug)]
pub(crate) struct JsonlScan<T> {
    /// Successfully parsed records, in file order.
    pub(crate) records: Vec<T>,
    /// Byte offset immediately after the last valid record (including its
    /// newline when it has one). Anything past this is a torn tail.
    pub(crate) valid_end: u64,
    /// True when the final valid record parsed but lacks its trailing newline,
    /// so a writer must append `\n` at `valid_end` before the next record.
    pub(crate) missing_final_newline: bool,
}

/// Scans `reader` as JSONL of `T`, tolerating a torn final record.
///
/// `label` names the stream in error messages (for example `"intent log"`).
pub(crate) fn scan_jsonl<T: DeserializeOwned>(mut reader: impl BufRead, label: &str) -> io::Result<JsonlScan<T>> {
    let mut records = Vec::new();
    let mut buf = Vec::new();
    let mut offset: u64 = 0;
    let mut valid_end: u64 = 0;

    loop {
        buf.clear();
        let read = reader.read_until(b'\n', &mut buf)?;
        if read == 0 {
            break;
        }
        let line_start = offset;
        offset += read as u64;
        let has_newline = buf.ends_with(b"\n");

        let Ok(line) = std::str::from_utf8(&buf) else {
            if !has_newline || reader_at_eof(&mut reader)? {
                // Torn final record (cut off mid-UTF-8 sequence).
                break;
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{label} contains invalid UTF-8 at byte offset {line_start}"),
            ));
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            if has_newline {
                valid_end = offset;
            }
            continue;
        }

        match serde_json::from_str::<T>(trimmed) {
            Ok(record) => {
                records.push(record);
                valid_end = offset;
                if !has_newline {
                    return Ok(JsonlScan {
                        records,
                        valid_end,
                        missing_final_newline: true,
                    });
                }
            }
            Err(_) if !has_newline || reader_at_eof(&mut reader)? => break,
            Err(err) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{label} record after seq {}: {err}", records.len()),
                ));
            }
        }
    }

    Ok(JsonlScan {
        records,
        valid_end,
        missing_final_newline: false,
    })
}

fn reader_at_eof(reader: &mut impl BufRead) -> io::Result<bool> {
    Ok(reader.fill_buf()?.is_empty())
}
