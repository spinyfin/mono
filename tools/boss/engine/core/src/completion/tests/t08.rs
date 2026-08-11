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
        ExecutionStatus::Running,
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
        ExecutionStatus::Running,
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

// -----------------------------------------------------------
// Binding vs finalization arming (incident-004 / PR URL split).
// A `gh pr view|edit` observation binds without arming; the recheck must
// still reach recovery paths, and Stop must not finalize a do-nothing
// revision on that bound URL alone.
// -----------------------------------------------------------

/// A URL observed from `gh pr view` binds the execution, but does not prove
/// this worker published anything. The recheck must not finalize/reap via
/// the staged arm — yet must still fall through to the cold-path recovery
/// path (an unarmed entry must not disable the whole recheck).
#[tokio::test]
async fn recheck_for_pr_binding_only_url_does_not_finalize_or_reap() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let pr_url = "https://github.com/spinyfin/mono/pull/459";
    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    // Drive the same path the dispatcher uses: feed → extract → gates → record.
    let feed = crate::driver::default_pr_url_capture_feed(
        "Bash",
        &serde_json::json!({ "command": "gh pr view 459" }),
        &serde_json::json!({
            "stdout": format!("{pr_url}\n"),
            "stderr": "",
        }),
    )
    .expect("view feed");
    let captured = crate::pr_url_capture::extract_pr_url_from_text(&feed.output_text).expect("url");
    assert!(crate::pr_url_capture::is_pr_url_binding_command_str(&feed.command));
    assert!(!crate::pr_url_capture::is_pr_url_finalization_command_str(
        &feed.command
    ));
    staged_pr_urls.record_command_observation(
        &execution_id,
        &captured,
        crate::pr_url_capture::is_pr_url_finalization_command_str(&feed.command),
    );

    // Detector is allowed to run (cold-path recovery); it fails here so we
    // observe DetectorFailed rather than a staged finalize.
    let detector = StubPrDetector::err("cold path retried; no PR yet");
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), detector.clone());
    let outcome = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .recheck_for_pr(&execution_id)
        .await;

    assert_eq!(
        outcome,
        StopOutcome::DetectorFailed,
        "unarmed binding must fall through to the cold-path detector, not short-circuit the recheck"
    );
    assert_eq!(staged_pr_urls.get(&execution_id).as_deref(), Some(pr_url));
    assert!(
        !staged_pr_urls
            .get_entry(&execution_id)
            .expect("bound URL")
            .finalization_armed,
        "a view/edit observation must bind without arming finalization"
    );
    assert_eq!(
        detector.call_count(),
        1,
        "binding-only URLs must still reach the cold-path detector for recovery"
    );
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "binding-only URLs must not reap the worker"
    );
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => assert!(t.pr_url.is_none(), "the task waits for publish evidence"),
        other => panic!("expected chore, got {other:?}"),
    }
}

/// Stop must not finalize a revision on a binding-only staged URL (parent PR
/// inspected via `gh pr view` with no push). Without the finalization_armed
/// gate on the Stop staged arm, verified_staged_pr_url would return the
/// parent PR (revision branch matches) and stop_staged would advance the
/// revision to in_review with no push.
#[tokio::test]
async fn on_stop_binding_only_url_does_not_finalize_revision() {
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1342";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);

    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    let feed = crate::driver::default_pr_url_capture_feed(
        "Bash",
        &serde_json::json!({ "command": "gh pr view 1342" }),
        &serde_json::json!({
            "stdout": format!("{parent_pr_url}\n"),
            "stderr": "",
        }),
    )
    .expect("view feed");
    let captured = crate::pr_url_capture::extract_pr_url_from_text(&feed.output_text).expect("url");
    staged_pr_urls.record_command_observation(
        &execution_id,
        &captured,
        crate::pr_url_capture::is_pr_url_finalization_command_str(&feed.command),
    );
    assert!(!staged_pr_urls.get_entry(&execution_id).unwrap().finalization_armed);

    // Branch verification would accept the parent PR for a revision — that is
    // exactly the failure mode if Stop ignores finalization_armed.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head_before.into())).await;
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus::clean())));

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), detector);
    let outcome = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(verifier)
        .with_merge_probe(probe)
        .on_stop(&execution_id)
        .await;

    assert!(
        !matches!(
            outcome,
            StopOutcome::DeliverableSatisfied { .. }
                | StopOutcome::PrDetected { .. }
                | StopOutcome::ReviewerEnqueued { .. }
        ),
        "binding-only must not finalize via stop_staged; got {outcome:?}"
    );
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "binding-only Stop must not reap the revision worker"
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected revision task, got {other:?}"),
    }
}

// -----------------------------------------------------------
// Incident-004 AI-3: four-case contribution gate on finalize.
//
// A revision may terminalize to PendingReview / InReview only when the
// engine can name which signal allowed it:
//   1. head SHA moved (pass)
//   2. metadata-only confirmation (pass)
//   3. explicit NO_CHANGES_NEEDED (pass — here via finalize_pr_transition)
//   4. silence after mid-turn reap (no push, no metadata, no noop) → refuse
// -----------------------------------------------------------

