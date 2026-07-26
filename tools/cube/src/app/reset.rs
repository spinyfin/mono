//! Resetting a workspace back to the default branch, including the guard that
//! refuses to discard unpushed work on release.

use std::path::Path;
use std::time::Instant;

use git_utils::repo_slug::parse_github_remote;

use crate::audit;
use crate::command_runner::{CommandRunner, RealCommandRunner};

use crate::app::errors::{CubeError, Result};
use crate::app::health::{audit_jj_op, read_head_status};
use crate::app::jj::{run_jj, run_jj_network, run_jj_within};
use crate::app::repo::is_unresolved_remote_target;

/// Resolve the GitHub remote name and `owner/repo` slug from `jj git remote
/// list` run inside the given workspace path.
pub(super) fn resolve_github_remote_for_workspace(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
) -> Result<(String, String)> {
    let remote_output = run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(workspace_path, "jj", &["git", "remote", "list"]),
    )?;
    parse_github_remote(&remote_output).ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "could not detect a github.com remote from `jj git remote list` in {}:\n{remote_output}",
            workspace_path.display()
        ))
    })
}

/// Fetch, then reset — refusing to run the destructive
/// `jj new <main>` step if the workspace's `@` still has the prior
/// lease holder's uncommitted work AND `prior_expired` says the lease
/// we just claimed was reclaimed-out-from-under that holder. Surfaces
/// [`CubeError::LeaseExpiredWorkspaceDirty`] so the lease handler can
/// abort cleanly instead of stomping on the still-active worker.
///
/// When `prior_expired` is `None` (normal release path, or a workspace
/// that was already `free`), the guard is a no-op and this is a plain
/// fetch-and-reset.
///
/// Every `jj` invocation here also writes an audit entry
/// (`workspace.jj_op`) so the next time someone reports "my `@`
/// moved", we can replay the log and prove or disprove a cube-side
/// reset.
pub(super) fn reset_workspace_guarded(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    main_branch: &str,
    prior_expired: Option<&crate::store::ExpiredLease>,
) -> Result<()> {
    audit_jj_op(database_path, workspace_path, "git", &["fetch"], prior_expired);
    run_jj_network(
        runner,
        database_path,
        &RealCommandRunner::invocation(workspace_path, "jj", &["git", "fetch"]),
    )?;

    if let Some(prior) = prior_expired {
        let head_status = read_head_status(runner, database_path, workspace_path, main_branch)?;
        if !head_status.is_reusable() {
            audit!(
                database_path,
                "workspace.reset_refused_dirty",
                workspace_path = workspace_path.display().to_string(),
                main_branch = main_branch,
                head_change_id = head_status.head_change_id(),
                head_is_empty = head_status.head_is_empty(),
                head_parent_bookmarks = head_status.head_parent_bookmarks(),
                parent_is_main = head_status.parent_is_main(),
                unpushed_commits = head_status.unpushed_summary(),
                prior_lease_id = prior.lease_id,
                prior_holder = prior.holder.as_deref(),
                prior_task = prior.task.as_deref(),
            );
            return Err(CubeError::LeaseExpiredWorkspaceDirty {
                workspace_path: workspace_path.to_path_buf(),
                prior_lease_id: prior.lease_id.clone(),
                prior_holder: prior.holder.clone().unwrap_or_else(|| "<unknown>".to_string()),
            });
        }
    }

    reset_workspace_after_fetch(runner, database_path, workspace_path, main_branch, prior_expired)
}

/// `last_release_reason` recorded when the release-time reuse guard preserves
/// a working copy and the caller supplied no reason of its own.
pub(super) const PRESERVED_UNPUSHED_RELEASE_REASON: &str = "unpushed_work_preserved";

/// What [`reset_workspace_on_release`] did to the working copy.
#[derive(Debug)]
pub(super) enum ReleaseResetOutcome {
    /// The workspace was fetched and reset to `<main>@<upstream>`.
    Reset,
    /// The destructive reset was skipped: `@` holds non-empty work that
    /// exists on no remote, so resetting would be the only thing standing
    /// between that work and oblivion.
    PreservedUnpushedWork(PreservedWorkingCopy),
}

/// Details of a working copy the release guard declined to reset, carried
/// through to the audit entry and the JSON payload so an operator can find
/// the preserved work without re-probing jj.
#[derive(Debug)]
pub(super) struct PreservedWorkingCopy {
    pub(super) head_change_id: String,
    /// `change:commit` pairs for the commits that exist on no remote.
    pub(super) unpushed_summary: String,
}

