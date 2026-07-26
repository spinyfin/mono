//! Reading back the recovery patches [`crate::recovery_backup`] writes.
//!
//! ## Why this module exists
//!
//! Before it, the recovery pipeline was write-only. Six sweep sites called
//! [`crate::recovery_backup::backup_dead_execution`], 86 patches accumulated
//! on disk going back to 2026-06-03, `boothby.md` even scheduled a GC pass
//! for them — and nothing anywhere read one. A crashed worker's uncommitted
//! work was captured to a file that no code path ever opened.
//!
//! ## Recovery order: cube first, patch second
//!
//! The patch is the *fallback*, not the primary. When a resume dispatch can
//! re-lease the exact cube workspace the dead worker was using, and that
//! workspace still holds the dirty working copy, the work is already live and
//! in place with its jj operation log intact — that is strictly better than
//! replaying a diff, and applying a patch on top of it would be actively
//! harmful (duplicate hunks, or a conflict against work that was already
//! there).
//!
//! So the caller's order is:
//!
//! 1. Lease `--prefer <workspace> --allow-dirty`. Cube reports
//!    `dirty_verified` on the lease payload: `true` means the working copy
//!    still held work that exists on no remote, i.e. cube recovered it in
//!    place. Nothing else to do.
//! 2. Only when cube could **not** recover — the lease failed outright, or it
//!    succeeded with `dirty_verified: false` because the tree had already
//!    been reset — apply the patch into whatever workspace the resuming
//!    worker actually got.
//!
//! ## Bookkeeping is filtered out
//!
//! Boss's own hook spool (`.boss/events-pending.jsonl`) is tracked in some
//! workspaces, so it lands in the captured diff. Of the four patches taken at
//! 14:42 PDT on 2026-07-23, three were 203 KB / 197 KB / 38 KB of *nothing
//! but* that spool; only an 11 KB patch held real code. Replaying a stale
//! spool into a fresh workspace is at best noise and at worst re-injects
//! already-processed hook events, so [`filter_bookkeeping`] drops those
//! sections. A patch that is *entirely* bookkeeping filters down to nothing
//! and is reported as "nothing to restore" rather than as a recovery.
//!
//! ## Failure is loud
//!
//! A patch that does not apply must never be swallowed. [`apply_recovery_patch`]
//! returns `Err` with git's stderr attached, and the caller's contract is to
//! surface that rather than let the worker start on a tree it believes was
//! recovered. Silently proceeding is the failure mode that makes recovery
//! code worse than no recovery code: the worker rebuilds from scratch while
//! believing it is resuming.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Path prefixes that are Boss's own bookkeeping rather than the worker's
/// work. Matched against the patch's `b/` (post-image) path.
///
/// `.boss/` is the engine-owned infra directory cube plants in every
/// workspace (hook spool, per-workspace logs, the self-ignore guard). Nothing
/// under it is ever part of a chore's diff.
const BOOKKEEPING_PREFIXES: &[&str] = &[".boss/"];

/// Suffix appended to a patch file once it has been successfully applied, so
/// a later engine restart does not replay it on top of the work it already
/// restored. Kept (rather than deleted) so the artifact survives for
/// forensics; `boothby`'s recovery-patch GC is what eventually removes it.
const CONSUMED_SUFFIX: &str = ".applied";

/// Basename of the marker the engine drops in a recovered workspace so the
/// worker's prompt can tell it what happened. Lives under `.boss/`, which
/// cube self-ignores, so it never pollutes a PR.
pub const RECOVERY_REPORT_FILE: &str = ".boss/recovery-report.json";

/// How the work in a resumed workspace got there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoverySource {
    /// Cube re-leased the dead worker's own workspace with its dirty working
    /// copy intact. The jj operation log is intact too — this is the good
    /// path.
    CubeInPlace,
    /// Cube could not recover, so the saved patch was replayed into a
    /// different workspace.
    Patch,
}

