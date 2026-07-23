//! `worker_proposals` accessors — the ingress ledger behind the mediated
//! worker→engine proposal API.
//!
//! Two operations, matching the two v1 verbs:
//!
//! - [`WorkDb::submit_worker_proposal`] — the write side of `SubmitProposal`.
//!   Replay lookup, rate-cap counting, and insert all happen inside one
//!   `Immediate` transaction so two concurrent `boss propose` calls from the
//!   same run cannot both observe "under the cap" and both insert.
//! - [`WorkDb::list_worker_proposals_for_work_item`] — the read side of
//!   `ListProposals`, scoped to a **work item** rather than an execution so a
//!   resumed or successor run sees prior executions' dispositions.
//!
//! Nothing here applies a proposal: every row this module writes lands in
//! `state = 'proposed'`, and the apply pipeline is a later task.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"Data model".

use super::*;
use boss_engine_proposal_validation::{ProposalCounts, check_rate_caps};
use boss_protocol::{ProposalKind, ProposalState, ProposalSubmissionError, WorkerProposal};

// ---- input types ----

/// One `SubmitProposal` write, after the handler has validated the payload
/// and resolved attribution. Every field is already engine-derived or
/// engine-checked — this type is deliberately not constructible from
/// worker-supplied strings alone.
pub struct SubmitWorkerProposalInput<'a> {
    /// The execution the socket peer resolved to. Never a caller-supplied
    /// value.
    pub execution_id: &'a str,
    /// The execution's work item, denormalised onto the row so
    /// [`WorkDb::list_worker_proposals_for_work_item`] needs no join.
    pub work_item_id: &'a str,
    pub kind: ProposalKind,
    /// The canonicalised payload from the validation layer, ready to store.
    pub payload_json: &'a str,
    /// Caller-supplied or engine-derived; one half of the UNIQUE replay key.
    pub idempotency_key: &'a str,
}

/// What [`WorkDb::submit_worker_proposal`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitWorkerProposalOutcome {
    pub proposal: WorkerProposal,
    /// `true` when an identical submission already existed and this call
    /// returned it untouched instead of inserting. A success either way —
    /// replay safety is the point of the idempotency key.
    pub already_submitted: bool,
}

// ---- mapper ----

const SELECT_WORKER_PROPOSAL: &str = "SELECT id, execution_id, work_item_id, kind, payload_json,
            idempotency_key, state, decided_by, decision_reason, applied_ref,
            created_at, decided_at
     FROM worker_proposals";

/// Map a `worker_proposals` row, per the DB-mapper convention: every column
/// is read positionally from the `SELECT` above, so adding a column without
/// mapping it is a compile error rather than a silent omission.
///
/// `kind` / `state` / `decided_by` are stored as TEXT with no `CHECK`
/// constraint (matching every sibling table's enum-as-TEXT convention), so
/// an unparseable value is a corrupt row rather than a normal case. Failing
/// the read is deliberate: silently coercing an unknown kind to a default
/// would mean the apply pipeline later acts on a proposal that says
/// something else.
fn map_worker_proposal(row: &Row<'_>) -> rusqlite::Result<WorkerProposal> {
    fn parse_column<T: std::str::FromStr<Err = String>>(raw: &str, index: usize) -> rusqlite::Result<T> {
        raw.parse::<T>()
            .map_err(|err| rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, err.into()))
    }

    let kind_raw: String = row.get(3)?;
    let state_raw: String = row.get(6)?;
    let decided_by_raw: Option<String> = row.get(7)?;

    Ok(WorkerProposal {
        id: row.get(0)?,
        execution_id: row.get(1)?,
        work_item_id: row.get(2)?,
        kind: parse_column(&kind_raw, 3)?,
        payload_json: row.get(4)?,
        idempotency_key: row.get(5)?,
        state: parse_column(&state_raw, 6)?,
        decided_by: decided_by_raw.map(|raw| parse_column(&raw, 7)).transpose()?,
        decision_reason: row.get(8)?,
        applied_ref: row.get(9)?,
        created_at: row.get(10)?,
        decided_at: row.get(11)?,
    })
}

