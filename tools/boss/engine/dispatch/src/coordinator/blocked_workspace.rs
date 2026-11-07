use super::fresh_start::{FreshStart, position_fresh_workspace};
use super::*;
use boss_engine_recovery::execution_bookmark::{
    ExecutionBookmark, is_base_unresolvable_error, is_missing_pointer_error, is_pointer_integrity_error,
    pointer_integrity_error,
};

/// What dispatch recovered into the new lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    pub predecessor: String,
    pub has_work: bool,
}

impl ExecutionCoordinator {
    /// Pointer creation also validates inherited baselines. A race or a
    /// partially created record from an earlier attempt uses the same fallback.
    pub(super) async fn create_or_start_fresh(
        &self,
        execution: &WorkExecution,
        lease: &CubeWorkspaceLease,
        adapter: &Arc<dyn HostAdapter>,
        inheritance: (Option<&ExecutionBookmark>, Option<&str>),
        positioning: FreshStart<'_>,
    ) -> Result<(ExecutionBookmark, bool)> {
        let (predecessor, inherited_base) = inheritance;
        // A restore report's copied PR baseline wins. On a fresh start, the
        // bound-PR snapshot on the execution row is the floor; never walk the
        // positioned workspace. A predecessor without a report inherits its
        // own resolved floor inside pointer creation.
        let inherited_base = if predecessor.is_some() {
            inherited_base
        } else {
            inherited_base.or(execution.pr_head_before.as_deref())
        };
        let result = adapter
            .create_execution_bookmark(&lease.workspace_path, &execution.id, predecessor, inherited_base)
            .await;
        let err = match result {
            Ok(record) => return Ok((record, true)),
            Err(err) => err,
        };
        if !is_pointer_integrity_error(&err) && !is_missing_pointer_error(&err) {
            return Err(err);
        }
        let Some(failure) = err.downcast_ref::<boss_engine_recovery::execution_bookmark::CreationFailure>() else {
            return Err(err);
        };
        let mut archives = adapter.release_execution_bookmark_heads(&failure.record, true).await?;
        // Stale names belong to this attempt, not to the successfully recovered
        // predecessor. Retry adoption before discarding that recovered work.
        if predecessor.is_some() {
            let reason = format!("{err:#}");
            tracing::warn!(execution_id = %execution.id, ?archives, reason,
                "stale current-execution pointers archived; continuing predecessor adoption");
            self.work_db
                .record_recovery_adoption_archive(execution, &reason, &archives)?;
            match adapter
                .create_execution_bookmark(&lease.workspace_path, &execution.id, predecessor, inherited_base)
                .await
            {
                Ok(record) => return Ok((record, true)),
                Err(retry) => {
                    if !is_pointer_integrity_error(&retry) && !is_missing_pointer_error(&retry) {
                        return Err(retry);
                    }
                    if let Some(partial) =
                        retry.downcast_ref::<boss_engine_recovery::execution_bookmark::CreationFailure>()
                    {
                        archives.extend(adapter.release_execution_bookmark_heads(&partial.record, true).await?);
                    } else {
                        return Err(retry);
                    }
                }
            }
        }
        if let Some(prior) = predecessor {
            let prior_execution = self.work_db.get_execution(&prior.execution_id)?;
            self.retire_unvouched_record(execution, &prior_execution, prior, adapter, &err)
                .await?;
        }
        let reason = format!("{err:#}");
        tracing::warn!(execution_id = %execution.id, ?archives, reason,
            "pointer initialization could not preserve recovery; starting fresh");
        if let Err(note_error) = self
            .work_db
            .record_recovery_fresh_start(execution, &execution.id, &reason, &archives)
        {
            tracing::error!(?note_error, "could not record pointer initialization archive note");
        }
        self.work_db.delete_execution_restore_report(&execution.id)?;
        let positioned = position_fresh_workspace(adapter.as_ref(), &lease.workspace_path, &positioning).await?;
        adapter.create_change(&lease.workspace_path, positioning.title).await?;
        let record = adapter
            .create_execution_bookmark(
                &lease.workspace_path,
                &execution.id,
                None,
                positioned.as_deref().or(execution.pr_head_before.as_deref()),
            )
            .await?;
        Ok((record, false))
    }

