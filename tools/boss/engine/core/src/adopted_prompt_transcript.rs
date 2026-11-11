//! Disk evidence for a turn whose hooks were lost across an engine restart.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::driver::AgentDriver;

pub(crate) async fn accepted(
    db: &crate::work::WorkDb,
    id: &str,
    workspace: &Path,
    driver: Arc<dyn AgentDriver>,
) -> anyhow::Result<bool> {
    let Some(started) = db.latest_run_started_epoch_for_execution(id)? else {
        anyhow::bail!("missing current-run start time for transcript confirmation");
    };
    let recorded = db
        .resolve_run_row_for_execution(id)?
        .map(|run| db.get_run(&run))
        .transpose()?
        .and_then(|run| run.transcript_path);
    let workspace = workspace.to_path_buf();
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut roots = Vec::new();
        if let Some(root) = driver.transcript_containment_root(&id)? {
            roots.push(root);
        } else if driver.descriptor().name == "claude" {
            let config = std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
                .ok_or_else(|| anyhow::anyhow!("missing Claude config home"))?;
            roots.push(crate::coordinator_handoff::transcript_projects_dir_in_config(
                &config, &workspace,
            ));
        }
        if let Some(path) = recorded {
            roots.push(PathBuf::from(path));
        }
        scan(&roots, driver.as_ref(), started)
    })
    .await?
}

fn scan(roots: &[PathBuf], driver: &dyn AgentDriver, started: i64) -> anyhow::Result<bool> {
    use std::io::Read;
    let mut pending = roots.to_vec();
    let mut visited = 0;
    while let Some(path) = pending.pop() {
        visited += 1;
        anyhow::ensure!(visited <= 4096, "transcript confirmation scan exceeded its file bound");
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                if !entry.file_type()?.is_symlink() {
                    pending.push(entry.path());
                }
            }
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        // Ignore old sessions in a reused workspace. A recent file alone is
        // insufficient: only a timestamped user turn from this run counts.
        if metadata.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs() < started.max(0) as u64 {
            continue;
        }
        const MAX_BYTES: u64 = 16 * 1024 * 1024;
        let mut text = String::new();
        std::fs::File::open(&path)?
            .take(MAX_BYTES + 1)
            .read_to_string(&mut text)?;
        anyhow::ensure!(
            text.len() as u64 <= MAX_BYTES,
            "transcript exceeds confirmation read bound"
        );
        let events = crate::driver_transcript::parse_transcript_with_driver(Some(driver), &text);
        if events.iter().any(|event| {
            matches!(&event.kind, crate::transcript_markdown::TranscriptEventKind::UserText(text) if !text.trim().is_empty())
                && event.timestamp.as_deref()
                    .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                    .is_some_and(|at| at.timestamp() >= started)
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_turn_survives_lost_hooks_but_old_turns_and_session_start_do_not_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let driver = crate::driver::ClaudeDriver;
        let started = 1577836800; // 2020-01-01T00:00:00Z
        for (text, expected) in [
            (
                r#"{"type":"system","timestamp":"2020-01-01T00:00:01Z","subtype":"init"}"#,
                false,
            ),
            (
                r#"{"type":"user","timestamp":"2019-12-31T23:59:59Z","message":{"role":"user","content":"old brief"}}"#,
                false,
            ),
            (
                r#"{"type":"user","timestamp":"2020-01-01T00:00:01Z","message":{"role":"user","content":"brief"}}"#,
                true,
            ),
        ] {
            std::fs::write(&path, format!("{text}\n")).unwrap();
            assert_eq!(scan(std::slice::from_ref(&path), &driver, started).unwrap(), expected);
        }
    }
}
