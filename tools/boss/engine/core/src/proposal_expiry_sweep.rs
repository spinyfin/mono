//! Periodic expiry sweep for undecided in-flight-only worker proposals.
//!
//! Implementation task 6 of `worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! ("Apply pipeline: gated `followup_task` + verified `automation_outcome` /
//! `pr_created`"). Per the design's state semantics
//! (`boss_protocol::ProposalState::Expired`): a proposal of an in-flight-only
//! kind (`effort_escalation` / `blocked`) that is still `proposed` when its
//! owning execution reaches a terminal status is expired, because that
//! kind's sole effect — a worker-signal attention plus an auto-nudge pause on
//! the *live* run — is meaningless once the run is over. `followup_task` is
//! never touched by this sweep: a pending followup proposal outlives its
//! execution by design, sitting in the `followup` attention group until the
//! human batch-accept gesture decides it.
//!
//! The actual query lives on [`crate::work::WorkDb::expire_stale_in_flight_proposals`]
//! — this module is just the periodic-loop scaffold around it, following the
//! same shape as [`crate::execution_retention_sweep`] (a pure `WorkDb` sweep
//! with no coordinator/dispatch-event dependency).

use std::sync::Arc;
use std::time::Duration;

use crate::work::WorkDb;

/// Cadence for the periodic pass. Expiry is not time-sensitive (an
/// in-flight-only proposal left `proposed` past its execution's terminal
/// state is already meaningless the moment that transition happens; a delay
/// before the sweep notices costs nothing beyond a lingering row), so this
/// runs on the same slow cadence as the execution-retention sweep.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Count from one sweep pass; logged whenever any row was expired.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProposalExpirySweepOutcome {
    pub expired: usize,
}

impl crate::sweep_loop::SweepOutcome for ProposalExpirySweepOutcome {
    fn has_activity(&self) -> bool {
        self.expired > 0
    }

    fn log(&self) {
        tracing::info!(
            expired = self.expired,
            "proposal-expiry sweep: expired undecided in-flight-only worker proposals \
             whose execution reached a terminal state",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
pub fn spawn_loop(work_db: Arc<WorkDb>, interval: Duration) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        async move { run_one_pass(work_db.as_ref()).await }
    })
}

/// Run a single expiry pass. Returns a summary; callers may log it.
pub async fn run_one_pass(work_db: &WorkDb) -> ProposalExpirySweepOutcome {
    match work_db.expire_stale_in_flight_proposals() {
        Ok(expired) => ProposalExpirySweepOutcome { expired },
        Err(err) => {
            tracing::warn!(?err, "proposal-expiry sweep: expiry query failed; skipping this pass");
            ProposalExpirySweepOutcome::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    /// Insert a `worker_proposals` row directly, bypassing
    /// `submit_worker_proposal` — this sweep's whole point is reaching rows
    /// the normal AutoApply path never leaves `proposed`, so tests must be
    /// able to construct that state by hand.
    fn insert_proposed_row(db: &WorkDb, execution_id: &str, work_item_id: &str, kind: &str, idempotency_key: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO worker_proposals
                 (id, execution_id, work_item_id, kind, payload_json, idempotency_key, state, created_at)
             VALUES (?1, ?2, ?3, ?4, '{}', ?5, 'proposed', '2026-01-01T00:00:00Z')",
            rusqlite::params![
                format!("prp_test_{idempotency_key}"),
                execution_id,
                work_item_id,
                kind,
                idempotency_key,
            ],
        )
        .unwrap();
    }

    fn mark_execution_terminal(db: &WorkDb, execution_id: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [execution_id],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn expires_an_undecided_in_flight_only_proposal_on_a_terminal_execution() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "Cleanup");
        let execution = create_ready_chore_execution(&db, chore.id.clone());
        insert_proposed_row(&db, &execution.id, &chore.id, "blocked", "k1");
        mark_execution_terminal(&db, &execution.id);

        let outcome = run_one_pass(&db).await;
        assert_eq!(outcome.expired, 1);

        let listed = db.list_worker_proposals_for_work_item(&chore.id, None, None).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, boss_protocol::ProposalState::Expired);
        assert_eq!(listed[0].decided_by, Some(boss_protocol::ProposalDecider::Policy));
        assert!(listed[0].decided_at.is_some());
        assert!(listed[0].decision_reason.is_some());
    }

    #[tokio::test]
    async fn leaves_a_still_live_executions_proposal_alone() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "Cleanup");
        let execution = create_ready_chore_execution(&db, chore.id.clone());
        insert_proposed_row(&db, &execution.id, &chore.id, "blocked", "k1");
        // Execution left `ready` — not terminal.

        let outcome = run_one_pass(&db).await;
        assert_eq!(outcome.expired, 0);

        let listed = db.list_worker_proposals_for_work_item(&chore.id, None, None).unwrap();
        assert_eq!(listed[0].state, boss_protocol::ProposalState::Proposed);
    }

    #[tokio::test]
    async fn never_expires_a_followup_task_proposal_even_on_a_terminal_execution() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "Cleanup");
        let execution = create_ready_chore_execution(&db, chore.id.clone());
        // A real followup_task submission (via the normal path) stays
        // `proposed` by design — exactly the state this sweep must never
        // touch for this kind.
        db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id: &chore.id,
            kind: boss_protocol::ProposalKind::FollowupTask,
            payload_json: r#"{"proposed_name":"n","proposed_description":"d","rationale":"r"}"#,
            idempotency_key: "k1",
        })
        .unwrap()
        .unwrap();
        mark_execution_terminal(&db, &execution.id);

        let outcome = run_one_pass(&db).await;
        assert_eq!(outcome.expired, 0);

        let listed = db.list_worker_proposals_for_work_item(&chore.id, None, None).unwrap();
        assert_eq!(listed[0].state, boss_protocol::ProposalState::Proposed);
    }

    #[tokio::test]
    async fn expires_effort_escalation_too() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "Cleanup");
        let execution = create_ready_chore_execution(&db, chore.id.clone());
        insert_proposed_row(&db, &execution.id, &chore.id, "effort_escalation", "k1");
        mark_execution_terminal(&db, &execution.id);

        let outcome = run_one_pass(&db).await;
        assert_eq!(outcome.expired, 1);
    }

    /// A kind outside the in-flight-only set (`attention`) left `proposed`
    /// by hand must never be swept — the filter is a closed, explicit kind
    /// list, not "anything still proposed on a dead execution."
    #[tokio::test]
    async fn leaves_other_kinds_alone_even_if_hand_left_proposed() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "Cleanup");
        let execution = create_ready_chore_execution(&db, chore.id.clone());
        insert_proposed_row(&db, &execution.id, &chore.id, "attention", "k1");
        mark_execution_terminal(&db, &execution.id);

        let outcome = run_one_pass(&db).await;
        assert_eq!(outcome.expired, 0);
    }
}
