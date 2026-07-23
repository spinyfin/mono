// Behaviour tests for the worker proposal RPC surface: `SubmitProposal`
// and `ListProposals` dispatched into `app::proposals`.
//
// Every assertion goes request-in / response-out: a `FrontendRequest` is
// handed to its handler and the `FrontendEvent` it sends back is what gets
// checked. Nothing asserts which `WorkDb` method a handler called — the
// storage layer has its own direct coverage in `work/proposals.rs`, and the
// payload schemas in the `proposal-validation` crate. What is unique to this
// layer, and therefore what is tested here, is the join of the three:
// attribution from the socket peer, validation refusals reaching the caller
// as typed errors, and idempotency/rate-cap outcomes rendered as events.
//
// Attribution is exercised with the test process's own pid registered as a
// worker: `is_descendant_of_any` / `lookup_with_ancestor_walk` both treat a
// pid as its own ancestor, so registering `std::process::id()` makes this
// process look exactly like a worker session to the handler — the same trick
// `t03` and `trust_authorization` use.

use super::*;
use crate::app::proposals;
use boss_protocol::{
    PROPOSAL_CAP_PER_KIND_PER_EXECUTION, ProposalErrorCode, ProposalKind, ProposalState, ProposalSubmissionError,
    WorkerProposal,
};
use serde_json::{Value, json};

// ── Fixtures ─────────────────────────────────────────────────────────────────

/// A product, chore, and `ready` execution, with this process registered as
/// the worker running that execution — i.e. the state a live worker session
/// presents to the engine.
struct WorkerFixture {
    server_state: Arc<ServerState>,
    _dir: tempfile::TempDir,
    execution_id: String,
    work_item_id: String,
    peer_pid: libc::pid_t,
}

impl WorkerFixture {
    fn new() -> Self {
        let (server_state, dir) = test_server_state();
        let (execution_id, work_item_id) = new_execution(&server_state, "Cleanup");
        let peer_pid = std::process::id() as libc::pid_t;
        server_state.worker_registry.register(peer_pid, execution_id.clone());
        Self {
            server_state,
            _dir: dir,
            execution_id,
            work_item_id,
            peer_pid,
        }
    }
}

/// Create a chore under a fresh product plus a `ready` execution for it,
/// returning `(execution_id, work_item_id)`.
fn new_execution(server_state: &Arc<ServerState>, chore_name: &str) -> (String, String) {
    let db = &server_state.work_db;
    let product = crate::test_support::create_test_product(db);
    let chore = crate::test_support::create_test_chore(db, product.id, chore_name);
    let execution = crate::test_support::create_ready_chore_execution(db, chore.id.clone());
    (execution.id, chore.id)
}

/// Build a per-request `Dispatch` the way `handle_frontend_connection` does
/// for a real socket frame, with `peer_pid` standing in for `SO_PEERCRED`.
fn dispatch_with_peer(state: &Arc<ServerState>, sink: &Arc<SessionSink>, peer_pid: Option<libc::pid_t>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
        .maybe_peer_pid(peer_pid)
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

/// The single response a handler enqueued. Closes the sink first so a
/// handler that replied with nothing surfaces as a panic rather than hanging
/// until Bazel's timeout.
async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

/// Drive one proposal verb through its handler and return the reply.
/// Mirrors `app.rs`'s dispatch table for these two verbs.
async fn call_with_peer(
    state: &Arc<ServerState>,
    peer_pid: Option<libc::pid_t>,
    req: FrontendRequest,
) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = dispatch_with_peer(state, &sink, peer_pid);
    match req {
        r @ FrontendRequest::SubmitProposal { .. } => proposals::handle_submit_proposal(ctx, r).await,
        r @ FrontendRequest::ListProposals { .. } => proposals::handle_list_proposals(ctx, r).await,
        other => panic!("not a proposal verb: {other:?}"),
    }
    sole_response(&sink).await
}

fn submit_request(run_id: &str, kind: ProposalKind, payload: Value) -> FrontendRequest {
    FrontendRequest::SubmitProposal {
        run_id: run_id.to_owned(),
        kind,
        payload,
        idempotency_key: None,
    }
}

fn submit_request_keyed(run_id: &str, kind: ProposalKind, payload: Value, key: &str) -> FrontendRequest {
    FrontendRequest::SubmitProposal {
        run_id: run_id.to_owned(),
        kind,
        payload,
        idempotency_key: Some(key.to_owned()),
    }
}

