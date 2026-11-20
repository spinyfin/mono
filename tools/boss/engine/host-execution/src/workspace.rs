use crate::{
    cube::{self, CubeJsonTransport},
    types::{CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus},
    validate_workspace,
};
use anyhow::{Context, Result, ensure};
use boss_engine_recovery::execution_bookmark::{self as recovery, ExecutionBookmark, Jj, LocalJj};
use boss_protocol_spoke::{
    Validate,
    v1::{
        Bookmark, Command, CommandResult, ResultCode, Run, WorkspaceKind as Kind, WorkspaceOp, WorkspaceResult,
        command, command_result,
    },
};
use serde_json::json;
use std::path::{Path, PathBuf};

/// A serialized host executor; callers persist attempting before invoking it.
/// It accepts identifiers only. Cube's local registry supplies filesystem paths.
pub struct Executor<T> {
    pub cube: T,
    pub host_id: String,
}

/// Stable cube task ownership across reconnects and lost acquisition replies.
pub fn lease_owner(run: &Run) -> String {
    format!(
        "boss-spoke:{}:{}:{}:{}",
        run.execution_id, run.run_id, run.assignment_generation, run.spawn_token
    )
}

fn owns_task(task: Option<&str>, owner: &str) -> bool {
    task.and_then(|task| task.lines().next()) == Some(owner)
}

