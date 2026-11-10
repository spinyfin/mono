//! Reporting for waits the engine cannot explain.
//!
//! Every waiting work item is meant to carry a named reason (see
//! `boss_engine_storage_values::dispatch_wait`). A wait the classifier could
//! not name, or a `ready` row that scheduler passes neither claim nor hold, is
//! a break in that model. The scheduler heartbeat calls
//! [`ExecutionCoordinator::flag_unexplained_waits`] on every beat; once such a
//! wait has lasted [`UNEXPLAINED_WAIT_GRACE`] it is logged with the item and
//! execution state and raised as a `dispatch_wait_unclassified` attention
//! item, and a stale `ready` row is reclassified so its card stops saying a
//! dispatch is imminent. Nothing here dispatches, promotes or repairs a row:
//! that would hide the defect this exists to surface.
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use boss_engine_storage_values::attention_kinds::DISPATCH_WAIT_UNCLASSIFIED_ATTENTION_KIND;
use boss_engine_storage_values::dispatch_wait::{
    DISPATCH_WAIT_REASON_AWAITING_DISPATCH, DISPATCH_WAIT_REASON_PREFLIGHT_HOLD, UnexplainedWait,
};

use super::ExecutionCoordinator;
use crate::work::{WorkDb, WorkExecution};

/// How long a wait may stay unexplained before it is reported. Long enough to
/// span several scheduler passes and the 30 s promotion sweep, so the short
/// gaps between one engine write and the next are never reported; short
/// enough that a card is not left saying nothing useful for minutes.
pub(super) const UNEXPLAINED_WAIT_GRACE: Duration = Duration::from_secs(60);

/// The unexplained waits seen on recent heartbeats, keyed by
/// [`UnexplainedWait::key`].
#[derive(Default)]
pub(super) struct UnexplainedWaitWatch {
    seen: HashMap<String, Observed>,
}

struct Observed {
    first_seen: Instant,
    reported: bool,
}

/// One pass over the engine's unexplained waits.
///
/// Returns the executions whose recorded wait reason this pass changed, so the
/// caller can tell their cards. `in_flight` excludes a row another dispatch
/// already has in hand: it is about to be claimed or resolved as a duplicate,
/// which is a known state the drain simply does not record.
fn sweep_unexplained_waits(
    work_db: &WorkDb,
    watch: &mut UnexplainedWaitWatch,
    now: Instant,
    grace: Duration,
    in_flight: &dyn Fn(&WorkExecution) -> bool,
) -> Vec<WorkExecution> {
    let waits: Vec<UnexplainedWait> = match work_db.list_unexplained_dispatch_waits() {
        Ok(waits) => waits
            .into_iter()
            .filter(|wait| !wait.execution.as_ref().is_some_and(in_flight))
            .collect(),
        Err(err) => {
            tracing::warn!(?err, "unexplained-wait check: failed to list waits; skipping pass");
            return Vec::new();
        }
    };
    let current: HashSet<String> = waits.iter().map(UnexplainedWait::key).collect();
    watch.seen.retain(|key, _| current.contains(key));

    let mut changed = Vec::new();
    for wait in &waits {
        let observed = watch.seen.entry(wait.key()).or_insert(Observed {
            first_seen: now,
            reported: false,
        });
        let waited = now.saturating_duration_since(observed.first_seen);
        if waited < grace || observed.reported {
            continue;
        }
        observed.reported = true;
        report(work_db, wait, waited, &mut changed);
    }

    // Lower the signal for every item that is no longer unexplained. Read from
    // the stored open items rather than this watch, so one filed before a
    // restart is resolved too. An item that is still unexplained keeps its
    // open item even while this watch is inside a fresh grace period for it.
    let unexplained_items: HashSet<&str> = waits.iter().map(|wait| wait.work_item_id.as_str()).collect();
    match work_db.list_open_attention_items_of_kind(DISPATCH_WAIT_UNCLASSIFIED_ATTENTION_KIND) {
        Ok(open) => {
            for work_item_id in open.into_iter().filter_map(|item| item.work_item_id) {
                if unexplained_items.contains(work_item_id.as_str()) {
                    continue;
                }
                if let Err(err) =
                    work_db.resolve_external_tracker_attention(&work_item_id, DISPATCH_WAIT_UNCLASSIFIED_ATTENTION_KIND)
                {
                    tracing::warn!(%work_item_id, ?err, "unexplained-wait check: failed to resolve attention");
                }
            }
        }
        Err(err) => tracing::warn!(?err, "unexplained-wait check: failed to list open attention items"),
    }
    changed
}

