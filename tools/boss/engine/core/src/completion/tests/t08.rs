//! Split out of `completion.rs`'s `#[cfg(test)] mod tests`.
//! Test functions only; shared fixtures, stubs, and helpers live
//! in the parent [`super`] module (`completion/tests.rs`).
//!
//! Whether a Stop boundary may read a bound PR's health as evidence that
//! *this run* delivered — the satisfied-deliverable gate's contribution
//! contract.
//!
//! A `revision_implementation` is dispatched INTO an open, mergeable,
//! CI-clean parent PR; that is the definition of the state a reviewer
//! pass hands it. So "the PR looks healthy" is true from the instant the
//! run starts and says nothing about what the run did. These tests pin
//! the resulting rule: a revision the SHA-delta gate proved did not move
//! the head must not terminalize as delivered, while the states that are
//! genuine evidence — a merged PR, a merge-queue acceptance, a cleared
//! conflict, or the worker's own `NO_CHANGES_NEEDED` declaration — must
//! keep working. See `super::super::health_alone_satisfies_deliverable`.

use super::*;

// -----------------------------------------------------------
// The revision-finalized-as-delivered-having-delivered-nothing
// incident. A `revision_implementation` was marked delivered and torn
// down 78 s in, having produced no code, no commit and no push, while
// the worker was still alive and mid-research. 1.7 s earlier the engine
// had logged `sha-delta gate: bound PR head unchanged — worker did not
// contribute`; the satisfied-deliverable gate finalized anyway, because
// its "PR open, mergeable, CI clean" predicate is the state a revision
// is DISPATCHED INTO and is therefore trivially true at t=0.
//
// The `sha_unchanged` finding used to be spent only on skipping the
// reviewer pass (`pr_review noop skip`) and on the nudge fingerprint. It
// is now load-bearing on the finalize decision — see
// `health_alone_satisfies_deliverable`.
// -----------------------------------------------------------

#[tokio::test]
async fn on_stop_refuses_to_finalize_revision_that_contributed_nothing() {
    // The incident, reduced: revision worker stops without pushing
    // (NoContribution — head byte-identical to the run-start snapshot),
    // bound PR open with CI green and no conflict. Finalizing here would
    // claim a delivery that demonstrably did not happen.
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1490";
    let head = "abcdef1111111111111111111111111111111111";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    // SHA unchanged → SHA-delta gate returns NoContribution.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;
    // PR is open, CI clean, no conflict — exactly the state the revision
    // was dispatched into, unchanged by the run.
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus::clean())));

    let detector = StubPrDetector::ok(None);
    let TestHarness {
        handler,
        cube,
        pane,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier).with_merge_probe(probe);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        !matches!(outcome, StopOutcome::DeliverableSatisfied { .. }),
        "a revision whose bound PR head did not move must NOT be finalized as delivered on the \
         strength of PR health that predates the run; got {outcome:?}",
    );
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "the run must fall through to the bounded nudge path instead; got {outcome:?}",
    );
    // The revision task must not advance — nothing was delivered.
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(
            t.status,
            TaskStatus::Active,
            "a revision that contributed nothing must stay in Doing, not advance to in_review",
        ),
        other => panic!("expected task, got {other:?}"),
    }
    // The worker must NOT be terminalized or reaped — in the incident it
    // was still alive and mid-research when the engine tore it down.
    let exec = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        exec.status,
        ExecutionStatus::WaitingHuman,
        "the execution must stay live so the worker can finish and push",
    );
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "no lease release: the run has not finished",
    );
    assert!(
        pane.calls.lock().await.is_empty(),
        "no pane teardown: the worker is still working",
    );
    // And it is nudged to push to the existing PR, not told to create one.
    let queued = probes.snapshot();
    assert_eq!(queued.len(), 1, "exactly one nudge must fire; got {queued:?}");
    assert_eq!(
        queued[0].1,
        probe_push_to_existing_pr(parent_pr_url),
        "a revision with a bound PR must be nudged to push to it, never to open one",
    );
}

