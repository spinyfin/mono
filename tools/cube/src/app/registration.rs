//! Store identity shared by provisioning and leasing. The source stamp in
//! .jj/cube-registration is copied into each provisioned workspace. A mismatch
//! identifies a previous registration whose files must be quarantined.

use super::errors::{CubeError, Result};
use crate::audit;
use crate::metadata::WorkspaceState;
use crate::store::Store;
use std::fs;
use std::path::Path;

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

/// Whether `workspace` carries the repo's current registration stamp. Repos
/// without a stamp (never reclone/ensure-stamped) match every workspace.
pub(super) fn registration_matches(repo: &crate::metadata::RepoRecord, workspace: &Path) -> Result<bool> {
    let Some(source) = &repo.source else {
        return Ok(true);
    };
    let generation = match fs::read(source.join(REGISTRATION)) {
        Ok(generation) => generation,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(CubeError::Io(error)),
    };
    Ok(fs::read(workspace.join(REGISTRATION)).ok().as_ref() == Some(&generation))
}

/// A name matching the pool prefix is insufficient evidence of store identity.
/// Quarantine preserves local files while preventing reuse of old working copies.
pub(super) fn quarantine_obsolete(
    store: &Store,
    database: Option<&Path>,
    repo: &crate::metadata::RepoRecord,
    candidates: &mut Vec<crate::metadata::WorkspaceCandidate>,
    recovery_target: Option<&str>,
) -> Result<()> {
    let Some(source) = &repo.source else {
        return Ok(());
    };
    let generation = match fs::read(source.join(REGISTRATION)) {
        Ok(generation) => Some(generation),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(CubeError::Io(error)),
    };
    let expected = crate::lock::shared_repo_dir(source).ok();
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
        if wrong_store || (wrong_registration && recovery_target != Some(candidate.workspace_id.as_str())) {
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
