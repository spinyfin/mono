//! Durable salvage of a workspace's unpushed work, taken before retention
//! expiry reclaims the workspace.
//!
//! ## Why this exists
//!
//! Cube preserves a workspace whose `@` holds work that exists on no remote
//! (`last_release_reason = "unpushed_work_preserved"`), on the theory the work
//! might still be wanted. Nothing ever decided it was not, so the preservation
//! was unbounded: 75 free workspaces sat withheld indefinitely while every
//! lease still paid to consider them, and the pool minted new capacity around
//! them. Retention now has a TTL — and a TTL is only acceptable if expiry
//! cannot lose work.
//!
//! ## What is actually at risk
//!
//! Less than it looks, and this module is explicit about that rather than
//! pretending otherwise. `jj new <main>@<remote>` does not abandon a non-empty
//! working-copy commit: it stays a visible head in the workspace-shared object
//! store, with its ancestors, reachable through `jj log -r 'all()'` and the op
//! log (see [`crate::reuse_guard`]'s module docs, which verified this against
//! jj rather than assuming it). What a reset genuinely disturbs is the working
//! tree on disk, and what expiry genuinely destroys is *findability* — nobody
//! is going to go spelunking in an op log for a change id they never knew.
//!
//! So salvage writes, outside the jj store and outside the workspace:
//!
//! - a `manifest.json` naming the repo, workspace, holder, task, retention
//!   clock, and every salvaged change id — so `jj log -r <change_id>` still
//!   works months later even though the artifact does not depend on it;
//! - `commits.txt`, one line per salvaged commit, oldest first;
//! - `patches/NNN-<change_id>.diff`, a real git-format patch per commit, so the
//!   work can be recovered with `git apply` into any checkout with no jj
//!   archaeology at all.
//!
//! Salvage failure is never overridden: if the artifact cannot be written, the
//! workspace stays retained and expiry does not happen. "Expired" must never be
//! a path to data loss.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::command_runner::{CommandRunner, RealCommandRunner};
use crate::metadata::WorkspaceRecord;
use crate::paths;
use crate::reuse_guard::escape_revset_string;

use crate::app::errors::{CubeError, Result};
use crate::app::jj::run_jj;

/// Schema version of `manifest.json`, so a future reader can tell what it is
/// looking at without guessing from the field set.
const SALVAGE_MANIFEST_SCHEMA: u32 = 1;

/// How long a salvage record is kept before the pool GC removes it.
///
/// Deliberately much longer than the workspace retention TTL it backs: the
/// point of salvage is that reclaiming a workspace is cheap *because* the
/// artifact outlives it. Thirty days is the window in which someone might
/// still plausibly ask "what was that worker doing before it died"; a few KB
/// of diff per record makes a longer window affordable but not obviously
/// useful.
pub(super) const SALVAGE_RETENTION_SECS: i64 = 30 * 86_400;

/// Ceiling on how many commits one salvage record captures. Matches the spirit
/// of [`crate::reuse_guard::UNPUSHED_PROBE_LIMIT`]: a workspace with more than
/// this much unpushed history is pathological, and salvage must stay bounded
/// because it runs inside a time-budgeted GC pass.
pub(super) const SALVAGE_COMMIT_LIMIT: &str = "50";

/// jj template for the salvage log: change id, commit id, description first
/// line, tab separated, one commit per line.
pub(super) const SALVAGE_LOG_TEMPLATE: &str =
    r#"change_id.short() ++ "\t" ++ commit_id.short() ++ "\t" ++ description.first_line() ++ "\n""#;

/// Everything in the workspace that no remote (and not local `<main>`) already
/// holds — ancestors included.
///
/// Broader than [`crate::reuse_guard::unpushed_work_revset`] on purpose. That
/// revset answers a narrow question ("is a worker still live in this tree?")
/// and is deliberately scoped to `@`. This one answers "what would a human
/// want back?", where including committed-but-unpushed ancestors is free and
/// omitting them would be the failure mode that matters.
pub(super) fn salvage_revset(main_branch: &str) -> String {
    format!(
        "(::@ ~ ::(remote_bookmarks() | bookmarks(exact:\"{}\"))) & ~empty()",
        escape_revset_string(main_branch)
    )
}

