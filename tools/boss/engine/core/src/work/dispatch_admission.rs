//! Read-only dispatch-admission facts for a work item — the DB-layer half
//! of the shared "would this work item dispatch right now, and if not,
//! why not?" evaluator. See
//! `ExecutionCoordinator::evaluate_dispatch_admission`
//! (`coordinator/dispatch_admission.rs`) for the half that adds the pause
//! snapshot and interactive-concurrency-cap check this can't answer
//! without the live worker pool. See
//! `docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md`.

use super::*;

/// Everything [`WorkDb`] alone can answer about whether `work_item_id`
/// would currently dispatch, independent of the dispatch pause and the
/// interactive concurrency cap (both live only on `ExecutionCoordinator`).
/// Purely read-only — no row is created and no attention item is
/// resolved, unlike the mutating `request_execution_in_tx_with_live_check`
/// path this mirrors the eligibility checks of.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct DispatchAdmissionFacts {
    pub resolved_work_item_id: String,
    /// `Some(reason)` when the work item is ineligible for dispatch
    /// outright — unknown, not a dispatchable kind, deleted, terminal,
    /// human-driven, or (once the status axis clears) no resolvable repo —
    /// mirrors the `bail!` conditions `request_execution_in_tx_with_live_check`
    /// enforces before ever creating a row. Deliberately does NOT include
    /// `in_review`: see `status_ineligibility`'s doc for why that stays
    /// eligible on both the ordinary and the forced path.
    pub ineligible_reason: Option<String>,
    /// Work items gating this one, per [`WorkDb::gating_prereqs_for`].
    /// Always empty when `ineligible_reason` is `Some` (there is no point
    /// resolving dependencies for a row that can't dispatch anyway).
    #[builder(default)]
    pub unmet_dependencies: Vec<String>,
    /// `true` when the item is churn-guard parked under either
    /// representation: an open `churn_guard_parked` attention item
    /// (`pr_review_recovery`'s path) or `tasks.dispatch_failed_reason =
    /// 'churn_guard'` (`orphan_sweep`'s path) — see the query in
    /// [`dispatch_admission_facts`] for why both are checked. Informational
    /// only: an explicit dispatch request (forced or not) has always
    /// cleared this rather than being blocked by it — see
    /// `request_execution_in_tx_with_live_check`'s unconditional
    /// `resolve_attention_kind_in_tx` call.
    #[builder(default)]
    pub churn_guard_parked: bool,
    /// `true` when the task has `autostart = false` and is still `todo`.
    /// Informational only, for the same reason as `churn_guard_parked`:
    /// explicit dispatch (forced or not) has always bypassed this gate —
    /// see `task_accepts_execution`'s doc comment.
    #[builder(default)]
    pub autostart_disabled: bool,
    /// `true` when a fresh execution for this work item would route to
    /// the interactive main pool (i.e. neither `pr_review` nor
    /// automation-sourced) — the only pool the concurrency cap governs.
    #[builder(default)]
    pub targets_main_pool: bool,
    /// `true` when a fresh execution for this work item would route to
    /// the dedicated review pool (i.e. `pr_review`) — the only pool an
    /// operator-originated dispatch pause exempts (`drain_ready_queue`
    /// holds only `paused && !is_review`). Used by
    /// `ExecutionCoordinator::evaluate_dispatch_admission` to report no
    /// pause in effect for such rows, rather than a bypassable one.
    #[builder(default)]
    pub exempt_from_operator_pause: bool,
}

