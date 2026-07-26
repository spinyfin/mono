//! `bossctl logs` — read the engine's on-disk logs and diagnostic streams.
//!
//! All log-path resolution, rotated-segment / day-file enumeration, and the
//! structured query/filter machinery live in `boss-log-files`, the single
//! source of truth shared with the engine. This module is the CLI
//! orchestration: flag → filter, print, truncation notice, and `--follow`.
//!
//! Operation is **file-scan-only** — no engine RPC, no socket. Diagnosing a
//! wedged engine is precisely when this verb is needed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use boss_log_files::{
    LogFilter, now_epoch_ms, parse_time_spec, query_log_files, read_new_content_filtered, resolve_log_source_files,
    resolve_log_source_path, truncation_notice,
};

use super::{LogSource, resolve_state_root};

/// Map the CLI's [`LogSource`] onto the shared crate's enum.
fn shared_source(source: &LogSource) -> boss_log_files::LogSource {
    match source {
        LogSource::Engine => boss_log_files::LogSource::EngineTrace,
        LogSource::Audit => boss_log_files::LogSource::Audit,
        LogSource::Dispatch => boss_log_files::LogSource::Dispatch,
        LogSource::Spawn => boss_log_files::LogSource::Spawn,
        LogSource::PopulationTiming => boss_log_files::LogSource::PopulationTiming,
    }
}

/// CLI query options collected from `bossctl logs` flags.
///
/// Builder-equipped (more than five fields) so additive CLI flags do not
/// force every construction site to list every field.
#[derive(Debug, Clone, Default, bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct LogsQuery {
    #[builder(default)]
    pub tail: usize,
    pub grep: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub target: Option<String>,
    pub level: Option<String>,
    /// Raw `key=value` pairs from repeated `--field` flags.
    #[builder(default)]
    pub fields: Vec<String>,
    pub execution_id: Option<String>,
}

impl LogsQuery {
    /// Build a [`LogFilter`] from CLI flags, parsing time specs against now.
    pub(crate) fn to_filter(&self) -> Result<LogFilter> {
        let now = now_epoch_ms();
        let since_ms = self
            .since
            .as_deref()
            // date-only → start of that UTC day
            .map(|s| parse_time_spec(s, now, true, false))
            .transpose()
            .context("parsing --since")?;
        let until_ms = self
            .until
            .as_deref()
            // date-only → inclusive end of that UTC day
            .map(|s| parse_time_spec(s, now, true, true))
            .transpose()
            .context("parsing --until")?;
        if let (Some(s), Some(u)) = (since_ms, until_ms)
            && s > u
        {
            bail!("--since ({s} ms) is after --until ({u} ms)");
        }

        let mut fields: Vec<(String, String)> = Vec::new();
        for raw in &self.fields {
            fields.push(parse_field_pair(raw)?);
        }

        Ok(LogFilter {
            grep: self.grep.clone(),
            target: self.target.clone(),
            level: self.level.clone(),
            fields,
            execution_or_run_id: self.execution_id.clone(),
            since_ms,
            until_ms,
        })
    }
}

fn parse_field_pair(raw: &str) -> Result<(String, String)> {
    let (k, v) = raw
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("invalid --field `{raw}`: expected key=value"))?;
    let k = k.trim();
    let v = v.trim();
    if k.is_empty() {
        bail!("invalid --field `{raw}`: empty key");
    }
    Ok((k.to_owned(), v.to_owned()))
}

fn display_label(source: boss_log_files::LogSource, root: &Path, scanned: &[PathBuf]) -> String {
    let primary = resolve_log_source_path(source, root);
    if scanned.len() <= 1 {
        return primary.display().to_string();
    }
    format!(
        "{} ({} files, including rotated/day history)",
        primary.display(),
        scanned.len()
    )
}

/// Live path to poll under `--follow`. For day-rotated sources this is the
/// newest day file (or the diagnostics directory placeholder if none exist).
fn follow_live_path(source: boss_log_files::LogSource, root: &Path, paths: &[PathBuf]) -> PathBuf {
    match source {
        boss_log_files::LogSource::Spawn | boss_log_files::LogSource::PopulationTiming => paths
            .last()
            .cloned()
            .unwrap_or_else(|| resolve_log_source_path(source, root)),
        _ => resolve_log_source_path(source, root),
    }
}

