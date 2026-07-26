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
//!
//! ## What counts as a successful salvage
//!
//! Only a *provably complete* one. `Ok` from [`salvage_workspace`] is what
//! licenses the destructive reset, so every way the capture could be partial
//! has to be an `Err` instead:
//!
//! - **Nothing captured.** Salvage only runs after the reuse probe refused —
//!   i.e. jj just said `@` holds work no remote has. A log that then yields
//!   zero commits means the revset, the template or the parse disagreed with
//!   the probe, not that there is nothing to save. Fail closed.
//! - **More work than the cap.** The log is asked for one more commit than
//!   [`SALVAGE_COMMIT_LIMIT`] precisely so "there are more" is *detectable*.
//!   `jj log -n N` keeps the *newest* N, so a silent truncation would drop the
//!   oldest history — and every patch after the cut is relative to a parent
//!   that was never exported, making the remaining series unapplicable rather
//!   than merely incomplete. Fail closed.
//! - **A half-written record.** The directory is built under a `.partial`
//!   suffix and renamed into place only once `manifest.json` is on disk, and
//!   removed on any error along the way. `list_salvage_records` keys off a
//!   parseable manifest, so a partial tree left behind would be invisible to
//!   listing *and* to salvage GC — an orphan that never ages out.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::command_runner::{CommandRunner, RealCommandRunner};
use crate::metadata::WorkspaceRecord;
use crate::paths;
use crate::reuse_guard::escape_revset_string;

use crate::app::errors::{CubeError, Result};
use crate::app::jj::run_jj_within;

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
///
/// Exceeding it is an error, not a truncation — see the module docs.
pub(super) const SALVAGE_COMMIT_LIMIT: usize = 50;

/// The `-n` argument actually handed to `jj log`: one *more* than
/// [`SALVAGE_COMMIT_LIMIT`], so a stack that exceeds the cap comes back with
/// `LIMIT + 1` rows and is detected rather than silently trimmed. Asking for
/// exactly `LIMIT` would make "complete stack of exactly 50" and "truncated
/// stack of 200" indistinguishable.
pub(super) fn salvage_log_limit_arg() -> String {
    (SALVAGE_COMMIT_LIMIT + 1).to_string()
}

/// Grace period before salvage GC removes a directory under the salvage root
/// that has no parseable `manifest.json`. Long enough that an in-flight
/// salvage from a concurrent process is never swept out from under itself,
/// short enough that debris from a crashed one does not linger for the full
/// [`SALVAGE_RETENTION_SECS`].
const SALVAGE_PARTIAL_GRACE_SECS: i64 = 3600;

