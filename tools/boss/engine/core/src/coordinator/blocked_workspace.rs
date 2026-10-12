use super::*;
use boss_engine_recovery::execution_bookmark::{
    ExecutionBookmark, PointerIntegrityKind, Reconciliation, is_base_unresolvable_error, is_missing_pointer_error,
    is_reconcilable_pointer_error, pointer_integrity_error, pointer_integrity_kind,
};

/// What dispatch recovered into the new lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recovered {
    pub(crate) predecessor: String,
    pub(crate) has_work: bool,
    /// Set when the predecessor's pointers had to be re-anchored first.
    pub(crate) reconciliation: Option<Reconciliation>,
}

/// Outcome of inspecting a recovery source's pointers.
enum Inspection {
    /// The unpublished diff, after any reconciliation that was needed.
    Patch(String, Option<Reconciliation>),
    /// The baseline was gone and nothing the engine recorded vouched for
    /// the heads: the record was retired and dispatch continues clean.
    Retired,
}

impl ExecutionCoordinator {
    /// Restore the predecessor's engine-created reference into the new lease.
    /// No read of its old workspace or lease history occurs.
    ///
    /// Missing predecessor heads degrade to clean dispatch with a warning.
    /// Invalid pointers still fail with `PointerIntegrityError`; transient
    /// fetch/goto/SSH failures stay retryable.
    ///
    /// `repo_id` is the handle `ensure_repo` returned for this execution; the
    /// registered repo is selected by it because `execution.repo_remote_url`
    /// may be a shorthand, resolver slug, or alternate URL spelling.
    ///
    /// `pr_for_goto` is the bound PR number already resolved by
    /// `pr_number_for_workspace_goto` (including the chain-root fallback), since
    /// resume executions do not carry `pr_url`.
    ///
    /// `pr_for_reconcile` is the item's recorded PR for any execution kind
    /// (a revision's bound PR, or the task's own open PR), used only to name
    /// the `pr/<n>` bookmark cube writes when damaged pointers must be
    /// re-anchored; it never changes positioning.
    pub(super) async fn recover_execution_bookmark(
        &self,
        execution: &WorkExecution,
        lease: &CubeWorkspaceLease,
        adapter: &Arc<dyn HostAdapter>,
        repo_id: &str,
        pr_for_goto: Option<u64>,
        pr_for_reconcile: Option<u64>,
    ) -> Result<Option<Recovered>> {
        let prior = if self.work_db.execution_bookmark_optional(&execution.id)?.is_some() {
            execution.clone()
        } else if let Some(prior) = self.work_db.recovery_predecessor(execution)? {
            prior
        } else {
            return Ok(None);
        };
        let Some(record) = self.work_db.execution_bookmark_optional(&prior.id)? else {
            self.warn_missing_execution_bookmark(execution, Some(&prior.id)).await;
            self.work_db.delete_execution_restore_report(&execution.id)?;
            return Ok(None);
        };
        if record.host_id != adapter.host_id() {
            return Err(pointer_integrity_error(format!(
                "execution {} recovery store is on host {}; dispatch selected {}",
                prior.id,
                record.host_id,
                adapter.host_id()
            )));
        }
        let is_implementation = matches!(
            execution.kind,
            ExecutionKind::ChoreImplementation
                | ExecutionKind::TaskImplementation
                | ExecutionKind::RevisionImplementation
        );
        // Inspect (and if needed repair) the pointers before anything reads
        // them: both restore paths below assume a single line from the
        // baseline to the head.
        let inspection = match adapter.execution_bookmark_diff(&record).await {
            Ok(patch) => Inspection::Patch(patch, None),
            Err(err) if is_missing_pointer_error(&err) => {
                self.degrade_missing_pointers(execution, Some(&prior.id), &record, &err)
                    .await;
                // A report from an earlier attempt would make dispatch skip PR positioning.
                self.work_db.delete_execution_restore_report(&execution.id)?;
                if prior.id == execution.id {
                    // The row names heads that no longer exist; keeping it
                    // would make dispatch treat the run as already recovered
                    // and skip creating fresh pointers. The surviving
                    // baseline would also make fresh creation fail, so drop
                    // it first (it refuses unless both heads are absent). Any
                    // restore report from an earlier attempt goes with the row,
                    // or dispatch would skip positioning on the bound PR.
                    adapter.discard_orphaned_execution_baseline(&record).await?;
                    self.work_db
                        .delete_execution_bookmark_and_restore_report(&execution.id)?;
                }
                return Ok(None);
            }
            Err(err) if is_reconcilable_pointer_error(&err) => {
                self.reconcile_or_retire(
                    execution,
                    &prior,
                    &record,
                    lease,
                    adapter,
                    repo_id,
                    pr_for_reconcile,
                    err,
                )
                .await?
            }
            Err(err) => return Err(err),
        };
        let (patch, reconciliation) = match inspection {
            Inspection::Patch(patch, reconciliation) => (patch, reconciliation),
            Inspection::Retired => {
                self.work_db.delete_execution_restore_report(&execution.id)?;
                return Ok(None);
            }
        };
        let has_work = if prior.id != execution.id && is_implementation {
            let has_work = !patch.trim().is_empty();
            if execution.kind == ExecutionKind::RevisionImplementation && pr_for_goto.is_none() && !has_work {
                return Ok(None);
            }
            let repos = adapter.list_repos().await?;
            let repo = repos
                .iter()
                .find(|repo| repo.repo_id == repo_id)
                .ok_or_else(|| anyhow!("recovery repository {repo_id} is absent from cube repo list"))?;
            let mut base_fallback = None;
            let base_branch = if execution.kind == ExecutionKind::RevisionImplementation {
                let pr = pr_for_goto
                    .ok_or_else(|| pointer_integrity_error("revision recovery requires its bound PR URL"))?;
                match adapter.recovery_pr_base(&repo.origin, pr).await {
                    Ok(base) => base,
                    // Only a base that can never be resolved (e.g. a non-GitHub
                    // origin) falls back; transient gh/network errors propagate to
                    // the pre-start retry path with the base unchanged.
                    Err(err) if is_base_unresolvable_error(&err) => {
                        tracing::warn!(execution_id = %execution.id, ?err, fallback = %repo.main_branch, "PR base branch is unresolvable; restaging recovery onto the main branch");
                        base_fallback = Some(format!(
                            "its base branch could not be determined: {err:#}; used `{}`",
                            repo.main_branch
                        ));
                        repo.main_branch.clone()
                    }
                    Err(err) => return Err(err),
                }
            } else {
                repo.main_branch.clone()
            };
            let pr_bookmark = if execution.kind == ExecutionKind::RevisionImplementation {
                let pr = pr_for_goto
                    .ok_or_else(|| pointer_integrity_error("revision recovery requires its bound PR URL"))?;
                // Cube resolves the bound PR head, fetches it, and writes pr/<n>.
                // The preserved execution pointer survives this checkout.
                adapter.goto_workspace(&lease.workspace_path, pr).await?;
                Some(format!("pr/{pr}"))
            } else {
                None
            };
            let restore = adapter
                .restore_rebased_execution_bookmark(
                    &record,
                    &lease.workspace_path,
                    pr_bookmark.as_deref(),
                    &base_branch,
                )
                .await;
            let mut report = match restore {
                Ok(report) => report,
                Err(err)
                    if execution.kind == ExecutionKind::RevisionImplementation
                        && base_branch != repo.main_branch
                        && is_base_unresolvable_error(&err) =>
                {
                    // The PR base resolved but its remote bookmark is gone (e.g. a
                    // stacked parent deleted after merge). Any other failure is
                    // transient and must not silently change the base.
                    tracing::warn!(execution_id = %execution.id, ?err, "PR base has no remote commit; retrying on the main branch");
                    base_fallback = Some(format!(
                        "requested base `{base_branch}`: {err:#}; used `{}`",
                        repo.main_branch
                    ));
                    adapter
                        .restore_rebased_execution_bookmark(
                            &record,
                            &lease.workspace_path,
                            pr_bookmark.as_deref(),
                            &repo.main_branch,
                        )
                        .await?
                }
                Err(err) => return Err(err),
            };
            report.base_fallback = base_fallback;
            self.work_db.record_execution_restore_report(&execution.id, &report)?;
            has_work
        } else {
            adapter
                .restore_execution_bookmark(
                    &record,
                    &lease.workspace_path,
                    is_implementation && prior.id == execution.id,
                )
                .await?
        };
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::WorkspaceRecovery, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_details(serde_json::json!({
                        "source": "execution_bookmark",
                        "predecessor": prior.id,
                        "bookmark": record.head(),
                        "has_work": has_work,
                        "reconciled": reconciliation.as_ref().map(|r| r.anchor.commit().to_owned()),
                    })),
            )
            .await;
        Ok(Some(Recovered {
            predecessor: prior.id,
            has_work,
            reconciliation,
        }))
    }

    /// The pointers broke in a way [`reconcile`] can repair from an
    /// engine-recorded anchor. Position the lease on the item's recorded PR
    /// first when it has one, so cube writes the `pr/<n>` bookmark the
    /// repair resumes from. When the baseline is gone and nothing recorded
    /// vouches for the heads, the record is retired and the item starts
    /// fresh, loudly: the rule is that recovery fires only on an unambiguous
    /// durable pointer. A stranded baseline or diverged heads with work
    /// nothing vouches for stays a hard failure, so the item blocks with the
    /// precise reason.
    ///
    /// [`reconcile`]: boss_engine_recovery::execution_bookmark::reconcile
    #[allow(clippy::too_many_arguments)]
    async fn reconcile_or_retire(
        &self,
        execution: &WorkExecution,
        prior: &WorkExecution,
        record: &ExecutionBookmark,
        lease: &CubeWorkspaceLease,
        adapter: &Arc<dyn HostAdapter>,
        repo_id: &str,
        pr_for_reconcile: Option<u64>,
        failure: anyhow::Error,
    ) -> Result<Inspection> {
        let kind = pointer_integrity_kind(&failure);
        tracing::warn!(
            execution_id = %execution.id,
            predecessor = %prior.id,
            ?kind,
            error = %failure,
            "recovery pointers are damaged; reconciling against engine-recorded pointers"
        );
        let pr_bookmark = match pr_for_reconcile {
            Some(pr) => {
                // Cube resolves the bound PR head, fetches it, and writes pr/<n>.
                adapter.goto_workspace(&lease.workspace_path, pr).await?;
                Some(format!("pr/{pr}"))
            }
            None => None,
        };
        let repos = adapter.list_repos().await?;
        let trunk = repos
            .iter()
            .find(|repo| repo.repo_id == repo_id)
            .map(|repo| repo.main_branch.clone())
            .ok_or_else(|| anyhow!("recovery repository {repo_id} is absent from cube repo list"))?;
        match adapter
            .reconcile_execution_bookmark(record, pr_bookmark.as_deref(), &trunk)
            .await
        {
            Ok(reconciliation) => {
                if let Some(reconciliation) = &reconciliation {
                    tracing::info!(
                        execution_id = %execution.id,
                        predecessor = %prior.id,
                        anchor = ?reconciliation.anchor,
                        "recovery pointers reconciled"
                    );
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(Stage::WorkspaceRecovery, DispatchOutcome::Ok, &execution.id)
                                .with_work_item(&execution.work_item_id)
                                .with_details(serde_json::json!({
                                    "source": "pointer_reconciliation",
                                    "predecessor": prior.id,
                                    "anchor": reconciliation.anchor,
                                    "failure": reconciliation.failure,
                                })),
                        )
                        .await;
                }
                let patch = adapter.execution_bookmark_diff(record).await?;
                Ok(Inspection::Patch(patch, reconciliation))
            }
            Err(err)
                if kind == Some(PointerIntegrityKind::MissingBaseline)
                    && pointer_integrity_kind(&err) == Some(PointerIntegrityKind::NotContained) =>
            {
                self.retire_unvouched_record(execution, prior, record, adapter, &err)
                    .await?;
                Ok(Inspection::Retired)
            }
            Err(err) => Err(err),
        }
    }

    /// Deliberate fresh start. The record stops being a recovery source, so
    /// no later dispatch retries the same dead pointer; the commits stay in
    /// the shared store and the attention item says where. A self-retry also
    /// releases the head bookmarks so it can create fresh pointers under its
    /// own names.
    async fn retire_unvouched_record(
        &self,
        execution: &WorkExecution,
        prior: &WorkExecution,
        record: &ExecutionBookmark,
        adapter: &Arc<dyn HostAdapter>,
        reason: &anyhow::Error,
    ) -> Result<()> {
        let heads = if prior.id == execution.id {
            adapter.release_execution_bookmark_heads(record).await?
        } else {
            Vec::new()
        };
        let reason = format!("{reason:#}");
        tracing::error!(
            execution_id = %execution.id,
            predecessor = %prior.id,
            reason,
            "recovery pointers retired; starting the item fresh"
        );
        self.work_db.retire_execution_bookmark(&prior.id, &reason)?;
        let released = if heads.is_empty() {
            format!(
                "Its bookmarks `{}` and `{}` were left in place for inspection.",
                record.head(),
                record.publication()
            )
        } else {
            format!(
                "Its head bookmarks were released so the retry could create fresh pointers under the same names; the commits they held are still in the store: {}.",
                heads
                    .iter()
                    .map(|(bookmark, commit)| format!("`{bookmark}` was at `{commit}` (`jj log -r {commit}`)"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        self.work_db.create_attention_item(CreateAttentionItemInput {
            execution_id: Some(prior.id.clone()),
            work_item_id: None,
            kind: crate::execution_bookmark_recovery::RECOVERY_RETIRED.to_owned(),
            status: None,
            title: "Recovery pointers retired; item restarted fresh".to_owned(),
            body_markdown: format!(
                "The recovery record of execution `{}` could not be resumed: {reason}\n\nThe engine started execution `{}` fresh from the item's branch instead and retired the record, so this pointer is not retried on later dispatches. Nothing was deleted from the shared jj store at `{}` on host `{}`. {released}",
                prior.id,
                execution.id,
                record.repo_path.display(),
                record.host_id
            ),
            resolved_at: None,
        })?;
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::WorkspaceRecovery, DispatchOutcome::Skipped, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_details(serde_json::json!({
                        "source": "execution_bookmark",
                        "predecessor": prior.id,
                        "reason": "pointers_retired",
                        "detail": reason,
                    })),
            )
            .await;
        Ok(())
    }

    /// Shared handling for a recorded recovery pointer whose bookmarks are both
    /// gone: name them in the log, emit the skipped-recovery event, and let the
    /// caller continue without recovery. Integrity and transport errors never
    /// come through here.
    pub(super) async fn degrade_missing_pointers(
        &self,
        execution: &WorkExecution,
        predecessor: Option<&str>,
        record: &boss_engine_recovery::execution_bookmark::ExecutionBookmark,
        err: &anyhow::Error,
    ) {
        tracing::warn!(execution_id = %execution.id, predecessor,
            recovery_pointer = %record.head(), publication_pointer = %record.publication(),
            error = %err, "recovery pointers are missing; continuing without recovery");
        self.warn_missing_execution_bookmark(execution, predecessor).await;
    }

    /// Loud, non-fatal: a missing pointer is visible in dispatch events and
    /// logs, but does not fail dispatch or resume. The abandoned-bookmark
    /// sweep is the place that still reports it as attention.
    pub(super) async fn warn_missing_execution_bookmark(&self, execution: &WorkExecution, predecessor: Option<&str>) {
        tracing::warn!(
            execution_id = %execution.id,
            predecessor,
            "engine-created recovery bookmark unavailable; continuing without recovery"
        );
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::WorkspaceRecovery, DispatchOutcome::Skipped, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_details(serde_json::json!({
                        "source": "execution_bookmark",
                        "predecessor": predecessor,
                        "reason": "missing_bookmark",
                    })),
            )
            .await;
    }

    pub(crate) async fn inspect_execution_bookmark(
        &self,
        record: &boss_engine_recovery::execution_bookmark::ExecutionBookmark,
    ) -> Result<String> {
        let host = self
            .work_db
            .get_host(&record.host_id)?
            .ok_or_else(|| anyhow!("recovery host {} is unavailable", record.host_id))?;
        self.host_adapter_provider
            .adapter_for(&host)
            .await?
            .execution_bookmark_diff(record)
            .await
    }
}