    /// Restore the predecessor's engine-created reference into the new lease.
    /// No read of its old workspace or lease history occurs.
    ///
    /// A record with no heads is an empty run and dispatches fresh without a warning.
    /// Integrity failures archive their source and dispatch fresh; transport failures retry.
    /// Failure to write the archive leaves all source pointers intact.
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
    /// re-anchored; a verified PR anchor also controls positioning.
    pub(super) async fn recover_execution_bookmark(
        &self,
        execution: &WorkExecution,
        lease: &CubeWorkspaceLease,
        adapter: &Arc<dyn HostAdapter>,
        repo_id: &str,
        pr_for_goto: Option<u64>,
        pr_for_reconcile: Option<u64>,
    ) -> Result<Option<Recovered>> {
        // Reviews have no unpublished implementation work to recover, even on retry.
        if execution.kind.is_read_only_review() {
            return Ok(None);
        }
        let Some(source) = self.work_db.recovery_source(execution)? else {
            return Ok(None);
        };
        let prior = source.execution;
        // Preserved work is the source of truth whenever a tip exists,
        // including a retry of the same execution. Legacy pointers are
        // only used when no preserved tip is recorded.
        if self.work_db.preserved_tip(&prior.id)?.is_some() {
            return self
                .recover_preserved_work(
                    execution,
                    &prior,
                    lease,
                    adapter,
                    repo_id,
                    (pr_for_goto, pr_for_reconcile),
                )
                .await;
        }
        let Some(record) = self.work_db.execution_bookmark_optional(&prior.id)? else {
            self.warn_missing_execution_bookmark(execution, Some(&prior.id)).await;
            self.work_db.delete_execution_restore_report(&execution.id)?;
            return Ok(None);
        };
        let result = self
            .recover_from_record(
                execution,
                (&prior, &record),
                lease,
                adapter,
                repo_id,
                (pr_for_goto, pr_for_reconcile),
            )
            .await;
        match result {
            Ok(recovered) => Ok(recovered),
            Err(err) if is_pointer_integrity_error(&err) || is_missing_pointer_error(&err) => {
                self.retire_unvouched_record(execution, &prior, &record, adapter, &err)
                    .await?;
                self.work_db.delete_execution_restore_report(&execution.id)?;
                let immutable = self.immutable_execution_target(execution)?;
                // Dispatch positions bound PRs and immutable targets below;
                // its describe-only path needs explicit fresh positioning here.
                if pr_for_goto.is_none() && immutable.is_none() {
                    position_fresh_workspace(
                        adapter.as_ref(),
                        &lease.workspace_path,
                        &FreshStart {
                            repo_id,
                            immutable_target: None,
                            bound_pr: None,
                            task_pr: pr_for_reconcile,
                            title: "",
                        },
                    )
                    .await?;
                }
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    async fn recover_from_record(
        &self,
        execution: &WorkExecution,
        source: (&WorkExecution, &ExecutionBookmark),
        lease: &CubeWorkspaceLease,
        adapter: &Arc<dyn HostAdapter>,
        repo_id: &str,
        prs: (Option<u64>, Option<u64>),
    ) -> Result<Option<Recovered>> {
        let (prior, record) = source;
        let (pr_for_goto, pr_for_reconcile) = prs;
        // An empty run recorded no recovery head and has no preserved tip
        // (the caller already took the preserved-work path). Do not contact
        // the source host: there is nothing to inspect or archive.
        if record.has_recovery_head == Some(false) {
            if prior.id == execution.id {
                self.work_db
                    .retire_execution_bookmark(&prior.id, "no recovery head: no unpublished work to recover")?;
            }
            self.work_db.delete_execution_restore_report(&execution.id)?;
            return Ok(None);
        }
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
        // Inspect the pointers before anything reads them. The diff floor is
        // the baseline commit recorded on the row, so a baseline bookmark that
        // was rewritten, abandoned or deleted needs no repair here.
        let patch = match adapter.execution_bookmark_diff(record).await {
            Ok(patch) => patch,
            // A record with no recovery head is an empty run: nothing was left
            // unpublished, so there is nothing to recover and nothing to report.
            // A retry's own stale row is retired so dispatch records a fresh
            // starting commit; a predecessor's row is left for the prune sweep.
            Err(err) if is_missing_pointer_error(&err) => {
                if prior.id == execution.id {
                    adapter.release_execution_bookmark_heads(record, true).await?;
                    self.work_db
                        .retire_execution_bookmark(&prior.id, "no recovery head: no unpublished work to recover")?;
                }
                self.work_db.delete_execution_restore_report(&execution.id)?;
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        let has_work = if prior.id != execution.id && is_implementation {
            let has_work = !patch.trim().is_empty();
            if execution.kind == ExecutionKind::RevisionImplementation && pr_for_goto.is_none() && !has_work {
                return Ok(None);
            }
            // Keep using the recorded open PR even when the recovered range is
            // empty, or restaging would discard the PR implementation.
            let positioning_pr = match pr_for_goto.or(pr_for_reconcile) {
                Some(pr)
                    if adapter
                        .prepare_recovery_pr(record, &lease.workspace_path, repo_id, pr)
                        .await? =>
                {
                    Some(pr)
                }
                _ => None,
            };
            if execution.kind == ExecutionKind::RevisionImplementation && positioning_pr.is_none() {
                return Err(pointer_integrity_error("revision recovery requires its bound PR URL"));
            }
            let repos = adapter.list_repos().await?;
            let repo = repos
                .iter()
                .find(|repo| repo.repo_id == repo_id)
                .ok_or_else(|| anyhow!("recovery repository {repo_id} is absent from cube repo list"))?;
            let mut base_fallback = None;
            let base_branch = if let Some(pr) = positioning_pr {
                match adapter.recovery_pr_base(&repo.origin, pr).await {
                    Ok(base) => base,
                    // Only a base that can never be resolved (e.g. a non-GitHub
                    // origin) substitutes main during restaging; other errors
                    // retry without retiring the preserved work.
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
            let pr_bookmark = positioning_pr.map(|pr| format!("pr/{pr}"));
            let restore = adapter
                .restore_rebased_execution_bookmark(record, &lease.workspace_path, pr_bookmark.as_deref(), &base_branch)
                .await;
            let mut report = match restore {
                Ok(report) => report,
                Err(err)
                    if positioning_pr.is_some()
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
                            record,
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
                    record,
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
                    })),
            )
            .await;
        Ok(Some(Recovered {
            predecessor: prior.id.clone(),
            has_work,
        }))
    }

    /// Archive first, then retire the recovery source so subsequent attempts
    /// cannot get stuck inspecting the same damaged pointers.
    pub(super) async fn retire_unvouched_record(
        &self,
        execution: &WorkExecution,
        prior: &WorkExecution,
        record: &ExecutionBookmark,
        adapter: &Arc<dyn HostAdapter>,
        reason: &anyhow::Error,
    ) -> Result<()> {
        let archive = async {
            let source_adapter = if adapter.host_id() == record.host_id {
                Arc::clone(adapter)
            } else {
                let host = self
                    .work_db
                    .get_host(&record.host_id)?
                    .ok_or_else(|| anyhow!("archive host {} is unavailable", record.host_id))?;
                self.host_adapter_provider.adapter_for(&host).await?
            };
            source_adapter
                .release_execution_bookmark_heads(record, prior.id == execution.id)
                .await
        }
        .await;
        let heads = archive?;
        if heads.is_empty() {
            // Nothing was archived: there is no unpublished work to keep.
            // Skip the recovery-retired note and the pointers_retired event
            // regardless of why the archive was empty (missing heads, a
            // cross-host empty run, or an integrity error whose range was
            // already published).
            tracing::debug!(execution_id = %execution.id, predecessor = %prior.id, "no recovery source to retire beyond the row");
            self.work_db
                .retire_execution_bookmark(&prior.id, "no preserved work; nothing to recover")?;
            return Ok(());
        }
        let reason = format!("pointer integrity: {reason:#}");
        tracing::info!(
            execution_id = %execution.id,
            predecessor = %prior.id,
            reason,
            archives = ?heads,
            "recovery pointers retired; starting the item fresh"
        );
        self.work_db.retire_execution_bookmark(&prior.id, &reason)?;
        if let Err(err) = self
            .work_db
            .record_recovery_fresh_start(prior, &execution.id, &reason, &heads)
        {
            tracing::error!(execution_id = %execution.id, ?err, "could not record recovery archive note");
        }
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

    /// Whether `recorded` still holds unpublished work on the record's host.
    pub async fn preserved_work_unpublished(
        &self,
        record: &boss_engine_recovery::execution_bookmark::ExecutionBookmark,
        recorded: &boss_engine_recovery::execution_bookmark::PreservedTip,
    ) -> Result<bool> {
        let host = self
            .work_db
            .get_host(&record.host_id)?
            .ok_or_else(|| anyhow!("recovery host {} is unavailable", record.host_id))?;
        self.host_adapter_provider
            .adapter_for(&host)
            .await?
            .preserved_work_unpublished(record, recorded)
            .await
    }

    pub async fn inspect_execution_bookmark(
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

    /// Forget a finished execution's pointers once `vouchers` prove its
    /// work published, on whichever host holds the record.
    pub async fn prune_execution_bookmark(
        &self,
        record: &boss_engine_recovery::execution_bookmark::ExecutionBookmark,
        vouchers: &[boss_engine_recovery::execution_bookmark::Voucher],
        trunk: &str,
    ) -> Result<boss_engine_recovery::execution_bookmark::PruneOutcome> {
        let host = self
            .work_db
            .get_host(&record.host_id)?
            .ok_or_else(|| anyhow!("recovery host {} is unavailable", record.host_id))?;
        self.host_adapter_provider
            .adapter_for(&host)
            .await?
            .prune_execution_bookmark(record, vouchers, trunk)
            .await
    }

    /// The main branch cube records for `repo_id` on `host_id`, when the
    /// repo is registered there.
    pub async fn recovery_trunk(&self, host_id: &str, repo_id: &str) -> Result<Option<String>> {
        let host = self
            .work_db
            .get_host(host_id)?
            .ok_or_else(|| anyhow!("recovery host {host_id} is unavailable"))?;
        let repos = self
            .host_adapter_provider
            .adapter_for(&host)
            .await?
            .list_repos()
            .await?;
        Ok(repos
            .into_iter()
            .find(|repo| repo.repo_id == repo_id)
            .map(|repo| repo.main_branch))
    }
}
