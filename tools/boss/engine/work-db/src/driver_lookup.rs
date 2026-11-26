//! DB lookup resolving which agent driver governs a given execution.
//!
//! Two callers, and the second is why the non-task-bound path below exists:
//!
//! - the post-hoc interception dispatch on the `PostToolUse` boundary
//!   (`dispatch_post_hoc_interception_on_post_tool_use`), and
//! - `boss_engine::events_socket::handle_connection`, which resolves the driver
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
    reasoning_raw: Option<String>,
    product_default_driver: Option<String>,
}

/// In-memory outcome after the batch (or single) lookups have been fetched.
enum ResolvedDriverSlug {
    Slug(Option<String>),
    AnswerAgent { work_item_id: String },
}

fn claimed_worker_pool_driver(claimed_worker_id: Option<&str>) -> Option<String> {
    claimed_worker_id
        .and_then(boss_engine_storage_values::pool_policy::pool_dispatch_policy_for_worker_id)
        .map(|policy| policy.driver.to_owned())
}

/// Task-bound half of [`WorkDb::get_execution_driver_slug`]: pool short-circuit,
/// live pin, recorded allocation, then the ordinary default chain.
fn resolve_task_bound_driver(
    execution_id: &str,
    claimed_worker_id: Option<&str>,
    row: &ExecutionDriverRow,
    allocation: Option<&DriverDecision>,
) -> Result<String> {
    let exec_kind: ExecutionKind = row.exec_kind_raw.parse().map_err(|err| {
        anyhow::anyhow!(
            "execution {execution_id} has unrecognised kind {:?}: {err}",
            row.exec_kind_raw
        )
    })?;
    // Before a worker slot is claimed, pool-bound kinds resolve to their home
    // pool. Once a slot exists, the caller used its actual policy instead: a
    // spill into an ordinary worker must honour the row's own driver.
    if claimed_worker_id.is_none()
        && let Some(pool_driver) =
            boss_engine_storage_values::pool_policy::pool_driver_slug_for_execution_kind(&exec_kind)
    {
        return Ok(pool_driver.to_owned());
    }

    // Precedence: a LIVE explicit pin (`tasks.driver`, then
    // `products.default_driver`) that clears the capability gate for this
    // execution → the recorded traffic-allocation decision → the ordinary
    // default resolution.
    //
    // The pins are read fresh rather than taken from the recorded decision on
    // purpose. An operator can pin a driver on a row after its execution row
    // already exists but before it dispatches, and that must still win when
    // the gate accepts it — "an explicit driver on the row always wins" is
    // about the row, not about the instant the execution was created.
    // Allocation is revalidated at claim and launch composition (see
    // `work::driver_allocation`). Hook reads use that saved decision so split
    // changes do not reroute a running worker.
    //
    // A pin the gate refuses for this execution kind is dropped here exactly
    // as at spawn: honouring it would point the events-socket normaliser at a
    // driver the worker never ran.
    //
    // Design/Investigation tier rows are NOT exempted from pin honouring here:
    // `decide_execution_driver` and `resolve_spawn_config_in` both honour a
    // live `tasks.driver` / `products.default_driver` pin for tier rows too
    // (subject to the same capability gate, plus — for a product default — a
    // dedicated tier model, mirrored below via
    // `driver_has_design_investigation_model`), so suppressing the pin here
    // would point the events-socket normaliser at a driver the worker never
    // ran on.
    let task_kind: Option<TaskKind> = row.task_kind_raw.parse().ok();
    let reasoning = row
        .reasoning_raw
        .as_deref()
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse().ok());
    let design_or_investigation_tier =
        boss_engine_effort::is_design_or_investigation_tier(task_kind.as_ref(), reasoning);
    let task_pin = row.task_driver.as_deref().filter(|s| !s.trim().is_empty());
    let product_pin = row.product_default_driver.as_deref().filter(|s| !s.trim().is_empty());
    let pinned = task_pin.or(product_pin);
    if let Some(pinned) = pinned {
        let from_task_pin = task_pin.is_some();
        let honourable = pin_is_honourable(
            pinned,
            task_kind.as_ref(),
            &exec_kind,
            design_or_investigation_tier,
            from_task_pin,
        );
        if honourable {
            return Ok(pinned.to_owned());
        }
    }
    // Only an `allocation` decision routes. An `explicit` one is an audit
    // record of a pin that the branch above reads live, so honouring it here
    // would resurrect a pin the operator removed.
    if let Some(allocated) = allocation
        .filter(|d| matches!(d.reason, REASON_ALLOCATION | REASON_LEGACY_PERCENTAGE))
        .and_then(|d| d.driver.clone())
    {
        // Compatibility for workers launched before allocation revalidation:
        // the old spawn resolver substituted the default for an allocated
        // driver without a tier model. New launches persist an eligible
        // allocation first.
        if design_or_investigation_tier && !driver_has_design_investigation_model(&allocated) {
            return Ok(boss_engine_effort::ENGINE_DEFAULT_DRIVER.to_owned());
        }
        return Ok(allocated);
    }
    // No honourable pin and no allocation (an ineligible row, a pin the gate
    // refused, or an execution created before this feature landed) — resolves
    // through the ordinary default chain. When a pin was dropped above it is
    // deliberately not re-fed into `resolve_driver`, which would re-honour it.
    let honourable_task = row.task_driver.as_deref().filter(|s| {
        !s.trim().is_empty()
            && task_kind
                .as_ref()
                .is_none_or(|tk| driver_clears_dispatch_gate(s, tk, &exec_kind))
    });
    let honourable_product = row.product_default_driver.as_deref().filter(|s| {
        !s.trim().is_empty()
            && task_kind
                .as_ref()
                .is_none_or(|tk| driver_clears_dispatch_gate(s, tk, &exec_kind))
    });
    Ok(boss_engine_effort::resolve_driver(honourable_task, honourable_product))
}

