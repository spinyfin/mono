//! Recovery that never asks a damaged jj store to read its own index.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use crate::audit;
use crate::command_runner::{CommandInvocation, CommandRunner, JJ_GATE_HELD_ENV, JjGate};
use crate::lock::RepoLock;
use crate::metadata::WorkspaceState;
use crate::store::Store;

use super::errors::{CubeError, Result, RunResult};
use super::repo::materialize_repo_source_if_missing;

/// Remove read-only build trees without following symlinks outside the workspace.
pub(super) fn remove_workspace_tree(path: &Path) -> Result<()> {
    fn writable_dirs(path: &Path) -> std::io::Result<()> {
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_dir() || meta.is_symlink() {
            return Ok(());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(meta.permissions().mode() | 0o700))?;
        }
        for entry in fs::read_dir(path)? {
            writable_dirs(&entry?.path())?;
        }
        Ok(())
    }
    let result = writable_dirs(path).and_then(|()| fs::remove_dir_all(path));
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !path.exists() => Ok(()),
        Err(source) => Err(CubeError::WorkspaceDirRemove {
            path: path.into(),
            source,
        }),
    }
}

/// Scan actual open files and working directories, rather than process names.
fn require_unused(runner: &dyn CommandRunner, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let invocation = CommandInvocation {
        cwd: path.parent().unwrap_or(path).into(),
        program: "lsof".into(),
        args: vec!["-nP".into(), "-Fpcn".into(), "+D".into(), path.display().to_string()],
        env: vec![],
    };
    match runner.run_with_timeout(&invocation, Duration::from_secs(120)) {
        Err(CubeError::CommandFailed {
            status: Some(1),
            stderr,
            ..
        }) if stderr.trim().is_empty() => Ok(()),
        Ok(output) if output.trim().is_empty() => Ok(()),
        Ok(output) => Err(CubeError::InvalidArgument(format!(
            "cannot reclone: processes are using {}; stop them and retry:\n{output}",
            path.display()
        ))),
        Err(err) => Err(CubeError::InvalidArgument(format!(
            "cannot verify that {} is unused: {err}; install lsof and ensure it can inspect the store before retrying",
            path.display()
        ))),
    }
}

/// Enumerate local inputs through Git without reading jj's damaged index.
pub(super) fn untracked_inputs(runner: &dyn CommandRunner, source: &Path) -> Result<Vec<PathBuf>> {
    let output = runner.run(&CommandInvocation {
        cwd: source.into(),
        program: "git".into(),
        args: ["ls-files", "--others", "-z", "--", ".", ":(exclude).jj", ":(exclude).git"]
            .into_iter().map(str::to_string).collect(),
        env: vec![],
    }).map_err(|error| CubeError::InvalidArgument(format!(
        "cannot enumerate untracked setup files in {}: {error}; repair the Git index before recloning so local setup inputs can be preserved", source.display()
    )))?;
    let inputs: Vec<PathBuf> = output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect();
    for path in &inputs {
        if path.components().any(|c| !matches!(c, Component::Normal(_)))
            || path.starts_with(".jj")
            || path.starts_with(".git")
        {
            return Err(CubeError::InvalidArgument(format!(
                "unsafe untracked setup path {}; original store left intact",
                path.display()
            )));
        }
    }
    Ok(inputs)
}

/// Delegate clone commands under the exclusive gate already held by recovery.
struct RecoveryRunner<'a>(&'a dyn CommandRunner);
impl CommandRunner for RecoveryRunner<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        self.run_with_timeout(invocation, super::jj::network_cmd_timeout())
    }
    fn run_with_timeout(&self, invocation: &CommandInvocation, timeout: Duration) -> Result<String> {
        let mut invocation = invocation.clone();
        invocation.env.push((JJ_GATE_HELD_ENV.into(), "1".into()));
        self.0.run_with_timeout(&invocation, timeout)
    }
}