/// What a patch application restored, in terms a human can check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    /// Post-image paths restored, excluding bookkeeping.
    pub paths: Vec<String>,
    /// Added lines across those paths.
    pub insertions: usize,
    /// Removed lines across those paths.
    pub deletions: usize,
    /// Bookkeeping paths dropped before applying (reported so a
    /// "nothing restored" outcome is explainable rather than mysterious).
    pub filtered_paths: Vec<String>,
}

impl ApplyReport {
    /// One-line human summary, e.g.
    /// `3 file(s), +120/-14 (2 bookkeeping file(s) filtered out)`.
    pub fn summary(&self) -> String {
        let mut s = format!("{} file(s), +{}/-{}", self.paths.len(), self.insertions, self.deletions);
        if !self.filtered_paths.is_empty() {
            s.push_str(&format!(
                " ({} bookkeeping file(s) filtered out)",
                self.filtered_paths.len()
            ));
        }
        s
    }
}

/// The marker written into a recovered workspace for the worker prompt to
/// read. Keyed by the *resuming* execution's id so a stale report left by an
/// earlier recovery in the same workspace is never mistaken for this one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryReport {
    /// The execution this report is for — the new, resuming one.
    pub for_execution_id: String,
    /// The dead execution whose work was recovered.
    pub from_execution_id: String,
    pub source: RecoverySource,
    /// Present only for [`RecoverySource::Patch`], and only when the apply
    /// succeeded and restored something.
    pub applied: Option<ApplyReport>,
    /// Set when the patch did NOT apply. The worker's prompt renders this as
    /// "recovery FAILED — do not assume resumed state", which is the whole
    /// reason the field exists: a report with `applied: None` and no error
    /// would be indistinguishable from "nothing needed restoring".
    #[serde(default)]
    pub patch_error: Option<String>,
}

impl RecoveryReport {
    /// Write the report into `<workspace>/.boss/recovery-report.json`.
    pub fn write(&self, workspace_path: &Path) -> Result<PathBuf> {
        let path = workspace_path.join(RECOVERY_REPORT_FILE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {} for the recovery report", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(self).context("serializing recovery report")?;
        std::fs::write(&path, body).with_context(|| format!("failed to write recovery report {}", path.display()))?;
        Ok(path)
    }

    /// Read the report for `execution_id` from a workspace, if one is there.
    ///
    /// Returns `None` when the file is absent, unreadable, malformed, or
    /// belongs to a different execution — a recovery marker is advisory
    /// prompt context, never a reason to fail a dispatch.
    pub fn read_for(workspace_path: &Path, execution_id: &str) -> Option<Self> {
        let path = workspace_path.join(RECOVERY_REPORT_FILE);
        let body = std::fs::read_to_string(&path).ok()?;
        let report: Self = serde_json::from_str(&body).ok()?;
        (report.for_execution_id == execution_id).then_some(report)
    }
}

/// A patch split into the sections that will be applied and the bookkeeping
/// sections that were dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredPatch {
    /// The patch text to hand to `git apply`. Empty when every section was
    /// bookkeeping.
    pub text: String,
    /// Post-image paths kept.
    pub kept_paths: Vec<String>,
    /// Post-image paths dropped.
    pub filtered_paths: Vec<String>,
    pub insertions: usize,
    pub deletions: usize,
}