fn resolve_taskless_driver(
    claimed_worker_id: Option<&str>,
    kind: ExecutionKind,
    work_item_id: String,
    launched_driver: Option<String>,
) -> ResolvedDriverSlug {
    match kind {
        ExecutionKind::AnswerAgent => ResolvedDriverSlug::AnswerAgent { work_item_id },
        // Fixed driver pin, never resolved per-comment/per-product like
        // `AnswerAgent` above: a `pr_review_guide` execution always
        // dispatches on `codex` (the enforced Astra profile) or does not
        // dispatch at all — see `resolve_review_guide_spawn_config`.
        ExecutionKind::PrReviewGuide => ResolvedDriverSlug::Slug(Some("codex".to_owned())),
        // A triage execution has no task row to supply a pin/allocation
        // ladder. If it spilled into an ordinary worker, its frozen launch
        // configuration is the only truthful driver source; do not reassert
        // the automation pool's driver from provenance.
        ExecutionKind::AutomationTriage if claimed_worker_id.is_some() => ResolvedDriverSlug::Slug(launched_driver),
        // `automation_triage` binds to an `automations.id`, so the tasks join
        // never matches. Its driver is still the automation pool's fixed pin
        // — return that rather than `None`, which would drop every hook event
        // the triage worker sends.
        kind if claimed_worker_id.is_none()
            && boss_engine_storage_values::pool_policy::kind_always_dispatches_on_pool_driver(&kind) =>
        {
            ResolvedDriverSlug::Slug(
                boss_engine_storage_values::pool_policy::pool_driver_slug_for_execution_kind(&kind).map(str::to_owned),
            )
        }
        _ => ResolvedDriverSlug::Slug(None),
    }
}

