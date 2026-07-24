//! Pre-lease health checks on a free workspace: dirty, conflicted, broken, or
//! stale working copies, and the reuse verdict for its `@`.

use std::path::Path;

use crate::command_runner::{CommandInvocation, CommandRunner, RealCommandRunner};
use crate::{audit, reuse_guard};

use crate::app::errors::Result;
use crate::app::jj::{
    jj_needs_colocate_init, jj_update_stale_recovery_kind, network_cmd_timeout, run_jj, run_jj_network,
};

/// Outcome of a pre-lease health check on a free workspace.
#[derive(Debug, Clone)]
pub(super) enum WorkspaceHealthOutcome {
    /// Working copy is clean and no bookmarks are conflicted. Ready to use as-is.
    Clean,
    /// Working copy is clean, but one or more bookmarks are conflicted.
    /// Auto-repairable: forget the named bookmarks before resetting.
    ConflictedBookmarks(Vec<String>),
    /// Working copy has uncommitted changes from a prior worker session.
    /// Not safe to auto-repair — skip this workspace.
    DirtyWorkingCopy,
    /// Workspace directory exists but has neither `.jj/` nor `.git/`.
    /// The directory was likely wiped externally. Requires manual re-clone or
    /// force-release. Not safe to auto-repair without the source.
    BrokenEmpty,
    /// `jj status` returned a stale-working-copy error (the op-log entry for
    /// the working copy could not be read). Auto-recoverable: the lease handler
    /// runs `jj workspace update-stale` and retries the health check. If the
    /// retry comes back clean the workspace is used; if recovery fails the
    /// workspace is skipped and a fresh one is provisioned instead.
    ///
    /// The inner string is the audit event name to emit on successful recovery
    /// (e.g. `"workspace.stale_recovered"` or `"workspace.op_diverged_recovered"`).
    StaleWorkingCopy(&'static str),
}

/// Check the health of a free workspace by running `jj status`. Returns
/// [`WorkspaceHealthOutcome`] so the lease handler can decide whether to
/// skip, repair, or immediately use the workspace.
///
/// This function does NOT transparently recover from stale working copies.
/// When `jj status` exits non-zero with a stale-op signature it returns
/// [`WorkspaceHealthOutcome::StaleWorkingCopy`] so the lease handler can
/// explicitly drive recovery with `jj workspace update-stale` and retry.
/// Colocate-init (a `.git`-only workspace missing its `.jj` overlay) IS
/// performed inline here, since it is a structural fix that must happen
/// before any further jj operations can succeed.
pub(super) fn check_workspace_health(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
) -> Result<WorkspaceHealthOutcome> {
    // Detect broken-empty workspaces by checking the directory state
    // directly rather than waiting for jj to report an error. A workspace
    // with neither .jj/ nor .git/ cannot be used or healed without a
    // full re-clone, so return early before spawning jj at all.
    if !workspace_path.join(".jj").is_dir() && !workspace_path.join(".git").is_dir() {
        return Ok(WorkspaceHealthOutcome::BrokenEmpty);
    }

    let status_invocation = CommandInvocation {
        cwd: workspace_path.to_path_buf(),
        program: "jj".to_string(),
        args: vec!["status".to_string(), "--no-pager".to_string()],
        env: vec![],
    };

    // Run jj status directly (not via run_jj) so we can intercept the
    // stale-working-copy error before run_jj would transparently recover it.
    // This makes StaleWorkingCopy a first-class outcome the lease handler can
    // audit and drive, rather than a transparent retry inside run_jj.
    let output = match runner.run_with_timeout(&status_invocation, network_cmd_timeout()) {
        Ok(out) => out,
        Err(err) => {
            // Stale working copy: surface as StaleWorkingCopy so the lease
            // handler decides whether to run update-stale and retry.
            if let Some(recovery_kind) = jj_update_stale_recovery_kind(&err) {
                return Ok(WorkspaceHealthOutcome::StaleWorkingCopy(recovery_kind));
            }
            // Colocate-init: workspace has .git but no .jj overlay. Fix it
            // in-place and retry jj status. If colocate-init fails, propagate
            // the original error so the lease loop can skip this workspace.
            if jj_needs_colocate_init(&err, workspace_path) {
                let init = RealCommandRunner::invocation(workspace_path, "jj", &["git", "init", "--colocate"]);
                if runner.run(&init).is_err() {
                    return Err(err);
                }
                audit!(
                    database_path,
                    "workspace.jj_colocate_initialised",
                    workspace_path = workspace_path.display().to_string(),
                    program = status_invocation.program,
                    args = status_invocation.args,
                );
                match runner.run_with_timeout(&status_invocation, network_cmd_timeout()) {
                    Ok(out) => out,
                    Err(_) => return Err(err),
                }
            } else {
                return Err(err);
            }
        }
    };

    // "Working copy changes:" appears when jj has file-level changes staged
    // or present in the working copy. Its absence means the working copy
    // itself is clean (though bookmarks may still be conflicted).
    if output
        .lines()
        .any(|l| l.trim_start().starts_with("Working copy changes:"))
    {
        return Ok(WorkspaceHealthOutcome::DirtyWorkingCopy);
    }

    let conflicted = parse_conflicted_bookmarks_from_status(&output);
    if !conflicted.is_empty() {
        return Ok(WorkspaceHealthOutcome::ConflictedBookmarks(conflicted));
    }

    Ok(WorkspaceHealthOutcome::Clean)
}

/// Extract conflicted bookmark names from `jj status` output.
///
/// `jj status` includes a section like:
/// ```text
/// These bookmarks have conflicts:
///   fix-spawn-worker-pane-burst-crash
///   Use `jj bookmark list` to see details. ...
/// ```
/// The bookmark names are the indented lines before the "Use …" hint.
fn parse_conflicted_bookmarks_from_status(status: &str) -> Vec<String> {
    let mut in_section = false;
    let mut bookmarks = Vec::new();
    for line in status.lines() {
        if line.contains("These bookmarks have conflicts:") {
            in_section = true;
            continue;
        }
        if in_section {
            let trimmed = line.trim();
            if trimmed.is_empty() || !line.starts_with(' ') {
                break; // end of section
            }
            // Skip the advisory "Use `jj bookmark list`…" line.
            if trimmed.starts_with("Use `jj bookmark") {
                continue;
            }
            bookmarks.push(trimmed.to_string());
        }
    }
    bookmarks
}

/// Forget each conflicted bookmark so the workspace no longer reports
/// bookmark conflicts. Called at lease time when the health check finds
/// `ConflictedBookmarks` and the workspace is otherwise clean (working
/// copy empty). `jj bookmark forget` removes the local tracking state
/// without touching remote refs.
pub(super) fn repair_conflicted_bookmarks(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    bookmarks: &[String],
) -> Result<()> {
    for bookmark in bookmarks {
        audit!(
            database_path,
            "workspace.bookmark_forgotten",
            workspace_path = workspace_path.display().to_string(),
            bookmark = bookmark,
        );
        run_jj(
            runner,
            database_path,
            &CommandInvocation {
                cwd: workspace_path.to_path_buf(),
                program: "jj".to_string(),
                args: vec!["bookmark".to_string(), "forget".to_string(), bookmark.clone()],
                env: vec![],
            },
        )?;
    }
    Ok(())
}

/// Audit one of cube's own `jj` invocations against a leased
/// workspace. Pair-reads of this with the cube audit log let an
/// investigator answer "did cube move my `@`?" without guesswork —
/// every fetch/new/log/etc. cube runs lands here with the workspace
/// path, the verb, and (when relevant) the lease that just expired.
pub(super) fn audit_jj_op(
    database_path: Option<&Path>,
    workspace_path: &Path,
    verb: &str,
    args: &[&str],
    prior_expired: Option<&crate::store::ExpiredLease>,
) {
    audit!(
        database_path,
        "workspace.jj_op",
        workspace_path = workspace_path.display().to_string(),
        verb = verb,
        args = args,
        prior_expired_lease_id = prior_expired.map(|e| e.lease_id.as_str()),
        prior_expired_holder = prior_expired.and_then(|e| e.holder.as_deref()),
    );
}

/// Snapshot of `jj`'s view of a workspace's `@`, plus the guard's verdict on
/// whether a destructive reset would orphan anything. See [`crate::reuse_guard`]
/// for the predicate itself and why the original one was wrong.
#[derive(Debug)]
pub(super) struct HeadStatus {
    head: reuse_guard::ParsedHead,
    /// Non-empty commits reachable from `@` that no remote bookmark holds.
    /// Empty when the fast path made the probe unnecessary.
    unpushed: Vec<reuse_guard::UnpushedCommit>,
    verdict: reuse_guard::ReuseVerdict,
}

impl HeadStatus {
    pub(super) fn is_reusable(&self) -> bool {
        matches!(self.verdict, reuse_guard::ReuseVerdict::Reuse(_))
    }

