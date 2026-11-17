//! Esc emitted no Stop in all four pane-study trials. Use only the private
//! --log-file cancellation message; permission denial shares the pane's
//! "Interrupted" text (57/57 trials) and transcripts do not record cancellation.
//! The engine owns the bounded observer and distinctly logged fallback.

use std::time::Duration;

use super::home::log_file_for_run;
use crate::InterruptRecoverySnapshot;

// Measured log latency was <50 ms. Leave scheduling/flush headroom.
pub(super) const SETTLE_WINDOW: Duration = Duration::from_secs(8);

pub(super) fn prepare_snapshot(run_id: &str, claimed_id: Option<&str>) -> Option<InterruptRecoverySnapshot> {
    let session_id = claimed_id.filter(|id| !id.is_empty())?;
    let events_path = log_file_for_run(run_id).ok()?;
    // Require a provisioned control directory. An absent log is OK: it can
    // first appear between this snapshot and the post-Esc observer.
    if !events_path.parent()?.is_dir() {
        return None;
    }
    let offset = match std::fs::metadata(&events_path) {
        Ok(meta) if meta.is_file() => meta.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        _ => return None,
    };
    Some(InterruptRecoverySnapshot {
        events_path,
        offset,
        session_id: session_id.to_owned(),
        settle_window: SETTLE_WINDOW,
    })
}

/// Match the measured Go log message, retaining the entire conversation id.
/// A child cancellation or an id prefix must never confirm the parent.
pub(super) fn cancelled_conversation(line: &str) -> Option<&str> {
    let (prefix, id) = line.split_once("Cancelling in-progress response for conversation ")?;
    // Require a Go source location immediately before the message, without
    // depending on the filename used by a particular CLI version.
    let location = prefix.split_whitespace().last()?.strip_suffix(']')?;
    let (file, line_number) = location.rsplit_once(':')?;
    if !file.ends_with(".go")
        || file.len() <= 3
        || line_number.is_empty()
        || !line_number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let id = id.trim();
    if id.len() != 36
        || !id.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return None;
    }
    Some(id)
}

#[cfg(test)]
#[path = "turn_end_recovery_tests.rs"]
mod tests;
