//! Cleanup declarations and assets pinned to the fetched trunk, independently
//! of either the source working copy or a workspace's unmerged branch.

use std::path::{Component, Path};
use std::time::Duration;

use crate::app::CubeError;
use crate::command_runner::{CommandRunner, RealCommandRunner};
use crate::setup::{CleanupStep, read_setup_config};

pub(crate) struct ResolvedCleanup {
    pub step: CleanupStep,
    // Keep the exported assets alive until the command finishes.
    _snapshot: Option<tempfile::TempDir>,
}

impl std::ops::Deref for ResolvedCleanup {
    type Target = CleanupStep;
    fn deref(&self) -> &Self::Target {
        &self.step
    }
}

pub(crate) fn read_cleanup_step(
    runner: &dyn CommandRunner,
    workspace: &Path,
    source: Option<&Path>,
    main_branch: &str,
) -> Result<Option<ResolvedCleanup>, CubeError> {
    let snapshot = if let Some(source) = source {
        let timeout = Duration::from_secs(30);
        let run = |args: &[&str]| runner.run_with_timeout(&RealCommandRunner::invocation(source, "jj", args), timeout);
        // Resolve once so a concurrent fetch cannot mix versions of assets.
        let revision = run(&[
            "--ignore-working-copy",
            "log",
            "-r",
            &format!("{main_branch}@origin"),
            "--no-graph",
            "-T",
            "commit_id",
        ])?;
        let files = run(&[
            "--ignore-working-copy",
            "file",
            "list",
            "-r",
            &revision,
            ".cube",
            "-T",
            "file_type ++ \"\\t\" ++ executable ++ \"\\t\" ++ path ++ \"\\n\"",
        ])?;
        let snapshot = tempfile::Builder::new()
            .prefix("cube-cleanup-")
            .tempdir()
            .map_err(CubeError::Io)?;
        for entry in files.lines() {
            let mut fields = entry.splitn(3, '\t');
            let kind = fields.next();
            let executable = fields.next() == Some("true");
            let file = fields
                .next()
                .ok_or_else(|| CubeError::InvalidArgument("invalid cleanup asset entry".into()))?;
            if kind != Some("file") {
                return Err(CubeError::InvalidArgument(format!(
                    "cleanup asset must be a regular file: {file}"
                )));
            }
            let path = Path::new(file);
            if !path.starts_with(".cube") || path.components().any(|c| !matches!(c, Component::Normal(_))) {
                return Err(CubeError::InvalidArgument(format!(
                    "invalid cleanup asset path: {file}"
                )));
            }
            let destination = snapshot.path().join(path);
            std::fs::create_dir_all(destination.parent().unwrap()).map_err(CubeError::Io)?;
            runner.run_to_file(
                &RealCommandRunner::invocation(
                    source,
                    "jj",
                    &["--ignore-working-copy", "file", "show", "-r", &revision, file],
                ),
                &destination,
                timeout,
            )?;
            #[cfg(unix)]
            if executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700))
                    .map_err(CubeError::Io)?;
            }
        }
        Some(snapshot)
    } else {
        None
    };
    // A registered source is authoritative even when trunk declares no cleanup.
    // Only repositories without a shared source use their workspace declaration.
    let root = snapshot.as_ref().map_or(workspace, |dir| dir.path());
    let Some(mut step) = read_setup_config(root)?.and_then(|config| config.cleanup) else {
        return Ok(None);
    };
    step.root = Some(root.to_path_buf());
    Ok(Some(ResolvedCleanup {
        step,
        _snapshot: snapshot,
    }))
}