/// One salvaged commit, as recorded in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SalvagedCommit {
    pub(super) change_id: String,
    pub(super) commit_id: String,
    pub(super) description: String,
    /// Path of this commit's patch, relative to the salvage record directory.
    pub(super) patch: String,
}

/// `manifest.json` — the human-facing index of a salvage record.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub(super) struct SalvageManifest {
    pub(super) schema: u32,
    pub(super) repo: String,
    pub(super) workspace_id: String,
    pub(super) workspace_path: String,
    pub(super) main_branch: String,
    pub(super) salvaged_at_epoch_s: i64,
    /// When the workspace first went unhealthy — the retention clock.
    pub(super) unhealthy_since_epoch_s: Option<i64>,
    /// How long it had been retained when salvage ran.
    pub(super) retained_secs: i64,
    pub(super) prior_health: String,
    pub(super) last_release_reason: Option<String>,
    /// The lease holder and task at the time the workspace was released, when
    /// the registry still had them — the only breadcrumb back to *why* this
    /// work exists.
    pub(super) holder: Option<String>,
    pub(super) task: Option<String>,
    pub(super) commits: Vec<SalvagedCommit>,
    pub(super) restore_hint: String,
}

impl SalvageManifest {
    /// One-line description for `cube workspace salvage`.
    pub(super) fn summary_line(&self) -> String {
        let head = self
            .commits
            .first()
            .map(|c| format!("{} {}", c.change_id, c.description))
            .unwrap_or_else(|| "(no commits captured)".to_string());
        format!("{}/{}  {}", self.repo, self.workspace_id, head)
    }
}

/// Root of the salvage store.
///
/// Threaded off `database_path` the same way the audit log is, so a test that
/// passes a tempdir database gets its salvage records next to it rather than in
/// the real data dir.
pub(super) fn salvage_dir(database_path: Option<&Path>) -> Result<PathBuf> {
    match database_path.and_then(Path::parent) {
        Some(parent) => Ok(parent.join("salvage")),
        None => Ok(paths::data_dir()?.join("salvage")),
    }
}

/// Capture a workspace's unpushed work into a durable salvage record and
/// return the record's directory.
///
/// Callers must treat an `Err` as "do not reclaim this workspace". The whole
/// contract of bounded retention rests on this call having succeeded.
pub(super) fn salvage_workspace(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    record: &WorkspaceRecord,
    main_branch: &str,
    now_epoch_s: i64,
) -> Result<PathBuf> {
    let revset = salvage_revset(main_branch);
    let log = run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            &record.workspace_path,
            "jj",
            &[
                "log",
                "--no-graph",
                "-n",
                SALVAGE_COMMIT_LIMIT,
                "-r",
                &revset,
                "-T",
                SALVAGE_LOG_TEMPLATE,
            ],
        ),
    )?;

    // jj logs newest first; a patch series reads oldest first.
    let mut parsed: Vec<(String, String, String)> = log
        .lines()
        .filter_map(|line| {
            let line = line.trim_end();
            if line.trim().is_empty() {
                return None;
            }
            let mut parts = line.splitn(3, '\t');
            let change_id = parts.next().unwrap_or_default().to_string();
            let commit_id = parts.next().unwrap_or_default().to_string();
            let description = parts.next().unwrap_or_default().trim().to_string();
            if change_id.is_empty() || commit_id.is_empty() {
                return None;
            }
            Some((change_id, commit_id, description))
        })
        .collect();
    parsed.reverse();

    let dir = salvage_dir(database_path)?
        .join(&record.repo)
        .join(format!("{}-{now_epoch_s}", record.workspace_id));
    let patches_dir = dir.join("patches");
    std::fs::create_dir_all(&patches_dir).map_err(CubeError::Io)?;

    let mut commits = Vec::with_capacity(parsed.len());
    let mut commits_txt = String::new();
    for (index, (change_id, commit_id, description)) in parsed.iter().enumerate() {
        let diff = run_jj(
            runner,
            database_path,
            &RealCommandRunner::invocation(
                &record.workspace_path,
                "jj",
                &["diff", "--no-pager", "--git", "-r", commit_id],
            ),
        )?;
        let patch_name = format!("{:03}-{change_id}.diff", index + 1);
        std::fs::write(patches_dir.join(&patch_name), diff.as_bytes()).map_err(CubeError::Io)?;
        commits_txt.push_str(&format!("{change_id}\t{commit_id}\t{description}\n"));
        commits.push(SalvagedCommit {
            change_id: change_id.clone(),
            commit_id: commit_id.clone(),
            description: description.clone(),
            patch: format!("patches/{patch_name}"),
        });
    }
    std::fs::write(dir.join("commits.txt"), commits_txt.as_bytes()).map_err(CubeError::Io)?;

    let manifest = SalvageManifest {
        schema: SALVAGE_MANIFEST_SCHEMA,
        repo: record.repo.clone(),
        workspace_id: record.workspace_id.clone(),
        workspace_path: record.workspace_path.display().to_string(),
        main_branch: main_branch.to_string(),
        salvaged_at_epoch_s: now_epoch_s,
        unhealthy_since_epoch_s: record.unhealthy_since_epoch_s,
        retained_secs: record
            .unhealthy_since_epoch_s
            .map(|since| now_epoch_s.saturating_sub(since))
            .unwrap_or(0),
        prior_health: record
            .health_status
            .map(|h| h.as_str())
            .unwrap_or("unknown")
            .to_string(),
        last_release_reason: record.last_release_reason.clone(),
        holder: record.holder.clone(),
        task: record.task.clone(),
        commits,
        restore_hint: "Apply patches/ in order with `git apply` (or `jj new main` then \
                       `git apply`) in any checkout of this repo. The original commits may \
                       also still be in the workspace's jj object store: `jj log -r <change_id>`."
            .to_string(),
    };
    let json = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(dir.join("manifest.json"), json.as_bytes()).map_err(CubeError::Io)?;

    Ok(dir)
}