/// Suffix a salvage record directory carries while it is still being written.
const SALVAGE_PARTIAL_SUFFIX: &str = ".partial";

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
/// contract of bounded retention rests on this call having succeeded — and on
/// `Ok` meaning the capture was *complete*, which is why an empty or
/// over-the-cap log is an error rather than a smaller record (see the module
/// docs).
///
/// `deadline`, when set, bounds every `jj` subprocess this spawns. Salvage runs
/// inside the pool GC's wall-clock budget, and a salvage cut short by the
/// deadline fails — which is the safe direction: the workspace stays retained
/// and the next pass tries again.
pub(super) fn salvage_workspace(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    record: &WorkspaceRecord,
    main_branch: &str,
    now_epoch_s: i64,
    deadline: Option<Instant>,
) -> Result<PathBuf> {
    let revset = salvage_revset(main_branch);
    let limit_arg = salvage_log_limit_arg();
    let log = run_jj_within(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            &record.workspace_path,
            "jj",
            &[
                "log",
                "--no-graph",
                "-n",
                &limit_arg,
                "-r",
                &revset,
                "-T",
                SALVAGE_LOG_TEMPLATE,
            ],
        ),
        deadline,
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

    // Fail closed on a capture that cannot be proven complete. Both of these
    // previously returned `Ok` and licensed the reset.
    if parsed.len() > SALVAGE_COMMIT_LIMIT {
        return Err(CubeError::SalvageIncomplete {
            workspace_id: record.workspace_id.clone(),
            reason: format!(
                "`@` has more than {SALVAGE_COMMIT_LIMIT} unpushed commits, the most this \
                 time-budgeted pass will export. Capturing only the newest {SALVAGE_COMMIT_LIMIT} \
                 would drop the oldest history and leave the remaining patches rooted on a parent \
                 that was never exported. The workspace stays retained; push or export the stack \
                 by hand (`jj log -r '{revset}'`) and release it, or raise the cap."
            ),
        });
    }
    if parsed.is_empty() {
        return Err(CubeError::SalvageIncomplete {
            workspace_id: record.workspace_id.clone(),
            reason: format!(
                "the reuse probe reported unpushed work but `jj log -r '{revset}'` returned no \
                 usable commits, so there is nothing to write and no basis for calling the work \
                 saved. The workspace stays retained."
            ),
        });
    }
    parsed.reverse();

    let repo_dir = salvage_dir(database_path)?.join(&record.repo);
    let dir = repo_dir.join(format!("{}-{now_epoch_s}", record.workspace_id));
    // Build under a `.partial` name and rename into place only once the
    // manifest is written, so a mid-flight failure never leaves a directory
    // that listing and salvage GC both ignore.
    let partial = repo_dir.join(format!("{}-{now_epoch_s}{SALVAGE_PARTIAL_SUFFIX}", record.workspace_id));
    let _ = std::fs::remove_dir_all(&partial);
    match write_salvage_record(
        runner,
        database_path,
        record,
        main_branch,
        now_epoch_s,
        deadline,
        &parsed,
        &partial,
    ) {
        Ok(()) => {}
        Err(e) => {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(e);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&partial, &dir).map_err(|e| {
        let _ = std::fs::remove_dir_all(&partial);
        CubeError::Io(e)
    })?;

    Ok(dir)
}

/// Write the patches, commit index and manifest for `parsed` into `dir`.
/// Everything that can fail lives here so the caller has exactly one place to
/// clean up after.
#[allow(clippy::too_many_arguments)]
fn write_salvage_record(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    record: &WorkspaceRecord,
    main_branch: &str,
    now_epoch_s: i64,
    deadline: Option<Instant>,
    parsed: &[(String, String, String)],
    dir: &Path,
) -> Result<()> {
    let patches_dir = dir.join("patches");
    std::fs::create_dir_all(&patches_dir).map_err(CubeError::Io)?;

    let mut commits = Vec::with_capacity(parsed.len());
    let mut commits_txt = String::new();
    for (index, (change_id, commit_id, description)) in parsed.iter().enumerate() {
        let diff = run_jj_within(
            runner,
            database_path,
            &RealCommandRunner::invocation(
                &record.workspace_path,
                "jj",
                &["diff", "--no-pager", "--git", "-r", commit_id],
            ),
            deadline,
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
        holder: attributed_holder(record).map(str::to_string),
        task: attributed_task(record).map(str::to_string),
        commits,
        restore_hint: "Apply patches/ in order with `git apply` (or `jj new main` then \
                       `git apply`) in any checkout of this repo. The original commits may \
                       also still be in the workspace's jj object store: `jj log -r <change_id>`."
            .to_string(),
    };
    let json = serde_json::to_string_pretty(&manifest)?;
    // Last write in the sequence: the rename that publishes this directory is
    // gated on it, so a reader never sees a manifest without its patches.
    std::fs::write(dir.join("manifest.json"), json.as_bytes()).map_err(CubeError::Io)?;
    Ok(())
}

/// Who was holding this workspace when the work was made.
///
/// A free workspace always has `holder = NULL` (release clears it), so
/// retention salvage — which by definition runs against a free workspace, a
/// day or more after the release — would otherwise never have an answer. The
/// snapshot taken at release time is the one that actually carries the
/// information; the live column is checked first only so a salvage of a still
/// -leased row (there is no such path today) would not report stale data.
pub(super) fn attributed_holder(record: &WorkspaceRecord) -> Option<&str> {
    record.holder.as_deref().or(record.last_holder.as_deref())
}

/// The task this workspace was leased for when the work was made. See
/// [`attributed_holder`].
pub(super) fn attributed_task(record: &WorkspaceRecord) -> Option<&str> {
    record.task.as_deref().or(record.last_task.as_deref())
}

/// Delete a salvage record written by [`salvage_workspace`].
///
/// Used when the reclaim the salvage was taken *for* then fails: the live
/// workspace is still there, still holding the work, and still retained, so
/// keeping the copy buys nothing and the next pass would write another one.
/// Repeated reset failures would otherwise multiply full copies of the same
/// stack until the 30-day salvage TTL.
pub(super) fn discard_salvage_record(path: &Path) {
    if let Err(e) = std::fs::remove_dir_all(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "cube: salvage: failed to discard {} after a failed reclaim: {e}",
            path.display()
        );
    }
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

/// Delete salvage records older than [`SALVAGE_RETENTION_SECS`], plus any
/// manifest-less debris older than [`SALVAGE_PARTIAL_GRACE_SECS`]. Returns how
/// many directories were removed. Best-effort: an unremovable record is
/// reported and skipped, never fatal.
pub(super) fn gc_aged_salvage_records(database_path: Option<&Path>, now_epoch_s: i64) -> usize {
    let mut removed = gc_orphaned_salvage_dirs(database_path, now_epoch_s);
    let Ok(records) = list_salvage_records(database_path, None, None) else {
        return removed;
    };
    let cutoff = now_epoch_s.saturating_sub(SALVAGE_RETENTION_SECS);
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

/// Sweep directories under the salvage root that have no parseable
/// `manifest.json` and are older than [`SALVAGE_PARTIAL_GRACE_SECS`].
///
/// [`list_salvage_records`] keys off the manifest, so without this a directory
/// left behind by a failed salvage is invisible to both listing *and* the
/// retention sweep above — it would accumulate under the salvage root forever.
/// [`salvage_workspace`] now cleans up after itself, so in practice this only
/// catches debris from a process that died mid-write; the grace period keeps
/// it from racing a concurrent, still-running salvage.
fn gc_orphaned_salvage_dirs(database_path: Option<&Path>, now_epoch_s: i64) -> usize {
    let Ok(root) = salvage_dir(database_path) else {
        return 0;
    };
    let Ok(repo_dirs) = std::fs::read_dir(&root) else {
        return 0;
    };
    let cutoff = now_epoch_s.saturating_sub(SALVAGE_PARTIAL_GRACE_SECS);
    let mut removed = 0usize;
    for repo_entry in repo_dirs.flatten() {
        if !repo_entry.path().is_dir() {
            continue;
        }
        let Ok(record_dirs) = std::fs::read_dir(repo_entry.path()) else {
            continue;
        };
        for entry in record_dirs.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let has_manifest = std::fs::read_to_string(path.join("manifest.json"))
                .ok()
                .and_then(|raw| serde_json::from_str::<SalvageManifest>(&raw).ok())
                .is_some();
            if has_manifest {
                continue;
            }
            if dir_modified_epoch_s(&path).is_some_and(|modified| modified > cutoff) {
                continue;
            }
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    eprintln!("cube: salvage gc: removed partial salvage record {}", path.display());
                    removed += 1;
                }
                Err(e) => eprintln!("cube: salvage gc: failed to remove partial {}: {e}", path.display()),
            }
        }
    }
    removed
}

/// A directory's mtime as a Unix timestamp, or `None` when it cannot be read
/// (in which case the caller treats the directory as old enough to sweep — a
/// directory whose metadata is unreadable is not one that is being written).
fn dir_modified_epoch_s(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}
