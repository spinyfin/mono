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
use super::fs_util::remove_workspace_tree;
use super::registration::stamp_registration;
use super::repo::materialize_repo_source_if_missing;

/// Scan actual open files and working directories, rather than process names.
pub(super) fn require_unused(runner: &dyn CommandRunner, paths: &[PathBuf]) -> Result<()> {
    let paths: Vec<_> = paths
        .iter()
        .filter(|p| p.exists())
        .map(|p| fs::canonicalize(p).map_err(CubeError::Io))
        .collect::<Result<_>>()?;
    let output = runner.run_output_with_timeout(
        &CommandInvocation {
            cwd: PathBuf::from("/"),
            program: "lsof".into(),
            args: vec!["-w".into(), "-nP".into(), "-Fpcn".into()],
            env: vec![],
        },
        Duration::from_secs(120),
    )?;
    let mut process = String::new();
    for line in output.stdout.lines() {
        if line.starts_with('p') {
            process = line.to_string();
        } else if line.starts_with('c') {
            process.push_str(&format!(" {line}"));
        } else if let Some(name) = line.strip_prefix('n')
            && paths.iter().any(|path| Path::new(name).starts_with(path))
        {
            return Err(CubeError::InvalidArgument(format!(
                "cannot reclone: processes are using {name}; stop them and retry: {process}"
            )));
        }
    }
    if !matches!(output.status, Some(0 | 1)) {
        return Err(CubeError::InvalidArgument(format!(
            "cannot verify stores are unused: lsof exited {:?}: {}",
            output.status, output.stderr
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Enumerate local inputs through Git without reading jj's damaged index.
pub(super) fn untracked_inputs(runner: &dyn CommandRunner, source: &Path) -> Result<Vec<PathBuf>> {
    let output = runner.run_raw(&CommandInvocation {
        cwd: source.into(),
        program: "git".into(),
        args: ["ls-files", "--others", "-z", "--", ".", ":(exclude).jj", ":(exclude).git"]
            .into_iter().map(str::to_string).collect(),
        env: vec![],
    }).map_err(|error| CubeError::InvalidArgument(format!(
        "cannot enumerate untracked setup files in {}: {error}; repair the Git index before recloning so local setup inputs can be preserved", source.display()
    )))?;
    let inputs: Vec<PathBuf> = output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(path_from_bytes)
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
    discard_dirty: bool,
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
    let dirty: Vec<_> = workspaces
        .iter()
        .filter(|w| {
            matches!(
                w.health_status,
                Some(crate::metadata::WorkspaceHealth::Dirty | crate::metadata::WorkspaceHealth::Quarantined)
            )
        })
        .map(|w| w.workspace_id.as_str())
        .collect();
    if !dirty.is_empty() && !discard_dirty {
        return Err(CubeError::InvalidArgument(format!(
            "repo {repo} has dirty or quarantined workspaces ({}); preserve their work, then pass --discard-dirty to authorize deletion",
            dirty.join(", ")
        )));
    }
    let mut paths: Vec<_> = workspaces.iter().map(|w| w.workspace_path.clone()).collect();
    for candidate in super::provision::discover_workspaces(&record)? {
        if !paths.contains(&candidate.workspace_path) {
            paths.push(candidate.workspace_path);
        }
    }
    let mut scan_paths = paths.clone();
    scan_paths.push(source.clone());
    require_unused(runner, &scan_paths)?;
    // Validate literal references before moving anything. A damaged index is
    // irrelevant: setup declarations and local secrets are ordinary files.
    for input in setup_inputs(source)? {
        validate_input(source, &input)?;
    }
    // Git's index can enumerate local files even when jj cannot rebuild its
    // own index. Include ignored files: secrets are commonly gitignored. This
    // also covers inputs used inside scripts rather than named in setup.yaml.
    let untracked = preservable_untracked(source, untracked_inputs(runner, source)?);
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
            let failed = backup_path(source, "failed-reclone")?;
            fs::rename(source, &failed).map_err(|rename| {
                CubeError::InvalidArgument(format!(
                    "reclone failed: {error}; cannot move the failed clone {} to {}: {rename}; the original store remains intact in the backup {}",
                    source.display(),
                    failed.display(),
                    backup.display()
                ))
            })?;
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
    // Setup declarations come only from the fresh clone, including when a
    // declared directory or untracked tree contains the old declaration.
    if relative == Path::new(crate::setup::SETUP_FILE_RELATIVE) {
        return Ok(());
    }
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
        if let Some(link) = symlink_in_path(root, relative) {
            return Err(fail(format!(
                "{} is a symlink; replace it with a regular file",
                link.display()
            )));
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

/// Validate the complete declared tree before moving or cloning the store.
fn validate_input(root: &Path, relative: &Path) -> Result<()> {
    reject_symlink(root, relative)?;
    let path = root.join(relative);
    let fail = |detail| {
        CubeError::InvalidArgument(format!(
            "cannot preserve setup file {}: {detail}; restore this file in the base repo before retrying cube repo reclone",
            path.display()
        ))
    };
    let metadata = fs::symlink_metadata(&path).map_err(|e| fail(e.to_string()))?;
    if metadata.is_dir() {
        for entry in fs::read_dir(&path).map_err(CubeError::Io)? {
            validate_input(root, &relative.join(entry.map_err(CubeError::Io)?.file_name()))?;
        }
    } else if !metadata.is_file() {
        return Err(fail("not a regular file or directory".into()));
    }
    Ok(())
}

/// The first symlink on `root/relative` (including intermediate components).
fn symlink_in_path(root: &Path, relative: &Path) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink()) {
            return Some(path);
        }
    }
    None
}

/// Fail early, before anything is moved, on a setup-declared input that is a symlink.
fn reject_symlink(root: &Path, relative: &Path) -> Result<()> {
    match symlink_in_path(root, relative) {
        Some(link) => Err(CubeError::InvalidArgument(format!(
            "cannot preserve setup file {}: {} is a symlink; replace it with a regular file; restore this file in the base repo before retrying cube repo reclone",
            root.join(relative).display(),
            link.display()
        ))),
        None => Ok(()),
    }
}

/// Incidental untracked entries (build-output links such as `bazel-bin`,
/// `node_modules/.bin/*`, sockets) cannot be copied faithfully. Skip them with
/// a warning instead of failing the whole reclone; files declared by setup are
/// validated separately and still refuse symlinks.
fn preservable_untracked(source: &Path, inputs: Vec<PathBuf>) -> Vec<PathBuf> {
    inputs
        .into_iter()
        .filter(|input| {
            if input == Path::new(crate::setup::SETUP_FILE_RELATIVE) {
                return false;
            }
            if let Some(link) = symlink_in_path(source, input) {
                eprintln!("cube: not preserving untracked symlink {}", link.display());
                return false;
            }
            let regular = fs::symlink_metadata(source.join(input)).is_ok_and(|m| m.is_file() || m.is_dir());
            if !regular {
                eprintln!(
                    "cube: not preserving untracked non-regular file {}",
                    source.join(input).display()
                );
            }
            regular
        })
        .collect()
}