fn report(work_db: &WorkDb, wait: &UnexplainedWait, waited: Duration, changed: &mut Vec<WorkExecution>) {
    let execution = wait.execution.as_ref();
    tracing::error!(
        work_item_id = %wait.work_item_id,
        task_status = wait.task_status.as_deref().unwrap_or("(not a task)"),
        execution_id = execution.map(|e| e.id.as_str()).unwrap_or("(none)"),
        execution_status = execution.map(|e| e.status.as_str()).unwrap_or("(none)"),
        execution_kind = execution.map(|e| e.kind.as_str()).unwrap_or("(none)"),
        execution_created_at = execution.map(|e| e.created_at.as_str()).unwrap_or("(none)"),
        recorded_wait_reason = execution.and_then(|e| e.dispatch_wait_reason.as_deref()).unwrap_or("(none)"),
        classified_as = %wait.reason,
        unexplained_secs = waited.as_secs(),
        "dispatch wait unclassified: this work item is waiting and the engine has no reason for it",
    );

    // A `ready` row that passes keep leaving alone is not "about to be
    // dispatched". Say so on the row, guarded on the hold it was classified
    // from so a pass that just explained it wins.
    if let Some(execution) = execution
        && wait.reason == DISPATCH_WAIT_REASON_AWAITING_DISPATCH
    {
        match work_db.mark_dispatch_wait_unclassified(&execution.id, execution.dispatch_wait_reason.as_deref()) {
            Ok(true) => changed.push(execution.clone()),
            Ok(false) => {}
            Err(err) => tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "unexplained-wait check: failed to reclassify the wait",
            ),
        }
    }

    let state = match execution {
        Some(execution) => format!(
            "Execution `{}` ({}) is `{}`",
            execution.id,
            execution.kind.as_str(),
            execution.status.as_str()
        ),
        None => "No execution is queued for it".to_owned(),
    };
    let body = format!(
        "Work item `{}`{} has been waiting for at least {}s and the engine cannot name why. {state}, with \
         no dependency, project order, scheduler hold or dispatch deadline behind it.\n\n\
         Every wait is meant to map to a named reason, so this is a defect in the engine's state \
         model rather than a queue state. Run `bossctl dispatch diagnose {}` for the dispatch \
         timeline; starting the item explicitly re-queues it. This item resolves on its own once the \
         item is running or waiting for a reason the engine can name.",
        wait.work_item_id,
        wait.task_status
            .as_deref()
            .map(|status| format!(" (status `{status}`)"))
            .unwrap_or_default(),
        waited.as_secs(),
        execution.map(|e| e.id.as_str()).unwrap_or(&wait.work_item_id),
    );
    if let Err(err) = work_db.upsert_work_item_attention(
        &wait.work_item_id,
        DISPATCH_WAIT_UNCLASSIFIED_ATTENTION_KIND,
        "Work item is waiting for a reason the engine cannot name",
        &body,
    ) {
        tracing::warn!(
            work_item_id = %wait.work_item_id,
            ?err,
            "unexplained-wait check: failed to file attention item",
        );
    }
}

impl ExecutionCoordinator {
    /// Heartbeat hook: report waits that have stayed unexplained past the
    /// grace period and tell the affected cards.
    pub(super) async fn flag_unexplained_waits(&self, watch: &mut UnexplainedWaitWatch) {
        let in_flight = |execution: &WorkExecution| self.inflight_dispatches.blocks_execution(execution, &self.work_db);
        let changed = sweep_unexplained_waits(&self.work_db, watch, Instant::now(), UNEXPLAINED_WAIT_GRACE, &in_flight);
        self.publish_dispatch_runtime_changed(&changed, "dispatch_wait_unclassified")
            .await;
    }

    /// Startup preflight is holding every local dispatch, so the drain walks
    /// nothing. Record that on each `ready` row: without it they would read as
    /// awaiting the next pass, which is not what is stopping them, and would
    /// be reported as unexplained once the hold outlasted the grace period.
    ///
    /// Nothing is pushed to clients here. The hold is engine-wide (every boot
    /// passes through it, and a failed runtime check raises its own attention
    /// item), so a push per queued row would only add invalidation traffic; a
    /// card picks the recorded reason up with its next read.
    pub(super) fn record_preflight_hold(&self) {
        let ready = match self.work_db.list_ready_executions() {
            Ok(ready) => ready,
            Err(err) => {
                tracing::warn!(?err, "failed to list ready executions held by startup preflight");
                return;
            }
        };
        for execution in &ready {
            self.record_dispatch_wait_reason(execution, DISPATCH_WAIT_REASON_PREFLIGHT_HOLD, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::test_support::{create_active_chore, create_product, open_db};
    use boss_engine_storage_values::dispatch_wait::{
        DISPATCH_WAIT_REASON_POOL_EXHAUSTED, DISPATCH_WAIT_REASON_UNCLASSIFIED,
    };
    use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus};

    fn pending(db: &WorkDb, chore: &str, status: ExecutionStatus) -> WorkExecution {
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore)
                .kind(ExecutionKind::ChoreImplementation)
                .status(status)
                .build(),
        )
        .unwrap()
    }

    fn open_attentions(db: &WorkDb) -> Vec<String> {
        db.list_open_attention_items_of_kind(DISPATCH_WAIT_UNCLASSIFIED_ATTENTION_KIND)
            .unwrap()
            .into_iter()
            .filter_map(|item| item.work_item_id)
            .collect()
    }

