//! Minting a brand-new workspace when the pool has nothing free, and running
//! its `.cube/setup.yaml` steps.

use std::fs;
use std::path::Path;

use crate::command_runner::{CommandInvocation, CommandRunner};
use crate::metadata::{RepoRecord, WorkspaceRecord};
use crate::setup::{SetupReport, run_setup_engine};
use crate::store::Store;
use crate::{audit, setup};

use crate::app::errors::{CubeError, Result};
use crate::app::jj::{network_cmd_timeout, workspace_path_exists};
use crate::app::util::current_epoch_s;

pub(super) fn next_workspace_id(prefix: &str, existing: &[String]) -> String {
    let mut max_n: u32 = 0;
    let mut found_any = false;
    for id in existing {
        if let Some(suffix) = id.strip_prefix(prefix)
            && let Ok(n) = suffix.parse::<u32>()
        {
            found_any = true;
            if n > max_n {
                max_n = n;
            }
        }
    }
    let next = if found_any { max_n + 1 } else { 1 };
    format!("{prefix}{next:03}")
}

pub(super) fn auto_create_workspace(
    runner: &dyn CommandRunner,
    repo_record: &RepoRecord,
    existing: &[crate::metadata::WorkspaceCandidate],
) -> Result<crate::metadata::WorkspaceCandidate> {
    let existing_ids: Vec<String> = existing.iter().map(|c| c.workspace_id.clone()).collect();
    let workspace_id = next_workspace_id(&repo_record.workspace_prefix, &existing_ids);
    let workspace_path = repo_record.workspace_root.join(&workspace_id);

    fs::create_dir_all(&repo_record.workspace_root).map_err(|e| CubeError::WorkspaceDirCreate {
        path: repo_record.workspace_root.clone(),
        source: e,
    })?;

    // The canonical repo is the single shared object store every pool workspace
    // attaches to via `jj workspace add` — the cube design (R4) chose the
    // shared-store/worktree model and explicitly REJECTED "fresh clone per
    // task". `cube repo ensure` materialises it at `repo_record.source`: a
    // colocated jj repo whose `origin` is the real GitHub upstream and which
    // carries a local `main` bookmark. Without it there is nothing to attach to,
    // so surface a clear, actionable error instead of silently regressing to an
    // independent clone.
    let canonical = repo_record.source.as_ref().ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "cannot grow the workspace pool for repo `{}`: it has no canonical source repo to \
             attach to. Run `cube repo ensure` first so cube materialises the shared object store \
             (default `~/.local/share/cube/repos/{}`); pool workspaces are `jj workspace add` \
             attachments to it, not independent clones.",
            repo_record.repo, repo_record.repo
        ))
    })?;
    if !canonical.join(".jj").is_dir() {
        return Err(CubeError::InvalidArgument(format!(
            "canonical source repo for `{}` at `{}` has no `.jj/` store — it was never \
             materialised or has been removed. Run `cube repo ensure` to (re)create the shared \
             store before leasing.",
            repo_record.repo,
            canonical.display()
        )));
    }

    // Attach under a dotted staging name first, then publish it under its final
    // name with an atomic rename. A create interrupted mid-flight leaves only the
    // dotted staging dir — which `discover_workspaces` ignores (it doesn't match
    // the workspace prefix) — so a partially-populated tree can never be observed
    // as a pool entry and become a "broken-empty" husk (issue #845 part 2a). The
    // rename is safe across `jj workspace add`: jj resolves a workspace from the
    // `.jj/` it finds relative to cwd, not from any path recorded in the store, so
    // moving the directory after the attach is transparent. The repo lock is held
    // across the whole lease, so the staging name can't race a concurrent create.
    let staging_path = repo_record.workspace_root.join(format!(".incoming-{workspace_id}"));
    if staging_path.exists() {
        // A leftover staging dir means a prior create was interrupted after
        // `jj workspace add` registered this workspace in the SHARED canonical
        // store but before the publish rename. Unlike an independent clone, the
        // attach mutated that shared store, so the dangling registration must be
        // forgotten or the re-add below collides with "workspace already exists".
        // Best-effort: tolerate "no such workspace" when there is nothing to forget.
        let _ = runner.run(&CommandInvocation {
            cwd: repo_record.workspace_root.clone(),
            program: "jj".to_string(),
            args: vec![
                "-R".to_string(),
                canonical.display().to_string(),
                "workspace".to_string(),
                "forget".to_string(),
                workspace_id.clone(),
            ],
            env: vec![],
        });
        fs::remove_dir_all(&staging_path).map_err(|source| CubeError::WorkspaceDirRemove {
            path: staging_path.clone(),
            source,
        })?;
    }

    // `jj workspace add` attaches a new working copy that SHARES the canonical
    // store: the new `.jj/repo` is a file POINTER to `<canonical>/.jj/repo`, not a
    // full history copy (the whole point — see incident: independent clones were
    // tens of GB each). `--name <workspace_id>` pins the store-side workspace name
    // to the pool id regardless of the staging basename, so the publish rename
    // needs no fix-up. This is a local operation; the timeout is only a backstop.
    runner.run_with_timeout(
        &CommandInvocation {
            cwd: repo_record.workspace_root.clone(),
            program: "jj".to_string(),
            args: vec![
                "-R".to_string(),
                canonical.display().to_string(),
                "workspace".to_string(),
                "add".to_string(),
                "--name".to_string(),
                workspace_id.clone(),
                staging_path.display().to_string(),
            ],
            env: vec![],
        },
        network_cmd_timeout(),
    )?;

    // No per-workspace remote/bookmark setup is needed: the shared store already
    // carries `origin` = the real GitHub upstream and a local `main` bookmark, both
    // established once by `materialize_repo_source_if_missing` at ensure time. Every
    // attached workspace sees them, so the lease's later `jj new main` resolves.

    // Publish atomically. Staging and final live under the same workspace_root
    // (one filesystem), so the rename is atomic and the final path appears
    // only when the attach is complete.
    fs::rename(&staging_path, &workspace_path).map_err(|source| CubeError::WorkspaceDirCreate {
        path: workspace_path.clone(),
        source,
    })?;

    Ok(crate::metadata::WorkspaceCandidate {
        workspace_id,
        workspace_path,
    })
}