pub(crate) fn logs_tail(json: bool, source: LogSource, state_root: Option<PathBuf>, query: LogsQuery) -> Result<()> {
    let root = resolve_state_root(state_root)?;
    let shared = shared_source(&source);
    let paths = resolve_log_source_files(shared, &root);
    let filter = query.to_filter()?;
    let result = query_log_files(&paths, &filter, query.tail)?;

    if json {
        // Emit original JSONL records (one per line) so `jq` pipelines keep
        // working. Truncation is reported on stderr, never mixed into stdout.
        for line in &result.lines {
            println!("{line}");
        }
    } else if result.lines.is_empty() {
        let label = display_label(shared, &root, &result.paths_scanned);
        eprintln!("==> {label} <== (no matching lines)");
    } else {
        let label = display_label(shared, &root, &result.paths_scanned);
        eprintln!("==> {label} <==");
        for line in &result.lines {
            println!("{line}");
        }
    }

    if let Some(notice) = truncation_notice(&result, query.tail) {
        eprintln!("{notice}");
    }
    Ok(())
}

pub(crate) async fn logs_follow(source: LogSource, state_root: Option<PathBuf>, query: LogsQuery) -> Result<()> {
    let root = resolve_state_root(state_root)?;
    let shared = shared_source(&source);
    let paths = resolve_log_source_files(shared, &root);
    let filter = query.to_filter()?;

    // Initial tail (same query as non-follow), including truncation notice.
    let result = query_log_files(&paths, &filter, query.tail)?;
    if !result.lines.is_empty() {
        let label = display_label(shared, &root, &result.paths_scanned);
        eprintln!("==> {label} <==");
        for line in &result.lines {
            println!("{line}");
        }
    }
    if let Some(notice) = truncation_notice(&result, query.tail) {
        eprintln!("{notice}");
    }

    let mut follow_path = follow_live_path(shared, &root, &paths);
    let mut pos: u64 = std::fs::metadata(&follow_path).map(|m| m.len()).unwrap_or(0);
    eprintln!("==> (following — Ctrl-C to stop) <==");

    // Follow ignores --since/--until (everything new is "now") but keeps
    // structured filters and grep.
    let follow_filter = LogFilter {
        since_ms: None,
        until_ms: None,
        ..filter
    };

    loop {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        // Day-rotated sources: pick up a new day file after midnight.
        if matches!(
            shared,
            boss_log_files::LogSource::Spawn | boss_log_files::LogSource::PopulationTiming
        ) {
            let refreshed = resolve_log_source_files(shared, &root);
            if let Some(newest) = refreshed.last()
                && newest != &follow_path
            {
                follow_path = newest.clone();
                pos = 0;
            }
        }

        match std::fs::metadata(&follow_path) {
            Ok(m) => {
                let new_len = m.len();
                if new_len < pos {
                    // File was rotated or truncated; reset so we catch the new content.
                    pos = 0;
                }
                if new_len > pos {
                    match read_new_content_filtered(&follow_path, pos, &follow_filter) {
                        Ok((lines, new_pos)) => {
                            for line in lines {
                                println!("{line}");
                            }
                            pos = new_pos;
                        }
                        Err(err) => {
                            eprintln!("bossctl: error reading {}: {err}", follow_path.display());
                        }
                    }
                }
            }
            Err(_) => {
                // File disappeared (e.g. mid-rotation); reset so we read from start when it reappears.
                pos = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_field_pair_splits_on_first_eq() {
        assert_eq!(
            parse_field_pair("execution_id=exec_abc").unwrap(),
            ("execution_id".into(), "exec_abc".into())
        );
        assert_eq!(parse_field_pair("msg=a=b=c").unwrap(), ("msg".into(), "a=b=c".into()));
    }

    #[test]
    fn parse_field_pair_rejects_missing_eq() {
        assert!(parse_field_pair("nope").is_err());
    }

    #[test]
    fn to_filter_maps_execution_id() {
        let q = LogsQuery {
            execution_id: Some("exec_1".into()),
            ..Default::default()
        };
        let f = q.to_filter().unwrap();
        assert_eq!(f.execution_or_run_id.as_deref(), Some("exec_1"));
    }
}
