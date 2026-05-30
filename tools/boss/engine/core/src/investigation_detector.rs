//! Investigation-doc auto-population from PR file scans.
//!
//! When a `kind=investigation` task's worker opens a PR, or that PR merges,
//! the engine scans the PR's changed files for a single markdown file under
//! any `investigations/` directory segment. If exactly one match is found it
//! becomes the task's `investigation_doc_path` / `investigation_doc_branch`
//! pointer. Zero matches or multiple matches skip auto-population with a
//! logged warning.
//!
//! Two entry points, called from their respective trigger modules:
//!
//! - [`on_investigation_pr_detected`] — fired when `tasks.pr_url` is set for
//!   a `kind=investigation` task (the `in_review` transition). Sets the
//!   task's doc pointer using the PR's **head** branch (e.g. `boss/exec_*`)
//!   so the in-app viewer can fetch the doc while the PR is still open.
//! - [`on_investigation_pr_merged`] — fired when `mark_chore_pr_merged`
//!   transitions the task to `done`. If the task already has a path,
//!   only `investigation_doc_branch` is updated to the PR's base branch
//!   (typically `"main"`). If the task has no path yet, the full pointer
//!   is written.

use std::process::Stdio;

use anyhow::{Context, Result};
use boss_protocol::SetTaskInvestigationDocInput;
use tokio::process::Command;

use crate::work::{WorkDb, WorkItem};

/// Metadata extracted from `gh pr view --json files,headRefName,baseRefName`.
struct PrScanResult {
    /// The single investigation-doc path found in the PR, or `None` if zero
    /// or multiple investigation docs were present.
    doc_path: Option<String>,
    /// Head branch name (e.g. `boss/exec_18b44c999f456508_4d`).
    head_ref_name: Option<String>,
    /// Base branch name (e.g. `main`).
    base_ref_name: Option<String>,
}

/// Fired by `completion::finalize_pr_transition` (target = `InReview`)
/// when the work item is `kind=investigation`.
///
/// Scans the PR's changed files for a single markdown file under any
/// `investigations/` directory. On a single match, sets the task's
/// `investigation_doc_path` and `investigation_doc_branch` using the PR's
/// **head** branch so the in-app viewer can fetch the doc from the open
/// PR branch.
pub async fn on_investigation_pr_detected(work_db: &WorkDb, task_id: &str, pr_url: &str) {
    let scan = match scan_pr(task_id, pr_url).await {
        Some(s) => s,
        None => return,
    };
    let Some(path) = scan.doc_path else {
        return;
    };
    let branch = scan.head_ref_name;
    let input = SetTaskInvestigationDocInput {
        task_id: task_id.to_owned(),
        investigation_doc_path: Some(path.clone()),
        investigation_doc_branch: branch.clone(),
        unset: false,
    };
    match work_db.set_task_investigation_doc(input) {
        Ok(_) => {
            tracing::info!(
                task_id,
                pr_url,
                path,
                branch,
                "investigation detector: set investigation-doc pointer (in_review)"
            );
        }
        Err(err) => {
            tracing::warn!(
                task_id,
                pr_url,
                ?err,
                "investigation detector: failed to set investigation-doc pointer (in_review)"
            );
        }
    }
}

/// Fired by `merge_poller::mark_merged` when the work item is
/// `kind=investigation`.
///
/// If the task already has `investigation_doc_path` set (from the in_review
/// detector or a prior manual edit), only `investigation_doc_branch` is
/// updated to `base_ref_name` (typically `"main"`), so consumers know the
/// doc is now on the default branch. The path is left unchanged.
///
/// If the path is not yet set, the PR is scanned and the full pointer is
/// written with `branch = base_ref_name`.
pub async fn on_investigation_pr_merged(
    work_db: &WorkDb,
    task_id: &str,
    pr_url: &str,
    base_ref_name: Option<&str>,
) {
    // Check whether the task already has an investigation-doc path set.
    let existing_path = match work_db.get_work_item(task_id) {
        Ok(WorkItem::Task(ref task) | WorkItem::Chore(ref task)) => {
            task.investigation_doc_path.clone()
        }
        Ok(other) => {
            tracing::warn!(
                task_id,
                kind = ?other,
                "investigation detector: work item is not a Task; skipping merge update"
            );
            return;
        }
        Err(err) => {
            tracing::warn!(
                task_id,
                ?err,
                "investigation detector: failed to fetch task for merge update"
            );
            return;
        }
    };

    if let Some(existing_path) = existing_path {
        // Path already set — update only the branch to the base (main).
        // Pass the existing path through because `set_task_investigation_doc`
        // requires a non-empty path even when the intent is a branch-only update.
        let effective_branch = base_ref_name.map(str::to_owned);
        let input = SetTaskInvestigationDocInput {
            task_id: task_id.to_owned(),
            investigation_doc_path: Some(existing_path.clone()),
            investigation_doc_branch: effective_branch.clone(),
            unset: false,
        };
        match work_db.set_task_investigation_doc(input) {
            Ok(_) => {
                tracing::info!(
                    task_id,
                    pr_url,
                    path = existing_path,
                    branch = effective_branch,
                    "investigation detector: updated investigation-doc branch to base after merge"
                );
            }
            Err(err) => {
                tracing::warn!(
                    task_id,
                    pr_url,
                    ?err,
                    "investigation detector: failed to update investigation-doc branch after merge"
                );
            }
        }
        return;
    }

    // Path not set — scan the PR files and write the full pointer.
    let scan = match scan_pr(task_id, pr_url).await {
        Some(s) => s,
        None => return,
    };
    let Some(path) = scan.doc_path else {
        return;
    };
    let effective_branch = base_ref_name.map(str::to_owned).or(scan.base_ref_name);
    let input = SetTaskInvestigationDocInput {
        task_id: task_id.to_owned(),
        investigation_doc_path: Some(path.clone()),
        investigation_doc_branch: effective_branch.clone(),
        unset: false,
    };
    match work_db.set_task_investigation_doc(input) {
        Ok(_) => {
            tracing::info!(
                task_id,
                pr_url,
                path,
                branch = effective_branch,
                "investigation detector: set investigation-doc pointer after merge"
            );
        }
        Err(err) => {
            tracing::warn!(
                task_id,
                pr_url,
                ?err,
                "investigation detector: failed to set investigation-doc pointer after merge"
            );
        }
    }
}

