//! DB lookup resolving which agent driver governs a given execution.
//!
//! Two callers, and the second is why the non-task-bound path below exists:
//!
//! - the post-hoc interception dispatch on the `PostToolUse` boundary
//!   (`dispatch_post_hoc_interception_on_post_tool_use`), and
//! - [`crate::events_socket::handle_connection`], which resolves the driver
//!   for EVERY inbound hook connection before it can decode the payload.
//!   A run this returns `None` for has its whole hook event rejected with
//!   `SocketError::UnresolvedDriver` — including the driver turn boundary
//!   the completion subsystem hangs off.

use super::*;

/// The columns [`WorkDb::get_execution_driver_slug`] reads to resolve an
/// execution's driver: the two kinds it short-circuits pool dispatch on and
/// the row/product pin. A named row rather than a five-wide tuple so each
/// column is read back by name at the point it is used.
struct ExecutionDriverRow {
    exec_kind_raw: String,
    task_kind_raw: String,
    task_driver: Option<String>,
    product_default_driver: Option<String>,
}

impl WorkDb {
    /// Resolve the driver slug for `execution_id`'s worker, applying the
    /// same precedence used at spawn time. A claimed worker slot is the
    /// authoritative pool-dispatch input; before a slot exists this mirrors
    /// [`crate::work::driver_allocation::decide_execution_driver`]'s durable
    /// row decision and home-pool handling:
    ///
    /// 1. A claimed review/automation worker slot resolves to that pool's
    ///    fixed driver via [`crate::coordinator::pool_dispatch_policy_for_worker_id`].
    ///    A claimed ordinary `worker-N` slot deliberately has no pool policy,
    ///    including work that spilled from automation, and therefore falls
    ///    through to its own driver resolution.
    /// 2. Before a worker slot is claimed, `pr_review` and
    ///    `automation_triage` resolve to their home pool's fixed driver.
    /// 3. A live pin (`tasks.driver`, then `products.default_driver`) that
    ///    clears the capability gate for this execution kind.
    /// 4. A recorded traffic-allocation decision.
    /// 5. [`boss_engine_effort::ENGINE_DEFAULT_DRIVER`] via
    ///    [`boss_engine_effort::resolve_driver`].
    ///
    /// Returns `Ok(None)` only when the execution row itself cannot be found,
    /// or when it binds to a work item this cannot resolve a driver for (a
    /// Product/Project execution) — the caller then skips rather than
    /// guessing a driver. A task whose `product_id` has no matching `products`
    /// row (a data-integrity problem) still resolves via the task's own
    /// `driver` or the engine default — it is not conflated with the
    /// "no task" case, since `products` is `LEFT JOIN`ed.
    ///
    /// An `answer_agent` execution binds to a `work_comments.id`, so the
    /// `tasks` join can never match it; see
    /// [`Self::answer_agent_driver_slug`] for how it resolves instead.
    /// An `automation_triage` execution binds to an automation id; the
    /// non-task path still returns the pool driver.
    pub fn get_execution_driver_slug(&self, execution_id: &str) -> Result<Option<String>> {
        let claimed_worker_id = self.latest_run_agent_id_for_execution(execution_id)?;
        if let Some(pool_driver) = claimed_worker_id
            .as_deref()
            .and_then(crate::coordinator::pool_dispatch_policy_for_worker_id)
            .map(|policy| policy.driver)
        {
            return Ok(Some(pool_driver.to_owned()));
        }
        // Scoped so the pooled connection is released before the non-task-bound
        // fallback below, which re-enters `WorkDb` methods that take the same
        // (non-reentrant) connection lock.
        let binding = {
            let conn = self.connect()?;
            // `e.kind` and `t.kind` are needed up front: pool-bound executions
            // short-circuit to the pool driver before any row pin can win, and
            // a pin that fails the capability gate for this execution kind
            // must yield (same substitution the spawn path applies).
            let row: Option<ExecutionDriverRow> = conn
                .query_row(
                    "SELECT e.kind, t.kind, t.driver, p.default_driver
                       FROM work_executions e
                       JOIN tasks t ON t.id = e.work_item_id
                       LEFT JOIN products p ON p.id = t.product_id
                      WHERE e.id = ?1",
                    [execution_id],
                    |row| {
                        Ok(ExecutionDriverRow {
                            exec_kind_raw: row.get(0)?,
                            task_kind_raw: row.get(1)?,
                            task_driver: row.get(2)?,
                            product_default_driver: row.get(3)?,
                        })
                    },
                )
                .optional()?;
            if let Some(ExecutionDriverRow {
                exec_kind_raw,
                task_kind_raw,
                task_driver,
                product_default_driver,
            }) = row
            {
                let exec_kind: ExecutionKind = exec_kind_raw.parse().map_err(|err| {
                    anyhow::anyhow!("execution {execution_id} has unrecognised kind {exec_kind_raw:?}: {err}")
                })?;
                // Before a worker slot is claimed, pool-bound kinds resolve
                // to their home pool. Once a slot exists, the branch at the
                // top of this method used its actual policy instead: a spill
                // into an ordinary worker must honour the row's own driver.
                if claimed_worker_id.is_none()
                    && let Some(pool_driver) = crate::coordinator::pool_driver_slug_for_execution_kind(&exec_kind)
                {
                    return Ok(Some(pool_driver.to_owned()));
                }

                // Precedence: a LIVE explicit pin (`tasks.driver`, then
                // `products.default_driver`) that clears the capability gate
                // for this execution → the recorded traffic-allocation
                // decision → the ordinary default resolution.
                //
                // The pins are read fresh rather than taken from the recorded
                // decision on purpose. An operator can pin a driver on a row
                // after its execution row already exists but before it
                // dispatches, and that must still win when the gate accepts
                // it — "an explicit driver on the row always wins" is about
                // the row, not about the instant the execution was created.
                // Only the *allocation* is frozen at insert time (see
                // `work::driver_allocation`), which is what keeps a row from
                // flipping driver between attempts of one dispatch and keeps
                // a split change off already-dispatched work.
                //
                // A pin the gate refuses for this execution kind is dropped
                // here exactly as at spawn: honouring it would point the
                // events-socket normaliser at a driver the worker never ran.
                let task_kind: Option<TaskKind> = task_kind_raw.parse().ok();
                let pinned = task_driver
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| product_default_driver.as_deref().filter(|s| !s.trim().is_empty()));
                if let Some(pinned) = pinned {
                    let honour_pin = match &task_kind {
                        Some(tk) => driver_clears_dispatch_gate(pinned, tk, &exec_kind),
                        // Unparseable task kind: keep pre-existing pin behaviour.
                        None => true,
                    };
                    if honour_pin {
                        return Ok(Some(pinned.to_owned()));
                    }
                }
                // Only an `allocation` decision routes. An `explicit` one is an
                // audit record of a pin that the branch above reads live, so
                // honouring it here would resurrect a pin the operator removed.
                if let Some(allocated) = get_execution_driver_decision_conn(&conn, execution_id)?
                    .filter(|d| matches!(d.reason, REASON_ALLOCATION | REASON_LEGACY_PERCENTAGE))
                    .and_then(|d| d.driver)
                {
                    return Ok(Some(allocated));
                }
                // No honourable pin and no allocation (an ineligible row, a
                // pin the gate refused, or an execution created before this
                // feature landed) — resolves through the ordinary default
                // chain. When a pin was dropped above it is deliberately not
                // re-fed into `resolve_driver`, which would re-honour it.
                let honourable_task = task_driver.as_deref().filter(|s| {
                    !s.trim().is_empty()
                        && task_kind
                            .as_ref()
                            .is_none_or(|tk| driver_clears_dispatch_gate(s, tk, &exec_kind))
                });
                let honourable_product = product_default_driver.as_deref().filter(|s| {
                    !s.trim().is_empty()
                        && task_kind
                            .as_ref()
                            .is_none_or(|tk| driver_clears_dispatch_gate(s, tk, &exec_kind))
                });
                return Ok(Some(boss_engine_effort::resolve_driver(
                    honourable_task,
                    honourable_product,
                )));
            }

            // No `tasks` row. Either the execution does not exist at all, or it
            // is one of the kinds that binds to something other than a task.
            query_execution(&conn, execution_id)?
                .map(|execution| (execution.kind, execution.work_item_id, execution.driver))
        };
        let Some((kind, work_item_id, launched_driver)) = binding else {
            return Ok(None);
        };
        match kind {
            ExecutionKind::AnswerAgent => Ok(Some(self.answer_agent_driver_slug(&work_item_id))),
            // A triage execution has no task row to supply a pin/allocation
            // ladder. If it spilled into an ordinary worker, its frozen
            // launch configuration is the only truthful driver source; do
            // not reassert the automation pool's driver from provenance.
            ExecutionKind::AutomationTriage if claimed_worker_id.is_some() => Ok(launched_driver),
            // `automation_triage` binds to an `automations.id`, so the tasks
            // join above never matches. Its driver is still the automation
            // pool's fixed pin — return that rather than `None`, which would
            // drop every hook event the triage worker sends.
            kind if claimed_worker_id.is_none() && crate::coordinator::kind_always_dispatches_on_pool_driver(&kind) => {
                Ok(crate::coordinator::pool_driver_slug_for_execution_kind(&kind).map(str::to_owned))
            }
            _ => Ok(None),
        }
    }

    /// The driver slug an `answer_agent` execution's worker was spawned with,
    /// resolved from the comment it is bound to.
    ///
    /// This mirrors the spawn path exactly rather than inventing a second
    /// precedence. `ExecutionCoordinator::synthetic_answer_agent_work_item`
    /// builds an in-memory `Chore` for the comment whose `product_id` is the
    /// comment's *doc owner* task's product and whose own `driver` field is
    /// unset; `runner::worker_spawn` then resolves the spawn driver from that
    /// synthetic item — i.e. `resolve_driver(None, product.default_driver)`.
    /// So the doc-owner task's own `driver` column is deliberately NOT
    /// consulted here: the worker was not spawned with it.
    ///
    /// Never fails to a `None`. An answer-agent execution whose comment, doc
    /// owner, or product no longer resolves still gets the engine default —
    /// the same value the spawn path would have landed on with no product
    /// override — because returning `None` here makes the events socket drop
    /// every hook event the worker sends, and the run's only completion signal
    /// (`finalize_answer_agent`, driven by the driver turn boundary) rides on
    /// those events. A wrong-but-decodable driver surfaces as a normalise
    /// error on one event; no driver at all strands the worker forever.
    ///
    /// MUST NOT be called while a pooled connection is held: every hop below
    /// takes the same connection lock, and that lock is not reentrant.
    fn answer_agent_driver_slug(&self, comment_id: &str) -> String {
        let product_default_driver = self.answer_agent_product_default_driver(comment_id);
        boss_engine_effort::resolve_driver(None, product_default_driver.as_deref())
    }

    /// The driver actually recorded as launching this execution's worker
    /// (`work_executions.driver`, frozen once by
    /// [`Self::record_execution_launch_config`]), as opposed to
    /// [`Self::get_execution_driver_slug`]'s live-pin resolution.
    ///
    /// A pin change (`tasks.driver` / `products.default_driver`) applied
    /// after a worker has already launched must not retroactively change
    /// which process the engine expects to see in that worker's PTY — the
    /// pane-input boundary needs "which driver actually launched this run",
    /// not "which driver would be chosen if dispatched right now". Returns
    /// `Ok(None)` when the execution row is missing or has not launched a
    /// worker yet (`driver` unset).
    pub fn launched_driver_slug(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        Ok(query_execution(&conn, execution_id)?.and_then(|execution| execution.driver))
    }

    /// `products.default_driver` for the product owning the document
    /// `comment_id` was left on, or `None` when any hop no longer resolves
    /// (comment deleted mid-flight, doc pointer moved, product gone). Every
    /// hop degrades to `None` rather than an error — see
    /// [`Self::answer_agent_driver_slug`] for why this must not fail.
    fn answer_agent_product_default_driver(&self, comment_id: &str) -> Option<String> {
        let comment = self.get_comment(comment_id).ok().flatten()?;
        let doc_owner = self
            .resolve_doc_owner(&comment.artifact_kind, &comment.artifact_id)
            .ok()
            .flatten()?;
        let owner = self.get_work_item(&doc_owner.task_id).ok()?;
        self.get_product(owner.product_id()).ok().flatten()?.default_driver
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};
    use crate::work::{CreateCommentInput, WorkDb, WorkItemPatch};

    const DOC_REPO: &str = "git@github.com:spinyfin/mono.git";
    const DOC_ARTIFACT: &str = "pr_doc:git@github.com:spinyfin/mono.git:main:docs/design.md";

    /// A `question` comment on a doc owned by an investigation task, plus the
    /// bound `answer_agent` execution — the shape whose `work_item_id` is a
    /// `work_comments.id` rather than a `tasks.id`. Returns the execution id.
    fn answer_agent_execution(db: &WorkDb, product_id: &str) -> String {
        let investigation = db
            .create_investigation(
                boss_protocol::CreateInvestigationInput::builder()
                    .product_id(product_id.to_owned())
                    .name("Investigate the thing")
                    .build(),
            )
            .unwrap();
        db.set_task_doc_pointer(&investigation.id, Some(DOC_REPO), Some("main"), Some("docs/design.md"))
            .unwrap();
        let comment = db
            .create_comment(CreateCommentInput {
                artifact_kind: "pr_doc".to_owned(),
                artifact_id: DOC_ARTIFACT.to_owned(),
                doc_version: "v1".to_owned(),
                anchor: boss_protocol::CommentAnchor {
                    exact: "the quoted text".to_owned(),
                    prefix: String::new(),
                    suffix: String::new(),
                },
                body: "why does this retry three times?".to_owned(),
                author: "operator".to_owned(),
                plain_text_projection_version: 0,
            })
            .unwrap();
        db.create_answer_agent_execution(&comment.id, DOC_REPO).unwrap().id
    }

    /// Regression: an `answer_agent` execution binds to a comment, so the
    /// `tasks` join finds nothing. Before this resolved, the events socket
    /// rejected EVERY hook connection from an answer-agent worker with
    /// `UnresolvedDriver` — including the turn boundary `finalize_answer_agent`
    /// rides on, so the execution never left `waiting_human` and held its
    /// worker slot until a human stopped the pane.
    #[test]
    fn answer_agent_execution_resolves_its_doc_owner_products_driver() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        db.update_work_item(
            &product.id,
            WorkItemPatch {
                default_driver: Some("codex".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let execution_id = answer_agent_execution(&db, &product.id);

        assert_eq!(
            db.get_execution_driver_slug(&execution_id).unwrap(),
            Some("codex".to_owned()),
        );
    }

    #[test]
    fn answer_agent_execution_with_no_product_override_resolves_the_engine_default() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let execution_id = answer_agent_execution(&db, &product.id);

        assert_eq!(
            db.get_execution_driver_slug(&execution_id).unwrap(),
            Some(boss_engine_effort::ENGINE_DEFAULT_DRIVER.to_owned()),
        );
    }

    /// A comment that vanished mid-flight must still resolve a driver: a
    /// `None` here would drop the worker's hook events, which is the exact
    /// failure this whole path exists to prevent.
    #[test]
    fn answer_agent_execution_whose_comment_is_gone_still_resolves_the_engine_default() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let execution_id = answer_agent_execution(&db, &product.id);
        let comment_id = db.get_execution(&execution_id).unwrap().work_item_id;
        let conn = db.connect().unwrap();
        conn.execute("DELETE FROM work_comments WHERE id = ?1", [&comment_id])
            .unwrap();
        drop(conn);

        assert_eq!(
            db.get_execution_driver_slug(&execution_id).unwrap(),
            Some(boss_engine_effort::ENGINE_DEFAULT_DRIVER.to_owned()),
        );
    }

    #[test]
    fn unknown_execution_resolves_to_none() {
        let (_dir, db) = open_db();
        assert_eq!(db.get_execution_driver_slug("exec_missing").unwrap(), None);
    }

    #[test]
    fn falls_back_to_product_default_driver_when_task_has_none() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        db.update_work_item(
            &product.id,
            WorkItemPatch {
                default_driver: Some("codex".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);

        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap(),
            Some("codex".to_owned()),
        );
    }

    #[test]
    fn task_driver_override_wins_over_product_default() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        db.update_work_item(
            &product.id,
            WorkItemPatch {
                default_driver: Some("codex".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let chore = create_test_chore(&db, &product.id, "test chore");
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                driver: Some("copilot".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let execution = create_ready_chore_execution(&db, &chore.id);

        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap(),
            Some("copilot".to_owned()),
        );
    }

    #[test]
    fn task_with_missing_product_row_still_resolves_via_task_driver_or_engine_default() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);

        // Drop the parent product behind FK enforcement, simulating the
        // data-integrity problem of a task whose product_id row no longer
        // exists. The LEFT JOIN must still resolve a driver rather than
        // silently dropping the dispatch (see driver_lookup module docs).
        let conn = db.connect().unwrap();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute("DELETE FROM products WHERE id = ?1", [&product.id])
            .unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        drop(conn);

        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap(),
            Some(boss_engine_effort::ENGINE_DEFAULT_DRIVER.to_owned()),
        );
    }

    #[test]
    fn no_driver_set_anywhere_falls_back_to_engine_default() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);

        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap(),
            Some(boss_engine_effort::ENGINE_DEFAULT_DRIVER.to_owned()),
        );
    }

    /// A `pr_review` execution's `work_item_id` is the producing task, so the
    /// tasks join matches and used to return that row's live pin — selecting
    /// the wrong normaliser for the Claude reviewer worker. Pool-bound kinds
    /// must short-circuit to the pool driver before any row pin wins.
    #[test]
    fn pr_review_execution_resolves_the_pool_driver_not_the_reviewed_rows_pin() {
        use boss_protocol::{CreateExecutionInput, DRIVER_SLUG_CODEX, ExecutionKind, ExecutionStatus};

        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "reviewed chore");
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                driver: Some(DRIVER_SLUG_CODEX.to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();

        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
            Some("claude"),
            "pr_review must resolve the review-pool driver, not the producing row's codex pin",
        );
    }
}