pub(super) fn reclone(
    store: &Store,
    runner: &dyn CommandRunner,
    database: Option<&Path>,
    repo: &str,
) -> Result<RunResult> {
    let data_dir = database
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .map(Ok)
        .unwrap_or_else(crate::paths::data_dir)?;
    // Match lease lock ordering: gate, then registry lock. Also exclude upkeep.
    let gate = JjGate::exclusive(&JjGate::path(&data_dir), Duration::from_secs(2))?;
    let _lock = RepoLock::acquire(&super::util::repo_lock_path(repo, database)?)?;
    let _upkeep = RepoLock::try_acquire(&super::store_gc::store_gc_lock_path(repo, database)?)?.ok_or_else(|| {
        CubeError::InvalidArgument(format!("repo `{repo}` upkeep is running; retry after it finishes"))
    })?;
    let record = store
        .get_repo(repo)?
        .ok_or_else(|| CubeError::RepoNotFound(repo.into()))?;
    let source = record.source.as_ref().ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "repo `{repo}` has no source path; run cube repo ensure {repo} first"
        ))
    })?;
    if let Some(owner) = gate.quarantine_owner()?
        && owner != source.display().to_string()
    {
        return Err(CubeError::MaintenanceQuarantined(owner));
    }
    if fs::symlink_metadata(source.join(".jj/repo")).is_ok_and(|m| !m.is_dir() || m.is_symlink()) {
        return Err(CubeError::InvalidArgument(format!(
            "{} is not a primary jj store (.jj/repo must be a directory); ensure the repo source points at its primary clone before recloning",
            source.display()
        )));
    }
    let workspaces = store.list_workspaces(repo)?;
    let leased: Vec<_> = workspaces
        .iter()
        .filter(|w| w.state == WorkspaceState::Leased)
        .map(|w| w.workspace_id.as_str())
        .collect();
    if !leased.is_empty() {
        return Err(CubeError::InvalidArgument(format!(
            "repo `{repo}` has leased workspaces ({}); release them before recloning",
            leased.join(", ")
        )));
    }
    let mut paths: Vec<_> = workspaces.iter().map(|w| w.workspace_path.clone()).collect();
    for candidate in super::provision::discover_workspaces(&record)? {
        if !paths.contains(&candidate.workspace_path) {
            paths.push(candidate.workspace_path);
        }
    }
    require_unused(runner, source)?;
    for path in &paths {
        require_unused(runner, path)?;
    }
    // Validate literal references before moving anything. A damaged index is
    // irrelevant: setup declarations and local secrets are ordinary files.
    setup_inputs(source)?;
    // Git's index can enumerate local files even when jj cannot rebuild its
    // own index. Include ignored files: secrets are commonly gitignored. This
    // also covers inputs used inside scripts rather than named in setup.yaml.
    let untracked = untracked_inputs(runner, source)?;
    let backup = backup_path(source, "before-reclone")?;
    fs::rename(source, &backup).map_err(CubeError::Io)?;
    audit!(
        database,
        "repo.reclone_started",
        repo = repo,
        backup = backup.display().to_string()
    );
    let recovery: Result<()> = (|| {
        materialize_repo_source_if_missing(&RecoveryRunner(runner), &record)?;
        // Keep the recorded branch even if origin's HEAD has since changed.
        RecoveryRunner(runner).run(&CommandInvocation {
            cwd: source.clone(),
            program: "jj".into(),
            args: vec![
                "bookmark".into(),
                "track".into(),
                format!("{}@origin", record.main_branch),
            ],
            env: vec![],
        })?;
        let setup = Path::new(crate::setup::SETUP_FILE_RELATIVE);
        if !source.join(setup).exists() && backup.join(setup).exists() {
            copy_input(&backup, source, setup)?;
        }
        for input in &untracked {
            copy_input(&backup, source, input)?;
        }
        for input in setup_inputs(source)?.into_iter().chain(setup_inputs(&backup)?) {
            copy_input(&backup, source, &input)?;
        }
        stamp_registration(&record)?;
        for path in &paths {
            remove_workspace_tree(path)?;
        }
        for workspace in &workspaces {
            store.forget_workspace(repo, &workspace.workspace_id)?;
        }
        store.reset_store_upkeep(repo)?;
        gate.clear_quarantine(source)?;
        Ok(())
    })();
    if let Err(error) = recovery {
        audit!(
            database,
            "repo.reclone_failed",
            repo = repo,
            error = error.to_string(),
            backup = backup.display().to_string()
        );
        // Keep the failed clone too; restore the original path so surviving
        // workspace pointers still resolve. No recovery artifact is deleted.
        if source.exists() {
            fs::rename(source, backup_path(source, "failed-reclone")?).map_err(CubeError::Io)?;
        }
        fs::rename(&backup, source).map_err(|restore| {
            CubeError::InvalidArgument(format!(
                "reclone failed: {error}; cannot restore {} to {}: {restore}; the old store remains in the backup",
                backup.display(),
                source.display()
            ))
        })?;
        return Err(CubeError::InvalidArgument(format!(
            "reclone failed; original store restored at {}: {error}",
            source.display()
        )));
    }
    audit!(
        database,
        "repo.recloned",
        repo = repo,
        backup = backup.display().to_string(),
        purged_workspaces = paths.len()
    );
    RunResult::new(
        format!(
            "Recloned `{repo}`; old store preserved at {}. Purged {} workspace(s).",
            backup.display(),
            paths.len()
        ),
        json!({"repo": record, "backup": backup, "purged_workspaces": paths.len()}),
    )
}