async fn submit(fx: &WorkerFixture, kind: ProposalKind, payload: Value) -> FrontendEvent {
    call_with_peer(
        &fx.server_state,
        Some(fx.peer_pid),
        submit_request(&fx.execution_id, kind, payload),
    )
    .await
}

// ── Response accessors ───────────────────────────────────────────────────────

/// The `(proposal, already_submitted)` pair from a successful submission, or
/// a panic naming what came back instead — so a refusal reads as its own
/// error text rather than a bare pattern-match failure.
fn submitted(event: FrontendEvent) -> (WorkerProposal, bool) {
    match event {
        FrontendEvent::ProposalSubmitted {
            proposal,
            already_submitted,
        } => (proposal, already_submitted),
        FrontendEvent::ProposalRejected { error } => {
            panic!("expected a successful submission, got rejection: {error}")
        }
        other => panic!("expected ProposalSubmitted, got {other:?}"),
    }
}

fn rejected(event: FrontendEvent) -> ProposalSubmissionError {
    match event {
        FrontendEvent::ProposalRejected { error } => error,
        FrontendEvent::ProposalSubmitted { proposal, .. } => {
            panic!("expected a rejection, got a stored proposal {}", proposal.id)
        }
        other => panic!("expected ProposalRejected, got {other:?}"),
    }
}

fn listed(event: FrontendEvent) -> (String, Vec<WorkerProposal>) {
    match event {
        FrontendEvent::ProposalsList {
            work_item_id,
            proposals,
        } => (work_item_id, proposals),
        FrontendEvent::ProposalRejected { error } => panic!("expected a listing, got rejection: {error}"),
        other => panic!("expected ProposalsList, got {other:?}"),
    }
}

// ── Happy path ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn submission_persists_a_proposed_row_attributed_to_the_caller() {
    let fx = WorkerFixture::new();

    let (proposal, already) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "stuck"})).await);

    assert!(!already);
    assert!(proposal.id.starts_with("prp_"), "{}", proposal.id);
    assert_eq!(proposal.execution_id, fx.execution_id);
    assert_eq!(proposal.work_item_id.as_deref(), Some(fx.work_item_id.as_str()));
    // No apply pipeline in this PR: everything stays `proposed`.
    assert_eq!(proposal.state, ProposalState::Proposed);
    assert_eq!(proposal.applied_ref, None);
    assert_eq!(proposal.decided_by, None);
}

/// The stored payload is the validation layer's canonical form, not the raw
/// bytes the caller sent — so the apply pipeline can deserialise it directly.
#[tokio::test]
async fn stored_payload_is_canonicalised() {
    let fx = WorkerFixture::new();
    let (proposal, _) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "  padded  "})).await);
    assert_eq!(proposal.payload_json, r#"{"reason":"padded"}"#);
}

/// Every v1 kind must be submittable — a kind the engine accepts on the wire
/// but cannot store would be a dead verb the CLI still advertises.
#[tokio::test]
async fn every_kind_can_be_submitted() {
    let fx = WorkerFixture::new();
    for &kind in ProposalKind::ALL {
        let payload = match kind {
            ProposalKind::Attention => json!({"title": "T", "body_markdown": "B"}),
            ProposalKind::EffortEscalation => json!({"requested_level": "large", "reason": "R"}),
            ProposalKind::Blocked => json!({"reason": "R"}),
            ProposalKind::DeferredScope => json!({"summary": "S", "reason": "R"}),
            ProposalKind::FollowupTask => {
                json!({"proposed_name": "N", "proposed_description": "D", "rationale": "R"})
            }
            ProposalKind::AutomationOutcome => json!({"outcome": "skip", "reason": "clean"}),
            ProposalKind::PrCreated => json!({"pr_url": "https://github.com/o/r/pull/1"}),
        };
        let (proposal, _) = submitted(submit(&fx, kind, payload).await);
        assert_eq!(proposal.kind, kind);
    }
}

// ── Validation ───────────────────────────────────────────────────────────────

/// The property the whole design turns on: a malformed submission comes back
/// as a typed, field-scoped error *during the run*, so the worker can fix it
/// and retry — rather than failing a transcript scrape at Stop.
#[tokio::test]
async fn invalid_payload_is_refused_with_field_level_detail() {
    let fx = WorkerFixture::new();

    let error = rejected(submit(&fx, ProposalKind::Blocked, json!({"resaon": "typo"})).await);

    assert_eq!(error.code, ProposalErrorCode::ValidationFailed);
    let fields: Vec<&str> = error.field_errors.iter().map(|e| e.field.as_str()).collect();
    assert!(fields.contains(&"reason"), "{fields:?}");
    assert!(fields.contains(&"resaon"), "{fields:?}");
}

