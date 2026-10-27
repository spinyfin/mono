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

/// Durable command-attempt storage supplied by the daemon journal.
pub trait RecoveryStore: Sync {
    fn load(&self, sequence: u64) -> Result<Option<Vec<u8>>>;
    fn save(&self, sequence: u64, bytes: &[u8]) -> Result<()>;
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedPointers {
    record: ExecutionBookmark,
    pointers: recovery::PointerSnapshot,
}

#[derive(Debug)]
struct RestoreFailed;
impl std::fmt::Display for RestoreFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("recovery pointer restoration failed; retain command attempt")
    }
}
impl std::error::Error for RestoreFailed {}

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
        self.execute_with_store(command, interrupted, now_ms, None)
            .await
            .unwrap_or_else(|error| CommandResult {
                command_id: command.command_id.clone(),
                code: ResultCode::OutcomeUncertain.into(),
                reason: format!("{error:#}"),
                value: None,
            })
    }

    pub async fn execute_with_store(
        &self,
        command: &Command,
        interrupted: bool,
        now_ms: i64,
        store: Option<&dyn RecoveryStore>,
    ) -> Result<CommandResult> {
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
                return Ok(result);
            }
        };
        if !interrupted && command.not_after_unix_ms <= now_ms {
            result.code = ResultCode::Rejected.into();
            result.reason = "command expired".into();
            return Ok(result);
        }
        if interrupted && kind == Kind::PrepareRecoveryPr {
            result.code = ResultCode::OutcomeUncertain.into();
            if let Some(store) = store {
                if let Some(bytes) = store.load(command.sequence)? {
                    let saved: SavedPointers = serde_json::from_slice(&bytes)?;
                    recovery::restore_pointers(&LocalJj, &saved.record, &saved.pointers)
                        .await
                        .context(RestoreFailed)?;
                    result.reason = "recovery pointers restored; PR positioning outcome is unknown".into();
                } else {
                    result.reason =
                        "no saved snapshot for interrupted PR positioning; hub reconciliation required".into();
                }
            } else {
                result.reason = "recovery attempt storage unavailable".into();
            }
            return Ok(result);
        }
        // Only reads and acquisition reconciliation may run after an uncertain
        // effect. In particular, replaying a heartbeat would extend a relative
        // TTL beyond the original attempt; that policy decision belongs to the hub.
        if interrupted
            && !matches!(
                kind,
                Kind::Lease | Kind::ListRepos | Kind::ListWorkspaces | Kind::Status | Kind::DiffBookmark
            )
        {
            result.code = ResultCode::OutcomeUncertain.into();
            result.reason = "workspace operation interrupted; hub reconciliation required".into();
            return Ok(result);
        }
        let mut phase = Phase::Authorizing;
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            self.dispatch(command, op, kind, interrupted, &mut phase, store),
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
                if error.downcast_ref::<RestoreFailed>().is_some() {
                    return Err(error);
                }
                result.code = phase.failure().into();
                result.reason = format!("workspace operation failed: {error:#}");
            }
            Err(_) => {
                if kind == Kind::PrepareRecoveryPr && store.is_some() && matches!(phase, Phase::Effect) {
                    anyhow::bail!("PR positioning timed out; retain saved pointers for restart restoration");
                }
                result.code = phase.failure().into();
                result.reason = "workspace operation timed out".into();
            }
        }
        // Keep failure diagnostics bounded too; never truncate a success payload.
        result.reason = boss_engine_utils::string_clip::clip_to_bytes_with_suffix(&result.reason, 4096, "…");
        Ok(result)
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
            Kind::Release | Kind::ForceRelease | Kind::Heartbeat | Kind::Status
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

    async fn dispatch(
        &self,
        command: &Command,
        op: &WorkspaceOp,
        kind: Kind,
        interrupted: bool,
        phase: &mut Phase,
        store: Option<&dyn RecoveryStore>,
    ) -> Result<(WorkspaceResult, bool)> {
        let run = command.run.as_ref();
        let mut result = WorkspaceResult {
            repo_id: op.repo_id.clone(),
            workspace_id: op.workspace_id.clone(),
            lease_id: op.lease_id.clone(),
            ..Default::default()
        };
        let mut value = json!({});
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
                        | Kind::ForceRelease
                        | Kind::Heartbeat
                        | Kind::CreateChange
                        | Kind::GotoPr
                        | Kind::GotoRevision
                ) {
                    *phase = Phase::Effect;
                }
                match kind {
                    Kind::Release => cube::release_workspace(&self.cube, &op.lease_id).await?,
                    Kind::ForceRelease => cube::force_release_lease(&self.cube, &op.lease_id, Some(&op.reason)).await?,
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
                            Kind::DiffBookmark => json!(recovery::unpublished_diff(&LocalJj, &record).await?),
                            Kind::RestoreBookmark => {
                                *phase = Phase::Effect;
                                json!(recovery::restore_for_retry(&LocalJj, &record, &path, op.self_retry).await?)
                            }
                            Kind::DiscardBaseline => {
                                *phase = Phase::Effect;
                                recovery::discard_orphaned_baseline(&LocalJj, &record).await?;
                                json!({})
                            }
                            Kind::CheckpointBaseline => {
                                *phase = Phase::Effect;
                                json!(recovery::checkpoint_baseline(&LocalJj, &record).await?)
                            }
                            Kind::RestoreRebased => {
                                *phase = Phase::Effect;
                                serde_json::to_value(
                                    restore_rebased(&LocalJj, &record, &path, optional(&op.pr_bookmark), &op.trunk)
                                        .await?,
                                )?
                            }
                            Kind::PrepareRecoveryPr => {
                                let pointers = recovery::snapshot_pointers(&LocalJj, &record).await?;
                                if let Some(store) = store {
                                    store.save(
                                        command.sequence,
                                        &serde_json::to_vec(&SavedPointers {
                                            record: record.clone(),
                                            pointers: pointers.clone(),
                                        })?,
                                    )?;
                                }
                                *phase = Phase::Effect;
                                let positioned =
                                    cube::goto_workspace(&self.cube, &path, "--pr", &op.pr_number.to_string()).await;
                                recovery::restore_pointers(&LocalJj, &record, &pointers)
                                    .await
                                    .context(RestoreFailed)?;
                                positioned?;
                                json!(true)
                            }
                            Kind::ReconcileBookmark => {
                                *phase = Phase::Effect;
                                serde_json::to_value(
                                    recovery::reconcile(&LocalJj, &record, optional(&op.pr_bookmark), &op.trunk)
                                        .await?,
                                )?
                            }
                            Kind::ReleaseHeads => {
                                *phase = Phase::Effect;
                                json!(recovery::release_heads(&LocalJj, &record).await?)
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
        })
    }
}

fn wire_bookmark(repo: &str, record: &ExecutionBookmark) -> Bookmark {
    Bookmark {
        repo_id: repo.into(),
        execution_id: record.execution_id.clone(),
        head: record.publication(),
        recovery: record.head(),
        base: record.base(),
        anchor_commit: record.base_commit.clone().unwrap_or_default(),
    }
}

/// Preserve all recovery pointers across cube's PR positioning side effects.
pub async fn preserve_recovery_pointers(
    record: &ExecutionBookmark,
    position: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    let saved = recovery::snapshot_pointers(&LocalJj, record).await?;
    let result = position.await;
    recovery::restore_pointers(&LocalJj, record, &saved).await?;
    result
}

/// Restore on the repository's GitHub remote, falling back to origin.
pub async fn restore_rebased(
    jj: &impl Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    pr_bookmark: Option<&str>,
    base_branch: &str,
) -> Result<recovery::RestoreReport> {
    let remotes = jj.run(workspace, &["git", "remote", "list"]).await?;
    let remote = git_utils::repo_slug::parse_github_remote(&remotes)
        .map(|(name, _)| name)
        .unwrap_or_else(|| "origin".into());
    recovery::restore_rebased(jj, record, workspace, pr_bookmark, base_branch, &remote).await
}