/// Call `gh pr view <pr_url> --json files,headRefName,baseRefName` and parse
/// the result. Returns `None` on tool failures; warnings are logged internally.
async fn scan_pr(task_id: &str, pr_url: &str) -> Option<PrScanResult> {
    match do_scan_pr(pr_url).await {
        Ok(result) => Some(result),
        Err(err) => {
            tracing::warn!(
                task_id,
                pr_url,
                ?err,
                "investigation detector: failed to scan PR files"
            );
            None
        }
    }
}

async fn do_scan_pr(pr_url: &str) -> Result<PrScanResult> {
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            pr_url,
            "--json",
            "files,headRefName,baseRefName",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to spawn `gh pr view {pr_url}`"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "`gh pr view {pr_url} --json files,headRefName,baseRefName` failed: {}",
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let root: serde_json::Value = serde_json::from_str(&stdout)
        .with_context(|| format!("failed to parse `gh pr view {pr_url}` JSON"))?;

    let head_ref_name = root
        .get("headRefName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let base_ref_name = root
        .get("baseRefName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let matches: Vec<String> = root
        .get("files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| f.get("path").and_then(|p| p.as_str()).map(str::to_owned))
                .filter(|p| is_investigation_doc_path(p))
                .collect()
        })
        .unwrap_or_default();

    let doc_path = match matches.len() {
        1 => Some(matches.into_iter().next().unwrap()),
        0 => {
            tracing::warn!(
                pr_url,
                "investigation detector: no `investigations/*.md` file in PR changed files; \
                 investigation-doc pointer not updated — add the file and re-push, or set \
                 manually with `boss task set-investigation-doc`"
            );
            None
        }
        n => {
            tracing::warn!(
                pr_url,
                count = n,
                "investigation detector: multiple `investigations/*.md` files in PR; \
                 skipping auto-populate — use `boss task set-investigation-doc` to resolve"
            );
            None
        }
    };

    Ok(PrScanResult {
        doc_path,
        head_ref_name,
        base_ref_name,
    })
}

/// Return `true` when `path` is a direct child of any `investigations/`
/// directory, regardless of the leading product prefix. Examples:
/// - `docs/investigations/foo.md`                    → true
/// - `tools/boss/docs/investigations/foo.md`         → true
/// - `investigations/foo.md`                         → true
/// - `tools/boss/docs/investigations/sub/foo.md`     → false (sub-directory)
/// - `tools/boss/docs/other/foo.md`                  → false (wrong segment)
fn is_investigation_doc_path(path: &str) -> bool {
    let rest = if let Some(rest) = path.strip_prefix("investigations/") {
        rest
    } else if let Some((_, rest)) = path.split_once("/investigations/") {
        rest
    } else {
        return false;
    };
    !rest.contains('/') && (rest.ends_with(".md") || rest.ends_with(".markdown"))
}

#[cfg(test)]
mod tests {
    use super::is_investigation_doc_path;

    #[test]
    fn test_is_investigation_doc_path() {
        assert!(is_investigation_doc_path("investigations/foo.md"));
        assert!(is_investigation_doc_path("docs/investigations/foo.md"));
        assert!(is_investigation_doc_path(
            "tools/boss/docs/investigations/foo.md"
        ));
        assert!(is_investigation_doc_path(
            "tools/boss/docs/investigations/chore-vs-project-task-collapse-2026-05-30.md"
        ));

        assert!(!is_investigation_doc_path(
            "tools/boss/docs/investigations/sub/foo.md"
        ));
        assert!(!is_investigation_doc_path("tools/boss/docs/other/foo.md"));
        assert!(!is_investigation_doc_path("docs/designs/foo.md"));
        assert!(!is_investigation_doc_path("foo.md"));
        assert!(!is_investigation_doc_path("investigations_extra/foo.md"));
    }
}