/// A refused submission must leave no trace: the worker retries the fixed
/// command, and a half-written row would make that retry look like a
/// duplicate.
#[tokio::test]
async fn a_refused_submission_stores_nothing() {
    let fx = WorkerFixture::new();
    rejected(submit(&fx, ProposalKind::Blocked, json!({"reason": ""})).await);

    let (_, proposals) = listed(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            FrontendRequest::ListProposals {
                run_id: fx.execution_id.clone(),
                kind: None,
                state: None,
            },
        )
        .await,
    );
    assert!(proposals.is_empty(), "a rejected submission must not persist a row");
}

/// Fix-and-retry is the documented remediation, so it has to actually work
/// within the same run.
#[tokio::test]
async fn a_worker_can_fix_and_retry_after_a_validation_error() {
    let fx = WorkerFixture::new();
    rejected(
        submit(
            &fx,
            ProposalKind::EffortEscalation,
            json!({"requested_level": "enormous", "reason": "R"}),
        )
        .await,
    );

    let (proposal, _) = submitted(
        submit(
            &fx,
            ProposalKind::EffortEscalation,
            json!({"requested_level": "large", "reason": "R"}),
        )
        .await,
    );
    assert_eq!(proposal.kind, ProposalKind::EffortEscalation);
}

// ── Idempotency ──────────────────────────────────────────────────────────────

/// Resubmitting identical content returns the existing row with
/// `already_submitted`, rather than erroring or duplicating.
#[tokio::test]
async fn identical_resubmission_is_idempotent() {
    let fx = WorkerFixture::new();
    let (first, first_already) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "stuck"})).await);
    let (replay, replay_already) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "stuck"})).await);

    assert!(!first_already);
    assert!(replay_already, "a replay must report already_submitted");
    assert_eq!(replay.id, first.id);
}

/// The derived key is content-addressed, so a *different* proposal of the
/// same kind is a new row — idempotency must not collapse distinct
/// submissions into one.
#[tokio::test]
async fn different_content_is_a_new_proposal() {
    let fx = WorkerFixture::new();
    let (first, _) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "stuck"})).await);
    let (second, already) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "differently stuck"})).await);

    assert!(!already);
    assert_ne!(first.id, second.id);
}

/// An explicit `--idempotency-key` overrides the derived one: two different
/// payloads under the same key collapse onto the first row. This is what
/// lets a caller declare "these are the same proposal" when the content
/// changes between retries.
#[tokio::test]
async fn an_explicit_key_overrides_content_addressing() {
    let fx = WorkerFixture::new();
    let (first, _) = submitted(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(
                &fx.execution_id,
                ProposalKind::Blocked,
                json!({"reason": "one"}),
                "my-key",
            ),
        )
        .await,
    );
    let (replay, already) = submitted(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(
                &fx.execution_id,
                ProposalKind::Blocked,
                json!({"reason": "two"}),
                "my-key",
            ),
        )
        .await,
    );

    assert!(already);
    assert_eq!(replay.id, first.id);
    assert_eq!(replay.payload_json, first.payload_json, "the stored row is unchanged");
    assert_eq!(first.idempotency_key, "my-key");
}

/// An unset shell variable expands to an empty string. Treating that as a
/// real key would make every keyless submission from a run collide on `""`
/// and silently return the first one forever.
#[tokio::test]
async fn a_blank_explicit_key_falls_back_to_the_derived_one() {
    let fx = WorkerFixture::new();
    let (first, _) = submitted(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(&fx.execution_id, ProposalKind::Blocked, json!({"reason": "one"}), "   "),
        )
        .await,
    );
    let (second, already) = submitted(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(&fx.execution_id, ProposalKind::Blocked, json!({"reason": "two"}), ""),
        )
        .await,
    );

    assert!(!already, "distinct content must not collide on a blank key");
    assert_ne!(first.id, second.id);
    assert!(first.idempotency_key.starts_with("auto:"), "{}", first.idempotency_key);
}

// ── Attribution ──────────────────────────────────────────────────────────────