impl WorkDb {
    pub(crate) fn dispatch_admission_facts(&self, work_item_id: &str) -> Result<DispatchAdmissionFacts> {
        // Every raw query against `conn` happens in this inner block, which
        // ends (dropping `conn`) before any call below to a `self.*` helper
        // that itself does `self.connect()` — `WorkDb::connect` is a single
        // `Mutex<Connection>`, not a real pool (see `schema_init::connect`),
        // so holding this guard across such a call would self-deadlock the
        // thread on the second `.lock()` rather than error.
        struct RawFacts {
            resolved_work_item_id: String,
            ineligible_reason: Option<String>,
            churn_guard_parked: bool,
            autostart_disabled: bool,
            kind: Option<ExecutionKind>,
        }
        let raw = {
            let conn = self.connect()?;
            let resolved_work_item_id =
                resolve_friendly_work_item_id(&conn, work_item_id)?.unwrap_or_else(|| work_item_id.to_owned());

            if !resolved_work_item_id.starts_with("task_") {
                // Projects/products never carry an execution lifecycle.
                RawFacts {
                    resolved_work_item_id: resolved_work_item_id.clone(),
                    ineligible_reason: Some(format!("{resolved_work_item_id} is not a dispatchable work item")),
                    churn_guard_parked: false,
                    autostart_disabled: false,
                    kind: None,
                }
            } else {
                let row: Option<(String, Option<String>, i64, i64)> = conn
                    .query_row(
                        "SELECT status, archived_reason, autostart, deleted_at IS NOT NULL FROM tasks WHERE id = ?1",
                        params![resolved_work_item_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?;
                let Some((status_str, archived_reason, autostart, deleted)) = row else {
                    return Ok(DispatchAdmissionFacts::builder()
                        .resolved_work_item_id(resolved_work_item_id.clone())
                        .ineligible_reason(format!("unknown work item: {resolved_work_item_id}"))
                        .build());
                };
                let status: TaskStatus = status_str.parse().map_err(|e: String| anyhow::anyhow!(e))?;
                // Shares `status_ineligibility` with
                // `request_execution_in_tx_with_live_check` (see its doc)
                // so this read-only evaluator can never diverge from what
                // the mutating request path actually enforces on the
                // status axis.
                let ineligible_reason = if let Some(ineligibility) =
                    status_ineligibility(&status, archived_reason.as_deref(), deleted != 0)
                {
                    Some(match ineligibility {
                        StatusIneligibility::Deleted => format!("{resolved_work_item_id} is deleted"),
                        StatusIneligibility::Terminal {
                            status,
                            archived_reason,
                        } => {
                            let reason_suffix = archived_reason.map(|r| format!(" — {r}")).unwrap_or_default();
                            format!(
                                "{resolved_work_item_id} is `{status}`{reason_suffix} — terminal work items \
                                 cannot be dispatched"
                            )
                        }
                    })
                } else if work_item_is_human_driven(&conn, &resolved_work_item_id)? {
                    Some(format!(
                        "{resolved_work_item_id} is human-driven — no agent worker will run"
                    ))
                } else {
                    // Mirrors the request path's unresolved-repo bail
                    // (`dispatch_helpers::request_execution_in_tx_with_live_check`)
                    // so this evaluator can't report `would_dispatch: true`
                    // for an item the mutating request would then reject —
                    // read-only, so unlike that path this never records the
                    // sticky repo-unresolved attention item itself.
                    match resolve_repo_for_work_item(&conn, &resolved_work_item_id)? {
                        Some(_) => None,
                        None => {
                            let label = repo_unresolved_kind_label(&conn, &resolved_work_item_id)?;
                            Some(repo_unresolved_attention_body(&resolved_work_item_id, label))
                        }
                    }
                };
                // Two representations, one per caller: `orphan_sweep` bounces
                // an `active` task straight to `dispatch_failed_reason =
                // 'churn_guard'` (see `WorkDb::bounce_churn_guard_parked_to_backlog`),
                // while `pr_review_recovery` still files the
                // `churn_guard_parked` attention item for an `in_review` task
                // (Backlog is not a meaningful bounce target for a task with
                // an open PR under review). Checking both keeps this
                // informational-only fact accurate for either source — see
                // `docs/designs/dispatch-halt-state-vs-attention-items.md`.
                let churn_guard_parked: i64 = conn.query_row(
                    "SELECT
                        (SELECT COUNT(*) FROM tasks
                          WHERE id = ?1 AND dispatch_failed_reason = ?3)
                        + (SELECT COUNT(*) FROM work_attention_items
                            WHERE work_item_id = ?1 AND kind = ?2 AND status = 'open')",
                    params![
                        resolved_work_item_id,
                        CHURN_GUARD_PARKED_ATTENTION_KIND,
                        CHURN_GUARD_DISPATCH_FAILED_REASON
                    ],
                    |row| row.get(0),
                )?;
                let autostart_disabled = autostart == 0 && status == TaskStatus::Todo;
                // Mirrors the request path's own idempotency/re-dispatch
                // guard (`request_execution_in_tx_with_live_check`'s
                // "governing" execution): a non-terminal execution already
                // present for this work item — notably a `ready`
                // `pr_review` execution — is REUSED rather than a fresh one
                // being created, so its own kind (not the task's kind-
                // derived hypothetical fresh-execution kind) is what
                // actually governs pool routing and the operator-pause
                // review exemption. Falls back to the fresh-execution kind
                // when no non-terminal execution exists yet.
                let live = query_live_execution_for_work_item(&conn, &resolved_work_item_id)?;
                let latest = query_latest_execution_for_work_item(&conn, &resolved_work_item_id)?;
                let governing = live.or(latest).filter(|e| !e.status.is_terminal());
                let kind = match governing {
                    Some(execution) => execution.kind,
                    None => execution_kind_for_work_item(&conn, &resolved_work_item_id)?,
                };
                RawFacts {
                    resolved_work_item_id,
                    ineligible_reason,
                    churn_guard_parked: churn_guard_parked > 0,
                    autostart_disabled,
                    kind: Some(kind),
                }
            }
        };

        let unmet_dependencies = if raw.ineligible_reason.is_none() {
            self.gating_prereqs_for(&raw.resolved_work_item_id)?
        } else {
            Vec::new()
        };
        let exempt_from_operator_pause = raw.kind == Some(ExecutionKind::PrReview);
        let targets_main_pool = match raw.kind {
            Some(kind) => {
                kind != ExecutionKind::PrReview
                    && kind != ExecutionKind::AutomationTriage
                    && !matches!(
                        self.source_automation_id_for_work_item(&raw.resolved_work_item_id),
                        Ok(Some(_))
                    )
            }
            None => false,
        };
        Ok(DispatchAdmissionFacts::builder()
            .resolved_work_item_id(raw.resolved_work_item_id)
            .maybe_ineligible_reason(raw.ineligible_reason)
            .unmet_dependencies(unmet_dependencies)
            .churn_guard_parked(raw.churn_guard_parked)
            .autostart_disabled(raw.autostart_disabled)
            .targets_main_pool(targets_main_pool)
            .exempt_from_operator_pause(exempt_from_operator_pause)
            .build())
    }
}

#[cfg(test)]
mod churn_guard_parked_fact_tests {
    use crate::test_support::*;

    /// `orphan_sweep`'s representation: `dispatch_failed_reason =
    /// 'churn_guard'` on the task row, no attention item.
    #[test]
    fn true_via_dispatch_failed_reason() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        db.bounce_churn_guard_parked_to_backlog(&work_item_id, "orphan_sweep", 3, &[], "terminal executions");

        let facts = db.dispatch_admission_facts(&work_item_id).unwrap();
        assert!(facts.churn_guard_parked);
    }

    /// `pr_review_recovery`'s representation: an open `churn_guard_parked`
    /// attention item, task status untouched.
    #[test]
    fn true_via_open_attention_item() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        db.file_churn_guard_parked_attention(
            &work_item_id,
            "pr_review_recovery",
            3,
            &[],
            "terminal pr_review executions",
        );

        let facts = db.dispatch_admission_facts(&work_item_id).unwrap();
        assert!(facts.churn_guard_parked);
    }

    /// Neither representation present: not reported as parked.
    #[test]
    fn false_when_neither_representation_present() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");

        let facts = db.dispatch_admission_facts(&work_item_id).unwrap();
        assert!(!facts.churn_guard_parked);
    }
}