fn driver_decision_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DriverDecision> {
    let reason_raw: String = row.get(2)?;
    let reason = match reason_raw.as_str() {
        REASON_EXPLICIT => REASON_EXPLICIT,
        REASON_ALLOCATION => REASON_ALLOCATION,
        REASON_POOL => REASON_POOL,
        REASON_LEGACY_PERCENTAGE => REASON_LEGACY_PERCENTAGE,
        REASON_ALLOCATION_FAILED => REASON_ALLOCATION_FAILED,
        _ => REASON_DEFAULT,
    };
    let split_raw: Option<String> = row.get(3)?;
    Ok(DriverDecision {
        model: row.get(4)?,
        driver: row.get(1)?,
        reason,
        split_at_decision: split_raw.and_then(|raw| serde_json::from_str(&raw).ok()),
    })
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Resolve the driver slug for `execution_id`'s worker, applying the
    /// same precedence used at spawn time. A claimed worker slot is the
    /// authoritative pool-dispatch input; before a slot exists this mirrors
    /// [`crate::driver_allocation::decide_execution_driver`]'s durable
    /// row decision and home-pool handling:
    ///
    /// 1. A review-batch member execution resolves to its member row's
    ///    `requested_driver`. The batch's durable member policy — not the
    ///    review pool's fixed driver — is what `compose_worker_spawn`
    ///    launches a leaf on (`resolve_batch_reviewer_spawn`), so the
    ///    `codex` and `grok` leaves of a fan-out batch run on a `review-N`
    ///    slot whose pool policy says `claude`. Resolving them through the
    ///    pool policy pointed the events-socket normaliser at the wrong
    ///    dialect: every hook event a Grok leaf sent was rejected with
    ///    `MissingField("session_id")` (its payload carries `sessionId`),
    ///    the leaf never produced a decoded `Stop`, and its pane ran on
    ///    after the review was delivered. The member row is written in the
    ///    same transaction as the leaf's execution row, so it is present
    ///    before any hook can arrive.
    /// 2. A claimed review/automation worker slot resolves to that pool's
    ///    fixed driver via [`boss_engine_storage_values::pool_policy::pool_dispatch_policy_for_worker_id`].
    ///    A claimed ordinary `worker-N` slot deliberately has no pool policy,
    ///    including work that spilled from automation, and therefore falls
    ///    through to its own driver resolution.
    /// 3. Before a worker slot is claimed, `pr_review` and
    ///    `automation_triage` resolve to their home pool's fixed driver.
    /// 4. A live pin (`tasks.driver`, then `products.default_driver`) that
    ///    clears the capability gate for this execution kind.
    /// 5. A recorded traffic-allocation decision.
    /// 6. [`boss_engine_effort::ENGINE_DEFAULT_DRIVER`] via
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
        // Each `WorkDb` call below takes and releases the (non-reentrant)
        // pooled connection on its own; none may run while another holds it.
        if let Some(member) = self.review_batch_member_for_execution(execution_id)? {
            return Ok(Some(member.requested_driver));
        }
        let claimed_worker_id = self.latest_run_agent_id_for_execution(execution_id)?;
        if let Some(pool_driver) = claimed_worker_pool_driver(claimed_worker_id.as_deref()) {
            return Ok(Some(pool_driver));
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
                    "SELECT e.kind, t.kind, t.driver, t.reasoning, p.default_driver
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
                            reasoning_raw: row.get(3)?,
                            product_default_driver: row.get(4)?,
                        })
                    },
                )
                .optional()?;
            if let Some(row) = row {
                let allocation = get_execution_driver_decision_conn(&conn, execution_id)?;
                return Ok(Some(resolve_task_bound_driver(
                    execution_id,
                    claimed_worker_id.as_deref(),
                    &row,
                    allocation.as_ref(),
                )?));
            }

            // No `tasks` row. Either the execution does not exist at all, or it
            // is one of the kinds that binds to something other than a task.
            query_execution(&conn, execution_id)?
                .map(|execution| (execution.kind, execution.work_item_id, execution.driver))
        };
        let Some((kind, work_item_id, launched_driver)) = binding else {
            return Ok(None);
        };
        match resolve_taskless_driver(claimed_worker_id.as_deref(), kind, work_item_id, launched_driver) {
            ResolvedDriverSlug::Slug(slug) => Ok(slug),
            ResolvedDriverSlug::AnswerAgent { work_item_id } => Ok(Some(self.answer_agent_driver_slug(&work_item_id))),
        }
    }

    /// Resolve drivers for many executions in a handful of queries rather than
    /// one [`Self::get_execution_driver_slug`] round-trip each. Same precedence
    /// as the single-id path. An unrecognised `kind` on one row is logged and
    /// recorded as `None` so a single bad execution cannot fail a usage window.
    pub(crate) fn get_execution_driver_slugs_lenient(
        &self,
        execution_ids: &[&str],
    ) -> Result<HashMap<String, Option<String>>> {
        let mut unique: Vec<&str> = execution_ids.iter().copied().filter(|id| !id.is_empty()).collect();
        unique.sort_unstable();
        unique.dedup();
        if unique.is_empty() {
            return Ok(HashMap::new());
        }
        let id_json = query_ensure::id_array(&unique);

        let mut review_drivers: HashMap<String, String> = HashMap::new();
        let mut claimed_agents: HashMap<String, Option<String>> = HashMap::new();
        let mut task_bound: HashMap<String, ExecutionDriverRow> = HashMap::new();
        let mut taskless: HashMap<String, (String, String, Option<String>)> = HashMap::new();
        let mut decisions: HashMap<String, DriverDecision> = HashMap::new();

        {
            let conn = self.connect()?;
            let mut stmt = conn.prepare(
                "SELECT execution_id, requested_driver
                 FROM pr_review_batch_members
                 WHERE execution_id IN (SELECT value FROM json_each(?1))",
            )?;
            let rows = stmt.query_map([&id_json], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, driver) = row?;
                review_drivers.insert(id, driver);
            }

            let mut stmt = conn.prepare(
                "SELECT execution_id, agent_id FROM (
                     SELECT execution_id, agent_id,
                            ROW_NUMBER() OVER (
                              PARTITION BY execution_id
                              ORDER BY
                                CASE WHEN finished_at IS NULL THEN 0 ELSE 1 END,
                                CASE WHEN status = 'failed' THEN 1 ELSE 0 END,
                                CASE WHEN transcript_path IS NOT NULL THEN 0 ELSE 1 END,
                                created_at DESC,
                                id DESC
                            ) AS rn
                     FROM work_runs
                     WHERE execution_id IN (SELECT value FROM json_each(?1))
                 )
                 WHERE rn = 1",
            )?;
            let rows = stmt.query_map([&id_json], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            for row in rows {
                let (id, agent_id) = row?;
                claimed_agents.insert(id, agent_id);
            }

            let mut stmt = conn.prepare(
                "SELECT e.id, e.kind, t.kind, t.driver, t.reasoning, p.default_driver,
                        e.work_item_id, e.driver
                   FROM work_executions e
                   LEFT JOIN tasks t ON t.id = e.work_item_id
                   LEFT JOIN products p ON p.id = t.product_id
                  WHERE e.id IN (SELECT value FROM json_each(?1))",
            )?;
            let rows = stmt.query_map([&id_json], |row| {
                let id: String = row.get(0)?;
                let exec_kind_raw: String = row.get(1)?;
                let task_kind_raw: Option<String> = row.get(2)?;
                let task_driver: Option<String> = row.get(3)?;
                let reasoning_raw: Option<String> = row.get(4)?;
                let product_default_driver: Option<String> = row.get(5)?;
                let work_item_id: String = row.get(6)?;
                let launched_driver: Option<String> = row.get(7)?;
                Ok((
                    id,
                    exec_kind_raw,
                    task_kind_raw,
                    task_driver,
                    reasoning_raw,
                    product_default_driver,
                    work_item_id,
                    launched_driver,
                ))
            })?;
            for row in rows {
                let (
                    id,
                    exec_kind_raw,
                    task_kind_raw,
                    task_driver,
                    reasoning_raw,
                    product_default_driver,
                    work_item_id,
                    launched_driver,
                ) = row?;
                if let Some(task_kind_raw) = task_kind_raw {
                    task_bound.insert(
                        id,
                        ExecutionDriverRow {
                            exec_kind_raw,
                            task_kind_raw,
                            task_driver,
                            reasoning_raw,
                            product_default_driver,
                        },
                    );
                } else {
                    taskless.insert(id, (exec_kind_raw, work_item_id, launched_driver));
                }
            }

            let mut stmt = conn.prepare(
                "SELECT execution_id, driver, reason, split_at_decision, model
                   FROM execution_driver_decisions
                  WHERE execution_id IN (SELECT value FROM json_each(?1))",
            )?;
            let rows = stmt.query_map([&id_json], |row| {
                let id: String = row.get(0)?;
                Ok((id, driver_decision_from_row(row)?))
            })?;
            for row in rows {
                let (id, decision) = row?;
                decisions.insert(id, decision);
            }
        }

        let mut out: HashMap<String, Option<String>> = HashMap::with_capacity(unique.len());
        let mut answer_agents: Vec<(String, String)> = Vec::new();
        for id in unique {
            if let Some(driver) = review_drivers.get(id) {
                out.insert(id.to_owned(), Some(driver.clone()));
                continue;
            }
            let claimed = claimed_agents.get(id).and_then(|v| v.as_deref());
            if let Some(pool_driver) = claimed_worker_pool_driver(claimed) {
                out.insert(id.to_owned(), Some(pool_driver));
                continue;
            }
            if let Some(row) = task_bound.get(id) {
                match resolve_task_bound_driver(id, claimed, row, decisions.get(id)) {
                    Ok(slug) => {
                        out.insert(id.to_owned(), Some(slug));
                    }
                    Err(err) => {
                        tracing::warn!(
                            execution_id = %id,
                            error = %format!("{err:#}"),
                            "failed to resolve driver for usage report execution; leaving unknown"
                        );
                        out.insert(id.to_owned(), None);
                    }
                }
                continue;
            }
            let Some((kind_raw, work_item_id, launched_driver)) = taskless.remove(id) else {
                out.insert(id.to_owned(), None);
                continue;
            };
            let kind: ExecutionKind = match kind_raw.parse() {
                Ok(kind) => kind,
                Err(err) => {
                    tracing::warn!(
                        execution_id = %id,
                        error = %format!(
                            "execution {id} has unrecognised kind {kind_raw:?}: {err}"
                        ),
                        "failed to resolve driver for usage report execution; leaving unknown"
                    );
                    out.insert(id.to_owned(), None);
                    continue;
                }
            };
            match resolve_taskless_driver(claimed, kind, work_item_id, launched_driver) {
                ResolvedDriverSlug::Slug(slug) => {
                    out.insert(id.to_owned(), slug);
                }
                ResolvedDriverSlug::AnswerAgent { work_item_id } => {
                    answer_agents.push((id.to_owned(), work_item_id));
                }
            }
        }
        for (id, comment_id) in answer_agents {
            out.insert(id, Some(self.answer_agent_driver_slug(&comment_id)));
        }
        Ok(out)
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
        let target = self
            .resolve_feedback_target(&comment.artifact_kind, &comment.artifact_id)
            .ok()
            .flatten()?;
        let owner = self.get_work_item(target.owner_task_id()).ok()?;
        self.get_product(owner.product_id()).ok().flatten()?.default_driver
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};
    use crate::{CreateCommentInput, WorkDb, WorkItemPatch};

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

    /// A Design row with a live `tasks.driver = codex` pin must resolve to
    /// `codex` at lookup, matching what `resolve_spawn_config_in` actually
    /// spawns the worker on: `codex`'s `ModelMenu::design_investigation_model`
    /// is `Some("gpt-6-astra")`, so the pin clears both the capability gate
    /// and the tier model requirement. Before this fixed, the tier gating
    /// suppressed the pin here and fell through to the engine default
    /// (`claude`) — pointing the events-socket normaliser at a driver the
    /// worker never ran on.
    #[test]
    fn design_row_with_codex_task_pin_resolves_codex_matching_spawn() {
        use boss_protocol::{DRIVER_SLUG_CODEX, TaskKind};

        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "design with codex pin");
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                driver: Some(DRIVER_SLUG_CODEX.to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET kind = ?1 WHERE id = ?2",
                rusqlite::params![TaskKind::Design.as_str(), &chore.id],
            )
            .unwrap();
        }
        let execution = create_ready_chore_execution(&db, &chore.id);
        let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
        assert_eq!(
            decision.driver.as_deref(),
            Some(DRIVER_SLUG_CODEX),
            "sanity: the row must actually record an explicit codex pin",
        );

        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
            Some(DRIVER_SLUG_CODEX),
            "live lookup must match the pin the worker actually spawned on",
        );
    }

    /// Legacy workers may retain a Grok decision even though the old spawn
    /// resolver substituted the default for their design/investigation tier.
    /// Preserve hook routing for those workers; new launches revalidate first.
    #[test]
    fn tier_row_with_recorded_grok_allocation_resolves_engine_default() {
        use boss_protocol::{DRIVER_SLUG_GROK, DriverTrafficSplit};

        let (_dir, db) = open_db();
        db.set_driver_traffic_split(DriverTrafficSplit::new(100, 0, 0, 0))
            .unwrap();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "later became a tier row");
        let execution = create_ready_chore_execution(&db, &chore.id);
        let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
        assert_eq!(
            decision.driver.as_deref(),
            Some(DRIVER_SLUG_GROK),
            "sanity: the ordinary row must have allocated to grok under a grok-only split",
        );

        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                reasoning: Some("investigation".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap(),
            Some(boss_engine_effort::ENGINE_DEFAULT_DRIVER.to_owned()),
            "a tier row's frozen grok allocation must resolve the engine default, not grok verbatim",
        );
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

    /// Build a pre-merge fan-out batch and return its two leaf executions
    /// paired with the driver each member row requested (claude, codex —
    /// `leaf_member_inputs`' fixed role policy).
    fn fan_out_batch_leaves(db: &WorkDb, cycle_root_id: &str) -> Vec<(boss_protocol::WorkExecution, String)> {
        use crate::{ReviewBatchCreateInput, ReviewBatchDispatch};
        use boss_protocol::{ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket, ReviewProfile};

        let classification = ReviewClassification::builder()
            .changed_files(vec!["tools/boss/engine/core/src/work/driver_lookup.rs".to_owned()])
            .complexity_flags(vec![])
            .has_production_code(true)
            .metadata_missing(vec![])
            .production_languages(vec![ReviewLanguageBucket::Rust])
            .profile(ReviewProfile::Standard)
            .subsystem_buckets(vec!["tools/boss/engine".to_owned()])
            .additions(10)
            .deletions(2)
            .build();
        let input = ReviewBatchCreateInput::builder()
            .cycle_root_id(cycle_root_id.to_owned())
            .base_sha("base-sha")
            .classification(classification)
            .phase(ReviewBatchPhase::PreMerge)
            .pr_number(42)
            .pr_url("https://github.com/example/repo/pull/42")
            .target_sha("head-sha")
            .build();
        let executions = match db
            .create_pre_merge_review_batch(input, "https://github.com/example/repo")
            .unwrap()
        {
            ReviewBatchDispatch::Created { executions, .. } => executions,
            other => panic!("expected a newly-created review batch, got {other:?}"),
        };
        executions
            .into_iter()
            .map(|execution| {
                let member = db
                    .review_batch_member_for_execution(&execution.id)
                    .unwrap()
                    .expect("every batch leaf has a member row");
                (execution, member.requested_driver)
            })
            .collect()
    }

    /// Regression for the grok review-batch leaves that emitted no decoded
    /// hook events: a fan-out batch leaf is a `pr_review` execution that
    /// spawns on the driver its member row requested, but the lookup
    /// resolved it through the review pool's fixed driver instead — before a
    /// slot was claimed via the `pr_review` home-pool rule, and after via the
    /// `review-N` slot's pool policy. Both paths said `claude` for the
    /// `codex` and `grok` leaves, so every Grok leaf hook payload was
    /// normalised as Claude and dropped with `MissingField("session_id")`.
    /// The member row must win on both paths, for every leaf.
    #[test]
    fn review_batch_leaf_resolves_its_member_rows_driver_not_the_review_pools() {
        use boss_protocol::CreateRunInput;

        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let cycle_root = crate::test_support::create_test_chore_manual(&db, &product.id, "review target");
        let leaves = fan_out_batch_leaves(&db, &cycle_root.id);
        assert_eq!(
            leaves.iter().map(|(_, driver)| driver.as_str()).collect::<Vec<_>>(),
            vec!["claude", "codex"],
        );

        for (execution, requested_driver) in &leaves {
            assert_eq!(
                db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
                Some(requested_driver.as_str()),
                "before a slot is claimed, leaf {} must resolve its member row's driver",
                execution.id,
            );
        }

        // Every leaf runs on a review-pool slot, whose pool policy is the
        // fixed reviewer driver — exactly the input that used to win.
        for (index, (execution, requested_driver)) in leaves.iter().enumerate() {
            let run = db
                .create_run(CreateRunInput {
                    execution_id: execution.id.clone(),
                    agent_id: "review-pending".to_owned(),
                    status: Some("active".to_owned()),
                    error_text: None,
                    result_summary: None,
                    transcript_path: None,
                    artifacts_path: None,
                    started_at: Some("100".to_owned()),
                    finished_at: None,
                })
                .unwrap();
            db.set_run_agent_id(&run.id, &format!("review-{}", index + 1)).unwrap();
            assert_eq!(
                db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
                Some(requested_driver.as_str()),
                "on a claimed review-pool slot, leaf {} must resolve its member row's driver",
                execution.id,
            );
        }
    }

    /// The member-row rule is scoped to batch members: a legacy memberless
    /// `pr_review` execution on a review-pool slot still resolves the pool
    /// driver (the case `pr_review_execution_resolves_the_pool_driver_not_the_reviewed_rows_pin`
    /// pins before a slot exists).
    #[test]
    fn memberless_pr_review_on_a_review_slot_still_resolves_the_pool_driver() {
        use boss_protocol::{CreateExecutionInput, CreateRunInput, DRIVER_SLUG_CODEX, ExecutionKind, ExecutionStatus};

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
        let run = db
            .create_run(CreateRunInput {
                execution_id: execution.id.clone(),
                agent_id: "review-pending".to_owned(),
                status: Some("active".to_owned()),
                error_text: None,
                result_summary: None,
                transcript_path: None,
                artifacts_path: None,
                started_at: Some("100".to_owned()),
                finished_at: None,
            })
            .unwrap();
        db.set_run_agent_id(&run.id, "review-1").unwrap();

        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
            Some("claude"),
        );
    }

    #[test]
    fn batched_lookup_matches_single_id_lookup_for_mixed_executions() {
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
        let chore = create_test_chore(&db, &product.id, "pinned chore");
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                driver: Some("copilot".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let pinned = create_ready_chore_execution(&db, &chore.id);
        let defaulted_chore = create_test_chore(&db, &product.id, "product default chore");
        let defaulted = create_ready_chore_execution(&db, &defaulted_chore.id);
        let answer = answer_agent_execution(&db, &product.id);
        let missing = "exec_missing";

        let ids = [pinned.id.as_str(), defaulted.id.as_str(), answer.as_str(), missing];
        let batch = db.get_execution_driver_slugs_lenient(&ids).unwrap();
        for id in ids {
            let single = db.get_execution_driver_slug(id).unwrap();
            assert_eq!(
                batch.get(id).cloned().flatten(),
                single,
                "batch and single lookup must agree for {id}"
            );
        }
    }
}