/// The cross-check: `BOSS_RUN_ID` can only make a call fail, never grant it
/// anything. A worker claiming another run's id is refused rather than
/// filing against that run's work item.
#[tokio::test]
async fn a_run_id_that_disagrees_with_the_peer_is_refused() {
    let fx = WorkerFixture::new();
    let (other_execution, _) = new_execution(&fx.server_state, "Someone else's chore");

    let error = rejected(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request(&other_execution, ProposalKind::Blocked, json!({"reason": "stuck"})),
        )
        .await,
    );

    assert_eq!(error.code, ProposalErrorCode::AttributionMismatch);
    assert!(error.message.contains(&other_execution), "{}", error.message);
    assert!(error.message.contains(&fx.execution_id), "{}", error.message);

    // And nothing was written against the run it tried to claim.
    let stored = fx
        .server_state
        .work_db
        .count_worker_proposals_for_execution(&other_execution, ProposalKind::Blocked)
        .unwrap();
    assert_eq!(stored.total, 0);
}

/// The mismatch check must also guard the read verb — otherwise a worker
/// could enumerate another work item's proposals by passing its run id.
#[tokio::test]
async fn listing_with_a_mismatched_run_id_is_refused() {
    let fx = WorkerFixture::new();
    let (other_execution, _) = new_execution(&fx.server_state, "Someone else's chore");

    let error = rejected(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            FrontendRequest::ListProposals {
                run_id: other_execution,
                kind: None,
                state: None,
            },
        )
        .await,
    );
    assert_eq!(error.code, ProposalErrorCode::AttributionMismatch);
}

/// v1 rejects remote workers outright: with no local peer pid there is no
/// verified identity to attribute a proposal to.
#[tokio::test]
async fn a_connection_with_no_peer_pid_is_refused() {
    let fx = WorkerFixture::new();

    let error = rejected(
        call_with_peer(
            &fx.server_state,
            None,
            submit_request(&fx.execution_id, ProposalKind::Blocked, json!({"reason": "stuck"})),
        )
        .await,
    );
    assert_eq!(error.code, ProposalErrorCode::NoLocalPeer);
}

/// Attribution fails closed: a local caller that is not a worker (the human's
/// own shell, a stray script) cannot submit, even with a real run id.
#[tokio::test]
async fn a_peer_with_no_registered_worker_ancestry_is_refused() {
    let (server_state, _dir) = test_server_state();
    let (execution_id, _) = new_execution(&server_state, "Cleanup");
    // Register some *other* pid as the only worker, so the ancestor walk
    // from our peer finds nothing.
    server_state.worker_registry.register(i32::MAX, execution_id.clone());

    let error = rejected(
        call_with_peer(
            &server_state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&execution_id, ProposalKind::Blocked, json!({"reason": "stuck"})),
        )
        .await,
    );
    assert_eq!(error.code, ProposalErrorCode::AttributionUnresolved);
}

/// A registry entry pointing at an execution the DB no longer has gets its
/// own code — the caller cannot fix it, so conflating it with a payload or
/// attribution problem would send the worker chasing the wrong thing.
#[tokio::test]
async fn a_peer_resolving_to_a_pruned_execution_is_refused() {
    let (server_state, _dir) = test_server_state();
    let peer_pid = std::process::id() as libc::pid_t;
    server_state.worker_registry.register(peer_pid, "exec_gone".to_owned());

    let error = rejected(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request("exec_gone", ProposalKind::Blocked, json!({"reason": "stuck"})),
        )
        .await,
    );
    assert_eq!(error.code, ProposalErrorCode::UnknownExecution);
}

// ── Rate caps ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_per_kind_cap_is_enforced_with_a_typed_error() {
    let fx = WorkerFixture::new();
    for i in 0..PROPOSAL_CAP_PER_KIND_PER_EXECUTION {
        submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": format!("n{i}")})).await);
    }

    let error = rejected(submit(&fx, ProposalKind::Blocked, json!({"reason": "one too many"})).await);
    assert_eq!(error.code, ProposalErrorCode::RateLimited);

    // The cap is per kind, so another kind still has its own budget.
    submitted(submit(&fx, ProposalKind::DeferredScope, json!({"summary": "S", "reason": "R"})).await);
}

/// A replay at an exhausted cap must still succeed — otherwise a worker that
/// spends its budget and then retries an earlier command (a dropped reply, a
/// resumed run re-running its script) is rate-limited for work already done.
#[tokio::test]
async fn a_replay_at_the_cap_still_succeeds() {
    let fx = WorkerFixture::new();
    let mut first_id = None;
    for i in 0..PROPOSAL_CAP_PER_KIND_PER_EXECUTION {
        let (proposal, _) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": format!("n{i}")})).await);
        first_id.get_or_insert(proposal.id);
    }
    rejected(submit(&fx, ProposalKind::Blocked, json!({"reason": "new content"})).await);

    let (replay, already) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "n0"})).await);
    assert!(already);
    assert_eq!(Some(replay.id), first_id);
}