fn backup_path(source: &Path, kind: &str) -> Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| CubeError::InvalidArgument(e.to_string()))?
        .as_secs();
    let name = source
        .file_name()
        .ok_or_else(|| CubeError::InvalidArgument("source has no file name".into()))?;
    Ok(source.with_file_name(format!(
        "{}.{kind}-{stamp}-{}",
        name.to_string_lossy(),
        uuid::Uuid::new_v4()
    )))
}

const REGISTRATION: &str = ".jj/cube-registration";

/// Keep quarantined jj registrations reachable for recovery, and avoid minting
/// another working copy with the same store-side name after remove + ensure.
pub(super) fn quarantined_workspace_ids(repo: &crate::metadata::RepoRecord) -> Result<Vec<String>> {
    if !repo.workspace_root.is_dir() {
        return Ok(vec![]);
    }
    let mut ids = Vec::new();
    for entry in fs::read_dir(&repo.workspace_root).map_err(CubeError::Io)? {
        let name = entry.map_err(CubeError::Io)?.file_name();
        let name = name.to_string_lossy();
        if let Some(suffix) = name
            .strip_prefix(".quarantine-")
            .and_then(|s| s.strip_prefix(&repo.workspace_prefix))
            && let Some((number, _)) = suffix.split_once('-')
            && number.parse::<u32>().is_ok()
        {
            ids.push(format!("{}{number}", repo.workspace_prefix));
        }
    }
    Ok(ids)
}

pub(super) fn stamp_registration(repo: &crate::metadata::RepoRecord) -> Result<()> {
    if let Some(source) = &repo.source
        && source.join(".jj").is_dir()
    {
        fs::write(source.join(REGISTRATION), uuid::Uuid::new_v4().to_string()).map_err(CubeError::Io)?;
    }
    Ok(())
}

pub(super) fn stamp_workspace(repo: &crate::metadata::RepoRecord, workspace: &Path) -> Result<()> {
    if let Some(source) = &repo.source
        && source.join(REGISTRATION).is_file()
    {
        fs::create_dir_all(workspace.join(".jj")).map_err(CubeError::Io)?;
        fs::copy(source.join(REGISTRATION), workspace.join(REGISTRATION)).map_err(CubeError::Io)?;
    }
    Ok(())
}

/// A name matching the pool prefix is insufficient evidence of store identity.
/// Quarantine preserves local files while preventing reuse of old working copies.
pub(super) fn quarantine_obsolete(
    store: &Store,
    database: Option<&Path>,
    repo: &crate::metadata::RepoRecord,
    candidates: &mut Vec<crate::metadata::WorkspaceCandidate>,
) -> Result<()> {
    let Some(source) = &repo.source else {
        return Ok(());
    };
    let generation = match fs::read(source.join(REGISTRATION)) {
        Ok(generation) => Some(generation),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(CubeError::Io(error)),
    };
    let expected = fs::canonicalize(source.join(".jj/repo")).ok();
    let workspaces = store.list_workspaces(&repo.repo)?;
    let mut keep = Vec::new();
    for candidate in candidates.iter() {
        if workspaces
            .iter()
            .any(|w| w.workspace_id == candidate.workspace_id && w.state == WorkspaceState::Leased)
        {
            keep.push(candidate.clone());
            continue;
        }
        let path = &candidate.workspace_path;
        let wrong_registration = generation
            .as_ref()
            .is_some_and(|g| fs::read(path.join(REGISTRATION)).ok().as_ref() != Some(g));
        let wrong_store = path.join(".jj/repo").exists()
            && expected
                .as_ref()
                .is_some_and(|expected| crate::lock::shared_repo_dir(path).ok().as_ref() != Some(expected));
        if wrong_registration || wrong_store {
            let quarantine = path.with_file_name(format!(
                ".quarantine-{}-{}",
                candidate.workspace_id,
                uuid::Uuid::new_v4()
            ));
            fs::rename(path, &quarantine).map_err(CubeError::Io)?;
            store.forget_workspace(&repo.repo, &candidate.workspace_id)?;
            audit!(
                database,
                "workspace.obsolete_quarantined",
                repo = repo.repo,
                workspace_id = candidate.workspace_id,
                quarantine = quarantine.display().to_string()
            );
            eprintln!(
                "cube: preserved obsolete workspace {} at {}; provisioning a workspace for the current registration",
                path.display(),
                quarantine.display()
            );
        } else {
            keep.push(candidate.clone());
        }
    }
    *candidates = keep;
    Ok(())
}