/// One salvage record on disk.
pub(super) struct SalvageRecord {
    pub(super) path: PathBuf,
    pub(super) manifest: SalvageManifest,
}

/// Read every salvage record, newest first, optionally filtered by repo and
/// workspace id. Unreadable or malformed records are skipped rather than
/// failing the listing — a corrupt record must not hide the good ones.
pub(super) fn list_salvage_records(
    database_path: Option<&Path>,
    repo: Option<&str>,
    workspace_id: Option<&str>,
) -> Result<Vec<SalvageRecord>> {
    let root = salvage_dir(database_path)?;
    let mut records = Vec::new();
    let Ok(repo_dirs) = std::fs::read_dir(&root) else {
        return Ok(records);
    };
    for repo_entry in repo_dirs.flatten() {
        if !repo_entry.path().is_dir() {
            continue;
        }
        if let Some(want) = repo
            && repo_entry.file_name().to_string_lossy() != want
        {
            continue;
        }
        let Ok(record_dirs) = std::fs::read_dir(repo_entry.path()) else {
            continue;
        };
        for entry in record_dirs.flatten() {
            let path = entry.path();
            let Ok(raw) = std::fs::read_to_string(path.join("manifest.json")) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<SalvageManifest>(&raw) else {
                continue;
            };
            if let Some(want) = workspace_id
                && manifest.workspace_id != want
            {
                continue;
            }
            records.push(SalvageRecord { path, manifest });
        }
    }
    records.sort_by(|a, b| {
        b.manifest
            .salvaged_at_epoch_s
            .cmp(&a.manifest.salvaged_at_epoch_s)
            .then_with(|| a.manifest.workspace_id.cmp(&b.manifest.workspace_id))
    });
    Ok(records)
}

/// Delete salvage records older than [`SALVAGE_RETENTION_SECS`]. Returns how
/// many were removed. Best-effort: an unremovable record is reported and
/// skipped, never fatal.
pub(super) fn gc_aged_salvage_records(database_path: Option<&Path>, now_epoch_s: i64) -> usize {
    let Ok(records) = list_salvage_records(database_path, None, None) else {
        return 0;
    };
    let cutoff = now_epoch_s.saturating_sub(SALVAGE_RETENTION_SECS);
    let mut removed = 0usize;
    for record in records {
        if record.manifest.salvaged_at_epoch_s > cutoff {
            continue;
        }
        match std::fs::remove_dir_all(&record.path) {
            Ok(()) => removed += 1,
            Err(e) => eprintln!("cube: salvage gc: failed to remove {}: {e}", record.path.display()),
        }
    }
    removed
}