// ── Listing ──────────────────────────────────────────────────────────────────

/// The read a successor run depends on: proposals from *every* execution of
/// the work item, with prior dispositions attached, so a resumed run sees
/// "rejected: duplicate of T123" and adjusts instead of re-proposing.
#[tokio::test]
async fn listing_spans_prior_executions_and_carries_dispositions() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.clone();
    let product = crate::test_support::create_test_product(&db);
    let chore = crate::test_support::create_test_chore(&db, product.id, "Cleanup");
    let first = crate::test_support::create_ready_chore_execution(&db, chore.id.clone());
    let second = crate::test_support::create_ready_chore_execution(&db, chore.id.clone());
    let peer_pid = std::process::id() as libc::pid_t;

    // The predecessor run files a followup, which is later rejected.
    server_state.worker_registry.register(peer_pid, first.id.clone());
    let (old, _) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &first.id,
                ProposalKind::FollowupTask,
                json!({"proposed_name": "N", "proposed_description": "D", "rationale": "R"}),
            ),
        )
        .await,
    );
    db.connect()
        .unwrap()
        .execute(
            "UPDATE worker_proposals SET state = 'rejected', decided_by = 'human',
             decision_reason = 'duplicate of T123', decided_at = '1747000000' WHERE id = ?1",
            rusqlite::params![old.id],
        )
        .unwrap();

    // The successor run takes over the same work item and lists.
    server_state.worker_registry.register(peer_pid, second.id.clone());
    let (work_item_id, proposals) = listed(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            FrontendRequest::ListProposals {
                run_id: second.id.clone(),
                kind: None,
                state: None,
            },
        )
        .await,
    );

    assert_eq!(work_item_id, chore.id, "scope is the work item, not the execution");
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].execution_id, first.id, "the predecessor's row is visible");
    assert_eq!(proposals[0].state, ProposalState::Rejected);
    assert_eq!(proposals[0].decision_reason.as_deref(), Some("duplicate of T123"));
}

/// Another work item's proposals must never appear: the scope is derived
/// from the caller's attributed execution, so there is no field to widen it.
#[tokio::test]
async fn listing_never_leaks_another_work_items_proposals() {
    let fx = WorkerFixture::new();
    let (other_execution, other_item) = new_execution(&fx.server_state, "Someone else's chore");
    fx.server_state
        .work_db
        .submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
            execution_id: &other_execution,
            work_item_id: &other_item,
            kind: ProposalKind::Blocked,
            payload_json: r#"{"reason":"theirs"}"#,
            idempotency_key: "theirs",
        })
        .unwrap()
        .unwrap();
    submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "mine"})).await);

    let (work_item_id, proposals) = listed(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            FrontendRequest::ListProposals {
                run_id: fx.execution_id.clone(),
                kind: None,
                state: None,
            },
        )
        .await,
    );

    assert_eq!(work_item_id, fx.work_item_id);
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].payload_json, r#"{"reason":"mine"}"#);
}

#[tokio::test]
async fn listing_honours_the_kind_and_state_filters() {
    let fx = WorkerFixture::new();
    submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "stuck"})).await);
    submitted(submit(&fx, ProposalKind::DeferredScope, json!({"summary": "S", "reason": "R"})).await);

    let list = |kind, state| {
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            FrontendRequest::ListProposals {
                run_id: fx.execution_id.clone(),
                kind,
                state,
            },
        )
    };

    let (_, blocked) = listed(list(Some(ProposalKind::Blocked), None).await);
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0].kind, ProposalKind::Blocked);

    let (_, proposed) = listed(list(None, Some(ProposalState::Proposed)).await);
    assert_eq!(proposed.len(), 2);

    let (_, applied) = listed(list(None, Some(ProposalState::Applied)).await);
    assert!(applied.is_empty(), "nothing is applied in this PR");
}

#[tokio::test]
async fn listing_an_execution_with_no_proposals_is_empty_not_an_error() {
    let fx = WorkerFixture::new();
    let (_, proposals) = listed(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            FrontendRequest::ListProposals {
                run_id: fx.execution_id.clone(),
                kind: None,
                state: None,
            },
        )
        .await,
    );
    assert!(proposals.is_empty());
}