#[tokio::test]
async fn revision_that_contributed_nothing_is_left_alone_while_background_work_is_live() {
    // The second signal the incident ignored: the worker process was
    // still running (the reaper logged `the run's recorded process is
    // ALIVE`) and its turn had ended only to wait on a backgrounded
    // subagent (`pendingBackgroundAgentCount: 1`).
    //
    // `nudge_or_park` already knows how to handle that — it suppresses
    // before the breaker is even consulted. The bug was that the
    // satisfied-deliverable gate terminalized the run before that
    // machinery could ever be reached. With the gate refusing, a
    // still-working revision is now simply left alone.
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1492";
    let head = "abcdef2222222222222222222222222222222222";
    let (_dir, db, _product_id, _revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus::clean())));

    let detector = StubPrDetector::ok(None);
    let TestHarness {
        handler,
        cube,
        pane,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(probe)
        .with_background_activity_probe(Arc::new(FixedDescendantProbe(1)));

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(
            outcome,
            StopOutcome::BackgroundChildrenPending {
                descendant_count: 1,
                ..
            }
        ),
        "a revision still waiting on a live background child must be left alone, not finalized \
         and not nudged; got {outcome:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "a worker waiting on background work must not be nudged; got {:?}",
        probes.snapshot(),
    );
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::WaitingHuman,
        "the live worker must not be terminalized",
    );
    assert!(cube.release_calls.lock().await.is_empty());
    assert!(pane.calls.lock().await.is_empty());
}

#[tokio::test]
async fn revision_declaring_no_changes_needed_closes_without_claiming_delivery() {
    // The honest terminal for a revision that genuinely has nothing to
    // do. It is the WORKER's explicit claim (the sanctioned
    // NO_CHANGES_NEEDED marker), not an inference the engine draws from a
    // PR whose health predates the run — and it closes the revision
    // without stamping a pr_url, while filing the attention item that
    // makes the unaddressed finding visible to a human.
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1493";
    let head = "abcdef3333333333333333333333333333333333";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "## Summary\nThe finding is already handled by the existing guard clause.\n\nNO_CHANGES_NEEDED\n",
    );
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus::clean())));

    let detector = StubPrDetector::ok(None);
    let TestHarness {
        handler,
        cube,
        pane,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier).with_merge_probe(probe);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::NoChangesNeeded { ref work_item_id } if work_item_id == &revision_id),
        "an explicit NO_CHANGES_NEEDED from a revision must close it as a declared no-op; \
         got {outcome:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "a declared no-op must not be nudged; got {:?}",
        probes.snapshot(),
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Done, "a declared no-op closes the revision");
            assert!(
                t.pr_url.is_none(),
                "a no-op produced no PR — none may be fabricated onto the revision",
            );
        }
        other => panic!("expected task, got {other:?}"),
    }
    // The dismissed finding must be visible to a human, not silently lost.
    let items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        items.iter().any(|i| i.kind == REVISION_NO_OP_ATTENTION_KIND),
        "closing a revision without addressing its finding must file an attention item; got {items:?}",
    );
    // Slot released — this is a real terminal, not a park.
    let exec = db.get_execution(&execution_id).unwrap();
    assert_eq!(exec.status, ExecutionStatus::Completed);
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
}

#[tokio::test]
async fn revision_that_contributed_nothing_still_finalizes_when_bound_pr_is_merged() {
    // Guards against over-correcting. A merged PR is not "health that
    // predates the run" — it is proof the deliverable has passed out of
    // the worker's hands entirely: there is nothing left to push to, and
    // nudging it to try would be actively wrong. This arm must keep
    // finalizing even with the SHA-delta gate reporting NoContribution.
    use crate::merge_poller::PrLifecycleState;

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1494";
    let head = "abcdef4444444444444444444444444444444444";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Merged));

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier).with_merge_probe(probe);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::DeliverableSatisfied { ref pr_url } if pr_url == parent_pr_url),
        "an already-merged bound PR must still finalize a no-push revision; got {outcome:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "nothing can be pushed to a merged PR — no nudge may fire; got {:?}",
        probes.snapshot(),
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Done),
        other => panic!("expected task, got {other:?}"),
    }
}