/// Reset a workspace as part of releasing its lease — but never at the cost
/// of work that exists nowhere else.
///
/// ## Why the guard is on by default rather than caller opt-in
///
/// `cube workspace release` used to reset unconditionally unless the caller
/// passed `--keep-dirty`. The Boss engine has more than a dozen release call
/// sites (normal completion, conflict watch, terminal-work sweep, host
/// reconcile, speculative conflict, …) and exactly one of them needs to be
/// the "the worker crashed, keep the tree" one. Getting that classification
/// right at every site, forever, is not a property any codebase has; the one
/// site that got it wrong (`cube_commands.rs`, which issues a bare
/// `workspace release --lease <id>`) is why in-flight work was destroyed on
/// every engine restart.
///
/// So the safe behaviour is the default and the destructive behaviour is the
/// opt-in (`--force-reset`). Cube is the component that can actually *see*
/// whether the tree holds unpushed work, so cube is where the decision
/// belongs — the caller does not have to know which kind of release it is
/// performing.
///
/// The predicate is [`crate::reuse_guard`]'s, unchanged and already load
/// bearing for TTL-expiry reclaim: `@` is non-empty AND is reachable from no
/// remote bookmark. A worker that finished and pushed its branch fails that
/// test (its work is on the remote) and is reset as before, so the steady
/// state of the pool is unaffected.
pub(super) fn reset_workspace_on_release(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    main_branch: &str,
    force_reset: bool,
) -> Result<ReleaseResetOutcome> {
    audit_jj_op(database_path, workspace_path, "git", &["fetch"], None);
    run_jj_network(
        runner,
        database_path,
        &RealCommandRunner::invocation(workspace_path, "jj", &["git", "fetch"]),
    )?;

    if !force_reset {
        // The fetch above is what makes this verdict trustworthy: the probe
        // asks "is this work on any remote?", and a stale remote view would
        // call already-pushed work unpushed and preserve the pool into
        // uselessness.
        let head_status = read_head_status(runner, database_path, workspace_path, main_branch)?;
        if !head_status.is_reusable() {
            audit!(
                database_path,
                "workspace.release_reset_preserved_dirty",
                workspace_path = workspace_path.display().to_string(),
                main_branch = main_branch,
                head_change_id = head_status.head_change_id(),
                head_is_empty = head_status.head_is_empty(),
                head_parent_bookmarks = head_status.head_parent_bookmarks(),
                parent_is_main = head_status.parent_is_main(),
                unpushed_commits = head_status.unpushed_summary(),
            );
            return Ok(ReleaseResetOutcome::PreservedUnpushedWork(PreservedWorkingCopy {
                head_change_id: head_status.head_change_id().to_string(),
                unpushed_summary: head_status.unpushed_summary(),
            }));
        }
    }

    reset_workspace_after_fetch(runner, database_path, workspace_path, main_branch, None)?;
    Ok(ReleaseResetOutcome::Reset)
}

/// The destructive half of a workspace reset, factored out so the
/// TTL-expiry path ([`reset_workspace_guarded`]) and the release path
/// ([`reset_workspace_on_release`]) can each apply their own guard between
/// the fetch and the `jj new`. Assumes `jj git fetch` has already run.
pub(super) fn reset_workspace_after_fetch(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    main_branch: &str,
    prior_expired: Option<&crate::store::ExpiredLease>,
) -> Result<()> {
    reset_workspace_after_fetch_within(runner, database_path, workspace_path, main_branch, prior_expired, None)
}

/// [`reset_workspace_after_fetch`] under a caller's wall-clock `deadline`, so
/// a time-budgeted pass (pool GC) cannot overrun its budget inside the reset
/// the way it could inside the probe that precedes it.
pub(super) fn reset_workspace_after_fetch_within(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    main_branch: &str,
    prior_expired: Option<&crate::store::ExpiredLease>,
    deadline: Option<Instant>,
) -> Result<()> {
    // Detect the real upstream remote by URL so both the fast-forward and the
    // `jj new` target the current GitHub HEAD. For source-pool workspaces this
    // resolves the `github` remote; for direct-GitHub clones it returns
    // `origin`. Using URL-based detection (rather than a `has_source` proxy)
    // means the correct remote is found even when the source mirror is later
    // GC'd after provisioning.
    let upstream_remote = detect_upstream_tracking_remote(runner, database_path, workspace_path, deadline);
    // Keep local `main` current so workers running `jj new main` themselves
    // (e.g. following their CLAUDE.md instruction) also branch from origin head.
    fast_forward_default_branch_to_origin(
        runner,
        database_path,
        workspace_path,
        main_branch,
        prior_expired,
        &upstream_remote,
        deadline,
    )?;

    // Branch directly from the remote-tracking bookmark, not the local one.
    // This guarantees the new-task `@` is on `main@origin` as of the fetch
    // above, even if the fast-forward step above warned and left local `main`
    // stale (incident: PR #1568 branched from a 3-commit-stale base because
    // `jj new main` used the local bookmark rather than the fetched remote head).
    let remote_ref = format!("{main_branch}@{upstream_remote}");
    audit_jj_op(database_path, workspace_path, "new", &[&remote_ref], prior_expired);
    run_jj_within(
        runner,
        database_path,
        &RealCommandRunner::invocation(workspace_path, "jj", &["new", &remote_ref]),
        deadline,
    )?;
    Ok(())
}