    const GRACE: Duration = Duration::from_secs(60);

    #[test]
    fn a_wait_with_no_reason_is_reported_only_after_the_grace_period() {
        let (_dir, db) = open_db();
        let chore = create_active_chore(&db, &create_product(&db), "active with nothing behind it");
        // An active item whose only execution is parked off the scheduler's
        // walk, with no prerequisite, project order or hold.
        let execution = pending(&db, &chore, ExecutionStatus::WaitingDependency);
        let mut watch = UnexplainedWaitWatch::default();
        let start = Instant::now();

        sweep_unexplained_waits(&db, &mut watch, start, GRACE, &|_| false);
        sweep_unexplained_waits(&db, &mut watch, start + GRACE / 2, GRACE, &|_| false);
        assert!(
            open_attentions(&db).is_empty(),
            "inside the grace period nothing is raised"
        );

        sweep_unexplained_waits(&db, &mut watch, start + GRACE, GRACE, &|_| false);
        assert_eq!(open_attentions(&db), vec![chore.clone()]);
        let item = &db.list_attention_items_for_work_item(&chore).unwrap()[0];
        assert!(item.body_markdown.contains(&execution.id), "{}", item.body_markdown);
        assert!(
            item.body_markdown.contains("waiting_dependency"),
            "{}",
            item.body_markdown
        );

        // Still unexplained: the item stays open and is not filed twice.
        sweep_unexplained_waits(&db, &mut watch, start + GRACE * 2, GRACE, &|_| false);
        assert_eq!(open_attentions(&db), vec![chore.clone()]);

        // The row leaves the waiting state, so the signal comes down.
        db.cancel_execution(&execution.id).unwrap();
        sweep_unexplained_waits(&db, &mut watch, start + GRACE * 3, GRACE, &|_| false);
        assert!(open_attentions(&db).is_empty());
    }

    #[test]
    fn a_ready_row_no_pass_explains_is_reclassified_and_a_held_one_is_not() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let stranded = create_active_chore(&db, &product, "ready, never held or claimed");
        let held = create_active_chore(&db, &product, "ready, held by a full pool");
        let stranded_execution = pending(&db, &stranded, ExecutionStatus::Ready);
        let held_execution = pending(&db, &held, ExecutionStatus::Ready);
        db.set_dispatch_wait_reason(&held_execution.id, DISPATCH_WAIT_REASON_POOL_EXHAUSTED)
            .unwrap();
        let mut watch = UnexplainedWaitWatch::default();
        let start = Instant::now();

        // A fresh ready row is expected to be picked up: nothing is reported.
        let changed = sweep_unexplained_waits(&db, &mut watch, start, GRACE, &|_| false);
        assert!(changed.is_empty());
        assert_eq!(
            db.get_task_runtime(&stranded).unwrap().dispatch_wait_reason.as_deref(),
            Some(DISPATCH_WAIT_REASON_AWAITING_DISPATCH)
        );

        let changed = sweep_unexplained_waits(&db, &mut watch, start + GRACE, GRACE, &|_| false);
        assert_eq!(
            changed.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec![stranded_execution.id.as_str()]
        );
        assert_eq!(
            db.get_task_runtime(&stranded).unwrap().dispatch_wait_reason.as_deref(),
            Some(DISPATCH_WAIT_REASON_UNCLASSIFIED)
        );
        assert_eq!(open_attentions(&db), vec![stranded.clone()]);
        assert_eq!(
            db.get_task_runtime(&held).unwrap().dispatch_wait_reason.as_deref(),
            Some(DISPATCH_WAIT_REASON_POOL_EXHAUSTED),
            "a row with a named hold is never reported"
        );

        // The scheduler finally takes the row: it starts running and the
        // signal is lowered.
        db.clear_dispatch_wait_reason(&stranded_execution.id).unwrap();
        db.start_execution_run(&stranded_execution.id, "worker-1", "repo", "lease", "ws", "/tmp/ws")
            .unwrap();
        sweep_unexplained_waits(&db, &mut watch, start + GRACE * 2, GRACE, &|_| false);
        assert!(open_attentions(&db).is_empty());
    }

    #[test]
    fn a_row_another_dispatch_has_in_hand_is_not_reported() {
        let (_dir, db) = open_db();
        let chore = create_active_chore(&db, &create_product(&db), "ready, being dispatched");
        let execution = pending(&db, &chore, ExecutionStatus::Ready);
        let mut watch = UnexplainedWaitWatch::default();
        let start = Instant::now();
        let in_flight = |candidate: &WorkExecution| candidate.id == execution.id;

        sweep_unexplained_waits(&db, &mut watch, start, GRACE, &in_flight);
        let changed = sweep_unexplained_waits(&db, &mut watch, start + GRACE * 2, GRACE, &in_flight);
        assert!(changed.is_empty());
        assert!(open_attentions(&db).is_empty());
    }
}