/// Case 1: head moved → finalize_pr_transition may advance to InReview.
#[tokio::test]
async fn revision_contribution_gate_allows_head_sha_moved() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/2601";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let head_after = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);

    let verifier = StubBranchVerifier::ok("boss/parent");
    verifier.set_head_oid(Ok(head_after.to_owned())).await;
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler.with_branch_verifier(verifier);

    let outcome = handler
        .finalize_pr_transition(
            &execution_id,
            parent_pr_url.to_owned(),
            WorkerPrCompletionTarget::InReview,
            "pr_recheck_staged",
        )
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "head SHA movement must allow InReview terminalization; got {outcome:?}",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview, "contributing revision reaches review");
        }
        other => panic!("expected task, got {other:?}"),
    }
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
}

/// Case 2: metadata-fix confirmed, head unchanged → still allowed.
#[tokio::test]
async fn revision_contribution_gate_allows_metadata_fix_confirmed() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/2602";
    let head = "cccccccccccccccccccccccccccccccccccccccc";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    db.mark_execution_metadata_fix_confirmed(&execution_id).unwrap();

    let verifier = StubBranchVerifier::ok("boss/parent");
    // Head deliberately unchanged — metadata alone must carry the decision.
    verifier.set_head_oid(Ok(head.to_owned())).await;
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler.with_branch_verifier(verifier);

    let outcome = handler
        .finalize_pr_transition(
            &execution_id,
            parent_pr_url.to_owned(),
            WorkerPrCompletionTarget::InReview,
            "metadata_only_fix",
        )
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "metadata_fix_confirmed must allow InReview without head movement; got {outcome:?}",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
        }
        other => panic!("expected task, got {other:?}"),
    }
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
}

/// Case 3: explicit NO_CHANGES_NEEDED, head unchanged → allowed through the
/// finalize chokepoint (the Stop path normally uses finalize_no_op instead).
#[tokio::test]
async fn revision_contribution_gate_allows_explicit_no_op() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/2603";
    let head = "dddddddddddddddddddddddddddddddddddddddd";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "## Summary\nFinding already fixed on main.\n\nNO_CHANGES_NEEDED\n",
    );

    let verifier = StubBranchVerifier::ok("boss/parent");
    verifier.set_head_oid(Ok(head.to_owned())).await;
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler.with_branch_verifier(verifier);

    let outcome = handler
        .finalize_pr_transition(
            &execution_id,
            parent_pr_url.to_owned(),
            WorkerPrCompletionTarget::InReview,
            "test_explicit_no_op",
        )
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "explicit NO_CHANGES_NEEDED must allow terminalization; got {outcome:?}",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
        }
        other => panic!("expected task, got {other:?}"),
    }
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
}

/// Case 4 (the defect): mid-turn reap path with no push, no metadata marker,
/// no explicit no-op — must NOT reach review. Couples `sha_unchanged` to
/// status success.
#[tokio::test]
async fn revision_contribution_gate_refuses_silence_after_mid_turn_reap() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/2604";
    let head = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);

    // Model the incident: worker still mid-turn when finalize is attempted.
    let live_states = Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
    live_states.register_spawn(
        9,
        &execution_id,
        "claude-opus-5",
        std::process::id() as i32,
        Some(boss_protocol::WorkItemBinding {
            work_item_id: revision_id.clone(),
            work_item_name: "revision silence".to_owned(),
            execution_id: execution_id.clone(),
        }),
    );
    live_states.apply_event(
        9,
        &boss_protocol::WorkerEvent::PreToolUse {
            session_id: "s".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
        },
    );

    let verifier = StubBranchVerifier::ok("boss/parent");
    // Head byte-identical to dispatch snapshot — proven non-contribution.
    verifier.set_head_oid(Ok(head.to_owned())).await;
    let TestHarness {
        handler, cube, pane, ..
    } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler
        .with_branch_verifier(verifier)
        .with_live_worker_states(live_states);

    let outcome = handler
        .finalize_pr_transition(
            &execution_id,
            parent_pr_url.to_owned(),
            WorkerPrCompletionTarget::InReview,
            "pr_recheck_staged",
        )
        .await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "silence after mid-turn reap must not terminalize toward review; got {outcome:?}",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => {
            assert_eq!(
                t.status,
                TaskStatus::Active,
                "revision must stay in Doing — not a false in_review success",
            );
        }
        other => panic!("expected task, got {other:?}"),
    }
    let exec = db.get_execution(&execution_id).unwrap();
    assert!(
        exec.status.is_live(),
        "execution must stay live so the worker is not reaped; status={:?}",
        exec.status,
    );
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "refused finalize must not release the cube lease",
    );
    assert!(
        pane.calls.lock().await.is_empty(),
        "refused finalize must not tear down the pane",
    );
    // Mid-turn reap accounting must not fire when we refuse (no reap happened).
    assert_eq!(
        handler.metrics.counter_value("completion.mid_turn_reap.total"),
        Some(0),
        "a refused finalize must not count as a mid-turn reap",
    );
}

/// Recheck staged arm is the historical fast path that manufactured the
/// defect: with head unchanged it must not advance the revision.
#[tokio::test]
async fn recheck_staged_revision_refuses_when_head_unchanged() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/2605";
    let head = "ffffffffffffffffffffffffffffffffffffffff";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);

    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_if_unset(&execution_id, parent_pr_url);

    let verifier = StubBranchVerifier::ok("boss/parent");
    verifier.set_head_oid(Ok(head.to_owned())).await;
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls)
        .with_branch_verifier(verifier);

    let outcome = handler.recheck_for_pr(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "pr_recheck_staged with sha_unchanged must refuse review terminalization; got {outcome:?}",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active);
        }
        other => panic!("expected task, got {other:?}"),
    }
    assert!(cube.release_calls.lock().await.is_empty());
}
