//! The dispatch scheduler loop: kick/heartbeat wakeups, the ready-queue
//! drain, chain-serialization holds, and automation preemption. Part of the
//! `coordinator` module split; see [`super`] for the struct and shared types.
use super::*;

impl ExecutionCoordinator {
    pub fn kick(self: &Arc<Self>) {
        // Order matters: `scheduling_pending` must be written BEFORE we
        // contend on `scheduling_active`. If we lose the swap race
        // (another scheduler is already running) the alive scheduler
        // will read `scheduling_pending` after it drains and notice
        // the wakeup; if we win, the fresh scheduler will reset
        // pending on its way into the drain loop.
        self.scheduling_pending.store(true, Ordering::Release);
        if self.scheduling_active.swap(true, Ordering::AcqRel) {
            tracing::debug!(
                "scheduler_kick outcome=noop reason=already_running — wakeup latched via scheduling_pending"
            );
            return;
        }
        tracing::debug!("scheduler_kick outcome=spawn — starting new run_scheduler task");
        let coordinator = self.clone();
        tokio::spawn(async move {
            coordinator.run_scheduler().await;
        });
    }

    /// Spawn a background task that periodically wakes the scheduler and
    /// surfaces a warning when a `ready` execution has been sitting in
    /// the queue for longer than one heartbeat interval.
    ///
    /// Rationale. The dispatch happy path is: kanban drag → insert
    /// `ready` execution → [`kick`] → `run_scheduler` picks the row up
    /// and emits `request_recorded` within milliseconds. PR #345 closed
    /// the canonical kick/drain TOCTOU by latching every kick into
    /// [`scheduling_pending`], but a `ready` row that stalls at
    /// `status_transition` (no follow-up `request_recorded`) was seen
    /// in the wild — see `exec_18af3ba5259d32a8_12` (2026-05-13), which
    /// sat for 131s before the 90s-age orphan-active reconciler
    /// (PR #429) abandoned it and inserted a fresh redispatch.
    ///
    /// The heartbeat is a second line of defence, not a replacement for
    /// either mechanism:
    ///
    /// * It calls [`kick`] regardless of the in-memory active flag, so
    ///   any kick that was lost to a race the existing latching can't
    ///   cover is re-issued within one interval. The scheduler still
    ///   serializes drains through `scheduling_active`, so two
    ///   schedulers can never run concurrently.
    /// * When the heartbeat actually observes a stranded `ready` row
    ///   (anything older than the interval), it logs a `warn!` line
    ///   carrying the execution id so an operator sees the failure on
    ///   the first occurrence instead of waiting for the orphan
    ///   reconciler. "Fail loudly" was an explicit constraint of the
    ///   reporting work item.
    /// * PR #429's orphan-active reconciler stays intact: that path
    ///   handles the harder case where the execution row itself is
    ///   stale (worker dead, row claimed but not `ready`), which this
    ///   heartbeat does NOT address.
    pub fn spawn_scheduler_heartbeat(self: &Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        let coordinator = self.clone();
        tokio::spawn(async move {
            // Stagger startup so the first beat doesn't race the
            // engine's own boot-time `kick()` (see `app.rs`).
            tokio::time::sleep(interval).await;
            let interval_ms = interval.as_millis() as u64;
            loop {
                let stranded = coordinator.stranded_ready_executions(interval_ms);
                if !stranded.is_empty() {
                    tracing::warn!(
                        count = stranded.len(),
                        oldest_age_ms = stranded
                            .iter()
                            .map(|(_, age_ms)| *age_ms)
                            .max()
                            .unwrap_or(0),
                        execution_ids = ?stranded
                            .iter()
                            .map(|(id, _)| id.as_str())
                            .collect::<Vec<_>>(),
                        "scheduler heartbeat: ready execution(s) older than \
                         the heartbeat interval found — kick/drain handoff \
                         may have dropped a wakeup; re-kicking now",
                    );
                }
                coordinator.kick();
                tokio::time::sleep(interval).await;
            }
        })
    }

