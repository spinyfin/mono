//! Narrow fixtures for consumers of storage; enabled only in test builds.
use super::{CiRemediation, WorkDb};
use boss_protocol::QueueCiEvidence;

/// Run a synchronization callback while holding the database connection.
/// The callback must not call another operation on this database.
pub fn with_connection_held(db: &WorkDb, callback: impl FnOnce()) {
    let _connection = db.connect().expect("hold fixture connection");
    callback();
}

/// Pause the next artifact read on the calling thread.
pub fn set_before_packet_read_hook(hook: Box<dyn FnOnce()>) {
    super::review_guide_sources::set_before_packet_read_hook(hook);
}

/// Exercise the same queue evidence gate used by proposal submission.
pub fn validate_queue_ci_evidence(attempt: &CiRemediation, evidence: Option<&QueueCiEvidence>) -> anyhow::Result<()> {
    super::queue_ci_evidence::validate(attempt, evidence)
}

/// Inspect the classification used by queue evidence validation.
pub fn is_lint_check(name: &str) -> bool {
    super::queue_ci_evidence::is_lint_check(name)
}

/// Count all versions, including unpublished or orphaned rows, for rollback assertions.
pub fn review_guide_version_count(db: &WorkDb) -> i64 {
    db.connect()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM pr_review_guide_versions", [], |row| row.get(0))
        .unwrap()
}

/// Read the lease bit that has no public run projection.
pub fn persona_lease_active(db: &WorkDb, run_id: &str) -> bool {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT persona_lease_active FROM work_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap()
}

/// Read the capture failure retained until an artifact has been recovered.
pub fn source_capture_error(db: &WorkDb, pr_url: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT last_capture_error FROM pr_review_guide_source_series WHERE canonical_pr_url = ?1",
            [pr_url],
            |row| row.get(0),
        )
        .unwrap()
}

/// Submit an intentionally incomplete observation to exercise the publication fence.
pub fn apply_review_verdict_without_freshness(
    db: &WorkDb,
    proposal_id: &str,
    checker: &dyn super::PrStateChecker,
) -> anyhow::Result<Option<String>> {
    db.apply_observed_review_verdict_proposal(
        proposal_id,
        checker,
        super::review_verdict_apply::ReviewVerdictObservation {
            inputs: db.review_verdict_comparison(proposal_id)?,
            freshness: None,
            deletions: Some(vec![]),
        },
    )
}