/// Detect the name of the remote that represents the real GitHub upstream for
/// a workspace, resolved by URL via `parse_github_remote` (github.com host).
///
/// In the shared-store model every pool workspace attaches (via
/// `jj workspace add`) to the canonical repo, whose sole remote `origin` IS the
/// real GitHub upstream — so this resolves to `"origin"`. The github.com lookup
/// (rather than a hard-coded `"origin"`) is retained as defense for two cases:
/// a canonical repo whose upstream happens to be named differently, and any
/// lingering pre-reprovision workspace cloned from a local mirror that still
/// carries a separate `github` remote. Falls back to `"origin"` when the remote
/// list cannot be resolved or no github.com remote is found.
fn detect_upstream_tracking_remote(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    deadline: Option<Instant>,
) -> String {
    let invocation = RealCommandRunner::invocation(workspace_path, "jj", &["git", "remote", "list"]);
    let remote_output = run_jj_within(runner, database_path, &invocation, deadline).unwrap_or_default();
    if let Some((name, _)) = parse_github_remote(&remote_output) {
        return name;
    }
    // No github.com remote found. If origin points to a local path this is
    // likely a source-pool workspace provisioned before the github-remote fix
    // (it has only `origin = /local/mirror` and no `github` remote). Warn so
    // operators know the workspace will keep fast-forwarding against the stale
    // mirror until it is re-provisioned.
    let origin_is_local = remote_output.lines().any(|line| {
        let mut parts = line.splitn(2, |c: char| c.is_whitespace());
        let name = parts.next().map(str::trim).unwrap_or_default();
        let url = parts.next().map(str::trim).unwrap_or_default();
        name == "origin" && (url.starts_with('/') || url.starts_with('.'))
    });
    if origin_is_local {
        eprintln!(
            "cube: warning: workspace at `{}` appears to be a pre-existing source-pool \
             workspace (origin is a local path, no github.com remote found). Fast-forward \
             will target the stale local mirror until the workspace is re-provisioned.",
            workspace_path.display()
        );
    }
    "origin".to_string()
}

/// Fast-forward the workspace's local default bookmark to the
/// `<main>@<upstream_remote>` position established by the preceding
/// `jj git fetch`. This keeps the local `<main>` bookmark current so that
/// workers running `jj new <main>` themselves (following their CLAUDE.md
/// instructions) also branch from the current upstream.
///
/// `jj git fetch` always updates remote-tracking bookmarks, but it
/// advances the *local* `<main>` bookmark only when it is still tracking
/// its remote and has not diverged. A reused workspace whose local
/// `<main>` fell out of tracking (or was nudged by an earlier op)
/// therefore keeps a days-old local `<main>` — which is exactly how
/// reused workspaces cut PR branches from a stale base (#1232). An
/// explicit `jj bookmark set` to the upstream tracking target closes
/// that gap unconditionally. `--allow-backwards` is intentional: the
/// local default branch must mirror the upstream exactly, even in the
/// rare case it somehow sits ahead.
///
/// NOTE: the workspace reset in `reset_workspace_guarded` now uses
/// `jj new <main>@<upstream>` directly (not `jj new <main>`), so the
/// positioning invariant no longer depends on this fast-forward having
/// succeeded. This step remains for local-bookmark hygiene; its failure
/// is still tolerated with a warning.
///
/// `upstream_remote` is the name of the remote that IS the real GitHub
/// upstream — `"origin"` for workspaces cloned directly from GitHub,
/// `"github"` for source-pool workspaces where `origin` is a local mirror.
///
/// Tolerant of an unresolvable target (a repo whose recorded default branch
/// has no matching remote bookmark): warn and continue. The caller
/// (`reset_workspace_guarded`) will then attempt `jj new <main>@<upstream>`
/// which will also fail and surface the misconfiguration as a hard error.
fn fast_forward_default_branch_to_origin(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    main_branch: &str,
    prior_expired: Option<&crate::store::ExpiredLease>,
    upstream_remote: &str,
    deadline: Option<Instant>,
) -> Result<()> {
    let remote_target = format!("{main_branch}@{upstream_remote}");
    audit_jj_op(
        database_path,
        workspace_path,
        "bookmark-set",
        &[main_branch, &remote_target],
        prior_expired,
    );
    let invocation = RealCommandRunner::invocation(
        workspace_path,
        "jj",
        &[
            "bookmark",
            "set",
            main_branch,
            "-r",
            &remote_target,
            "--allow-backwards",
        ],
    );
    match run_jj_within(runner, database_path, &invocation, deadline) {
        Ok(_) => Ok(()),
        Err(err) if is_unresolved_remote_target(&err) => {
            eprintln!(
                "warning: cube could not fast-forward `{main_branch}` to `{remote_target}` \
                 in {}: the remote-tracking bookmark did not resolve. Leaving local \
                 `{main_branch}` in place; check the repo's recorded default branch.",
                workspace_path.display()
            );
            Ok(())
        }
        Err(err) => Err(err),
    }
}