/// Self-heal a broken-empty pool entry: a workspace directory that exists but
/// has neither `.jj/` nor `.git/`. Such a husk holds no recoverable work
/// (no commits, no working copy), so remove the directory and forget its
/// registry row, freeing the slot for a fresh clone. Called from the lease
/// path so a degraded pool self-repairs instead of blocking a lease
/// (issue #845 part 2b).
pub(super) fn gc_broken_empty_workspace(
    store: &mut Store,
    database_path: Option<&Path>,
    repo: &str,
    workspace_id: &str,
    workspace_path: &Path,
) -> Result<()> {
    if workspace_path.exists() {
        fs::remove_dir_all(workspace_path).map_err(|source| CubeError::WorkspaceDirRemove {
            path: workspace_path.to_path_buf(),
            source,
        })?;
    }
    store.forget_workspace(repo, workspace_id)?;
    eprintln!(
        "warning: cube workspace `{repo}/{workspace_id}` had neither .git/ nor .jj/ \
         (broken-empty) at {}; removing the husk and provisioning a fresh workspace",
        workspace_path.display(),
    );
    audit!(
        database_path,
        "workspace.broken_empty_gc",
        repo = repo,
        workspace_id = workspace_id,
        workspace_path = workspace_path.display().to_string(),
    );
    Ok(())
}

pub(super) fn run_setup_for_workspace(
    store: &Store,
    runner: &dyn CommandRunner,
    workspace: &WorkspaceRecord,
) -> Result<SetupReport> {
    let Some(config) = setup::read_setup_config(&workspace.workspace_path)? else {
        return Ok(SetupReport::empty());
    };
    let now = current_epoch_s()?;
    run_setup_engine(store, runner, workspace, &config, now)
}

pub(super) fn format_lease_message(lease_message: &str, report: &SetupReport) -> String {
    if report.steps.is_empty() {
        return lease_message.to_string();
    }
    format!(
        "{lease_message} Setup: {} ran, {} skipped.",
        report.ran_count(),
        report.skipped_count()
    )
}

pub(super) fn format_setup_message(workspace_id: &str, report: &SetupReport) -> String {
    if report.steps.is_empty() {
        return format!("No setup steps are configured for {workspace_id}.");
    }
    format!(
        "Setup complete for {workspace_id}: {} ran, {} skipped.",
        report.ran_count(),
        report.skipped_count()
    )
}

pub(super) fn discover_workspaces(repo: &RepoRecord) -> Result<Vec<crate::metadata::WorkspaceCandidate>> {
    let mut candidates = Vec::new();
    if !repo.workspace_root.is_dir() {
        return Ok(candidates);
    }
    for entry in fs::read_dir(&repo.workspace_root).map_err(|e| CubeError::WorkspaceDirRead {
        path: repo.workspace_root.clone(),
        source: e,
    })? {
        let entry = entry.map_err(|e| CubeError::WorkspaceDirRead {
            path: repo.workspace_root.clone(),
            source: e,
        })?;
        let file_type = entry.file_type().map_err(|e| CubeError::WorkspaceDirRead {
            path: entry.path(),
            source: e,
        })?;
        if !file_type.is_dir() {
            continue;
        }

        let workspace_id = entry.file_name();
        let workspace_id = workspace_id.to_string_lossy().to_string();
        if !workspace_id.starts_with(&repo.workspace_prefix) {
            continue;
        }

        candidates.push(crate::metadata::WorkspaceCandidate {
            workspace_id,
            workspace_path: entry.path(),
        });
    }

    candidates.sort_by(|left, right| left.workspace_id.cmp(&right.workspace_id));
    Ok(candidates)
}

pub(super) fn find_workspace_record(
    store: &mut Store,
    workspace_path: &Path,
) -> Result<Option<crate::metadata::WorkspaceRecord>> {
    if let Some(record) = store.get_workspace_by_path(workspace_path)? {
        if workspace_path_exists(&record) {
            return Ok(Some(record));
        }
        store.forget_workspace(&record.repo, &record.workspace_id)?;
    }

    for repo in store.list_repos()? {
        if workspace_path.starts_with(&repo.workspace_root) {
            let candidates = discover_workspaces(&repo)?;
            store.sync_workspaces(&repo.repo, &candidates)?;
        }
    }

    if let Some(record) = store.get_workspace_by_path(workspace_path)? {
        if workspace_path_exists(&record) {
            return Ok(Some(record));
        }
        store.forget_workspace(&record.repo, &record.workspace_id)?;
    }

    Ok(None)
}