/// Shell expansion is deliberately not executed. Reject dynamic paths rather
/// than guessing and losing a secret that setup will need on the next lease.
fn setup_inputs(source: &Path) -> Result<Vec<PathBuf>> {
    let Some(config) = crate::setup::read_setup_config(source)? else {
        return Ok(vec![]);
    };
    let mut inputs = Vec::new();
    for step in config.steps {
        let words = shlex::split(&step.command).ok_or_else(|| {
            CubeError::InvalidArgument(format!(
                "setup step `{}` cannot be parsed; use literal $CUBE_BASE_REPO/file paths before recloning",
                step.id
            ))
        })?;
        for word in words {
            let mut recognized = false;
            for prefix in ["$CUBE_BASE_REPO", "${CUBE_BASE_REPO}"] {
                if let Some((_, suffix)) = word.split_once(prefix) {
                    recognized = true;
                    let relative = suffix.strip_prefix('/').unwrap_or("");
                    let path = PathBuf::from(relative);
                    if relative.is_empty()
                        || relative.contains(['$', '`', '*', '?', ';', '|', '&', '>', '<', '(', ')'])
                        || path.components().any(|c| !matches!(c, Component::Normal(_)))
                        || path.starts_with(".jj")
                        || path.starts_with(".git")
                    {
                        return Err(CubeError::InvalidArgument(format!(
                            "setup step `{}` uses dynamic base-repo path `{word}`; replace it with literal $CUBE_BASE_REPO/file paths before recloning",
                            step.id
                        )));
                    }
                    inputs.push(path);
                }
            }
            if !recognized && word.contains("CUBE_BASE_REPO") {
                return Err(CubeError::InvalidArgument(format!(
                    "setup step `{}` uses unsupported base-repo expansion `{word}`; use literal $CUBE_BASE_REPO/file paths before recloning",
                    step.id
                )));
            }
        }
    }
    Ok(inputs)
}

fn copy_input(old: &Path, new: &Path, relative: &Path) -> Result<()> {
    let source = old.join(relative);
    let destination = new.join(relative);
    let fail = |detail: String| {
        CubeError::InvalidArgument(format!(
            "cannot preserve setup file {}: {detail}; restore this file in the base repo before retrying cube repo reclone",
            source.display()
        ))
    };
    // Never traverse a symlink in either tree (including intermediate paths).
    for root in [old, new] {
        let mut path = root.to_path_buf();
        for component in relative.components() {
            path.push(component);
            if fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink()) {
                return Err(fail(format!(
                    "{} is a symlink; replace it with a regular file",
                    path.display()
                )));
            }
        }
    }
    if destination.is_file() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(&source).map_err(|e| fail(e.to_string()))?;
    if metadata.is_dir() {
        let existed = destination.is_dir();
        fs::create_dir_all(&destination).map_err(CubeError::Io)?;
        for entry in fs::read_dir(&source).map_err(CubeError::Io)? {
            copy_input(old, new, &relative.join(entry.map_err(CubeError::Io)?.file_name()))?;
        }
        if !existed {
            fs::set_permissions(&destination, metadata.permissions()).map_err(CubeError::Io)?;
        }
    } else if metadata.is_file() {
        fs::create_dir_all(destination.parent().unwrap()).map_err(CubeError::Io)?;
        fs::copy(&source, &destination).map_err(|e| fail(e.to_string()))?;
    } else {
        return Err(fail("not a regular file or directory".into()));
    }
    Ok(())
}