fn optional(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

#[derive(Default)]
enum Phase {
    #[default]
    Authorizing,
    Effect,
}
impl Phase {
    fn failure(&self) -> ResultCode {
        match self {
            Self::Authorizing => ResultCode::Rejected,
            Self::Effect => ResultCode::OutcomeUncertain,
        }
    }
}

impl<T: CubeJsonTransport> Executor<T> {
    pub async fn execute(&self, command: &Command, interrupted: bool, now_ms: i64) -> CommandResult {
        let mut result = CommandResult {
            command_id: command.command_id.clone(),
            ..Default::default()
        };
        let validated = command.validate().map_err(anyhow::Error::from).and_then(|()| {
            let Some(command::Body::Workspace(op)) = &command.body else {
                anyhow::bail!("execution capability unavailable");
            };
            Ok((op, validate_workspace(op, command.run.as_ref())?))
        });
        let (op, kind) = match validated {
            Ok(value) => value,
            Err(error) => {
                result.code = ResultCode::Rejected.into();
                result.reason = error.to_string();
                return result;
            }
        };
        if !interrupted && command.not_after_unix_ms <= now_ms {
            result.code = ResultCode::Rejected.into();
            result.reason = "command expired".into();
            return result;
        }
        // Reads, acquisition and release reconciliation may run after an uncertain
        // effect. In particular, replaying a heartbeat would extend a relative
        // TTL beyond the original attempt; that policy decision belongs to the hub.
        if interrupted
            && !matches!(
                kind,
                Kind::Lease
                    | Kind::Release
                    | Kind::ReleaseWithReset
                    | Kind::ForceRelease
                    | Kind::ListRepos
                    | Kind::ListWorkspaces
                    | Kind::Status
                    | Kind::PositionedParent
                    | Kind::CheckPreservedUnpublished
            )
        {
            result.code = ResultCode::OutcomeUncertain.into();
            result.reason = "workspace operation interrupted; hub reconciliation required".into();
            return result;
        }
        let mut phase = Phase::Authorizing;
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            self.dispatch(command, op, kind, interrupted, &mut phase),
        )
        .await;
        match outcome {
            Ok(Ok((value, uncertain))) => {
                result.code = if uncertain {
                    ResultCode::OutcomeUncertain
                } else {
                    ResultCode::Ok
                }
                .into();
                if uncertain {
                    result.reason = "lease ownership recovered; setup completion is unknown".into();
                }
                result.value = Some(command_result::Value::Workspace(value));
            }
            Ok(Err(error)) => {
                result.code = phase.failure().into();
                result.reason = format!("workspace operation failed: {error:#}");
                if let Some(cube_error) = error.downcast_ref::<cube::CubeCliError>()
                    && cube_error.positioning_refused()
                {
                    // A cube refusal is definitive even after entering the effect phase.
                    // Preserve the typed diagnostic across the spoke boundary.
                    result.code = ResultCode::Rejected.into();
                    let mut diagnostic = cube_error.clone();
                    diagnostic.host = self.host_id.clone();
                    result.value = Some(command_result::Value::Workspace(WorkspaceResult {
                        cube_result_json: serde_json::to_vec(&diagnostic).expect("cube error is serializable"),
                        ..Default::default()
                    }));
                }
            }
            Err(_) => {
                result.code = phase.failure().into();
                result.reason = "workspace operation timed out".into();
            }
        }
        // Keep failure diagnostics bounded too; never truncate a success payload.
        result.reason = boss_engine_utils::string_clip::clip_to_bytes_with_suffix(&result.reason, 4096, "…");
        result
    }

    async fn repo(&self, id: &str) -> Result<CubeRepoSummary> {
        cube::list_repos(&self.cube)
            .await?
            .into_iter()
            .find(|repo| repo.repo_id == id)
            .context("repository is not registered on this host")
    }

    fn path(repo: &CubeRepoSummary, workspace: &CubeWorkspaceStatus) -> Result<PathBuf> {
        let root = repo
            .workspace_root
            .canonicalize()
            .context("workspace root unavailable")?;
        let path = workspace
            .workspace_path
            .canonicalize()
            .context("workspace unavailable")?;
        ensure!(
            path.parent() == Some(root.as_path())
                && path
                    .file_name()
                    .is_some_and(|name| name == workspace.workspace_id.as_str()),
            "workspace path escapes registered root"
        );
        ensure!(
            !std::fs::symlink_metadata(&workspace.workspace_path)?
                .file_type()
                .is_symlink(),
            "workspace path is a symlink"
        );
        Ok(path)
    }

    async fn owned(&self, op: &WorkspaceOp, run: &Run) -> Result<(PathBuf, CubeWorkspaceStatus)> {
        let row = cube::registered_workspaces(&self.cube)
            .await?
            .into_iter()
            .find(|row| row.status.workspace_id == op.workspace_id && row.repo == op.repo_id)
            .context("workspace is not registered in repository")?;
        let ws = row.status;
        ensure!(
            ws.state == "leased"
                && ws.lease_id.as_deref() == Some(&op.lease_id)
                && owns_task(ws.task.as_deref(), &lease_owner(run)),
            "workspace lease belongs to another assignment"
        );
        // Lease administration addresses cube's recorded lease, even when its
        // working directory disappeared. Only filesystem operations open it.
        let path = if matches!(
            Kind::try_from(op.kind)?,
            Kind::Release | Kind::ReleaseWithReset | Kind::ForceRelease | Kind::Heartbeat | Kind::Status
        ) {
            ws.workspace_path.clone()
        } else {
            Self::path(&self.repo(&op.repo_id).await?, &ws)?
        };
        Ok((path, ws))
    }

    async fn acquire(
        &self,
        op: &WorkspaceOp,
        run: &Run,
        interrupted: bool,
        phase: &mut Phase,
    ) -> Result<(WorkspaceResult, bool)> {
        let repo = self.repo(&op.repo_id).await?;
        let owner = lease_owner(run);
        let existing: Vec<_> = cube::registered_workspaces(&self.cube)
            .await?
            .into_iter()
            .filter(|row| owns_task(row.status.task.as_deref(), &owner) && row.status.state == "leased")
            .collect();
        ensure!(
            existing.len() <= 1,
            "assignment owns multiple leases; reconciliation required"
        );
        if let Some(row) = existing.first() {
            ensure!(
                row.repo == op.repo_id,
                "assignment already owns a lease in another repository"
            );
            let ws = &row.status;
            let path = Self::path(&repo, ws)?;
            return Ok((
                WorkspaceResult {
                    repo_id: op.repo_id.clone(),
                    workspace_id: ws.workspace_id.clone(),
                    lease_id: ws.lease_id.clone().context("recorded lease has no identity")?,
                    display_path: path.display().to_string(),
                    cube_result_json: serde_json::to_vec(&CubeWorkspaceLease {
                        workspace_id: ws.workspace_id.clone(),
                        workspace_path: ws.workspace_path.clone(),
                        lease_id: ws.lease_id.clone().context("recorded lease has no identity")?,
                        dirty_verified: None,
                    })?,
                    ..Default::default()
                },
                true,
            ));
        }
        ensure!(
            !interrupted,
            "interrupted acquisition has no recorded owner; refusing a second acquisition"
        );
        let exclusions: Vec<_> = op.exclude_workspace_ids.iter().map(String::as_str).collect();
        let task = if op.task_label.is_empty() {
            owner
        } else {
            format!("{owner}\n{}", op.task_label)
        };
        *phase = Phase::Effect;
        let lease = cube::lease_workspace(
            &self.cube,
            &op.repo_id,
            &task,
            optional(&op.workspace_id),
            op.allow_dirty,
            &exclusions,
        )
        .await?;
        let bound = WorkspaceOp {
            workspace_id: lease.workspace_id.clone(),
            lease_id: lease.lease_id.clone(),
            ..op.clone()
        };
        let (path, _) = self.owned(&bound, run).await?;
        Ok((
            WorkspaceResult {
                repo_id: op.repo_id.clone(),
                workspace_id: lease.workspace_id.clone(),
                lease_id: lease.lease_id.clone(),
                display_path: path.display().to_string(),
                cube_result_json: serde_json::to_vec(&lease)?,
                ..Default::default()
            },
            false,
        ))
    }

    /// After authorization, cube may commit a release and then lose its
    /// reply. Its registry proves whether this exact lease is still held.
    async fn release(&self, op: &WorkspaceOp, kind: Kind) -> Result<()> {
        let outcome = match kind {
            Kind::ReleaseWithReset => cube::release_workspace_with_reset(&self.cube, &op.lease_id).await,
            Kind::ForceRelease => cube::force_release_lease(&self.cube, &op.lease_id, Some(&op.reason)).await,
            _ => cube::release_workspace(&self.cube, &op.lease_id).await,
        };
        if outcome.is_err() && self.lease_is_gone(op).await? {
            return Ok(());
        }
        outcome
    }

    async fn lease_is_gone(&self, op: &WorkspaceOp) -> Result<bool> {
        Ok(!cube::registered_workspaces(&self.cube)
            .await?
            .iter()
            .any(|row| row.status.lease_id.as_deref() == Some(&op.lease_id)))
    }

    async fn dispatch(
        &self,
        command: &Command,
        op: &WorkspaceOp,
        kind: Kind,
        interrupted: bool,
        phase: &mut Phase,
    ) -> Result<(WorkspaceResult, bool)> {
        let run = command.run.as_ref();
        let mut result = WorkspaceResult {
            repo_id: op.repo_id.clone(),
            workspace_id: op.workspace_id.clone(),
            lease_id: op.lease_id.clone(),
            ..Default::default()
        };
        let mut value = json!({});
        // A replay of a journaled, interrupted release may arrive after
        // cube ended the lease. Never touch a newer holder of the workspace.
        if interrupted
            && matches!(kind, Kind::Release | Kind::ReleaseWithReset | Kind::ForceRelease)
            && self.lease_is_gone(op).await?
        {
            result.cube_result_json = serde_json::to_vec(&value)?;
            return Ok((result, false));
        }
        match kind {
            Kind::EnsureRepo => {
                *phase = Phase::Effect;
                result.repo_id = cube::ensure_repo(&self.cube, &op.origin).await?.repo_id;
            }
            Kind::ListRepos => {
                value = serde_json::to_value(cube::list_repos(&self.cube).await?)?;
            }
            Kind::ListWorkspaces => {
                value = serde_json::to_value(cube::list_workspaces(&self.cube).await?)?;
            }
            Kind::Lease => return self.acquire(op, run.expect("validated run"), interrupted, phase).await,
            _ => {
                let run = run.expect("validated run");
                let (path, status) = if crate::validation::store_operation(kind) {
                    let source = self
                        .repo(&op.repo_id)
                        .await?
                        .source
                        .context("registered repository has no shared source")?;
                    (source.canonicalize()?, None)
                } else {
                    let (path, status) = self.owned(op, run).await?;
                    (path, Some(status))
                };
                result.display_path = path.display().to_string();
                if matches!(
                    kind,
                    Kind::Release
                        | Kind::ReleaseWithReset
                        | Kind::ForceRelease
                        | Kind::Heartbeat
                        | Kind::CreateChange
                        | Kind::GotoPr
                        | Kind::GotoRevision
                        | Kind::StartFresh
                ) {
                    *phase = Phase::Effect;
                }
                match kind {
                    Kind::Release | Kind::ReleaseWithReset | Kind::ForceRelease => self.release(op, kind).await?,
                    Kind::Heartbeat => {
                        cube::heartbeat_lease(
                            &self.cube,
                            &op.lease_id,
                            (op.ttl_seconds != 0).then_some(op.ttl_seconds),
                        )
                        .await?
                    }
                    Kind::Status => value = serde_json::to_value(status.expect("workspace status"))?,
                    Kind::CreateChange => {
                        result.change_id = cube::create_change(&self.cube, &path, &op.title).await?.change_id
                    }
                    Kind::GotoPr => cube::goto_workspace(&self.cube, &path, "--pr", &op.pr_number.to_string()).await?,
                    Kind::GotoRevision => cube::goto_workspace(&self.cube, &path, "--revision", &op.revision).await?,
                    Kind::PositionedParent => value = crate::recovery_ops::positioned_parent(&path).await?,
                    Kind::StartFresh => value = crate::recovery_ops::start_fresh(&path).await?,
                    _ => {
                        let record = self.record(&path, &run.execution_id, op.bookmark.as_ref()).await?;
                        value = match kind {
                            Kind::CreateBookmark => {
                                let predecessor = match &op.predecessor {
                                    Some(p) => Some(self.record(&path, &p.execution_id, Some(p)).await?),
                                    None => None,
                                };
                                *phase = Phase::Effect;
                                let created = recovery::create_from(
                                    &LocalJj,
                                    &path,
                                    &run.execution_id,
                                    &self.host_id,
                                    predecessor.as_ref(),
                                    optional(&op.inherited_base),
                                )
                                .await?;
                                result.bookmark = Some(wire_bookmark(&op.repo_id, &created));
                                json!({})
                            }
                            Kind::CheckpointBaseline => {
                                *phase = Phase::Effect;
                                json!(record.base_commit.clone().unwrap_or_default())
                            }
                            Kind::ConfirmRecovery => {
                                *phase = Phase::Effect;
                                let request = crate::recovery_ops::parse_request(&op.request_json)?;
                                crate::recovery_ops::confirm(&record, &path, &request).await?
                            }
                            Kind::RestorePreserved => {
                                *phase = Phase::Effect;
                                let request = crate::recovery_ops::parse_request(&op.request_json)?;
                                crate::recovery_ops::restore_preserved(&record, &path, &request).await?
                            }
                            Kind::CheckPreservedUnpublished => {
                                let request = crate::recovery_ops::parse_request(&op.request_json)?;
                                crate::recovery_ops::preserved_unpublished(&record, &request).await?
                            }
                            Kind::ForgetPreserved => {
                                *phase = Phase::Effect;
                                let request = crate::recovery_ops::parse_request(&op.request_json)?;
                                crate::recovery_ops::forget_preserved(&record, &request).await?
                            }
                            _ => anyhow::bail!("unsupported workspace operation"),
                        };
                    }
                }
            }
        }
        result.cube_result_json = serde_json::to_vec(&value)?;
        Ok((result, false))
    }

    async fn record(&self, path: &Path, execution: &str, bookmark: Option<&Bookmark>) -> Result<ExecutionBookmark> {
        Ok(ExecutionBookmark {
            execution_id: execution.into(),
            repo_path: LocalJj.shared_repo(path).await?,
            host_id: self.host_id.clone(),
            base_commit: bookmark.and_then(|b| optional(&b.anchor_commit)).map(str::to_owned),
            pr_head_before: bookmark.and_then(|b| optional(&b.pr_head_before)).map(str::to_owned),
        })
    }
}

fn wire_bookmark(repo: &str, record: &ExecutionBookmark) -> Bookmark {
    Bookmark {
        repo_id: repo.into(),
        execution_id: record.execution_id.clone(),
        anchor_commit: record.base_commit.clone().unwrap_or_default(),
        pr_head_before: record.pr_head_before.clone().unwrap_or_default(),
    }
}

/// Discover the recovery remote for both host execution and hub PR repair.
pub async fn recovery_remote(jj: &dyn Jj, workspace: &Path) -> Result<String> {
    let remotes = jj.run(workspace, &["git", "remote", "list"]).await?;
    Ok(git_utils::repo_slug::parse_github_remote(&remotes)
        .map(|(name, _)| name)
        .unwrap_or_else(|| "origin".into()))
}