impl FilteredPatch {
    /// True when nothing survived the filter — the capture held only Boss's
    /// own bookkeeping, so there is no work to restore.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// Locate the recovery patch for `execution_id`, if one was captured and has
/// not already been consumed.
///
/// Mirrors [`crate::recovery_backup`]'s naming exactly (including the
/// defensive id sanitisation), so a patch written by the backup path is the
/// one found here.
pub fn find_patch(recovery_dir: &Path, execution_id: &str) -> Option<PathBuf> {
    let path = recovery_dir.join(patch_file_name(execution_id));
    path.is_file().then_some(path)
}

/// Map an execution id to a safe single-segment patch filename.
///
/// Execution ids are already `exec_<hex>_<n>`-shaped, but we defensively
/// replace anything outside `[A-Za-z0-9_-]` with `_` so a hostile or
/// malformed id can never escape the recovery directory via `/` or `..`.
///
/// This is the single canonical home for the naming; the backup path
/// ([`crate::recovery_backup`]) calls it so a patch it writes is the one
/// [`find_patch`] locates here.
pub(crate) fn patch_file_name(execution_id: &str) -> String {
    let stem: String = execution_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{stem}.patch")
}

/// Extract the post-image path from a `diff --git a/<x> b/<y>` header line.
///
/// Returns the `b/` side with its prefix stripped. Uses the ` b/` separator
/// rather than whitespace splitting so a path containing spaces (git emits
/// those unquoted when neither path needs quoting) is handled.
fn post_image_path(header: &str) -> Option<&str> {
    let rest = header.strip_prefix("diff --git ")?;
    let idx = rest.rfind(" b/")?;
    Some(&rest[idx + 3..])
}

/// True when `path` is Boss's own bookkeeping rather than the worker's work.
fn is_bookkeeping(path: &str) -> bool {
    BOOKKEEPING_PREFIXES.iter().any(|p| path.starts_with(p))
}

/// Split a git-format patch into per-file sections and drop the bookkeeping
/// ones, counting insertions/deletions across what remains.
///
/// A section runs from one `diff --git ` line up to (but not including) the
/// next. Anything before the first `diff --git ` (there is nothing in a
/// `jj diff --git` capture, but be defensive) is preserved verbatim as a
/// preamble so an unexpected patch shape is never silently truncated.
pub fn filter_bookkeeping(patch: &str) -> FilteredPatch {
    let mut text = String::new();
    let mut kept_paths = Vec::new();
    let mut filtered_paths = Vec::new();
    let mut insertions = 0usize;
    let mut deletions = 0usize;

    // Section boundaries: index of every `diff --git ` line.
    let lines: Vec<&str> = patch.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("diff --git "))
        .map(|(i, _)| i)
        .collect();

    if starts.is_empty() {
        // No recognisable sections. Hand the text back untouched rather than
        // guessing; `git apply` will give a real error if it is malformed.
        return FilteredPatch {
            text: patch.to_owned(),
            kept_paths,
            filtered_paths,
            insertions,
            deletions,
        };
    }

    // Preamble before the first section, if any.
    if starts[0] > 0 {
        for line in &lines[..starts[0]] {
            text.push_str(line);
            text.push('\n');
        }
    }

    for (n, &start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(lines.len());
        let section = &lines[start..end];
        let path = post_image_path(section[0]).unwrap_or_default().to_owned();
        if is_bookkeeping(&path) {
            filtered_paths.push(path);
            continue;
        }
        for line in section {
            // `+++`/`---` are the file headers, not content lines.
            if line.starts_with("+++") || line.starts_with("---") {
                // fall through to the copy below without counting
            } else if line.starts_with('+') {
                insertions += 1;
            } else if line.starts_with('-') {
                deletions += 1;
            }
            text.push_str(line);
            text.push('\n');
        }
        kept_paths.push(path);
    }

    FilteredPatch {
        text,
        kept_paths,
        filtered_paths,
        insertions,
        deletions,
    }
}