// -----------------------------------------------------------
// The satisfied-deliverable guard was restructured from
// `(ci_clean || merge_conflict_revision || queued_for_merge)` to
// `(merge_conflict_revision || queued_for_merge || (ci_clean &&
// health_alone_satisfies))`. `health_alone_satisfies` is false for every
// revision reaching this gate with `ProvenAbsent` evidence, so the
// merge-conflict and merge-queue short-circuits are the only thing still
// keeping those two arms alive for a no-push revision. Pin both directly
// so a future edit that quietly drops a disjunct fails a test instead of
// silently stranding a resolved-conflict or queued-for-merge revision.
// -----------------------------------------------------------

#[tokio::test]
async fn merge_conflict_revision_with_proven_absent_still_finalizes_despite_dirty_ci() {
    use crate::merge_poller::{OpenPrCiStatus, OpenPrMergeability, OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1980";
    let head = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let (_dir, db, _product_id, parent_chore_id, revision_id, execution_id, attempt_id) =
        conflict_revision_fixture(workspace.path(), parent_pr_url, head);

    // The conflict is already resolved from the engine's point of view —
    // ledger retired and parent unblocked — so `try_retire_cleared_blocking_signal`
    // finds no live attempt and falls through to the satisfied-deliverable
    // gate, exactly like the merge-poller race this fixture models.
    db.mark_conflict_resolution_succeeded(&attempt_id, None).unwrap();
    db.clear_chore_blocked_merge_conflict_for_attempt(&parent_chore_id, parent_pr_url, &attempt_id)
        .unwrap();

    let detector = StubPrDetector::ok(None);
    // SHA-delta gate: head unchanged since dispatch → ProvenAbsent.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;
    // Mergeable, but CI is NOT clean — this arm must not require it.
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus {
        mergeability: OpenPrMergeability::Clean,
        ci: OpenPrCiStatus::InFlight,
    })));

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier).with_merge_probe(probe);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::DeliverableSatisfied { ref pr_url } if pr_url == parent_pr_url),
        "a merge-conflict-provenance revision with ProvenAbsent evidence must still finalize on \
         mergeability alone — the merge_conflict_revision disjunct must survive the guard \
         restructure regardless of CI state; got {outcome:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "no nudge must fire once the conflict-cleared arm accepts; got {:?}",
        probes.snapshot(),
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected task, got {other:?}"),
    }
}

#[tokio::test]
async fn queued_for_merge_revision_with_proven_absent_still_finalizes_despite_dirty_ci() {
    use crate::merge_poller::{OpenPrMergeability, OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1981";
    let head = "cccccccccccccccccccccccccccccccccccccccc";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);

    let detector = StubPrDetector::ok(None);
    // SHA-delta gate: head unchanged since dispatch → ProvenAbsent.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    struct QueuedForMergeProbe;
    #[async_trait::async_trait]
    impl MergeProbe for QueuedForMergeProbe {
        async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
            Ok(PrLifecycleProbe::builder()
                .url(url.to_owned())
                .state(PrLifecycleState::Open(OpenPrStatus {
                    mergeability: OpenPrMergeability::Clean,
                    ci: OpenPrCiStatus::InFlight,
                }))
                .labels(Vec::new())
                .review(crate::merge_poller::PrReviewState::Unknown)
                .in_merge_queue(true)
                .merge_queue_entry_state("AWAITING_CHECKS")
                .build())
        }
    }

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(QueuedForMergeProbe));

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::DeliverableSatisfied { ref pr_url } if pr_url == parent_pr_url),
        "a revision with ProvenAbsent evidence whose PR is already accepted into the merge queue \
         (entry state not UNMERGEABLE) must still finalize — the queued_for_merge disjunct must \
         survive the guard restructure regardless of CI state; got {outcome:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "no nudge must fire once the merge-queue arm accepts; got {:?}",
        probes.snapshot(),
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected task, got {other:?}"),
    }
}