/// Fetch the row for `(execution_id, idempotency_key)` — the replay lookup.
fn find_by_idempotency_key(
    conn: &Connection,
    execution_id: &str,
    idempotency_key: &str,
) -> Result<Option<WorkerProposal>> {
    let sql = format!("{SELECT_WORKER_PROPOSAL} WHERE execution_id = ?1 AND idempotency_key = ?2");
    conn.query_row(&sql, params![execution_id, idempotency_key], map_worker_proposal)
        .optional()
        .map_err(Into::into)
}

// ---- WorkDb accessors ----

impl WorkDb {
    /// Insert a proposal row, or return the existing one when this
    /// `(execution_id, idempotency_key)` has already been submitted.
    ///
    /// Ordering inside the transaction is load-bearing:
    ///
    /// 1. **Replay lookup first.** A resubmission returns the stored row and
    ///    is *not* charged against the rate caps. Charging it would let a
    ///    worker whose connection dropped mid-reply retry itself out of its
    ///    own budget for work it already did.
    /// 2. **Then the caps**, counted from this execution's committed rows.
    /// 3. **Then the insert.**
    ///
    /// `Immediate` behaviour takes the write lock up front, so two
    /// concurrent submissions from the same run serialise rather than both
    /// reading a pre-insert count and both landing over the cap.
    ///
    /// Returns `Err` for a genuine storage failure and
    /// `Ok(Err(ProposalSubmissionError))` for a rate-cap refusal — the
    /// refusal is a normal, typed outcome the worker is meant to see, not an
    /// engine fault.
    pub fn submit_worker_proposal(
        &self,
        input: SubmitWorkerProposalInput<'_>,
    ) -> Result<std::result::Result<SubmitWorkerProposalOutcome, ProposalSubmissionError>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) = find_by_idempotency_key(&tx, input.execution_id, input.idempotency_key)? {
            return Ok(Ok(SubmitWorkerProposalOutcome {
                proposal: existing,
                already_submitted: true,
            }));
        }

        let counts = count_proposals_in_tx(&tx, input.execution_id, input.kind)?;
        if let Err(refusal) = check_rate_caps(input.kind, counts) {
            return Ok(Err(refusal));
        }

        let id = next_id("prp");
        let now = now_string();
        tx.execute(
            "INSERT INTO worker_proposals
                 (id, execution_id, work_item_id, kind, payload_json,
                  idempotency_key, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                input.execution_id,
                input.work_item_id,
                input.kind.as_str(),
                input.payload_json,
                input.idempotency_key,
                ProposalState::Proposed.as_str(),
                now,
            ],
        )?;
        tx.commit()?;

        Ok(Ok(SubmitWorkerProposalOutcome {
            proposal: WorkerProposal::builder()
                .id(id)
                .execution_id(input.execution_id)
                .created_at(now)
                .idempotency_key(input.idempotency_key)
                .kind(input.kind)
                .payload_json(input.payload_json)
                .state(ProposalState::Proposed)
                .work_item_id(input.work_item_id)
                .build(),
            already_submitted: false,
        }))
    }

    /// Every proposal filed against `work_item_id`, across **all** its
    /// executions, newest first.
    ///
    /// Work-item scope (not execution scope) is the whole point: a resumed
    /// or successor run must see that a prior execution's followup proposal
    /// came back `rejected: duplicate of T123`, so it adjusts instead of
    /// re-proposing (design §"Disposition of P383", P383's Q4). `state` is
    /// unfiltered by default for the same reason — the `rejected` and
    /// `expired` history *is* the useful part.
    pub fn list_worker_proposals_for_work_item(
        &self,
        work_item_id: &str,
        kind: Option<ProposalKind>,
        state: Option<ProposalState>,
    ) -> Result<Vec<WorkerProposal>> {
        let conn = self.connect()?;
        let sql = format!(
            "{SELECT_WORKER_PROPOSAL}
             WHERE work_item_id = ?1
               AND (?2 IS NULL OR kind = ?2)
               AND (?3 IS NULL OR state = ?3)
             ORDER BY created_at DESC, id DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![work_item_id, kind.map(|k| k.as_str()), state.map(|s| s.as_str())],
            map_worker_proposal,
        )?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// The work item `execution_id` belongs to, or `None` when no such
    /// execution row exists.
    ///
    /// The attribution step needs exactly this one column, and it needs
    /// "row is gone" to be distinguishable from "the read failed" — a
    /// pruned execution and a broken database call for different typed
    /// errors. [`WorkDb::get_execution`] collapses both into `Err`, so this
    /// is the narrow optional-returning read the proposal verbs use.
    pub fn work_item_for_execution(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT work_item_id FROM work_executions WHERE id = ?1",
            params![execution_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Proposal counts for `execution_id` — total and for `kind`.
    ///
    /// Exposed for the rate-cap surface (and for tests to assert the caps
    /// without reaching into SQL); the submission path counts inside its own
    /// transaction rather than calling this, so the count and the insert
    /// cannot be split by a concurrent writer.
    pub fn count_worker_proposals_for_execution(
        &self,
        execution_id: &str,
        kind: ProposalKind,
    ) -> Result<ProposalCounts> {
        let conn = self.connect()?;
        count_proposals_in_tx(&conn, execution_id, kind)
    }
}

/// Count this execution's committed proposals, total and for one kind.
///
/// Counts every row regardless of `state`: the cap bounds how much a run can
/// *submit*, so a proposal that was later rejected or expired still consumed
/// a slot. Anything else would let a loop that keeps getting rejected keep
/// proposing forever — exactly the runaway the cap exists to bound.
fn count_proposals_in_tx(conn: &Connection, execution_id: &str, kind: ProposalKind) -> Result<ProposalCounts> {
    let (total, for_kind): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(kind = ?2), 0)
         FROM worker_proposals WHERE execution_id = ?1",
        params![execution_id, kind.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(ProposalCounts {
        total: total.max(0) as usize,
        for_kind: for_kind.max(0) as usize,
    })
}

// ---- tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use boss_protocol::{PROPOSAL_CAP_PER_KIND_PER_EXECUTION, PROPOSAL_CAP_TOTAL_PER_EXECUTION, ProposalErrorCode};

    /// A product, chore, and `ready` execution — the minimum context a
    /// proposal needs, since `execution_id` is a hard FK.
    fn execution_for_new_chore(db: &WorkDb, chore_name: &str) -> (String, String) {
        let product = create_test_product(db);
        let chore = create_test_chore(db, product.id, chore_name);
        let execution = create_ready_chore_execution(db, chore.id.clone());
        (execution.id, chore.id)
    }

    fn submit(
        db: &WorkDb,
        execution_id: &str,
        work_item_id: &str,
        kind: ProposalKind,
        payload_json: &str,
        idempotency_key: &str,
    ) -> std::result::Result<SubmitWorkerProposalOutcome, ProposalSubmissionError> {
        db.submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id,
            work_item_id,
            kind,
            payload_json,
            idempotency_key,
        })
        .unwrap()
    }

    /// Submit `n` distinct proposals of `kind`, asserting each is accepted.
    /// Distinct keys, so none of them collapses onto a replay.
    fn fill(db: &WorkDb, execution_id: &str, work_item_id: &str, kind: ProposalKind, n: usize) {
        for i in 0..n {
            submit(
                db,
                execution_id,
                work_item_id,
                kind,
                &format!(r#"{{"reason":"n{i}"}}"#),
                &format!("key-{kind}-{i}"),
            )
            .unwrap_or_else(|err| panic!("submission {i} of {n} should be under the cap, got {err}"));
        }
    }

    #[test]
    fn fresh_submission_lands_in_proposed_with_a_prp_id() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-1",
        )
        .unwrap();

        assert!(!outcome.already_submitted);
        assert!(outcome.proposal.id.starts_with("prp_"), "{}", outcome.proposal.id);
        assert_eq!(outcome.proposal.state, ProposalState::Proposed);
        assert_eq!(outcome.proposal.execution_id, execution_id);
        assert_eq!(outcome.proposal.work_item_id.as_deref(), Some(chore_id.as_str()));
        assert_eq!(outcome.proposal.kind, ProposalKind::Blocked);
        // No apply pipeline in this PR — nothing decides a row on submission.
        assert_eq!(outcome.proposal.decided_by, None);
        assert_eq!(outcome.proposal.decided_at, None);
        assert_eq!(outcome.proposal.applied_ref, None);
    }

    /// The returned row must be what a later read sees, not an optimistic
    /// reconstruction that drifts from storage.
    #[test]
    fn returned_row_matches_the_stored_row() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let outcome = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-1",
        )
        .unwrap();

        let listed = db.list_worker_proposals_for_work_item(&chore_id, None, None).unwrap();
        assert_eq!(listed, vec![outcome.proposal]);
    }

    #[test]
    fn resubmitting_the_same_key_returns_the_existing_row() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        let first = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-1",
        )
        .unwrap();
        let replay = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-1",
        )
        .unwrap();

        assert!(replay.already_submitted);
        assert_eq!(replay.proposal, first.proposal);
        assert_eq!(
            db.list_worker_proposals_for_work_item(&chore_id, None, None)
                .unwrap()
                .len(),
            1,
            "a replay must not insert a second row"
        );
    }

    /// The same key under a *different* execution is a different proposal:
    /// the UNIQUE constraint is `(execution_id, idempotency_key)`, so a
    /// successor run replaying its predecessor's command still files its own
    /// row and is attributable to itself.
    #[test]
    fn same_key_under_another_execution_is_a_distinct_row() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "Cleanup");
        let first = create_ready_chore_execution(&db, chore.id.clone());
        let second = create_ready_chore_execution(&db, chore.id.clone());

        let a = submit(
            &db,
            &first.id,
            &chore.id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-1",
        )
        .unwrap();
        let b = submit(
            &db,
            &second.id,
            &chore.id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-1",
        )
        .unwrap();

        assert!(!b.already_submitted);
        assert_ne!(a.proposal.id, b.proposal.id);
        assert_eq!(
            db.list_worker_proposals_for_work_item(&chore.id, None, None)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn per_kind_cap_refuses_the_submission_past_the_limit() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");
        fill(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            PROPOSAL_CAP_PER_KIND_PER_EXECUTION,
        );

        let refusal = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"one too many"}"#,
            "key-over",
        )
        .unwrap_err();
        assert_eq!(refusal.code, ProposalErrorCode::RateLimited);

        // Only this kind is exhausted — a different kind still goes through.
        submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::DeferredScope,
            r#"{"summary":"s","reason":"r"}"#,
            "key-other-kind",
        )
        .expect("a different kind has its own budget");
    }

    #[test]
    fn total_cap_refuses_once_the_whole_budget_is_spent() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");

        // Spread across kinds so the per-kind cap never fires first.
        let mut submitted = 0usize;
        for &kind in ProposalKind::ALL {
            let room = PROPOSAL_CAP_TOTAL_PER_EXECUTION - submitted;
            let take = room.min(PROPOSAL_CAP_PER_KIND_PER_EXECUTION);
            fill(&db, &execution_id, &chore_id, kind, take);
            submitted += take;
            if submitted == PROPOSAL_CAP_TOTAL_PER_EXECUTION {
                break;
            }
        }
        assert_eq!(submitted, PROPOSAL_CAP_TOTAL_PER_EXECUTION);

        let refusal = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Attention,
            r#"{"title":"t","body_markdown":"b"}"#,
            "key-over-total",
        )
        .unwrap_err();
        assert_eq!(refusal.code, ProposalErrorCode::RateLimited);
        assert!(refusal.message.contains("across all kinds"), "{}", refusal.message);
    }

    /// A replay at the cap must still succeed. Otherwise a worker that
    /// spends its budget and then retries any earlier command — a dropped
    /// reply, a resumed run re-running its script — gets a spurious
    /// rate-limit for work it already did.
    #[test]
    fn replay_is_not_charged_against_an_exhausted_cap() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");
        fill(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            PROPOSAL_CAP_PER_KIND_PER_EXECUTION,
        );

        let replay = submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"n0"}"#,
            "key-blocked-0",
        )
        .expect("a replay of an already-stored proposal must not be rate-limited");
        assert!(replay.already_submitted);
    }

    /// Caps are per execution, so a fresh execution on the same work item
    /// starts with a full budget — the cap bounds one runaway run, not the
    /// work item's whole lifetime.
    #[test]
    fn caps_reset_for_a_new_execution_on_the_same_work_item() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "Cleanup");
        let first = create_ready_chore_execution(&db, chore.id.clone());
        fill(
            &db,
            &first.id,
            &chore.id,
            ProposalKind::Blocked,
            PROPOSAL_CAP_PER_KIND_PER_EXECUTION,
        );

        let second = create_ready_chore_execution(&db, chore.id.clone());
        submit(
            &db,
            &second.id,
            &chore.id,
            ProposalKind::Blocked,
            r#"{"reason":"fresh run"}"#,
            "key-fresh",
        )
        .expect("a new execution starts with a full budget");
    }

    /// The read a successor run depends on: proposals from every execution
    /// of the work item, with the prior run's disposition attached.
    #[test]
    fn listing_spans_executions_and_carries_dispositions() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "Cleanup");
        let first = create_ready_chore_execution(&db, chore.id.clone());
        let second = create_ready_chore_execution(&db, chore.id.clone());

        let old = submit(
            &db,
            &first.id,
            &chore.id,
            ProposalKind::FollowupTask,
            r#"{"proposed_name":"n","proposed_description":"d","rationale":"r"}"#,
            "key-old",
        )
        .unwrap();
        submit(
            &db,
            &second.id,
            &chore.id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-new",
        )
        .unwrap();

        // Stamp the predecessor's disposition the way the apply pipeline
        // eventually will, so the successor's read carries the reason.
        db.connect()
            .unwrap()
            .execute(
                "UPDATE worker_proposals
                 SET state = 'rejected', decided_by = 'human',
                     decision_reason = 'duplicate of T123', decided_at = '1747000000'
                 WHERE id = ?1",
                params![old.proposal.id],
            )
            .unwrap();

        let all = db.list_worker_proposals_for_work_item(&chore.id, None, None).unwrap();
        assert_eq!(all.len(), 2, "listing must span both executions");

        let rejected = all.iter().find(|p| p.id == old.proposal.id).unwrap();
        assert_eq!(rejected.state, ProposalState::Rejected);
        assert_eq!(rejected.decision_reason.as_deref(), Some("duplicate of T123"));
        assert_eq!(rejected.decided_by, Some(boss_protocol::ProposalDecider::Human));
    }

    #[test]
    fn listing_filters_by_kind_and_state() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");
        submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::Blocked,
            r#"{"reason":"stuck"}"#,
            "key-b",
        )
        .unwrap();
        submit(
            &db,
            &execution_id,
            &chore_id,
            ProposalKind::DeferredScope,
            r#"{"summary":"s","reason":"r"}"#,
            "key-d",
        )
        .unwrap();

        let blocked = db
            .list_worker_proposals_for_work_item(&chore_id, Some(ProposalKind::Blocked), None)
            .unwrap();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].kind, ProposalKind::Blocked);

        assert_eq!(
            db.list_worker_proposals_for_work_item(&chore_id, None, Some(ProposalState::Proposed))
                .unwrap()
                .len(),
            2
        );
        assert!(
            db.list_worker_proposals_for_work_item(&chore_id, None, Some(ProposalState::Applied))
                .unwrap()
                .is_empty()
        );
    }

    /// Another work item's proposals must never leak into this one's
    /// listing — the scope is the caller's own work item, by construction.
    #[test]
    fn listing_excludes_other_work_items() {
        let (_dir, db) = open_db();
        let (mine, my_chore) = execution_for_new_chore(&db, "Mine");
        let (theirs, their_chore) = execution_for_new_chore(&db, "Theirs");

        submit(
            &db,
            &mine,
            &my_chore,
            ProposalKind::Blocked,
            r#"{"reason":"mine"}"#,
            "key-1",
        )
        .unwrap();
        submit(
            &db,
            &theirs,
            &their_chore,
            ProposalKind::Blocked,
            r#"{"reason":"theirs"}"#,
            "key-1",
        )
        .unwrap();

        let listed = db.list_worker_proposals_for_work_item(&my_chore, None, None).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].execution_id, mine);
    }

    #[test]
    fn counts_are_scoped_to_the_execution_and_kind() {
        let (_dir, db) = open_db();
        let (execution_id, chore_id) = execution_for_new_chore(&db, "Cleanup");
        fill(&db, &execution_id, &chore_id, ProposalKind::Blocked, 3);
        fill(&db, &execution_id, &chore_id, ProposalKind::Attention, 2);

        let counts = db
            .count_worker_proposals_for_execution(&execution_id, ProposalKind::Blocked)
            .unwrap();
        assert_eq!(counts.total, 5);
        assert_eq!(counts.for_kind, 3);
    }

    #[test]
    fn counting_an_execution_with_no_proposals_is_zero_not_an_error() {
        let (_dir, db) = open_db();
        let (execution_id, _) = execution_for_new_chore(&db, "Cleanup");
        let counts = db
            .count_worker_proposals_for_execution(&execution_id, ProposalKind::Blocked)
            .unwrap();
        assert_eq!(counts, ProposalCounts { total: 0, for_kind: 0 });
    }
}