    /// Return every `ready` execution whose `created_at` is older than
    /// `min_age_ms` milliseconds ago, paired with its age in
    /// milliseconds. Used by [`spawn_scheduler_heartbeat`] to surface
    /// stranded rows; kept as a separate method so the heartbeat path
    /// is testable without involving any timers.
    pub(super) fn stranded_ready_executions(&self, min_age_ms: u64) -> Vec<(String, u64)> {
        let ready = match self.work_db.list_ready_executions() {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "scheduler heartbeat: failed to list ready executions; skipping pass",
                );
                return Vec::new();
            }
        };
        let now_secs = boss_engine_utils::epoch_time::now_epoch_secs() as u64;
        let cutoff_ms = min_age_ms;
        ready
            .into_iter()
            .filter_map(|exec| {
                let created_at_secs: u64 = exec.created_at.parse().ok()?;
                let age_ms = now_secs.saturating_sub(created_at_secs).saturating_mul(1000);
                if age_ms < cutoff_ms {
                    return None;
                }
                // A `ready` row that the per-PR single-writer guard is
                // deliberately holding behind a live chain sibling is NOT
                // stranded — it is correctly queued and will dispatch when
                // the sibling reaps. Excluding it keeps the heartbeat's
                // "kick/drain handoff may have dropped a wakeup" warning
                // honest (it would otherwise fire every interval for the
                // entire lifetime of the live sibling). Fail open: if the
                // chain query errors, treat the row as stranded as before.
                if matches!(
                    self.work_db.live_execution_elsewhere_in_chain(&exec.work_item_id),
                    Ok(Some(_))
                ) {
                    return None;
                }
                Some((exec.id, age_ms))
            })
            .collect()
    }

    /// Skip-the-queue dispatch for `bossctl agents launch`. Looks the
    /// execution up directly, claims a worker via
    /// `WorkerPool::claim_worker_force` (which grows the pool by one
    /// slot up to the hard cap when every configured slot is busy),
    /// and runs the same `schedule_execution` path the auto-dispatcher
    /// uses. Returns the worker id we landed on so callers can echo it
    /// back to the human.
    ///
    /// Errors when the execution is not in `ready` (already claimed by
    /// the auto-dispatcher in a race, terminal, or unknown), or when
    /// the worker pool is already at the hard cap with no idle slot.
    pub async fn force_dispatch(self: &Arc<Self>, execution_id: &str) -> Result<String> {
        let execution = self
            .work_db
            .get_execution(execution_id)
            .with_context(|| format!("failed to look up execution {execution_id}"))?;
        if execution.status != ExecutionStatus::Ready {
            return Err(anyhow!(
                "execution {execution_id} is in status {status:?}, not ready — cannot force-dispatch",
                status = execution.status,
            ));
        }
        let preferred_workspace_id = execution.preferred_workspace_id.clone();
        let worker_id = self
            .worker_pool
            .claim_worker_force(&execution.id, preferred_workspace_id.as_deref())
            .await
            .ok_or_else(|| {
                anyhow!(
                    "worker pool already at hard cap ({MAX_WORKER_POOL_SIZE}); cannot \
                     force-dispatch {execution_id}"
                )
            })?;
        if let Err(err) = self.schedule_execution(&execution, &worker_id).await {
            self.worker_pool
                .release_worker(&worker_id, preferred_workspace_id.as_deref())
                .await;
            return Err(err);
        }
        Ok(worker_id)
    }

    async fn run_scheduler(self: Arc<Self>) {
        // Lossless-wakeup loop. The `scheduling_pending` flag is reset
        // at the top of each iteration so we have a clean "have we
        // seen any new kicks since this drain started?" reading at
        // the bottom. The pattern handles three race classes:
        //
        //   1. Kick during drain: caught by the post-drain
        //      `scheduling_pending.load()` and re-enters the inner
        //      loop without releasing `scheduling_active`.
        //   2. Kick after we declared no-pending but before we set
        //      `scheduling_active=false`: the kicker observed active=true
        //      and noop'd, but our second `scheduling_pending.load()`
        //      (after active=false) picks it up and we re-acquire
        //      active to resume draining.
        //   3. Kick after we set `scheduling_active=false`: the kicker
        //      spawns a fresh scheduler; we observe that via the
        //      swap returning `true` and exit cleanly.
        //
        // Without this, the original `_guard`/`break` pattern lost
        // wakeups in the narrow window between "queue empty" and
        // "guard drops" — kicks landing in that window noop'd against
        // `scheduling_active=true` and the new `ready` row sat
        // forever with no scheduler running to pick it up. That is
        // the symptom motivating this fix (see `task_18ae9d21044843b8_44`).
        loop {
            self.scheduling_pending.store(false, Ordering::Release);
            let drain_started_at = std::time::Instant::now();
            let drain_outcome = self.drain_ready_queue().await;
            crate::dispatch_metrics::record_drain_pass_duration_ms(
                &self.metrics,
                drain_started_at.elapsed().as_millis() as i64,
            );

            // Pool-exhaustion exits don't re-loop here: another
            // scheduler will spawn from the post-`release_worker`
            // `kick()`, and re-looping immediately would just hit the
            // same exhaustion. Fall through to the same active-release
            // logic — `scheduling_pending` may still have been set,
            // and respecting it lets a "fresh row arrived while we
            // were blocked on the pool" case re-attempt once a worker
            // is free without waiting for the next external event.
            let _ = drain_outcome;

            if self.scheduling_pending.load(Ordering::Acquire) {
                // A kick raced us during drain. Reset and re-drain
                // without giving up `scheduling_active`.
                continue;
            }

            // Relinquish the active flag. Any kick that lands from
            // here on will see `scheduling_active=false` on its swap
            // and spawn its own scheduler — but a kick that races
            // between this store and the post-store load below still
            // needs to be caught, hence the second check.
            self.scheduling_active.store(false, Ordering::Release);
            if !self.scheduling_pending.load(Ordering::Acquire) {
                return;
            }
            // A kick landed in the gap. Try to re-claim active; if
            // someone else (a freshly spawned scheduler) already has
            // it, they'll handle the drain.
            if self.scheduling_active.swap(true, Ordering::AcqRel) {
                return;
            }
            // We re-acquired; loop back to drain.
        }
    }

    /// Resolve `WorkDb::live_execution_elsewhere_in_chain` the way every
    /// caller actually needs it: as "is another execution on this PR/chain
    /// genuinely still alive", not "does a row exist whose `status` column
    /// says `running`/`waiting_human`".
    ///
    /// `status IN ('running', 'waiting_human')` is a *paper* liveness
    /// signal — a row can sit `waiting_human` forever after its worker died
    /// without a `Stop` hook (the 2026-06-14 incident this exact gap
    /// re-created for chain-serialization: T251 / `exec_18af40745c552070_26`,
    /// a 56-day-old `waiting_human` zombie with no live pane, wedged every
    /// subsequent execution on its PR/chain behind `chain_serialized` in a
    /// ~10s dispatcher loop until a human noticed and ran `bossctl agents
    /// reap` by hand). `schedule_execution`'s double-spawn guard already
    /// runs the sibling through the same-work-item zombie reconcilers before
    /// treating it as blocking (see the "Liveness gate" comment there); this
    /// applies the identical reconciliation to the *cross-work-item* chain
    /// sibling this guard inspects, using the same two positive-evidence
    /// checks: [`crate::lost_workspace_sweep::reconcile_if_execution_dead`]
    /// (cube workspace directory gone, pane pid dead, or pane never attached) and
    /// [`crate::dead_pane_sweep::reconcile_if_pane_dead`] (durable shell pid
    /// is `ESRCH`). Both only ever act on positive evidence of death, so
    /// this can only ever *unblock* a wrongly-serialized dispatch — it can
    /// never falsely treat a genuinely live sibling as dead.
    ///
    /// Reconciling one dead sibling can reveal a second, older live sibling
    /// earlier in the chain (a further `waiting_human` execution masked
    /// behind the one just reaped), so the check re-queries in a small
    /// bounded loop rather than returning after a single reconciliation.
    /// Returns EVERY live chain sibling of `work_item_id`, reconciling
    /// zombies along the way. `resolve_chain_hold`'s
    /// review-bypass decision must be made against the full set: the
    /// underlying `member_ids` walk is chain-root-first
    /// ([`crate::work::dispatch::WorkDb::live_executions_elsewhere_in_chain`]),
    /// so trusting only the first live sibling lets a root `pr_review` mask
    /// a live descendant *writer* — reintroducing the exact two-writer
    /// T1577/T1815 hazard this guard exists to prevent.
    async fn live_chain_siblings(&self, work_item_id: &str) -> Result<Vec<WorkExecution>> {
        const MAX_RECONCILE_ATTEMPTS: u8 = 4;
        for _ in 0..MAX_RECONCILE_ATTEMPTS {
            let siblings = self.work_db.live_executions_elsewhere_in_chain(work_item_id)?;
            if siblings.is_empty() {
                return Ok(siblings);
            }
            let mut any_reconciled = false;
            for sibling in &siblings {
                let reconciled_lost_workspace = crate::lost_workspace_sweep::reconcile_if_execution_dead(
                    self.work_db.as_ref(),
                    self.dispatch_events.as_ref(),
                    sibling,
                )
                .await;
                let reconciled_dead_pane = !reconciled_lost_workspace
                    && crate::dead_pane_sweep::reconcile_if_pane_dead(
                        self.work_db.as_ref(),
                        self.dispatch_events.as_ref(),
                        sibling,
                        boss_engine_utils::epoch_time::now_epoch_secs(),
                    )
                    .await;
                if reconciled_lost_workspace || reconciled_dead_pane {
                    any_reconciled = true;
                    tracing::warn!(
                        work_item_id,
                        reconciled_execution_id = %sibling.id,
                        reason = if reconciled_dead_pane { "pane_dead" } else { "workspace_lost" },
                        "chain-serialization guard: 'live' chain sibling's worker pane is gone; \
                         reconciled it and re-checking for still-live siblings",
                    );
                }
            }
            if !any_reconciled {
                return Ok(siblings);
            }
        }
        // Exhausted retries without converging on a stable answer (e.g. a
        // pathological chain with many zombies reconciling one per pass).
        // Fail closed: treat whatever is there now as live rather than risk
        // co-dispatching two workers onto the same shared jj backing store.
        self.work_db.live_executions_elsewhere_in_chain(work_item_id)
    }

    /// Resolve the per-PR single-writer chain check for `execution`,
    /// applying the review-yields-to-conflict-fix carve-out: a live
    /// `pr_review` sibling never blocks a merge-conflict-fix revision
    /// (`DispatchClass::MergeConflictRevision`).
    ///
    /// Rationale (the 2026-07-10 T270/T258 priority-inversion incident): a
    /// `pr_review` execution is strictly read-only — never writes, commits,
    /// or pushes (enforced by the reviewer CLAUDE.md, its tool denylist, and
    /// its prompt mandate; see `crate::pr_review` module docs) — so it
    /// cannot participate in the writer-vs-writer T1577/T1815 hazard this
    /// guard exists to prevent (two *writers* rebasing/rewriting each
    /// other's commits on the shared jj backing store). Meanwhile a pending
    /// merge-conflict fix is urgent and, once it lands, immediately
    /// invalidates whatever the in-flight review was looking at anyway —
    /// the completion path's revision-triggered-review re-fire
    /// (`enable_revision_triggered_reviews`) already spawns a fresh review
    /// pass against the new head, so nothing is lost by not waiting for the
    /// stale one to finish. Every other pairing — writer vs writer, writer
    /// vs anything else, or a *non*-conflict revision (CI-fix,
    /// review-findings, operator-filed) waiting behind a review — keeps
    /// serializing exactly as before; only this one combination bypasses.
    ///
    /// The bypass decision is made against **every** live chain sibling, not
    /// just the first one a naive single-sibling lookup would return. The chain
    /// walk is root-first, so trusting a single sibling meant a live
    /// `pr_review` on the chain root could mask a live *writer* further down
    /// the chain (a descendant conflict-fix revision) — bypassing would then
    /// co-dispatch a second writer alongside that still-live one, the exact
    /// two-writer T1577/T1815 hazard this guard exists to prevent. So the
    /// bypass only fires when EVERY live sibling is a review; if even one is
    /// a non-review (writer), this fails closed to `Blocked`.
    ///
    /// Shared by all three chain-guard call sites (`drain_ready_queue`'s
    /// pre-claim check, `schedule_execution`'s pre-lease backstop, and its
    /// post-lease TOCTOU assertion) so the bypass decision — and therefore
    /// whether a merge-conflict revision ever gets refused — is identical
    /// at every checkpoint. Without that consistency a checkpoint later in
    /// the pipeline could re-defer what an earlier one just bypassed,
    /// wedging the row in a defer loop instead of actually dispatching it.
    pub(super) async fn resolve_chain_hold(&self, execution: &WorkExecution) -> Result<ChainHold> {
        let siblings = self.live_chain_siblings(&execution.work_item_id).await?;
        let Some(first_sibling) = siblings.first().cloned() else {
            return Ok(ChainHold::Clear);
        };
        let queue_len = siblings.len();
        let all_review_siblings = siblings.iter().all(|s| s.kind == ExecutionKind::PrReview);
        let is_conflict_revision = matches!(
            self.work_db.classify_work_item_for_dispatch(&execution.work_item_id),
            Ok(DispatchClass::MergeConflictRevision)
        );
        if all_review_siblings && is_conflict_revision {
            Ok(ChainHold::ReviewBypassed(first_sibling))
        } else {
            // Prefer surfacing a non-review (writer) sibling in the
            // `Blocked` outcome when one is present, since that is the
            // actually-blocking reason — a mix of a review and a writer
            // sibling should report the writer, not the review, in trace
            // output and wait-reason labeling.
            let sibling = siblings
                .iter()
                .find(|s| s.kind != ExecutionKind::PrReview)
                .cloned()
                .unwrap_or(first_sibling);
            Ok(ChainHold::Blocked {
                sibling,
                review_held: all_review_siblings,
                queue_len,
            })
        }
    }

    /// Build the operator-facing string persisted into
    /// `dispatch_wait_reason` (and rendered verbatim on the kanban card)
    /// for a `ChainHold::Blocked` outcome. Names the concrete blocking
    /// task and PR instead of the opaque engine-internal "sibling"
    /// vocabulary — see the T2469 incident (mono#1901) where the card read
    /// "Waiting — blocked behind a live PR sibling" with no way to tell
    /// what a "sibling" was, which task was blocking, or which PR was
    /// involved. When more than one sibling is queued, names the count and
    /// the currently-live one.
    /// Persist the operator-facing `dispatch_wait_reason` for an execution,
    /// logging a warning (rather than failing dispatch) if the DB write errors.
    fn record_dispatch_wait_reason(&self, execution_id: &str, reason: &str) {
        if let Err(err) = self.work_db.set_dispatch_wait_reason(execution_id, reason) {
            tracing::warn!(execution_id = %execution_id, ?err, "failed to record dispatch_wait_reason");
        }
    }

    fn chain_serialized_wait_reason(&self, sibling: &WorkExecution, review_held: bool, queue_len: usize) -> String {
        let sibling_task = self
            .resolve_execution_work_item(sibling)
            .ok()
            .and_then(|item| match item {
                WorkItem::Task(task) | WorkItem::Chore(task) => Some(task),
                _ => None,
            });
        let sibling_label = sibling_task
            .as_ref()
            .map(|task| format!("{} '{}'", task.short_label(), task.name))
            .unwrap_or_else(|| sibling.work_item_id.clone());
        // The chain-root task's `pr_url` (set once the PR exists) is the
        // reliable source — a root `chore_implementation`/`task_implementation`
        // execution never carries its own `pr_url` (the PR doesn't exist yet
        // when it was dispatched), only revision executions do. Fall back to
        // the execution's own `pr_url` for the revision-sibling case.
        let pr_ref = sibling_task
            .as_ref()
            .and_then(|task| task.pr_url.as_deref())
            .or(sibling.pr_url.as_deref())
            .and_then(pr_short_reference)
            .map(|r| format!(" on {r}"))
            .unwrap_or_default();
        let queue_prefix = if queue_len > 1 {
            format!("{queue_len} revisions queued; currently running: ")
        } else {
            String::new()
        };
        let cause = if review_held {
            "an automated PR review runs at a time"
        } else {
            "revisions on the same PR run one at a time"
        };
        format!("blocked by {queue_prefix}{sibling_label}{pr_ref} ({cause})")
    }

    /// After `execution` has sat `ready` and chain-serialized for at least
    /// [`CHAIN_SERIALIZED_STALL_THRESHOLD_SECS`], file a durable
    /// [`CHAIN_SERIALIZED_STALL_ATTENTION_KIND`] attention on its work item
    /// so a human notices without grepping `engine-trace.jsonl` — the T251
    /// incident sat in this exact state, re-deferred every ~10s, for ~20
    /// silent minutes before a human found it by hand.
    ///
    /// Uses `execution.created_at` as the "stuck since" clock: a `ready`
    /// row is re-evaluated every drain pass, so its age is a reasonable
    /// proxy for how long it has been waiting (a row that spent time in
    /// `waiting_dependency` before promotion only makes this an
    /// under-estimate, never a false alarm). Idempotent — repeated calls
    /// while the stall persists are a no-op after the first.
    fn surface_chain_serialized_stall_if_overdue(&self, execution: &WorkExecution, sibling: &WorkExecution) {
        let Some(created_at) = execution.created_epoch() else {
            return;
        };
        let elapsed = boss_engine_utils::epoch_time::now_epoch_secs() - created_at;
        if elapsed < CHAIN_SERIALIZED_STALL_THRESHOLD_SECS {
            return;
        }
        let title = "Execution stuck behind a chain-serialized sibling".to_owned();
        let body = format!(
            "Execution `{}` (work item `{}`) has been deferred for ~{} minutes with \
             `reason=chain_serialized`, waiting behind live sibling execution `{}` \
             (work item `{}`).\n\n\
             If that sibling is actually still working, this will clear on its own once \
             it finishes. If it is actually a dead worker (its pane exited without a `Stop` \
             hook), the engine's periodic zombie sweeps (`lost_workspace_sweep` / \
             `dead_pane_sweep`) reconcile it automatically on their next pass; if it \
             persists, `bossctl agents reap {}` clears it by hand.",
            execution.id,
            execution.work_item_id,
            elapsed / 60,
            sibling.id,
            sibling.work_item_id,
            sibling.id,
        );
        if let Err(err) = self.work_db.upsert_work_item_attention(
            &execution.work_item_id,
            CHAIN_SERIALIZED_STALL_ATTENTION_KIND,
            &title,
            &body,
        ) {
            tracing::warn!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                ?err,
                "drain: failed to raise chain_serialized_stall attention",
            );
        }
    }

    /// Drain every currently-`ready` execution. Returns the reason the
    /// drain stopped so the caller can decide whether to re-enter
    /// immediately (queue empty + pending wakeup) or yield (pool
    /// exhausted).
    /// Drain the `ready` execution queue, routing each execution to the
    /// correct pool (main, automation, or review). Per-pool exhaustion is
    /// handled independently: a full pool does not block dispatch on the
    /// other pools.
    ///
    /// All `ready` rows are fetched once at the top of each drain pass.
    /// Executions whose pool is already known to be exhausted are skipped
    /// for this pass; they remain `ready` and will be picked up on the
    /// next `kick()` triggered by `release_worker_and_kick`.
    ///
    /// # Priority order: mainline > review > spilled automation
    ///
    /// Each drain runs in **two passes** over one `ready` snapshot:
    ///
    /// 1. **Home pools.** Every row attempts a claim on the pool
    ///    [`Self::pool_for_execution`] routes it to. Mainline and review
    ///    rows that miss are deferred (`pool_exhausted`) as always. An
    ///    automation row that misses its full automation pool is instead
    ///    collected as a *spill candidate*.
    /// 2. **Spillover.** Each spill candidate tries to claim a free
    ///    **Lower Decks** slot (page 1 of the interactive pool, never
    ///    Bridge Crew — see [`WorkerPool::claim_worker_spill`]).
    ///
    /// The pass boundary is the whole point: by the time any automation
    /// row can touch an interactive slot, every ready mainline row in the
    /// snapshot has already claimed or been rejected. So **any ready
    /// mainline item beats any ready automation item for a
    /// non-automation slot regardless of arrival order** — the guarantee
    /// does not depend on `list_ready_executions`' sort order, which
    /// interleaves the pools by dispatch class and can rank an automation
    /// row ahead of a mainline one.
    ///
    /// # Preemption: mainline's last resort
    ///
    /// When a *mainline* row misses its claim and the interactive pool is
    /// full on both pages, the drain may stop one in-progress spilled
    /// automation run and requeue its work to free a slot — see
    /// [`Self::try_preempt_automation_for`]. Never for a review or
    /// automation row, never against a mainline or review victim, and at
    /// most **one per drain pass** (`preempted_this_pass`), so a burst of
    /// mainline arrivals cannot cascade into wiping out every spilled
    /// automation run at once. See [`crate::dispatch_spillover`] for the
    /// policy and its rationale.
    pub(super) async fn drain_ready_queue(self: &Arc<Self>) -> DrainOutcome {
        // Global pause gate. `pr_review` executions are the lifecycle of a
        // change already in flight, not new work, so an operator-originated
        // pause exempts them — they keep draining into the review pool while
        // main/automation rows are held. A breaker-originated pause (the
        // app's spawn path itself is broken — see `spawn_health.rs`) exempts
        // nothing, since dispatching a review would just burn another spawn
        // attempt against the same dead path.
        let paused = self.dispatch_paused.load(Ordering::Acquire);
        let reviews_exempt_from_pause = paused && self.dispatch_pause_exempts_reviews.load(Ordering::Acquire);
        // Automation-pause gate — independent of `paused` above (see
        // `FrontendRequest::SetAutomationPaused`). Checked per-row below
        // alongside `paused`, rather than short-circuiting the whole drain
        // here, because an automation-only pause must still let main/review
        // rows dispatch normally.
        let automation_paused = self.automation_paused.load(Ordering::Acquire);

        let executions = match self.work_db.list_ready_executions() {
            Ok(e) => e,
            Err(err) => {
                tracing::error!(?err, "failed to list ready executions");
                return DrainOutcome::QueueEmpty;
            }
        };

        // Queue-level depth/oldest-wait gauges, sampled here (before the
        // pause gate and any filtering below) so they reflect the true
        // ready-queue state on every pass — including an all-zero
        // snapshot on an empty queue, and the real backlog while dispatch
        // sits paused. See `dispatch_metrics.rs`.
        {
            let now_secs = boss_engine_utils::epoch_time::now_epoch_secs();
            let mut snapshot = crate::dispatch_metrics::QueueSnapshot::default();
            for execution in &executions {
                let is_review = self.execution_targets_review_pool(execution);
                let is_automation = !is_review && self.execution_targets_automation_pool(execution);
                let sample = if is_review {
                    &mut snapshot.review
                } else if is_automation {
                    &mut snapshot.automation
                } else {
                    &mut snapshot.main
                };
                sample.depth += 1;
                if let Some(created_secs) = execution.created_epoch() {
                    let age_secs = (now_secs - created_secs).max(0);
                    sample.oldest_wait_secs = sample.oldest_wait_secs.max(age_secs);
                }
            }
            crate::dispatch_metrics::record_queue_snapshot(&self.metrics, snapshot);
        }

        if executions.is_empty() {
            return DrainOutcome::QueueEmpty;
        }

        if paused {
            let review_count = executions
                .iter()
                .filter(|e| self.execution_targets_review_pool(e))
                .count();
            let held_count = executions.len() - review_count;
            if reviews_exempt_from_pause {
                tracing::debug!(
                    held_count,
                    review_exempt_count = review_count,
                    "drain_ready_queue: dispatch is globally paused — holding non-review rows, \
                     draining review-pool exemptions",
                );
            } else {
                tracing::debug!(
                    held_count,
                    review_exempt_count = 0,
                    "drain_ready_queue: dispatch is globally paused — skipping (breaker pause, no exemptions)",
                );
                return DrainOutcome::QueueEmpty;
            }
        }

        let mut main_pool_exhausted = false;
        let mut auto_pool_exhausted = false;
        let mut review_pool_exhausted = false;
        // Automation rows whose home pool was full. Placed onto free Lower
        // Decks slots in pass 2, after every mainline/review row has had
        // its claim attempt. Queue order is preserved.
        let mut spill_candidates: Vec<WorkExecution> = Vec::new();
        // At most one automation run may be preempted per drain pass, so a
        // burst of mainline arrivals never cascades into tearing down every
        // spilled automation run at once. The next mainline item waits for
        // the next pass, by which point the freed slot may already have
        // absorbed it.
        let mut preempted_this_pass = false;

        // Per-pool candidate counts for this drain pass, computed up front
        // so the `request_recorded` event below can report "how many other
        // eligible rows this execution's dispatch class beat" without a
        // second query per row. `executions` is already sorted by
        // `(DispatchClass, priority, created_at, id)` (see
        // `WorkDb::list_ready_executions`), so within a pool the row order
        // below IS the priority order — a class=1 row that appears first is
        // winning against every other row counted for its pool here.
        let pool_ready_counts: HashMap<&'static str, usize> = {
            let mut counts: HashMap<&'static str, usize> = HashMap::new();
            for execution in &executions {
                let is_review = self.execution_targets_review_pool(execution);
                let is_automation = !is_review && self.execution_targets_automation_pool(execution);
                let label = if is_review {
                    "review"
                } else if is_automation {
                    "automation"
                } else {
                    "main"
                };
                *counts.entry(label).or_insert(0) += 1;
            }
            counts
        };

        for execution in executions {
            let preferred_workspace_id = execution.preferred_workspace_id.clone();
            // Classify the target pool. Review is checked first (and excludes
            // the others) so a reviewer of an automation-produced task is
            // counted against the review pool, not the automation pool.
            let is_review = self.execution_targets_review_pool(&execution);
            let is_automation = !is_review && self.execution_targets_automation_pool(&execution);
            let is_main = !is_review && !is_automation;
            let pool_label = if is_review {
                "review"
            } else if is_automation {
                "automation"
            } else {
                "main"
            };

            // Dispatch is paused and this row isn't exempt: leave it `ready`
            // for the next drain after resume. Reached only when
            // `reviews_exempt_from_pause` is true (a non-exempt pause already
            // returned above), so this holds every non-review row while
            // review rows fall through to normal dispatch below.
            if paused && !is_review {
                continue;
            }

            // Automation is paused: hold this row regardless of the global
            // dispatch-pause state above. Unlike the dispatch-pause hold,
            // this also prevents the row from ever reaching the spill-
            // candidate queue below — an automation-paused row must not
            // claim ANY slot, home or spilled.
            if automation_paused && is_automation {
                continue;
            }

            // Skip executions for pools we already know are full.
            // They remain `ready` and will be retried on the next kick.
            //
            // Automation is deliberately NOT short-circuited on
            // `auto_pool_exhausted`: a full automation pool is now the
            // precondition for spilling, not a dead end, so every
            // automation row must still flow through the pipeline below
            // and attempt its home claim in order to be queued as a spill
            // candidate. Short-circuiting here would let only the FIRST
            // automation row of the pass ever reach the spill queue and
            // leave the rest of Lower Decks idle. The redundant home claim
            // this costs is one in-memory mutex acquisition per row.
            if is_review && review_pool_exhausted {
                continue;
            }
            if is_main && main_pool_exhausted {
                continue;
            }

            // TEMPORARY interactive-pool concurrency cap (operator directive,
            // 2026-07-15): main-pool rows are held once the interactive
            // pool's live workers reach [`MAX_CONCURRENT_INTERACTIVE_WORKERS`],
            // even though the pool has 16 slots. Automation and review rows
            // dispatched from their OWN home pools are never held by this
            // gate — their pools' own sizes govern them. Spilled automation
            // is a different story: it claims an interactive-pool slot (see
            // `claim_worker_spill`), so it DOES count toward the live number
            // this cap compares against — see the constant's doc for why
            // that's intentional.
            //
            // Checked BEFORE `request_recorded` (same invariant the earlier,
            // pre-preemption gate carried): a capped row must never enter
            // the dispatch pipeline — no `RequestRecorded` emission, no
            // `picked_up` log, no chain-hold check, no merge_order stagger —
            // since none of that reflects what actually happened to it. The
            // ONE exception is the once-per-pass automation-preemption
            // fallback: a preemption is a trade (one live worker for
            // another), not growth in the live-worker count, so it can
            // never itself push the pool over the cap, and capped rows must
            // still get a chance at it or they'd starve permanently whenever
            // every interactive slot is full — exactly the case preemption
            // exists to resolve. A capped row that wins via preemption falls
            // straight through to `dispatch_claimed_execution` (which does
            // its own `WorkerClaimed` emit); a capped row that doesn't is
            // deferred here with the `interactive_concurrency_cap` reason
            // and never touches the pipeline below. See the constant's doc
            // for rationale and removal criteria.
            let live_workers_at_cap_check = if is_main {
                Some(self.worker_pool.busy_count().await)
            } else {
                None
            };
            let capped = live_workers_at_cap_check.is_some_and(|live| live >= self.max_concurrent_interactive_workers);

            if capped {
                let claimed = if preempted_this_pass {
                    None
                } else {
                    match self.try_preempt_automation_for(&execution).await {
                        // Latch on the teardown having happened, NOT on
                        // having won the slot — see `PreemptionAttempt`.
                        PreemptionAttempt::Preempted { claimed } => {
                            preempted_this_pass = true;
                            claimed
                        }
                        PreemptionAttempt::NotPreempted => None,
                    }
                };
                let Some(worker_id) = claimed else {
                    let live_workers = live_workers_at_cap_check.unwrap_or_default();
                    let cap = self.max_concurrent_interactive_workers;
                    tracing::info!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        pool = pool_label,
                        live_workers,
                        cap,
                        "spawn_attempt status=ready -> held reason=interactive_concurrency_cap"
                    );
                    self.record_dispatch_wait_reason(
                        &execution.id,
                        &format!(
                            "Held by the interactive concurrency cap ({live_workers}/{cap} \
                             workers live) — dispatches as workers finish"
                        ),
                    );
                    continue;
                };
                self.dispatch_claimed_execution(&execution, &worker_id, pool_label, false)
                    .await;
                continue;
            }

            // Dispatch-class + "why it won" bookkeeping (operator directive:
            // revisions before tasks/chores, ordered by revision kind).
            // Recomputed here (rather than threaded through from
            // `list_ready_executions`) because it's only needed for the
            // trace, not the hot dispatch path. See `DispatchClass`.
            let dispatch_class = self
                .work_db
                .classify_work_item_for_dispatch(&execution.work_item_id)
                .unwrap_or(DispatchClass::OtherWork);
            let pool_ready_count = pool_ready_counts.get(pool_label).copied().unwrap_or(1);

            // Stage 1: request_recorded
            self.dispatch_events
                .emit(
                    DispatchEvent::new(Stage::RequestRecorded, DispatchOutcome::Ok, &execution.id)
                        .with_work_item(&execution.work_item_id)
                        .with_details(serde_json::json!({
                            "preferred_workspace_id": preferred_workspace_id,
                            "pool": pool_label,
                            "dispatch_class": dispatch_class.as_ordinal(),
                            "dispatch_class_label": dispatch_class.label(),
                            "pool_ready_count": pool_ready_count,
                            "beaten_candidates": pool_ready_count.saturating_sub(1),
                        })),
                )
                .await;
            tracing::info!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                preferred_workspace_id = ?preferred_workspace_id,
                pool = pool_label,
                dispatch_class = dispatch_class.as_ordinal(),
                dispatch_class_label = dispatch_class.label(),
                beaten_candidates = pool_ready_count.saturating_sub(1),
                "spawn_attempt status=ready -> picked_up"
            );

            // Per-PR single-writer guard (T1577 / T1815 incident): defer this
            // execution if ANOTHER work item on the same PR/revision chain is
            // already live. Checked BEFORE claiming a worker so a serialized
            // row never burns a slot or pollutes its dispatch timeline. The
            // row stays `ready` and re-attempts on the next kick (which fires
            // when the live sibling reaps), so it runs strictly after it.
            // `schedule_execution` re-checks this as the chokepoint backstop
            // for the `force_dispatch` path. Goes through `resolve_chain_hold`
            // (not the raw `WorkDb` query) so a `waiting_human` sibling whose
            // worker pane is actually dead doesn't wedge this row forever —
            // see `live_chain_siblings`'s docs for the T251 incident this
            // closes — and so a merge-conflict-fix revision never waits
            // behind a read-only review (see `resolve_chain_hold`'s docs for
            // the T270/T258 priority-inversion incident this closes).
            match self.resolve_chain_hold(&execution).await {
                Ok(ChainHold::Blocked {
                    sibling,
                    review_held,
                    queue_len,
                }) => {
                    let event_reason = if review_held {
                        "chain_serialized_review_held"
                    } else {
                        "chain_serialized"
                    };
                    tracing::info!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        live_sibling_execution_id = %sibling.id,
                        live_sibling_work_item_id = %sibling.work_item_id,
                        pool = pool_label,
                        review_held,
                        "spawn_attempt status=ready -> deferred reason={event_reason}"
                    );
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(Stage::WorkerClaimed, DispatchOutcome::Skipped, &execution.id)
                                .with_work_item(&execution.work_item_id)
                                .with_details(serde_json::json!({
                                    "reason": event_reason,
                                    "review_held": review_held,
                                    "live_sibling_execution_id": sibling.id,
                                    "live_sibling_work_item_id": sibling.work_item_id,
                                })),
                        )
                        .await;
                    // Operator-facing wait reason: names the concrete blocking
                    // task/PR instead of the opaque "PR sibling" wording (T2469
                    // incident — the card read "blocked behind a live PR
                    // sibling" with no way to tell what a sibling was or which
                    // task/PR was involved). `dispatch_events`/tracing above
                    // keep the terse `event_reason` code for stats grouping;
                    // this is the string persisted into `dispatch_wait_reason`
                    // and rendered verbatim on the kanban card.
                    let wait_reason = self.chain_serialized_wait_reason(&sibling, review_held, queue_len);
                    self.record_dispatch_wait_reason(&execution.id, &wait_reason);
                    self.surface_chain_serialized_stall_if_overdue(&execution, &sibling);
                    // Leave the row `ready`; do NOT mark any pool exhausted —
                    // other executions in this pass may still dispatch.
                    continue;
                }
                Ok(ChainHold::ReviewBypassed(sibling)) => {
                    // Reviews are read-only — a pending merge-conflict fix
                    // must not wait the length of a review run behind one.
                    // Fall through and dispatch this pass; the review keeps
                    // running (it will self-terminate normally, and its
                    // findings will already be stale against the fix's
                    // upcoming push — `enable_revision_triggered_reviews`
                    // fires a fresh pass once it lands).
                    tracing::info!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        live_sibling_execution_id = %sibling.id,
                        live_sibling_work_item_id = %sibling.work_item_id,
                        pool = pool_label,
                        "spawn_attempt status=ready -> chain_hold_bypassed reason=review_yields_to_conflict_fix"
                    );
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(Stage::WorkerClaimed, DispatchOutcome::Ok, &execution.id)
                                .with_work_item(&execution.work_item_id)
                                .with_details(serde_json::json!({
                                    "chain_hold_bypassed": "review_yields_to_conflict_fix",
                                    "review_held": true,
                                    "live_sibling_execution_id": sibling.id,
                                    "live_sibling_work_item_id": sibling.work_item_id,
                                })),
                        )
                        .await;
                    if let Err(err) = self.work_db.resolve_external_tracker_attention(
                        &execution.work_item_id,
                        CHAIN_SERIALIZED_STALL_ATTENTION_KIND,
                    ) {
                        tracing::warn!(
                            execution_id = %execution.id,
                            work_item_id = %execution.work_item_id,
                            ?err,
                            "drain: failed to resolve chain_serialized_stall attention on bypass",
                        );
                    }
                    // Fall through to normal dispatch below.
                }
                Ok(ChainHold::Clear) => {
                    // No longer (or never) chain-serialized — clear any stall
                    // attention a prior pass raised for this work item so it
                    // doesn't linger `open` once dispatch actually proceeds.
                    if let Err(err) = self.work_db.resolve_external_tracker_attention(
                        &execution.work_item_id,
                        CHAIN_SERIALIZED_STALL_ATTENTION_KIND,
                    ) {
                        tracing::warn!(
                            execution_id = %execution.id,
                            work_item_id = %execution.work_item_id,
                            ?err,
                            "drain: failed to resolve chain_serialized_stall attention on unblock",
                        );
                    }
                }
                Err(err) => {
                    // Fail open: a DB error must not wedge the queue. The
                    // `schedule_execution` backstop still guards the spawn.
                    tracing::warn!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        ?err,
                        "drain: chain single-writer check failed — proceeding without pre-claim defer",
                    );
                }
            }

            // merge_order dispatch stagger (direction 2, optional, default off):
            // when configured, the "later" side of a high-overlap merge_order
            // pair whose "first" side is still in flight gets a one-shot bounded
            // dispatch offset so the two workers' diffs interleave less. This is
            // NOT a block and never waits for a merge — the row simply becomes
            // dispatchable again after the window via the `dispatch_not_before`
            // gate + the scheduler heartbeat. It runs after the chain-hold gate
            // (so a serialized row is never double-handled) and before claiming a
            // slot (so a staggered row never burns a worker). Fail open on any DB
            // error — a stagger check must never wedge the queue.
            if self.merge_order_stagger_secs > 0 {
                match self.work_db.maybe_stagger_merge_order_dispatch(
                    &execution.id,
                    &execution.work_item_id,
                    self.merge_order_stagger_secs,
                ) {
                    Ok(Some(not_before)) => {
                        tracing::info!(
                            execution_id = %execution.id,
                            work_item_id = %execution.work_item_id,
                            not_before,
                            stagger_secs = self.merge_order_stagger_secs,
                            pool = pool_label,
                            "spawn_attempt status=ready -> deferred reason=merge_order_stagger"
                        );
                        self.dispatch_events
                            .emit(
                                DispatchEvent::new(Stage::WorkerClaimed, DispatchOutcome::Skipped, &execution.id)
                                    .with_work_item(&execution.work_item_id)
                                    .with_details(serde_json::json!({
                                        "reason": "merge_order_stagger",
                                        "not_before": not_before,
                                        "stagger_secs": self.merge_order_stagger_secs,
                                    })),
                            )
                            .await;
                        // Leave the row `ready` (now with a future
                        // `dispatch_not_before`); do NOT mark any pool exhausted.
                        continue;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!(
                            execution_id = %execution.id,
                            work_item_id = %execution.work_item_id,
                            ?err,
                            "drain: merge_order stagger check failed — dispatching without offset",
                        );
                    }
                }
            }

            let pool = self.pool_for_execution(&execution);

            // Not capped (capped mainline rows already `continue`d above,
            // either dispatched via preemption or deferred with
            // `interactive_concurrency_cap`), so a genuinely idle slot is
            // fair game.
            let claimed = pool
                .claim_worker(&execution.id, preferred_workspace_id.as_deref())
                .await;

            // A mainline row that missed its slot gets ONE chance to
            // reclaim interactive capacity from a spilled automation run
            // before it settles for waiting. Gated on `is_main` (never
            // review, never automation — see `dispatch_spillover`) and on
            // the once-per-pass latch, which is what bounds a single
            // mainline arrival to at most one preemption.
            let claimed = match claimed {
                Some(worker_id) => Some(worker_id),
                None if is_main && !preempted_this_pass => {
                    match self.try_preempt_automation_for(&execution).await {
                        // Latch on the teardown having happened, NOT on
                        // having won the slot — see `PreemptionAttempt`.
                        PreemptionAttempt::Preempted { claimed } => {
                            preempted_this_pass = true;
                            claimed
                        }
                        PreemptionAttempt::NotPreempted => None,
                    }
                }
                None => None,
            };

            let Some(worker_id) = claimed else {
                // This pool is fully claimed. Record exhaustion and continue
                // so executions for the other pools can still be dispatched.
                let pool_capacity = pool.capacity().await;

                // An automation row that missed its home pool is NOT
                // finished yet: it becomes a spill candidate, and pass 2
                // below tries to place it on a free Lower Decks slot once
                // every mainline/review row in this snapshot has already
                // had its claim attempt. Deferring it that way — rather
                // than spilling inline here — is what makes "mainline
                // beats automation for an interactive slot regardless of
                // arrival order" structural instead of a property of the
                // `list_ready_executions` sort order.
                //
                // `auto_pool_exhausted` is still latched: the automation
                // pool genuinely is full, so later automation rows in this
                // pass skip their (certain to fail) home claim and join
                // the spill queue via the same path.
                if is_automation {
                    auto_pool_exhausted = true;
                    tracing::debug!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        pool_capacity,
                        "automation pool exhausted — queued as a Lower Decks spill candidate"
                    );
                    spill_candidates.push(execution);
                    continue;
                }

                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    pool_capacity,
                    pool = pool_label,
                    "spawn_attempt status=ready -> deferred reason=pool_exhausted"
                );

                // Ghost-active invariant check (main pool only; automation and
                // review executions are excluded from the normal kanban).
                if is_main {
                    let orphans = self.work_db.list_active_chores_without_live_run().unwrap_or_default();
                    if !orphans.is_empty() {
                        tracing::warn!(
                            ghost_active = ?orphans,
                            pool_capacity,
                            "active chores without a running execution after pool exhaustion \
                             — `boss chore list --status active` and `bossctl agents list` will \
                             diverge until a slot frees up"
                        );
                    }
                }

                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::WorkerClaimed, DispatchOutcome::Skipped, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_details(serde_json::json!({
                                "reason": "pool_exhausted",
                                "pool": pool_label,
                                "pool_capacity": pool_capacity,
                            })),
                    )
                    .await;
                self.record_dispatch_wait_reason(&execution.id, "pool_exhausted");

                if is_review {
                    review_pool_exhausted = true;
                } else {
                    main_pool_exhausted = true;
                }
                continue;
            };

            self.dispatch_claimed_execution(&execution, &worker_id, pool_label, false)
                .await;
        }

        // ---- Pass 2: automation spillover into Lower Decks ----
        //
        // Reached only after every mainline and review row above has
        // already claimed or been rejected, so any interactive slot still
        // free here is capacity no mainline work in this snapshot wanted.
        // Candidates keep their queue order, so automation priority among
        // themselves is unchanged — they are simply all ranked below all
        // mainline work.
        for execution in spill_candidates {
            let preferred_workspace_id = execution.preferred_workspace_id.clone();
            // Re-read per candidate: each successful spill in this loop
            // raises the live-worker count, so a cap snapshotted once
            // before the loop would let later candidates in the same
            // pass slip past it. See `claim_worker_spill`'s doc for why
            // spilled automation counts toward this cap at all.
            let Some(worker_id) = self
                .worker_pool
                .claim_worker_spill(
                    &execution.id,
                    preferred_workspace_id.as_deref(),
                    self.max_concurrent_interactive_workers,
                )
                .await
            else {
                // No free Lower Decks slot either, or the interactive
                // concurrency cap is already saturated: this automation
                // row waits, exactly as it did before spillover existed.
                let pool_capacity = self.automation_pool.capacity().await;
                let live_workers = self.worker_pool.busy_count().await;
                let cap = self.max_concurrent_interactive_workers;
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    pool_capacity,
                    live_workers,
                    cap,
                    pool = "automation",
                    "spawn_attempt status=ready -> deferred reason=pool_exhausted (no Lower Decks slot to spill into, or interactive_concurrency_cap reached)"
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::WorkerClaimed, DispatchOutcome::Skipped, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_details(serde_json::json!({
                                "reason": "pool_exhausted",
                                "pool": "automation",
                                "pool_capacity": pool_capacity,
                                "spill_attempted": true,
                            })),
                    )
                    .await;
                self.record_dispatch_wait_reason(&execution.id, "pool_exhausted");

                // Keep the Automations tab honest: "Queued", not a failure
                // badge. Same treatment the pre-spillover exhaustion path
                // gave this row.
                if execution.kind == ExecutionKind::AutomationTriage {
                    let detail = format!(
                        "automation pool exhausted ({pool_capacity}/{pool_capacity} busy) and no free \
                         Lower Decks slot to spill into; triage queued, will dispatch when a slot frees"
                    );
                    if let Err(err) = self
                        .work_db
                        .update_automation_run_for_pool_throttle(&execution.id, &detail)
                    {
                        tracing::warn!(
                            execution_id = %execution.id,
                            ?err,
                            "failed to record pool_throttled outcome on automation_runs row",
                        );
                    }
                }
                continue;
            };

            tracing::info!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                worker_id = %worker_id,
                "automation spilled into Lower Decks — automation pool full, no mainline work wanted this slot"
            );
            self.dispatch_claimed_execution(&execution, &worker_id, "automation", true)
                .await;
        }

        if main_pool_exhausted || auto_pool_exhausted || review_pool_exhausted {
            DrainOutcome::PoolExhausted
        } else {
            DrainOutcome::QueueEmpty
        }
    }

    /// Last-resort attempt to free an interactive slot for a starved
    /// mainline `execution` by preempting one in-progress **spilled**
    /// automation run. See [`PreemptionAttempt`] for the return contract.
    /// When nothing is preempted the caller falls through to the ordinary
    /// `pool_exhausted` deferral and the mainline item waits for the next
    /// drain — the pre-spillover behaviour.
    ///
    /// # Why only spilled runs are victims
    ///
    /// A victim must be occupying a slot the mainline item could actually
    /// use. The automation pool is a *physically separate* slot range
    /// (`auto-worker-N` → slots 17..=22); mainline work never runs there,
    /// so preempting an automation run in its home pool would tear down a
    /// worker and free a slot the starved item still cannot claim — all
    /// cost, no benefit. Only automation that has spilled into an
    /// interactive slot is a useful victim, so the candidate set is drawn
    /// from the interactive pool's claims alone. That also makes "never
    /// preempt mainline or review work" hold structurally: a review run
    /// is in the review pool's own range and is never enumerated here,
    /// and each interactive claim is classified before it is eligible.
    ///
    /// # Ordering: tear down, then requeue
    ///
    /// The teardown runs BEFORE any DB mutation. If it reports
    /// [`PreemptOutcome::MidSpawn`] or [`PreemptOutcome::Failed`] we
    /// abandon the whole preemption having changed nothing, leaving the
    /// victim to run to completion normally. Cancelling first and
    /// releasing second (the shape [`crate::completion::WorkerCompletionHandler::cancel_and_release`]
    /// uses, where teardown failure is recoverable) would instead strand
    /// a cancelled-but-still-alive worker here.
    async fn try_preempt_automation_for(self: &Arc<Self>, execution: &WorkExecution) -> PreemptionAttempt {
        // Re-check fullness under the current pool state. The claim that
        // just failed is strong evidence, but a slot may have freed in
        // between — and preemption is destructive enough to be worth
        // proving the precondition rather than inferring it.
        let views = self.worker_pool.slot_views().await;
        if !crate::dispatch_spillover::interactive_pool_is_full(&views) {
            return PreemptionAttempt::NotPreempted;
        }

        // Build the victim set: interactive-pool claims whose execution is
        // automation-classified and still non-terminal.
        let mut candidates: Vec<crate::dispatch_spillover::PreemptionCandidate> = Vec::new();
        for claim in self.worker_pool.claims().await {
            let Ok(claimed) = self.work_db.get_execution(&claim.execution_id) else {
                continue;
            };
            if claimed.status.is_terminal() {
                continue;
            }
            if self.execution_targets_review_pool(&claimed) || !self.execution_targets_automation_pool(&claimed) {
                continue;
            }
            candidates.push(crate::dispatch_spillover::PreemptionCandidate {
                execution_id: claimed.id.clone(),
                work_item_id: claimed.work_item_id.clone(),
                worker_id: claim.worker_id.clone(),
                started_epoch: claimed.started_epoch(),
            });
        }

        let Some(victim) = crate::dispatch_spillover::select_preemption_victim(&candidates) else {
            // Every interactive slot holds mainline or review work (or a
            // mid-spawn automation run with no start time). Nothing to
            // preempt — mainline waits, as it did before this feature.
            tracing::debug!(
                execution_id = %execution.id,
                interactive_claims = candidates.len(),
                "preemption declined: no eligible spilled automation victim"
            );
            self.dispatch_events
                .emit(
                    DispatchEvent::new(Stage::AutomationPreempted, DispatchOutcome::Skipped, &execution.id)
                        .with_work_item(&execution.work_item_id)
                        .with_details(serde_json::json!({
                            "reason": "no_eligible_victim",
                            "automation_candidates": candidates.len(),
                        })),
                )
                .await;
            return PreemptionAttempt::NotPreempted;
        };
        let victim = victim.clone();

        tracing::warn!(
            preempting_execution_id = %execution.id,
            preempting_work_item_id = %execution.work_item_id,
            victim_execution_id = %victim.execution_id,
            victim_work_item_id = %victim.work_item_id,
            victim_worker_id = %victim.worker_id,
            "preempting spilled automation run: mainline work is ready and every \
             Bridge Crew and Lower Decks slot is occupied"
        );

        // Graceful teardown: pane reap (full process-tree SIGTERM/SIGKILL
        // ladder), pool-slot release, cube lease release.
        match self.automation_preemptor.preempt_worker(&victim.execution_id).await {
            PreemptOutcome::Released => {}
            PreemptOutcome::MidSpawn => {
                // T981: the victim is still spawning and its lease is
                // deliberately held. Nothing was torn down and nothing was
                // requeued — abandon cleanly.
                tracing::info!(
                    victim_execution_id = %victim.execution_id,
                    "preemption abandoned: victim is mid-spawn; leaving it to complete"
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::AutomationPreempted, DispatchOutcome::Skipped, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_details(serde_json::json!({
                                "reason": "victim_mid_spawn",
                                "candidate_execution_id": victim.execution_id,
                            })),
                    )
                    .await;
                return PreemptionAttempt::NotPreempted;
            }
            PreemptOutcome::Failed => {
                tracing::warn!(
                    victim_execution_id = %victim.execution_id,
                    "preemption abandoned: victim teardown failed"
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::AutomationPreempted, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_details(serde_json::json!({
                                "reason": "teardown_failed",
                                "candidate_execution_id": victim.execution_id,
                            })),
                    )
                    .await;
                return PreemptionAttempt::NotPreempted;
            }
        }

        // The victim's worker is gone. Retire its execution row and queue
        // a fresh one for the same work, so the automation redispatches
        // later exactly like a new arrival.
        let requeued_as = self.requeue_preempted_automation(&victim).await;

        // Both sides of the story, so `bossctl dispatch diagnose` reads
        // correctly from either execution's timeline.
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::AutomationPreempted, DispatchOutcome::Ok, &victim.execution_id)
                    .with_work_item(&victim.work_item_id)
                    .with_worker(&victim.worker_id)
                    .with_details(serde_json::json!({
                        "reason": "mainline_starved_no_free_interactive_slot",
                        "preempting_execution_id": execution.id,
                        "preempting_work_item_id": execution.work_item_id,
                        "requeued_as": requeued_as,
                        "victim_selection": "most_recently_started",
                    })),
            )
            .await;
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::AutomationPreempted, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_details(serde_json::json!({
                        "reason": "mainline_starved_no_free_interactive_slot",
                        "preempted_execution_id": victim.execution_id,
                        "preempted_work_item_id": victim.work_item_id,
                        "requeued_as": requeued_as,
                        "victim_selection": "most_recently_started",
                    })),
            )
            .await;

        // Take the slot the teardown just freed. `release_worker_and_kick`
        // inside the teardown also kicks the scheduler, but drains are
        // serialized by `scheduling_active`, so that kick only latches
        // `scheduling_pending` — this drain keeps the slot.
        let worker_id = self
            .worker_pool
            .claim_worker(&execution.id, execution.preferred_workspace_id.as_deref())
            .await;
        if worker_id.is_none() {
            // Lost the freed slot to a concurrent out-of-band claimer
            // (`bossctl agents launch`'s `claim_worker_force`). Nothing is
            // lost — the automation work is already safely requeued — but
            // the preemption bought this item nothing, so say so plainly
            // rather than leaving an unexplained teardown in the log.
            tracing::warn!(
                execution_id = %execution.id,
                victim_execution_id = %victim.execution_id,
                "preempted an automation run but the freed slot was taken before this \
                 execution could claim it; the automation work is requeued and this item \
                 waits for the next drain"
            );
        }
        PreemptionAttempt::Preempted { claimed: worker_id }
    }

    /// Retire a preempted automation execution and queue a fresh one for
    /// the same work. Returns the new execution id when one was created.
    ///
    /// Losslessness is the whole contract here: the work item must not be
    /// lost, must not land in a terminal `failed` state, and must not
    /// leak its pool claim. The victim row is marked `cancelled` (not
    /// `failed`: it did nothing wrong, and a failure badge would be a lie
    /// on the Automations tab), which also stops the orphan sweep and
    /// reconciler from trying to resurrect it, and lets
    /// `request_execution`'s live-execution guard see the item as free.
    ///
    /// The two requeue shapes mirror the ones the slot-busy path
    /// established (T2685 / #2030):
    ///
    /// - `automation_triage` binds to an `automations.id` and has no
    ///   `tasks` row, so it needs its own fresh triage execution and its
    ///   `automation_runs` row re-pointed at the retry — which also flips
    ///   the occurrence to `failed_will_retry` ("Queued") rather than
    ///   leaving it reading as a terminal failure.
    /// - An automation-*produced* task/chore goes through
    ///   `request_execution` directly rather than
    ///   `rescan_active_dispatch`, because that path honors `autostart`,
    ///   which `start_execution_run_on_host` has already consumed by the
    ///   time a run is in progress. Preemption is the engine deciding on
    ///   the item's behalf that this attempt must be retried, so the
    ///   autostart gate does not apply.
    async fn requeue_preempted_automation(
        self: &Arc<Self>,
        victim: &crate::dispatch_spillover::PreemptionCandidate,
    ) -> Option<String> {
        let victim_execution = match self.work_db.get_execution(&victim.execution_id) {
            Ok(execution) => execution,
            Err(err) => {
                tracing::error!(
                    victim_execution_id = %victim.execution_id,
                    ?err,
                    "preemption: failed to re-read victim execution; cannot requeue its work",
                );
                return None;
            }
        };

        if let Err(err) = self.work_db.cancel_running_execution(&victim.execution_id) {
            tracing::warn!(
                victim_execution_id = %victim.execution_id,
                ?err,
                "preemption: failed to cancel victim execution; requeueing anyway",
            );
        }

        if victim_execution.kind == ExecutionKind::AutomationTriage {
            match self
                .work_db
                .create_automation_triage_execution(&victim_execution.work_item_id, &victim_execution.repo_remote_url)
            {
                Ok(retry) => {
                    let detail = format!(
                        "preempted to free a Lower Decks slot for mainline work; requeued as {}",
                        retry.id
                    );
                    if let Err(err) = self.work_db.requeue_automation_run_after_transient_spawn_failure(
                        &victim.execution_id,
                        &retry.id,
                        &detail,
                    ) {
                        tracing::warn!(
                            victim_execution_id = %victim.execution_id,
                            retry_execution_id = %retry.id,
                            ?err,
                            "preemption: failed to re-point automation_runs row at the retry execution",
                        );
                    }
                    tracing::info!(
                        victim_execution_id = %victim.execution_id,
                        retry_execution_id = %retry.id,
                        "preemption: automation triage requeued",
                    );
                    return Some(retry.id);
                }
                Err(err) => {
                    tracing::error!(
                        victim_execution_id = %victim.execution_id,
                        ?err,
                        "preemption: failed to create a replacement triage execution — automation \
                         occurrence will wait for its next scheduled fire",
                    );
                    return None;
                }
            }
        }

        match self.work_db.request_execution(
            boss_protocol::RequestExecutionInput::builder()
                .work_item_id(victim_execution.work_item_id.clone())
                .build(),
        ) {
            Ok(fresh) => {
                tracing::info!(
                    victim_execution_id = %victim.execution_id,
                    work_item_id = %victim_execution.work_item_id,
                    requeued_execution_id = %fresh.id,
                    "preemption: automation-produced work requeued",
                );
                Some(fresh.id)
            }
            Err(err) => {
                tracing::error!(
                    victim_execution_id = %victim.execution_id,
                    work_item_id = %victim_execution.work_item_id,
                    ?err,
                    "preemption: failed to requeue automation work after teardown",
                );
                None
            }
        }
    }

    /// Shared tail of a successful claim: record the landing slot, clear
    /// the wait reason, and hand off to `schedule_execution`, releasing
    /// the slot again if the handoff fails.
    ///
    /// `spilled` marks an automation execution that claimed an
    /// interactive (Lower Decks) slot because its own pool was full. It
    /// is threaded into the `worker_claimed` event rather than inferred
    /// downstream, because a spilled run's `worker_id` is an ordinary
    /// `worker-N` — indistinguishable by prefix from mainline work — and
    /// per-pool dispatch diagnostics must not silently reattribute
    /// automation load to the main pool.
    async fn dispatch_claimed_execution(
        self: &Arc<Self>,
        execution: &WorkExecution,
        worker_id: &str,
        pool_label: &str,
        spilled: bool,
    ) {
        // Record the physical slot + page the claim landed on so a later
        // spawn failure on this slot is attributable to Bridge Crew vs
        // Lower Decks in `bossctl dispatch diagnose` (the page is `null`
        // for automation/review pools, which are single-page).
        let claimed_slot = slot_id_from_worker_id(worker_id);
        let claimed_page = claimed_slot.and_then(worker_page_label);
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::WorkerClaimed, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_details(serde_json::json!({
                        "pool": pool_label,
                        "slot_id": claimed_slot,
                        "page": claimed_page,
                        "spilled": spilled,
                    })),
            )
            .await;
        crate::dispatch_metrics::record_dispatch_completed(&self.metrics);
        // Clear any `dispatch_stage_stalled` attention item this execution
        // may have accumulated while it sat stuck pre-dispatch — it just
        // claimed a slot, so whatever was blocking it is resolved. Mirrors
        // how the churn-guard `parked` attention item resolves on the work
        // item's next successful dispatch attempt rather than being
        // proactively cleared by the sweep that raised it.
        if let Err(err) = self.work_db.resolve_external_tracker_attention(
            &execution.work_item_id,
            crate::work::DISPATCH_STAGE_STALLED_ATTENTION_KIND,
        ) {
            tracing::warn!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                ?err,
                "failed to resolve dispatch_stage_stalled attention item on successful claim",
            );
        }
        if let Err(err) = self.work_db.clear_dispatch_wait_reason(&execution.id) {
            tracing::warn!(execution_id = %execution.id, ?err, "failed to clear dispatch_wait_reason");
        }

        match self.schedule_execution(execution, worker_id).await {
            Ok(()) => {
                tracing::info!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    worker_id = %worker_id,
                    spilled,
                    "spawn_attempt status=ready -> spawned"
                );
            }
            Err(err) => {
                tracing::error!(
                    ?err,
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    worker_id = %worker_id,
                    "spawn_attempt status=ready -> failed reason=schedule_execution_error"
                );
                self.pool_for_worker_id(worker_id)
                    .release_worker(worker_id, execution.preferred_workspace_id.as_deref())
                    .await;
            }
        }
    }
}