/// Apply the recovery patch at `patch_path` into `workspace_path`.
///
/// Returns `Ok(None)` when the patch held nothing but bookkeeping — there was
/// genuinely nothing to restore, which is a real outcome and not an error.
/// Returns `Err` when `git apply` refuses; the caller MUST surface that
/// rather than let the worker proceed believing its state was recovered.
///
/// Uses `git apply --3way`, which falls back to a three-way merge when a hunk
/// does not apply cleanly against the current tree — the resuming workspace is
/// on a fresh `main@origin` that may have moved since the capture, so exact
/// context matches are not guaranteed. A three-way merge that leaves conflict
/// markers still exits non-zero, so it surfaces as the loud failure it is.
pub fn apply_recovery_patch(workspace_path: &Path, patch_path: &Path) -> Result<Option<ApplyReport>> {
    let raw = std::fs::read_to_string(patch_path)
        .with_context(|| format!("failed to read recovery patch {}", patch_path.display()))?;
    let filtered = filter_bookkeeping(&raw);
    if filtered.is_empty() {
        tracing::info!(
            patch = %patch_path.display(),
            filtered_paths = ?filtered.filtered_paths,
            "recovery-apply: patch held only Boss bookkeeping; nothing to restore",
        );
        return Ok(None);
    }

    // Write the filtered patch next to the original so a failed apply leaves
    // the exact bytes git rejected on disk for a human to inspect.
    let staged = patch_path.with_extension("filtered.patch");
    std::fs::write(&staged, filtered.text.as_bytes())
        .with_context(|| format!("failed to stage filtered patch at {}", staged.display()))?;

    let output = Command::new("git")
        .args(["apply", "--3way", "--whitespace=nowarn"])
        .arg(&staged)
        .current_dir(workspace_path)
        .output()
        .with_context(|| {
            format!(
                "failed to spawn `git apply` in {} for {}",
                workspace_path.display(),
                staged.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "`git apply --3way {}` failed in {} with {}: {}\n\
             The resuming worker MUST NOT proceed as if its state was recovered. \
             The filtered patch was left on disk for inspection.",
            staged.display(),
            workspace_path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    // Clean up only on success — a failure keeps the evidence.
    let _ = std::fs::remove_file(&staged);

    Ok(Some(ApplyReport {
        paths: filtered.kept_paths,
        insertions: filtered.insertions,
        deletions: filtered.deletions,
        filtered_paths: filtered.filtered_paths,
    }))
}

/// Rename a consumed patch to `<name>.patch.applied` so a later restart does
/// not replay it over the work it already restored.
///
/// Best-effort: a rename failure is logged, not propagated. Failing a
/// successful recovery because the bookkeeping rename did not stick would be
/// the tail wagging the dog — the worst case is one redundant `--3way` apply
/// on a later restart, which is idempotent for an already-applied diff.
pub fn mark_patch_consumed(patch_path: &Path) -> Option<PathBuf> {
    let mut consumed = patch_path.as_os_str().to_owned();
    consumed.push(CONSUMED_SUFFIX);
    let consumed = PathBuf::from(consumed);
    match std::fs::rename(patch_path, &consumed) {
        Ok(()) => Some(consumed),
        Err(err) => {
            tracing::warn!(
                patch = %patch_path.display(),
                error = %err,
                "recovery-apply: could not mark patch consumed; a later restart may replay it",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A minimal but real git-format section for one file.
    fn section(path: &str, added: &[&str], removed: &[&str]) -> String {
        let mut s = format!("diff --git a/{path} b/{path}\n");
        s.push_str("index 1111111..2222222 100644\n");
        s.push_str(&format!("--- a/{path}\n"));
        s.push_str(&format!("+++ b/{path}\n"));
        s.push_str("@@ -1,1 +1,1 @@\n");
        for line in removed {
            s.push_str(&format!("-{line}\n"));
        }
        for line in added {
            s.push_str(&format!("+{line}\n"));
        }
        s
    }

    // ── post_image_path ───────────────────────────────────────────

    #[test]
    fn post_image_path_reads_the_b_side() {
        assert_eq!(
            post_image_path("diff --git a/src/main.rs b/src/main.rs"),
            Some("src/main.rs")
        );
    }

    #[test]
    fn post_image_path_handles_paths_containing_spaces() {
        // `rfind(" b/")` rather than whitespace splitting, so a space in the
        // path does not truncate it.
        assert_eq!(
            post_image_path("diff --git a/docs/my notes.md b/docs/my notes.md"),
            Some("docs/my notes.md")
        );
    }

    #[test]
    fn post_image_path_rejects_a_non_header_line() {
        assert_eq!(post_image_path("+++ b/src/main.rs"), None);
    }

    // ── patch_file_name ───────────────────────────────────────────

    #[test]
    fn patch_file_name_keeps_well_formed_execution_id() {
        assert_eq!(
            patch_file_name("exec_18b434effe0b8340_b"),
            "exec_18b434effe0b8340_b.patch"
        );
    }

    #[test]
    fn patch_file_name_sanitizes_path_separators_and_traversal() {
        // A `/` or `..` in the id must never escape the recovery dir.
        let name = patch_file_name("../../etc/passwd");
        assert_eq!(name, "______etc_passwd.patch");
        assert!(!name.contains('/'));
        assert!(!name.contains(".."));
    }

    // ── filter_bookkeeping ────────────────────────────────────────

    #[test]
    fn filter_keeps_real_work_and_drops_boss_spool() {
        let patch = format!(
            "{}{}",
            section(".boss/events-pending.jsonl", &["{\"e\":1}"], &[]),
            section("tools/cube/src/app.rs", &["let x = 1;", "let y = 2;"], &["let x = 0;"]),
        );
        let filtered = filter_bookkeeping(&patch);
        assert_eq!(filtered.kept_paths, ["tools/cube/src/app.rs"]);
        assert_eq!(filtered.filtered_paths, [".boss/events-pending.jsonl"]);
        assert_eq!(filtered.insertions, 2);
        assert_eq!(filtered.deletions, 1);
        assert!(
            !filtered.text.contains("events-pending"),
            "the spool section must not survive the filter: {}",
            filtered.text
        );
        assert!(filtered.text.contains("tools/cube/src/app.rs"));
    }

    /// The three big patches captured at 14:42 PDT on 2026-07-23 were 203 KB
    /// / 197 KB / 38 KB of nothing but the hook spool. Size is not a signal
    /// of value; such a patch must report "nothing to restore", not a
    /// recovery.
    #[test]
    fn a_patch_of_pure_bookkeeping_filters_down_to_nothing() {
        let patch = section(".boss/events-pending.jsonl", &["{\"e\":1}", "{\"e\":2}"], &[]);
        let filtered = filter_bookkeeping(&patch);
        assert!(filtered.is_empty(), "expected empty, got: {:?}", filtered.text);
        assert!(filtered.kept_paths.is_empty());
        assert_eq!(filtered.filtered_paths, [".boss/events-pending.jsonl"]);
        assert_eq!(filtered.insertions, 0);
    }

    #[test]
    fn filter_does_not_count_file_headers_as_content() {
        // `+++ b/x` and `--- a/x` start with + and -, and naive counting
        // would report one phantom insertion and one phantom deletion.
        let filtered = filter_bookkeeping(&section("x.txt", &["only real addition"], &[]));
        assert_eq!(filtered.insertions, 1);
        assert_eq!(filtered.deletions, 0);
    }

    #[test]
    fn filter_passes_through_a_patch_with_no_recognisable_sections() {
        let filtered = filter_bookkeeping("this is not a patch\n");
        assert_eq!(filtered.text, "this is not a patch\n");
        assert!(filtered.kept_paths.is_empty());
        assert!(!filtered.is_empty());
    }

    // ── find_patch / mark_patch_consumed ──────────────────────────

    #[test]
    fn find_patch_matches_the_backup_naming() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exec_18b434effe0b8340_b.patch");
        std::fs::write(&path, "x").unwrap();
        assert_eq!(find_patch(dir.path(), "exec_18b434effe0b8340_b"), Some(path));
        assert_eq!(find_patch(dir.path(), "exec_absent"), None);
    }

    #[test]
    fn find_patch_sanitises_traversal_in_the_execution_id() {
        let dir = TempDir::new().unwrap();
        // Would resolve outside `dir` without the sanitisation.
        assert_eq!(find_patch(dir.path(), "../../etc/passwd"), None);
    }

    #[test]
    fn mark_patch_consumed_renames_so_a_restart_does_not_replay_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exec_1.patch");
        std::fs::write(&path, "x").unwrap();
        let consumed = mark_patch_consumed(&path).expect("rename should succeed");
        assert_eq!(consumed, dir.path().join("exec_1.patch.applied"));
        assert!(!path.exists());
        // And the whole point: a later lookup no longer finds it.
        assert_eq!(find_patch(dir.path(), "exec_1"), None);
    }

    #[test]
    fn mark_patch_consumed_is_best_effort_on_a_missing_file() {
        let dir = TempDir::new().unwrap();
        assert_eq!(mark_patch_consumed(&dir.path().join("nope.patch")), None);
    }

    // ── RecoveryReport ────────────────────────────────────────────

    #[test]
    fn recovery_report_round_trips_through_the_workspace_marker() {
        let ws = TempDir::new().unwrap();
        let report = RecoveryReport {
            for_execution_id: "exec_new_1".to_owned(),
            from_execution_id: "exec_dead_9".to_owned(),
            source: RecoverySource::Patch,
            patch_error: None,
            applied: Some(ApplyReport {
                paths: vec!["a.rs".to_owned()],
                insertions: 3,
                deletions: 1,
                filtered_paths: vec![".boss/events-pending.jsonl".to_owned()],
            }),
        };
        report.write(ws.path()).expect("write");
        assert_eq!(RecoveryReport::read_for(ws.path(), "exec_new_1"), Some(report));
    }

    /// A marker left by an earlier recovery in the same workspace must not be
    /// reported to a different execution as its own.
    #[test]
    fn recovery_report_is_ignored_for_another_execution() {
        let ws = TempDir::new().unwrap();
        RecoveryReport {
            for_execution_id: "exec_old".to_owned(),
            from_execution_id: "exec_dead".to_owned(),
            source: RecoverySource::CubeInPlace,
            applied: None,
            patch_error: None,
        }
        .write(ws.path())
        .expect("write");
        assert_eq!(RecoveryReport::read_for(ws.path(), "exec_new"), None);
    }

    #[test]
    fn recovery_report_read_tolerates_absent_and_malformed_markers() {
        let ws = TempDir::new().unwrap();
        assert_eq!(RecoveryReport::read_for(ws.path(), "exec_1"), None);
        std::fs::create_dir_all(ws.path().join(".boss")).unwrap();
        std::fs::write(ws.path().join(RECOVERY_REPORT_FILE), "{not json").unwrap();
        assert_eq!(RecoveryReport::read_for(ws.path(), "exec_1"), None);
    }

    #[test]
    fn apply_report_summary_mentions_filtered_bookkeeping() {
        let report = ApplyReport {
            paths: vec!["a.rs".to_owned(), "b.rs".to_owned()],
            insertions: 120,
            deletions: 14,
            filtered_paths: vec![".boss/events-pending.jsonl".to_owned()],
        };
        assert_eq!(
            report.summary(),
            "2 file(s), +120/-14 (1 bookkeeping file(s) filtered out)"
        );
    }

    // ── apply_recovery_patch, against a real git repo ─────────────

    fn git(args: &[&str], cwd: &Path) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Init a git repo with one committed file so `--3way` has blobs to work
    /// with. Returns false when git is unavailable so tests skip rather than
    /// fail in a hermetic sandbox.
    fn init_git_repo(path: &Path) -> bool {
        if !git(&["init", "--initial-branch=main"], path) {
            return false;
        }
        let _ = git(&["config", "user.email", "test@example.com"], path);
        let _ = git(&["config", "user.name", "Test"], path);
        std::fs::write(path.join("hello.txt"), "original\n").unwrap();
        git(&["add", "."], path) && git(&["commit", "-m", "seed"], path)
    }

    /// The P2 happy path: a patch holding real work is applied into the
    /// workspace and the restored content is actually on disk.
    #[test]
    fn applies_a_real_patch_and_reports_what_was_restored() {
        let ws = TempDir::new().unwrap();
        if !init_git_repo(ws.path()) {
            eprintln!("skipping: git unavailable in sandbox");
            return;
        }
        let recovery = TempDir::new().unwrap();
        let patch_path = recovery.path().join("exec_1.patch");
        std::fs::write(
            &patch_path,
            "diff --git a/hello.txt b/hello.txt\n\
             index 0000000..1111111 100644\n\
             --- a/hello.txt\n\
             +++ b/hello.txt\n\
             @@ -1 +1 @@\n\
             -original\n\
             +recovered work\n",
        )
        .unwrap();

        let report = apply_recovery_patch(ws.path(), &patch_path)
            .expect("apply should succeed")
            .expect("a patch with real work must report a restoration");
        assert_eq!(report.paths, ["hello.txt"]);
        assert_eq!(report.insertions, 1);
        assert_eq!(report.deletions, 1);
        assert_eq!(
            std::fs::read_to_string(ws.path().join("hello.txt")).unwrap(),
            "recovered work\n",
            "the recovered content must actually be in the working copy"
        );
        // The staged intermediate is cleaned up on success.
        assert!(!recovery.path().join("exec_1.filtered.patch").exists());
    }

    /// A patch that cannot apply must fail loudly with git's own message,
    /// never be swallowed into a silent "recovered" claim.
    #[test]
    fn a_failed_apply_is_loud() {
        let ws = TempDir::new().unwrap();
        if !init_git_repo(ws.path()) {
            eprintln!("skipping: git unavailable in sandbox");
            return;
        }
        let recovery = TempDir::new().unwrap();
        let patch_path = recovery.path().join("exec_2.patch");
        // References a blob that does not exist and a file that does not
        // exist, so neither the direct apply nor the 3-way fallback can work.
        std::fs::write(
            &patch_path,
            "diff --git a/absent.txt b/absent.txt\n\
             index deadbee..cafebab 100644\n\
             --- a/absent.txt\n\
             +++ b/absent.txt\n\
             @@ -1 +1 @@\n\
             -this line was never here\n\
             +replacement\n",
        )
        .unwrap();

        let err = apply_recovery_patch(ws.path(), &patch_path).expect_err("apply must fail loudly");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("git apply --3way"),
            "the error must name the failed command: {msg}"
        );
        assert!(
            msg.contains("MUST NOT proceed"),
            "the error must say the worker cannot treat this as recovered: {msg}"
        );
        // Evidence is preserved on failure.
        assert!(
            recovery.path().join("exec_2.filtered.patch").exists(),
            "the filtered patch git rejected must be left on disk"
        );
    }

    /// A bookkeeping-only patch reports "nothing restored" and runs no git at
    /// all — proven by the absence of any staged file and by working in a
    /// directory that is not a git repo.
    #[test]
    fn bookkeeping_only_patch_reports_nothing_restored_without_touching_git() {
        let not_a_repo = TempDir::new().unwrap();
        let recovery = TempDir::new().unwrap();
        let patch_path = recovery.path().join("exec_3.patch");
        std::fs::write(&patch_path, section(".boss/events-pending.jsonl", &["{\"e\":1}"], &[])).unwrap();

        let outcome = apply_recovery_patch(not_a_repo.path(), &patch_path).expect("must not error");
        assert!(outcome.is_none(), "a bookkeeping-only patch restores nothing");
        assert!(!recovery.path().join("exec_3.filtered.patch").exists());
    }

    #[test]
    fn apply_surfaces_a_missing_patch_file_rather_than_claiming_success() {
        let ws = TempDir::new().unwrap();
        let err = apply_recovery_patch(ws.path(), &ws.path().join("nope.patch")).expect_err("must error");
        assert!(format!("{err:#}").contains("failed to read recovery patch"));
    }
}
