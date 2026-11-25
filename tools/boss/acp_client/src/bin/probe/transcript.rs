//! Checks on the files Claude Code keeps for a session that ACP hosts: its
//! transcript and its cross-session inbox registration.
//!
//! Both live under Claude Code's own configuration directory. The probe only
//! reads them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use boss_transcript_markdown::{RenderOpts, TranscriptEventKind, parse_transcript_checked, render_text};
use clap::Args;
use serde_json::{Value, json};

#[derive(Args)]
pub struct TranscriptArgs {
    /// The session directory the conversation ran in.
    #[arg(long)]
    cwd: PathBuf,
    /// The session id; Claude Code names the transcript `<session>.jsonl`.
    #[arg(long)]
    session: String,
    /// Claude Code's configuration directory. Defaults to `$CLAUDE_CONFIG_DIR`, then `~/.claude`.
    #[arg(long)]
    config_dir: Option<PathBuf>,
}

/// Claude Code's configuration directory for this user.
fn claude_config_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        return Ok(explicit.to_owned());
    }
    if let Some(configured) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(configured));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".claude"))
}

/// Where Claude Code keeps the transcripts of sessions run in `cwd`: every
/// character of the path that is not an ASCII letter or digit becomes `-`.
///
/// The engine derives the coordinator's transcript directory by the same rule
/// (`transcript_projects_dir_in_config` in `engine/core`); it is restated here
/// because the probe must not link the engine.
fn transcript_dir(config: &Path, cwd: &Path) -> PathBuf {
    let encoded: String = cwd
        .to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    config.join("projects").join(encoded)
}

pub fn check(args: &TranscriptArgs) -> Result<()> {
    let config = claude_config_dir(args.config_dir.as_deref())?;
    // Claude Code resolves symlinks in the working directory before encoding it.
    let cwd = std::fs::canonicalize(&args.cwd).unwrap_or_else(|_| args.cwd.clone());
    let directory = transcript_dir(&config, &cwd);
    let path = directory.join(format!("{}.jsonl", args.session));
    let report = match std::fs::read_to_string(&path) {
        Ok(content) => summarize(&content),
        Err(error) => json!({"found": false, "error": error.to_string()}),
    };
    let mut report = report;
    report["expected_directory"] = json!(directory);
    report["transcript"] = json!(path);
    report["other_files_in_directory"] = json!(
        std::fs::read_dir(&directory)
            .map(|entries| entries.filter_map(|entry| entry.ok()).count())
            .unwrap_or(0)
    );
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Parses a transcript with Boss's parser and counts what it understood.
fn summarize(content: &str) -> Value {
    let mut record_types: BTreeMap<String, usize> = BTreeMap::new();
    let mut unreadable_lines = 0;
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<Value>(line) {
            Ok(record) => {
                let kind = record.get("type").and_then(Value::as_str).unwrap_or("(no type)");
                *record_types.entry(kind.to_owned()).or_default() += 1;
            }
            Err(_) => unreadable_lines += 1,
        }
    }
    let lines = content.lines().filter(|line| !line.trim().is_empty()).count();
    match parse_transcript_checked(content) {
        Err(unrecognized) => json!({
            "found": true,
            "lines": lines,
            "record_types": record_types,
            "unreadable_lines": unreadable_lines,
            "parsed": false,
            "parse_error": unrecognized.to_string(),
        }),
        Ok(events) => {
            let mut event_kinds: BTreeMap<&str, usize> = BTreeMap::new();
            for event in &events {
                let kind = match &event.kind {
                    TranscriptEventKind::UserText(_) => "user_text",
                    TranscriptEventKind::AssistantText(_) => "assistant_text",
                    TranscriptEventKind::Thinking(_) => "thinking",
                    TranscriptEventKind::ToolUse { .. } => "tool_use",
                    TranscriptEventKind::ToolResult { .. } => "tool_result",
                    TranscriptEventKind::System { .. } => "system",
                };
                *event_kinds.entry(kind).or_default() += 1;
            }
            json!({
                "found": true,
                "lines": lines,
                "record_types": record_types,
                "unreadable_lines": unreadable_lines,
                "parsed": true,
                "events": events.len(),
                "event_kinds": event_kinds,
                "rendered_characters": render_text(&events, &RenderOpts::default()).chars().count(),
            })
        }
    }
}

/// Whether the Claude Code process `pid` registered a cross-session inbox.
///
/// Claude Code writes `sessions/<pid>.json` for a session that other local
/// sessions can message; the engine reads the same file to deliver to the
/// terminal coordinator. Only the shape is reported, never the socket path.
pub fn inbox_registration(pid: i32, command: &str) -> Value {
    let path = match claude_config_dir(None) {
        Ok(config) => config.join("sessions").join(format!("{pid}.json")),
        Err(error) => return json!({"pid": pid, "command": command, "error": error.to_string()}),
    };
    let registration = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    match registration {
        None => json!({"pid": pid, "command": command, "registered": false}),
        Some(registration) => json!({
            "pid": pid,
            "command": command,
            "registered": true,
            "keys": registration.as_object().map(|object| object.keys().cloned().collect::<Vec<_>>()),
            "peer_protocol": registration.get("peerProtocol"),
            "has_messaging_socket": registration.get("messagingSocketPath").is_some_and(|path| !path.is_null()),
            "messaging_socket_exists": registration
                .get("messagingSocketPath")
                .and_then(Value::as_str)
                .map(|socket| Path::new(socket).exists()),
            "session_id": registration.get("sessionId"),
            "kind": registration.get("kind"),
            "entrypoint": registration.get("entrypoint"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transcript_directory_replaces_every_non_alphanumeric_character() {
        assert_eq!(
            transcript_dir(
                Path::new("/Users/someone/.claude"),
                Path::new("/private/tmp/boss probe/session_1")
            ),
            PathBuf::from("/Users/someone/.claude/projects/-private-tmp-boss-probe-session-1")
        );
    }

    #[test]
    fn a_claude_code_transcript_is_parsed_and_counted_by_kind() {
        let transcript = [
            r#"{"type":"user","message":{"role":"user","content":"What is 2+2?"},"timestamp":"2026-11-25T20:00:00Z"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","model":"claude-opus-5-5","content":[{"type":"text","text":"4"}]},"timestamp":"2026-11-25T20:00:01Z"}"#,
            "this line is not JSON",
        ]
        .join("\n");
        let report = summarize(&transcript);
        assert_eq!(report["found"], true);
        assert_eq!(report["parsed"], true);
        assert_eq!(report["lines"], 3);
        assert_eq!(report["unreadable_lines"], 1);
        assert_eq!(report["record_types"], json!({"assistant": 1, "user": 1}));
        assert_eq!(report["event_kinds"], json!({"assistant_text": 1, "user_text": 1}));
    }

    #[test]
    fn a_process_with_no_registration_file_is_reported_as_unregistered() {
        // No real process has this pid, so no session file can exist for it.
        let report = inbox_registration(i32::MAX, "claude");
        assert_eq!(report["registered"], false);
        assert_eq!(report["pid"], i32::MAX);
    }
}