    pub(super) fn head_change_id(&self) -> &str {
        &self.head.change_id
    }

    pub(super) fn head_is_empty(&self) -> bool {
        self.head.is_empty
    }

    /// jj's rendered bookmark text (`main*`, `main@git`, …), carried purely so
    /// the audit trail stays comparable with pre-fix events.
    pub(super) fn head_parent_bookmarks(&self) -> &str {
        &self.head.rendered_parent_bookmarks
    }

    /// Whether any parent carries the main bookmark, compared by identity.
    pub(super) fn parent_is_main(&self) -> bool {
        self.head.parent_is_main
    }

    /// The orphanable commits, rendered for the audit trail.
    pub(super) fn unpushed_summary(&self) -> String {
        self.unpushed
            .iter()
            .map(|c| format!("{}:{}", c.change_id, c.commit_id))
            .collect::<Vec<_>>()
            .join(",")
    }

    pub(super) fn reuse_reason(&self) -> Option<&'static str> {
        match self.verdict {
            reuse_guard::ReuseVerdict::Reuse(reason) => Some(reason.as_str()),
            reuse_guard::ReuseVerdict::Refuse => None,
        }
    }
}

pub(super) fn read_head_status(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    main_branch: &str,
) -> Result<HeadStatus> {
    let output = run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            workspace_path,
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", reuse_guard::HEAD_STATUS_TEMPLATE],
        ),
    )?;
    let head = reuse_guard::parse_head_status(&output, main_branch);

    // Only ask jj the expensive question when the cheap one didn't settle it:
    // an empty `@` sitting on main is the steady state and needs no probe.
    let unpushed = if reuse_guard::needs_unpushed_probe(&head) {
        let revset = reuse_guard::unpushed_work_revset(main_branch);
        let output = run_jj(
            runner,
            database_path,
            &RealCommandRunner::invocation(
                workspace_path,
                "jj",
                &[
                    "log",
                    "--no-graph",
                    "-n",
                    reuse_guard::UNPUSHED_PROBE_LIMIT,
                    "-r",
                    &revset,
                    "-T",
                    reuse_guard::UNPUSHED_PROBE_TEMPLATE,
                ],
            ),
        )?;
        reuse_guard::parse_unpushed_commits(&output)
    } else {
        Vec::new()
    };

    let verdict = reuse_guard::decide(&head, &unpushed);
    Ok(HeadStatus {
        head,
        unpushed,
        verdict,
    })
}

/// Fetch, then run the reuse guard's probe against a workspace nobody holds.
///
/// The fetch matters: the probe decides "is this work anywhere else?" against
/// the remote-tracking bookmarks, so a stale view would call already-pushed
/// work orphaned and refuse. Used by the quarantine-reclaim paths, which need
/// the verdict *without* performing the reset that `reset_workspace_guarded`
/// couples it to.
pub(super) fn probe_workspace_reuse(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    main_branch: &str,
) -> Result<HeadStatus> {
    audit_jj_op(database_path, workspace_path, "git", &["fetch"], None);
    run_jj_network(
        runner,
        database_path,
        &RealCommandRunner::invocation(workspace_path, "jj", &["git", "fetch"]),
    )?;
    read_head_status(runner, database_path, workspace_path, main_branch)
}
