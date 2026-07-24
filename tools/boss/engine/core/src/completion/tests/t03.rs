//! Split out of `completion.rs`'s `#[cfg(test)] mod tests`.
//! Test functions only; shared fixtures, stubs, and helpers live
//! in the parent [`super`] module (`completion/tests.rs`).

use super::*;

#[tokio::test]
async fn execution_started_hook_persists_pr_head_before_when_bound() {
    // The run-start hook must snapshot the bound PR's head SHA
    // into `work_executions.pr_head_before` so the Stop-boundary
    // SHA-delta gate has something to compare against. Skips
    // gracefully when no PR is bound (new-PR flow).
    let workspace = tempdir().unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/606";
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.update_work_item(
        &chore_id,
        crate::work::WorkItemPatch {
            pr_url: Some(pr_url.into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();
    let detector = StubPrDetector::ok(None);
    let verifier = StubBranchVerifier::ok("boss/exec_old");
    verifier.set_head_oid(Ok("abcdef0123456789".into())).await;

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier);

    // Before the hook: no snapshot.
    assert_eq!(db.get_execution(&execution_id).unwrap().pr_head_before, None);
    handler.on_execution_started(&execution_id).await;
    assert_eq!(
        db.get_execution(&execution_id).unwrap().pr_head_before.as_deref(),
        Some("abcdef0123456789"),
        "hook must persist the snapshot when a PR is bound",
    );
}

#[tokio::test]
async fn execution_started_hook_skips_when_no_pr_bound() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);
    let verifier = StubBranchVerifier::ok("boss/exec_old");
    // A verifier that would explode if called — we expect it not
    // to be touched at all when no PR is bound.
    verifier.set_head_oid(Err("must not be called".into())).await;

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier);

    handler.on_execution_started(&execution_id).await;
    assert_eq!(
        db.get_execution(&execution_id).unwrap().pr_head_before,
        None,
        "no bound PR ⇒ no snapshot",
    );
}

// ── Bug B: recheck_for_pr_late ─────────────────────────────────────────

#[tokio::test]
async fn recheck_for_pr_late_binds_pr_to_active_task() {
    let (_dir, db, _product_id, chore_id, execution_id) = abandoned_execution_fixture();
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/42"));
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);

    let candidate = crate::work::LatePrCandidate {
        execution_id: execution_id.clone(),
        work_item_id: chore_id.clone(),
        repo_remote_url: "git@github.com:spinyfin/mono.git".into(),
        branch_naming: BranchNaming::BossExecPrefix,
        worker_branch_prefix: None,
    };
    let outcome = handler.recheck_for_pr_late(&candidate).await;

    assert!(
        matches!(outcome, StopOutcome::PrDetected { .. }),
        "expected PrDetected, got {outcome:?}"
    );
    let task = match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::InReview);
    assert_eq!(task.pr_url.as_deref(), Some("https://github.com/spinyfin/mono/pull/42"));
    // Execution itself stays abandoned — recheck_for_pr_late does not
    // touch the execution row.
    let exec = db.get_execution(&execution_id).unwrap();
    assert_eq!(exec.status, ExecutionStatus::Abandoned);
}

#[tokio::test]
async fn recheck_for_pr_late_returns_awaiting_input_when_no_pr() {
    let (_dir, db, _product_id, chore_id, execution_id) = abandoned_execution_fixture();
    let detector = StubPrDetector::ok(None); // no PR found
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);

    let candidate = crate::work::LatePrCandidate {
        execution_id: execution_id.clone(),
        work_item_id: chore_id.clone(),
        repo_remote_url: "git@github.com:spinyfin/mono.git".into(),
        branch_naming: BranchNaming::BossExecPrefix,
        worker_branch_prefix: None,
    };
    let outcome = handler.recheck_for_pr_late(&candidate).await;

    assert!(
        matches!(outcome, StopOutcome::AwaitingInput),
        "expected AwaitingInput when no PR found, got {outcome:?}"
    );
    // Chore stays active.
    let task = match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::Active);
    assert!(task.pr_url.is_none());
}

// -----------------------------------------------------------
// Revision task completion via SHA-delta gate in recheck_for_pr
//
// Reproduces T848: a revision worker pushed its commit to the parent
// PR but the revision task stayed in `doing` (active). The on_stop SHA
// delta gate failed transiently; the merge-poller's recheck_for_pr had
// no SHA-delta fallback, so the revision was stranded forever.
//
// The fix adds the SHA-delta gate to recheck_for_pr. Tests below pin:
//   1. Revision worker pushed → SHA moved → recheck_for_pr finalises.
//   2. Revision worker not yet pushed → SHA unchanged → recheck quiet.
//   3. Revision with no pr_head_before snapshot → Inapplicable → cold
//      path still runs (returns quiet; no regression).
// -----------------------------------------------------------

#[tokio::test]
async fn recheck_for_pr_sha_delta_advances_revision_to_in_review() {
    // T848 regression: revision worker pushed a commit to the parent
    // PR (head SHA changed), but `on_stop` failed to detect it (GitHub
    // API timeout during SHA fetch). The merge-poller's `recheck_for_pr`
    // should advance the revision to `in_review` on the next sweep via
    // the SHA-delta gate.
    //
    // Before the fix, `recheck_for_pr` had no SHA-delta gate; it fell
    // through to the cold-path branch-keyed detector which always returns
    // None for revisions (they never open their own PR), so the revision
    // stayed in `active` indefinitely.
    //
    // The SHA-delta gate is gated on `stop_seen` (T1503/T1496 fix): it only
    // fires after `on_stop_inner` has been called at least once. Here we
    // stamp `stop_seen` manually to simulate a prior on_stop that failed
    // transiently — this is the recovery path the gate is designed for.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/922";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    // Stamp stop_seen and revision_stop_contributed_head to simulate
    // on_stop_inner having observed Contributed (revision pushed "bbbb")
    // and attempted finalization, which failed transiently. The recovery
    // gate in recheck_for_pr requires revision_stop_contributed_head to
    // match the current head so it knows the head movement was the
    // revision's own contribution, not a concurrent parent push.
    db.set_execution_stop_seen(&execution_id).unwrap();
    db.set_revision_stop_contributed_head(&execution_id, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        .unwrap();
    // Cold-path detector returns None — correct for revisions which
    // have no branch of their own.
    let detector = StubPrDetector::ok(None);
    // Branch verifier: SHA moved (worker pushed the revision commit).
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
        .await;

    let TestHarness {
        handler,
        cube,
        publisher,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier);

    let outcome = handler.recheck_for_pr(&execution_id).await;

    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "SHA-delta gate must advance revision to in_review when head moved; got {outcome:?}",
    );
    // Revision task must be in_review; pr_url stays NULL (revisions don't own PRs).
    let item = db.get_work_item(&revision_id).unwrap();
    match item {
        WorkItem::Task(t) => {
            assert_eq!(t.status, TaskStatus::InReview, "revision must move to in_review");
            assert!(t.pr_url.is_none(), "revision pr_url must stay NULL; parent owns the PR");
        }
        other => panic!("expected task, got {other:?}"),
    }
    // Execution must be completed; lease must be released.
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(execution.finished_at.is_some());
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "cube lease must be released after revision finalises",
    );
    // Work-item changed event must fire so the kanban updates.
    let work_events = publisher.events.lock().await.clone();
    assert!(
        work_events
            .iter()
            .any(|(p, w, _)| p == &product_id && w == &revision_id),
        "work-item invalidation must fire for the revision, got {work_events:?}",
    );
    // No probe must be queued — the revision is done.
    assert!(
        probes.snapshot().is_empty(),
        "no probe must fire when revision is finalised; got {:?}",
        probes.snapshot(),
    );
}

#[tokio::test]
async fn recheck_for_pr_sha_unchanged_leaves_revision_active() {
    // Revision worker has not pushed yet (no commit since execution
    // started). The SHA-delta gate returns NoContribution; the cold
    // path returns quietly (no PR on revision branch). The revision
    // stays in `active` so the merge-poller will retry on the next
    // sweep.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/922";
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    let detector = StubPrDetector::ok(None);
    // Branch verifier: SHA unchanged.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    let TestHarness {
        handler, cube, probes, ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier);

    let outcome = handler.recheck_for_pr(&execution_id).await;

    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "unchanged SHA means revision not yet done; got {outcome:?}",
    );
    let item = db.get_work_item(&revision_id).unwrap();
    match item {
        WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected task, got {other:?}"),
    }
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "lease must stay held when revision is not done",
    );
    assert!(probes.snapshot().is_empty(), "recheck must not nudge");
}

// -----------------------------------------------------------
// 2026-07-14 incident (T342 / exec_18c2124d2f06d768_106d): a revision
// worker's fix was pushed and its Stop was terminal, but the row was
// neither reaped nor advanced. Root-caused to three chained defects,
// each regression-guarded below:
//   (a) the primary staged-URL path computed `expected_branch` from
//       the revision's OWN execution id, which can never match the
//       chain root's branch a revision actually pushes to, so a
//       legitimate staged URL was always dropped.
//   (b) the merge-poller's `recheck_for_pr`, which runs for the
//       worker's ENTIRE `waiting_human` session (not just once it
//       goes idle), could race a live worker's in-flight push and
//       misattribute an unattributed SHA delta as "parent pushed",
//       absorbing the just-pushed head as the new baseline — poisoning
//       the worker's own later SHA-delta comparison.
//   (c) the branch-keyed cold-path detector was invoked for revisions
//       on every inconclusive sweep, burning a futile
//       `query_pr_by_branch_suffix` scan (up to the 100-PR API cap)
//       that can never match a revision's branch.
// -----------------------------------------------------------

#[tokio::test]
async fn on_stop_staged_url_accepted_for_revision_chain_root_branch() {
    // Fix (a): the staged URL a compliant `cube pr update` call prints
    // for a revision IS the chain root's PR — accept it whenever it
    // matches the execution's resolved bound PR, with no branch-name
    // lookup (which can never succeed for a revision).
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/826";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);

    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_if_unset(&execution_id, parent_pr_url);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_staged_pr_urls(staged_pr_urls.clone());

    let outcome = handler.on_stop(&execution_id).await;

    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "staged chain-root URL must finalize the revision via the primary path; got {outcome:?}",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::InReview, "revision must move to in_review"),
        other => panic!("expected task, got {other:?}"),
    }
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "cube lease must be released via the primary staged-URL path",
    );
}

#[tokio::test]
async fn on_stop_staged_url_rejected_for_revision_wrong_pr() {
    // Fix (a), negative case: a staged URL that is NOT the revision's
    // bound (chain root) PR — e.g. captured from an unrelated `gh pr
    // view` the worker ran mid-session — must still be rejected.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/826";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);

    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_if_unset(&execution_id, "https://github.com/spinyfin/mono/pull/1");

    // SHA-delta gate fallback: head unchanged, so it won't finalize either.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head_before.into())).await;
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(verifier);

    let outcome = handler.on_stop(&execution_id).await;

    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "a staged URL that isn't the revision's bound (chain root) PR must not finalize; got {outcome:?}",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected task, got {other:?}"),
    }
    assert!(
        staged_pr_urls.get(&execution_id).is_none(),
        "mismatched staged URL must be cleared from the cache",
    );
    assert!(cube.release_calls.lock().await.is_empty());
}

#[tokio::test]
async fn recheck_for_pr_revision_unattributed_contributed_does_not_clobber_baseline() {
    // Fix (b): `execution.status` is `waiting_human` for a worker's
    // ENTIRE session (set at pane spawn, not at exit), so a
    // merge-poller sweep can land between a live worker's push and its
    // own Stop event — observing a head movement with
    // `revision_stop_contributed_head` not yet stamped (on_stop_inner
    // hasn't run for this delta yet). Before the fix, this absorbed
    // the just-observed head as the new `pr_head_before` baseline; the
    // worker's own later on_stop would then see head_now ==
    // pr_head_before (the clobbered baseline) and falsely conclude
    // NoContribution, stranding the revision forever. The fix: leave
    // `pr_head_before` untouched and defer to the worker's own Stop.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/826";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
        .await;
    let detector = StubPrDetector::ok(None);

    let TestHarness {
        handler, cube, probes, ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier);

    let outcome = handler.recheck_for_pr(&execution_id).await;

    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "unattributed Contributed must defer to the worker's own Stop, not finalize here; got {outcome:?}",
    );
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.pr_head_before.as_deref(),
        Some(head_before),
        "recheck_for_pr must NOT absorb an unattributed Contributed head into pr_head_before — \
         doing so poisons the worker's own later SHA-delta comparison",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected task, got {other:?}"),
    }
    assert!(cube.release_calls.lock().await.is_empty());
    assert!(probes.snapshot().is_empty(), "recheck_for_pr must not nudge");
}

#[tokio::test]
async fn recheck_for_pr_unattributed_then_worker_own_stop_still_finalizes() {
    // Companion to the regression above: after an earlier poller sweep
    // deferred (leaving `pr_head_before` untouched), the worker's own
    // on_stop must still see the real delta and finalize correctly —
    // proving the fix closes the loop rather than just suppressing
    // the false positive.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/826";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
        .await;
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier);

    // A poller sweep races in first and defers.
    let recheck_outcome = handler.recheck_for_pr(&execution_id).await;
    assert_eq!(recheck_outcome, StopOutcome::AwaitingInput);

    // The worker's own Stop now fires for the same push.
    let stop_outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(stop_outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "the worker's own Stop must still detect and finalize its real contribution \
         after an earlier unattributed poller sweep deferred; got {stop_outcome:?}",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected task, got {other:?}"),
    }
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
}

#[tokio::test]
async fn recheck_for_pr_never_calls_cold_path_detector_for_revision() {
    // Fix (c): the branch-keyed cold-path detector can structurally
    // never match a revision (it pushes to the chain root's branch,
    // never one derived from its own execution id) — calling it burns
    // a `query_pr_by_branch_suffix` scan (up to the 100-PR API cap)
    // for nothing before landing on the same AwaitingInput outcome
    // anyway. Assert recheck_for_pr skips it entirely for a revision
    // whose SHA-delta gate is Inapplicable.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/826";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    // Force the SHA-delta gate to Inapplicable via a transient fetch failure.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Err("transient GitHub API error".to_owned()))
        .await;
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_branch_verifier(verifier);

    let outcome = handler.recheck_for_pr(&execution_id).await;

    assert_eq!(outcome, StopOutcome::AwaitingInput);
    assert_eq!(
        detector.call_count(),
        0,
        "recheck_for_pr must never invoke the branch-keyed cold-path detector for a revision",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected task, got {other:?}"),
    }
}

// -----------------------------------------------------------
// PR-metadata-only CI-fix revision finalize (issue #1252), re-solved
// without the #1262 regression (rolled back in #1293).
//
// A CI-fix revision can legitimately finish WITHOUT moving the bound
// PR head — it repairs a PR-description validator via `gh pr edit
// --body`, no commit. The SHA-delta gate returns NoContribution on
// every sweep. We finalize such a revision ONLY on positive evidence:
//   1. a real Stop boundary (only `on_stop` stamps the marker; a
//      dead/cut-off worker emits no Stop hook), AND
//   2. an operator-visible PR-body delta (live body != run-start
//      snapshot), AND
//   3. CI green on the bound PR.
// The merge poller may finalize only what `on_stop` already marked,
// so a worker that contributed nothing (R1 dead, R2 reaped-while-live)
// is never mis-finalized.
// -----------------------------------------------------------

#[tokio::test]
async fn on_stop_finalizes_metadata_only_revision_when_body_changed_and_ci_clean() {
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1252";
    let head = "1111111111111111111111111111111111111111";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    // Worker edited the PR body during this run: live body differs from
    // the run-start snapshot. Head SHA unchanged → NoContribution.
    db.set_execution_pr_body_before(&execution_id, "## Summary\nold body")
        .unwrap();
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;
    verifier
        .set_body(Ok("## Summary\nold body\n\n## Testing\nfixed PR-template check".into()))
        .await;
    // The PR-template check went green after the edit.
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
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "metadata-only CI-fix revision with a body delta + clean CI must finalize to \
         in_review; got {outcome:?}",
    );
    // Positive-evidence marker stamped.
    assert!(
        db.execution_metadata_fix_confirmed(&execution_id).unwrap(),
        "on_stop must stamp the metadata-fix marker after observing the body delta",
    );
    // Revision advanced out of Doing to Review (revisions never own a pr_url).
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
            assert!(t.pr_url.is_none(), "revision tasks must not own a pr_url");
        }
        other => panic!("expected revision task, got {other:?}"),
    }
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(execution.cube_lease_id.is_none());
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
    assert!(
        probes.snapshot().is_empty(),
        "a clean metadata-only completion must NOT nudge",
    );
}

#[tokio::test]
async fn on_stop_records_marker_but_awaits_ci_when_body_changed_but_ci_not_green() {
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState, RequiredCheckFailure};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1253";
    let head = "2222222222222222222222222222222222222222";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    db.set_execution_pr_body_before(&execution_id, "old body").unwrap();
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;
    verifier
        .set_body(Ok("edited body that fixes the template".into()))
        .await;
    // The PR-template check is still re-running after the edit.
    let failures = vec![RequiredCheckFailure {
        name: "pr-template".into(),
        conclusion: "IN_PROGRESS".into(),
        target_url: String::new(),
        provider: crate::merge_poller::CiProvider::GithubActions,
        provider_job_id: None,
    }];
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus::ci_failing(
        failures,
    ))));

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
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "body delta + not-yet-green CI must record the marker and await CI, not nudge; \
         got {outcome:?}",
    );
    // Marker persisted so the merge poller can finalize once CI greens.
    assert!(
        db.execution_metadata_fix_confirmed(&execution_id).unwrap(),
        "the metadata-fix marker must persist for the poller's later finalize",
    );
    // Revision stays in Doing; lease held; no nudge (nothing to push).
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected task, got {other:?}"),
    }
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::WaitingHuman
    );
    assert!(cube.release_calls.lock().await.is_empty());
    assert!(pane.calls.lock().await.is_empty());
    assert!(
        probes.snapshot().is_empty(),
        "a recorded metadata-only fix awaiting CI must NOT nudge the worker to push",
    );
}

#[tokio::test]
async fn on_stop_does_not_finalize_revision_when_body_unchanged() {
    // R2 at the Stop boundary: the worker made no commit AND no PR-body
    // edit (head unchanged, body unchanged). This is "contributed
    // nothing", NOT a clean no-op completion. It must never be marked
    // or finalized as a metadata-only fix — it falls through to the
    // normal nudge.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1254";
    let head = "3333333333333333333333333333333333333333";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    db.set_execution_pr_body_before(&execution_id, "unchanged body")
        .unwrap();
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;
    verifier.set_body(Ok("unchanged body".into())).await; // identical → no delta

    let detector = StubPrDetector::ok(None);
    let TestHarness {
        handler, cube, pane, ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier);

    let _ = handler.on_stop(&execution_id).await;
    assert!(
        !db.execution_metadata_fix_confirmed(&execution_id).unwrap(),
        "no body delta must NOT stamp the metadata-fix marker",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(
            t.status,
            TaskStatus::Active,
            "a no-contribution run must not be finalized as a metadata-only fix",
        ),
        other => panic!("expected task, got {other:?}"),
    }
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::WaitingHuman
    );
    assert!(cube.release_calls.lock().await.is_empty());
    assert!(pane.calls.lock().await.is_empty());
}

#[tokio::test]
async fn recheck_finalizes_metadata_only_revision_after_ci_greens_when_marked() {
    // The CI-went-green-after-Stop recovery: on_stop already stamped the
    // marker (real Stop boundary + body delta) but CI was still
    // re-running. A later merge-poller sweep finalizes it now CI is green.
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1255";
    let head = "4444444444444444444444444444444444444444";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    // Simulate the marker on_stop stamped on a prior turn.
    db.mark_execution_metadata_fix_confirmed(&execution_id).unwrap();
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await; // head unchanged → NoContribution
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus::clean())));

    let detector = StubPrDetector::ok(None);
    let TestHarness {
        handler, cube, probes, ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier).with_merge_probe(probe);

    let outcome = handler.recheck_for_pr(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "marked metadata-only revision must finalize once its bound PR CI is green; \
         got {outcome:?}",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected task, got {other:?}"),
    }
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    assert!(probes.snapshot().is_empty(), "recovery must not nudge");
}

#[tokio::test]
async fn recheck_does_not_finalize_unmarked_revision_even_with_green_ci() {
    // The #1262 regression guard (T1256 R1 dead worker, T1265 R2 live
    // worker). The bound PR head is unchanged and CI is GREEN, but
    // on_stop never stamped the marker (the worker died / was reaped
    // before reaching a clean Stop with an operator-visible delta). The
    // merge poller must NOT finalize it — that was the rolled-back
    // behaviour. It stays Doing for the incomplete-execution paths.
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1256";
    let head = "5555555555555555555555555555555555555555";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    // NO marker stamped (the load-bearing difference from the test above).
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await; // head unchanged → NoContribution
    // CI is green — proving we gate on the marker, not on "head
    // unchanged + CI green".
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

    let outcome = handler.recheck_for_pr(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "an unmarked revision must NOT finalize even with green CI; got {outcome:?}",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(
            t.status,
            TaskStatus::Active,
            "the #1262 regression must stay fixed: no marker means no finalize",
        ),
        other => panic!("expected task, got {other:?}"),
    }
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::WaitingHuman
    );
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "an unmarked revision's lease must stay held (not reaped)",
    );
    assert!(pane.calls.lock().await.is_empty());
    assert!(probes.snapshot().is_empty());
}

// -----------------------------------------------------------
// T939 regression: revision on_stop with pr_head_before set
//
// When on_stop fires for a revision_implementation execution in
// waiting_human status with pr_head_before captured at execution start:
//   1. SHA-delta Contributed (worker pushed) → finalize directly, no nudge.
//   2. SHA-delta Inapplicable due to transient API failure → return quietly,
//      no nudge (avoids the probe loop: probe → response → Stop → nudge →
//      repeat that kept Crusher stuck in T939).
// The merge poller's recheck_for_pr handles case 2 when the API recovers.
// -----------------------------------------------------------

#[tokio::test]
async fn revision_on_stop_sha_delta_contributed_finalizes_with_no_nudge() {
    // T939 ideal path: the revision worker pushed commits to the parent PR
    // branch (head SHA moved). on_stop detects the contribution via the
    // SHA-delta gate and finalizes without queuing any nudge probe.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1032";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    // Branch verifier: SHA moved (worker pushed revision commit).
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()))
        .await;
    let detector = StubPrDetector::ok(None);

    let TestHarness {
        handler,
        cube,
        publisher,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "on_stop must finalize revision when SHA-delta detects contribution; got {outcome:?}",
    );
    // No probe must be queued — the revision is done.
    assert!(
        probes.snapshot().is_empty(),
        "no probe must fire when revision is finalised via SHA-delta; got {:?}",
        probes.snapshot(),
    );
    // Revision task must be in_review; task.pr_url stays NULL (parent owns it).
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) => {
            assert_eq!(t.status, TaskStatus::InReview, "revision must move to in_review");
            assert!(t.pr_url.is_none(), "revision task.pr_url must stay NULL");
        }
        other => panic!("expected task, got {other:?}"),
    }
    // Execution must be completed with pr_url populated (= parent PR URL).
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        execution.pr_url.as_deref(),
        Some(parent_pr_url),
        "execution.pr_url must be populated with parent PR URL after finalization",
    );
    // Cube lease must be released.
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "cube lease must be released after revision finalises",
    );
    // Work-item invalidation must fire.
    let work_events = publisher.events.lock().await.clone();
    assert!(
        work_events
            .iter()
            .any(|(p, w, _)| p == &product_id && w == &revision_id),
        "work-item invalidation must fire for the revision, got {work_events:?}",
    );
}

#[tokio::test]
async fn revision_on_stop_sha_delta_api_failure_does_not_nudge() {
    // T939 regression fix: when on_stop fires for a revision_implementation
    // execution in waiting_human with pr_head_before set, but the GitHub
    // API fails transiently (SHA-delta gate → Inapplicable), the engine
    // must NOT queue a nudge probe. Queuing a probe causes the worker to
    // respond, which fires another Stop, which nudges again — an infinite
    // loop. Return AwaitingInput silently; the merge poller's recheck_for_pr
    // will finalize once the API recovers.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1032";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, _revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    // Branch verifier: fetch_pr_head_oid fails — simulates transient GitHub API failure.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier
        .set_head_oid(Err("transient GitHub API error".to_owned()))
        .await;
    // Cold-path detector returns None — revision has no branch of its own.
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier);

    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "revision with pr_head_before set but transient SHA-delta failure must return \
         AwaitingInput silently (no nudge loop); got {outcome:?}",
    );
    // CRITICAL: no probe must be queued.
    assert!(
        probes.snapshot().is_empty(),
        "revision must NOT be nudged when SHA-delta fails with pr_head_before set \
         (T939 regression guard); got {:?}",
        probes.snapshot(),
    );
    // Execution must still be waiting_human — not completed, not parked.
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::WaitingHuman,
        "execution must remain in waiting_human until merge poller finalizes it",
    );
}

// -----------------------------------------------------------
// Stuck-revision-on-Stop regression (T2130 / exec_18b5d1ea40c1380_45b):
// a revision worker pushes its fix commit to the parent PR and stops
// cleanly, but `pr_head_before` was NEVER captured for this execution
// (the dispatch-time `on_execution_started` snapshot failed, or the
// execution predates reliable snapshotting). Before this fix, that
// permanently-missing baseline made `evaluate_sha_delta_gate` return
// `Inapplicable` on every single Stop, forever — with no way to ever
// observe "Contributed" via SHA comparison. `on_stop_inner` fell
// through to the branch-keyed cold-path detector (always empty for
// revisions) and then to `resolve_bound_pr_url`'s "push to existing
// PR" nudge — a dead end once the commit already landed, since there
// is nothing left to push. The nudge repeats on every Stop until the
// circuit breaker trips, stranding the execution in `waiting_human`
// forever even though the worker did everything right.
//
// The fix: when the SHA-delta gate is Inapplicable for a revision with
// a resolvable bound PR, fall back to the CI-state-based
// satisfied-deliverable gate (no SHA baseline required) before ever
// reaching the cold-path nudge.
// -----------------------------------------------------------

#[tokio::test]
async fn revision_on_stop_no_pr_head_before_snapshot_finalizes_via_satisfied_deliverable() {
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1709";
    let (_dir, db, product_id, revision_id, execution_id) = revision_fixture(
        workspace.path(),
        parent_pr_url,
        "0000000000000000000000000000000000000000",
    );
    // Simulate a dispatch-time snapshot that never landed: no baseline
    // exists for this execution's whole lifetime, unlike the T939
    // fixtures above which all carry a valid `pr_head_before`.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET pr_head_before = NULL WHERE id = ?1",
            rusqlite::params![execution_id],
        )
        .unwrap();
    }
    assert!(
        db.get_execution(&execution_id).unwrap().pr_head_before.is_none(),
        "fixture must have no SHA-delta baseline",
    );
    // Cold-path branch-keyed detector always finds nothing for revisions.
    let detector = StubPrDetector::ok(None);
    // The parent PR is open, CI clean, no conflict — the worker's push
    // landed and the deliverable is satisfied even though we can't prove
    // it via SHA comparison.
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus::clean())));

    let TestHarness {
        handler,
        cube,
        publisher,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_merge_probe(probe);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::DeliverableSatisfied { ref pr_url } if pr_url == parent_pr_url),
        "on_stop must finalize a revision with no SHA-delta baseline once the bound PR is \
         satisfied (CI clean, no conflict); got {outcome:?}",
    );
    // No nudge — the T939-class probe loop must not fire for this case either.
    assert!(
        probes.snapshot().is_empty(),
        "no probe must fire when the revision finalises via the satisfied-deliverable gate; \
         got {:?}",
        probes.snapshot(),
    );
    // Revision task must reach in_review; pr_url stays NULL (parent owns it).
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) => {
            assert_eq!(t.status, TaskStatus::InReview, "revision must move to in_review");
            assert!(t.pr_url.is_none(), "revision task.pr_url must stay NULL");
        }
        other => panic!("expected task, got {other:?}"),
    }
    // Execution must be terminal and the lease released — the actual bug
    // symptom (execution stuck in waiting_human forever) must be fixed.
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(execution.finished_at.is_some());
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "cube lease must be released — the slot must not be stranded",
    );
    let work_events = publisher.events.lock().await.clone();
    assert!(
        work_events
            .iter()
            .any(|(p, w, _)| p == &product_id && w == &revision_id),
        "work-item invalidation must fire for the revision, got {work_events:?}",
    );
}

#[tokio::test]
async fn revision_on_stop_no_pr_head_before_snapshot_and_ci_not_ready_awaits_without_nudge() {
    // Same missing-baseline scenario, but the bound PR's CI has not gone
    // green yet. The gate must NOT finalize prematurely, and — this is
    // the load-bearing assertion — it must NOT fall through to the
    // cold-path "push to existing PR" nudge either, since we have no
    // evidence the worker failed to contribute. Silence and a later
    // retry (a subsequent Stop, or the merge poller once a SHA
    // baseline becomes available some other way) is the only safe move.
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState, RequiredCheckFailure};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1710";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(
        workspace.path(),
        parent_pr_url,
        "0000000000000000000000000000000000000000",
    );
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET pr_head_before = NULL WHERE id = ?1",
            rusqlite::params![execution_id],
        )
        .unwrap();
    }
    let detector = StubPrDetector::ok(None);
    let failures = vec![RequiredCheckFailure {
        name: "build".into(),
        conclusion: "IN_PROGRESS".into(),
        target_url: String::new(),
        provider: crate::merge_poller::CiProvider::GithubActions,
        provider_job_id: None,
    }];
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus::ci_failing(
        failures,
    ))));

    let TestHarness {
        handler, cube, probes, ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_merge_probe(probe);

    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "no baseline + CI not ready must await quietly, not finalize; got {outcome:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "no baseline + inconclusive PR state must NOT fall through to the \
         push-to-existing-PR nudge (the stuck-revision failure mode); got {:?}",
        probes.snapshot(),
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected task, got {other:?}"),
    }
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::WaitingHuman,
        "execution stays live so a later Stop can retry",
    );
    assert!(cube.release_calls.lock().await.is_empty());
}

// -----------------------------------------------------------
// Merge-queue satisfied-deliverable regression (2026-07-14 incident,
// exec_18c21b03972f3920_49 / spinyfin/mono#1980): a revision worker
// pushed its fix, re-enqueued the PR for GitHub's merge queue, and
// stopped cleanly. The bound PR's head had already advanced to the
// pushed commit by the time this Stop's SHA-delta gate ran, so the
// gate reported `NoContribution` (head unchanged *this* Stop). CI was
// `InFlight` (the merge queue re-runs required checks against a
// synthetic ref before merging), which the old satisfied-deliverable
// gate didn't recognise as anything but "not ready" — it fell through
// to `probe_push_to_existing_pr`, the exact identical nudge that fired
// three times in the incident before the circuit breaker parked (and
// effectively abandoned) an execution that had already finished.
//
// The fix: `probe.in_merge_queue` (short of `UNMERGEABLE`) is itself
// satisfying evidence — GitHub, not the worker, owns getting the PR to
// `main` from here.
// -----------------------------------------------------------

#[tokio::test]
async fn revision_queued_for_merge_finalizes_without_nudge_even_with_ci_in_flight() {
    use crate::merge_poller::{OpenPrMergeability, OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1980";
    let head = "520191ca85b57aeceb458de88058f371a1d43149";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);

    // Cold-path branch-keyed detector always finds nothing for revisions.
    let detector = StubPrDetector::ok(None);
    // SHA-delta gate: head unchanged since the baseline captured for
    // *this* Stop (the push already landed before this Stop's fetch) →
    // NoContribution, the arm that used to fall straight to the "push
    // to the existing PR" nudge once the satisfied-deliverable gate
    // declined it.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    struct QueuedForMergeProbe;
    #[async_trait]
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

    let TestHarness {
        handler, cube, probes, ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(QueuedForMergeProbe));

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::DeliverableSatisfied { ref pr_url } if pr_url == parent_pr_url),
        "a PR queued for auto-merge (CI InFlight, not Conflict, not UNMERGEABLE) must finalize \
         without a nudge; got {outcome:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "no push-to-existing-PR probe must fire once the PR is queued for merge; got {:?}",
        probes.snapshot(),
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::InReview, "revision must move to in_review"),
        other => panic!("expected task, got {other:?}"),
    }
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "cube lease must be released — the slot must not be stranded waiting on a merge \
         GitHub is already handling",
    );
}

#[tokio::test]
async fn revision_merge_queue_rejection_still_falls_through_to_nudge() {
    // The flip side: `mergeQueueEntry.state == "UNMERGEABLE"` means the
    // queue itself rejected the PR — that is a real problem, not a
    // "nothing left to do" state, and must not be papered over by the
    // merge-queue carve-out. It falls through to the ordinary nudge
    // like any other non-clean CI state.
    use crate::merge_poller::{OpenPrMergeability, OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1981";
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, _revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);

    let detector = StubPrDetector::ok(None);
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    struct RejectedFromQueueProbe;
    #[async_trait]
    impl MergeProbe for RejectedFromQueueProbe {
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
                .merge_queue_entry_state("UNMERGEABLE")
                .build())
        }
    }

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(RejectedFromQueueProbe));

    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "an UNMERGEABLE queue entry must not be treated as satisfied; got {outcome:?}",
    );
    assert_eq!(
        probes.snapshot().len(),
        1,
        "an UNMERGEABLE queue rejection must still fall through to the ordinary nudge; got {:?}",
        probes.snapshot(),
    );
}

// -----------------------------------------------------------
// Signal-already-cleared gate tests
//
// The gate fires in the NoContribution arm: conflict/CI revision worker
// stops without pushing, but the blocking signal is already cleared.
// Expected: attempt retired as succeeded, parent snapped to in_review,
// execution finalised, NO nudge.
// -----------------------------------------------------------

#[tokio::test]
async fn conflict_revision_signal_cleared_retires_attempt_and_finalises() {
    // Riker scenario (T927 / exec_18b431dc9b016e88_1a regression):
    // conflict-resolution revision worker stops without pushing because
    // the conflict was already resolved by a sibling. The SHA-delta gate
    // returns NoContribution; the signal-cleared gate must detect the PR
    // is now mergeable, retire the attempt as succeeded, snap the parent
    // back to in_review, and finalise the execution — no nudge.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/966";
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, product_id, parent_chore_id, _revision_id, execution_id, attempt_id) =
        conflict_revision_fixture(workspace.path(), parent_pr_url, head);

    let detector = StubPrDetector::ok(None); // no branch-keyed PR

    // SHA-delta gate: head unchanged → NoContribution.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    // MergeProbe: PR is now mergeable (conflict cleared).
    struct CleanMergeProbe;
    #[async_trait]
    impl MergeProbe for CleanMergeProbe {
        async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
            Ok(PrLifecycleProbe::builder()
                .url(url.to_owned())
                .state(PrLifecycleState::Open(crate::merge_poller::OpenPrStatus::clean()))
                .labels(Vec::new())
                .review(crate::merge_poller::PrReviewState::Unknown)
                .build())
        }
    }

    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(CleanMergeProbe));

    let outcome = handler.on_stop(&execution_id).await;

    assert!(
        matches!(outcome, StopOutcome::SignalAlreadyCleared { ref pr_url } if pr_url == parent_pr_url),
        "signal-cleared gate must short-circuit the nudge; got {outcome:?}",
    );

    // Conflict attempt must be succeeded.
    let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
    assert_eq!(
        attempt.status, "succeeded",
        "conflict_resolutions attempt must be retired as succeeded",
    );

    // Parent chore must be snapped back to in_review.
    let parent = match db.get_work_item(&parent_chore_id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(
        parent.status,
        TaskStatus::InReview,
        "parent chore must be snapped back to in_review",
    );

    // Execution must be finalised (completed, lease released).
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "execution cube lease must be released",
    );
    assert!(
        !pane.calls.lock().await.is_empty(),
        "pane must be released after finalisation",
    );

    // No probe must be queued — the worker is done.
    assert!(
        probes.snapshot().is_empty(),
        "no nudge probe must fire on signal-cleared path; got {:?}",
        probes.snapshot(),
    );

    // ConflictResolutionSucceeded frontend event must have been published.
    let typed = publisher.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(pid, ev)| {
            pid == &product_id
                && matches!(
                    ev,
                    FrontendEvent::ConflictResolutionSucceeded {
                        work_item_id,
                        ..
                    } if work_item_id == &parent_chore_id
                )
        }),
        "ConflictResolutionSucceeded must be published; typed events: {typed:?}",
    );
}

#[tokio::test]
async fn conflict_revision_signal_cleared_defers_on_unknown_mergeability() {
    // mono#1398/#1764 root cause: the signal-cleared gate used to retire
    // the conflict attempt on ANY `mergeability != Conflict`, which treats
    // `Unknown` (GitHub still recomputing mergeability) as success. On a
    // NoContribution Stop (worker pushed nothing → head unchanged) that
    // records a premature `succeeded` at an un-advanced head; when GitHub
    // settles back to CONFLICTING, the succeeded row's UNIQUE key wedges
    // conflict_watch's re-arm loop forever. The gate must require GENUINE
    // mergeability (`Clean`) and defer on `Unknown` — so the attempt here
    // must NOT be marked succeeded.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1398";
    let head = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let (_dir, db, _product_id, _parent_chore_id, _revision_id, execution_id, attempt_id) =
        conflict_revision_fixture(workspace.path(), parent_pr_url, head);

    let detector = StubPrDetector::ok(None); // no branch-keyed PR

    // SHA-delta gate: head unchanged → NoContribution.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    // MergeProbe: GitHub reports mergeable=UNKNOWN (recompute in flight).
    struct UnknownMergeProbe;
    #[async_trait]
    impl MergeProbe for UnknownMergeProbe {
        async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
            Ok(PrLifecycleProbe::builder()
                .url(url.to_owned())
                .state(PrLifecycleState::Open(
                    crate::merge_poller::OpenPrStatus::unknown_mergeability(),
                ))
                .labels(Vec::new())
                .review(crate::merge_poller::PrReviewState::Unknown)
                .build())
        }
    }

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(UnknownMergeProbe))
        .with_conflict_unknown_backoff(std::time::Duration::ZERO);

    let outcome = handler.on_stop(&execution_id).await;

    // The signal-cleared retire path must NOT fire on Unknown.
    assert!(
        !matches!(outcome, StopOutcome::SignalAlreadyCleared { .. }),
        "signal-cleared retire must not fire on mergeable=UNKNOWN; got {outcome:?}",
    );

    // Crucially: the conflict attempt must stay `pending` — no premature
    // `succeeded` recorded while mergeability is indeterminate.
    let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
    assert_eq!(
        attempt.status, "pending",
        "conflict_resolutions attempt must NOT be retired as succeeded on mergeable=UNKNOWN",
    );
}

// -----------------------------------------------------------
// GitHub-authoritative conflict stop gate (2026-07-23 incident,
// spinyfin/mono#2070).
//
// A merge-conflict revision worker stopped 26s in without pushing,
// declaring "this conflict was already resolved and pushed in a prior
// attempt". It had never queried `mergeable`, and never ran the
// mandatory `cube workspace rebase`. GitHub reported CONFLICTING /
// DIRTY throughout. The only guard was the generic SHA-delta nudge,
// whose text invites exactly that reply ("if there is nothing left to
// do, say so"). These tests pin the engine-side enforcement: the
// claim is checked against GitHub, not accepted.
// -----------------------------------------------------------

/// [`MergeProbe`] reporting an open PR with a fixed mergeability and
/// the raw GitHub strings the probe text quotes back at the worker.
struct FixedMergeabilityProbe {
    mergeability: crate::merge_poller::OpenPrMergeability,
}

#[async_trait]
impl MergeProbe for FixedMergeabilityProbe {
    async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
        use crate::merge_poller::{OpenPrMergeability, OpenPrStatus};
        let (status, raw_mergeable, raw_state) = match self.mergeability {
            OpenPrMergeability::Conflict => (OpenPrStatus::conflict_only(), "CONFLICTING", "DIRTY"),
            OpenPrMergeability::Unknown => (OpenPrStatus::unknown_mergeability(), "UNKNOWN", "UNKNOWN"),
            OpenPrMergeability::Clean => (OpenPrStatus::clean(), "MERGEABLE", "CLEAN"),
        };
        Ok(PrLifecycleProbe::builder()
            .url(url.to_owned())
            .state(PrLifecycleState::Open(status))
            .labels(Vec::new())
            .review(crate::merge_poller::PrReviewState::Unknown)
            .raw_mergeable(raw_mergeable)
            .raw_merge_state_status(raw_state)
            .build())
    }
}

#[tokio::test]
async fn conflict_revision_still_conflicting_gets_targeted_probe_not_the_generic_escape_hatch() {
    // Incident replay: head unchanged (worker pushed nothing) and
    // GitHub says CONFLICTING. The engine must refuse the implicit
    // "already resolved" claim with a probe that quotes the live
    // GitHub values — and must NOT send the generic nudge, whose
    // "or there is nothing left to do" clause is the escape hatch the
    // worker took.
    use crate::merge_poller::OpenPrMergeability;

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/2070";
    let head = "39f8adcdfe055f98d1d2ebc56431f376ebbed683";
    let (_dir, db, _product_id, _parent_chore_id, _revision_id, execution_id, attempt_id) =
        conflict_revision_fixture(workspace.path(), parent_pr_url, head);

    let detector = StubPrDetector::ok(None);
    // SHA-delta gate: head unchanged → NoContribution.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(FixedMergeabilityProbe {
            mergeability: OpenPrMergeability::Conflict,
        }));

    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "a still-conflicting PR must keep the revision awaiting input, not finalize it",
    );

    let queued = probes.snapshot();
    assert_eq!(queued.len(), 1, "exactly one probe must fire; got {queued:?}");
    let text = &queued[0].1;
    assert!(
        text.contains("CONFLICTING") && text.contains("DIRTY"),
        "probe must quote the live GitHub values so the worker cannot argue with local jj state: {text}",
    );
    assert!(
        text.contains("cube workspace rebase"),
        "probe must name the mandatory command the worker skipped: {text}",
    );
    assert!(
        !text.contains("or there is nothing left to do"),
        "probe must NOT offer the generic nudge's escape hatch: {text}",
    );

    // The attempt stays live — nothing about a refused claim retires it.
    let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
    assert_eq!(
        attempt.status, "pending",
        "a refused claim must not touch the attempt ledger"
    );
    // And the execution must NOT be finalized off an unverified claim.
    assert_ne!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed,
        "a conflict revision that pushed nothing against a CONFLICTING PR must not complete",
    );
}

#[tokio::test]
async fn conflict_revision_unknown_mergeability_is_never_read_as_resolved() {
    // `mergeable: UNKNOWN` is an unanswered question, not a clean bill
    // of health. Before this gate, the satisfied-deliverable check
    // accepted any `mergeability != Conflict` and — because a
    // merge-conflict revision also waives the CI half of the test —
    // finalized the run on no evidence whatsoever.
    use crate::merge_poller::OpenPrMergeability;

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/2071";
    let head = "cccccccccccccccccccccccccccccccccccccccc";
    let (_dir, db, _product_id, _parent_chore_id, _revision_id, execution_id, _attempt_id) =
        conflict_revision_fixture(workspace.path(), parent_pr_url, head);

    let detector = StubPrDetector::ok(None);
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(FixedMergeabilityProbe {
            mergeability: OpenPrMergeability::Unknown,
        }))
        .with_conflict_unknown_backoff(std::time::Duration::ZERO);

    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "mergeable=UNKNOWN must not finalize a merge-conflict revision; got {outcome:?}",
    );
    assert_ne!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed,
        "UNKNOWN mergeability is not evidence the conflict cleared — the run must not complete",
    );

    let queued = probes.snapshot();
    assert_eq!(queued.len(), 1, "exactly one probe must fire; got {queued:?}");
    assert!(
        queued[0].1.contains("UNKNOWN") && queued[0].1.contains("cube workspace rebase"),
        "probe must name the indeterminate value and the command that settles it: {}",
        queued[0].1,
    );
}

#[tokio::test]
async fn conflict_revision_with_terminal_attempt_is_not_second_guessed() {
    // A worker that correctly escalated — `boss engine conflicts
    // mark-failed … --reason product_decision_required` — leaves the
    // attempt terminal and deliberately does not push. The gate must
    // not dog it about GitHub's `mergeable`: with no live attempt the
    // engine no longer believes this conflict is its to resolve, so
    // the ordinary nudge path applies unchanged.
    use crate::merge_poller::OpenPrMergeability;

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/2072";
    let head = "dddddddddddddddddddddddddddddddddddddddd";
    let (_dir, db, _product_id, _parent_chore_id, _revision_id, execution_id, attempt_id) =
        conflict_revision_fixture(workspace.path(), parent_pr_url, head);
    db.mark_conflict_resolution_failed(&attempt_id, "product_decision_required")
        .unwrap();

    let detector = StubPrDetector::ok(None);
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(FixedMergeabilityProbe {
            mergeability: OpenPrMergeability::Conflict,
        }));

    let _ = handler.on_stop(&execution_id).await;

    let queued = probes.snapshot();
    assert_eq!(queued.len(), 1, "exactly one probe must fire; got {queued:?}");
    assert_eq!(
        queued[0].1,
        probe_push_to_existing_pr(parent_pr_url),
        "with no live conflict attempt the generic nudge must be used unchanged",
    );
}


#[tokio::test]
async fn conflict_revision_on_stop_no_baseline_finalizes_without_false_failure() {
    // Second manifestation of the T2130 incident class (operator note,
    // 2026-07-02): two conflict-resolution revision workers resolved
    // their conflicts, pushed the merge commit, posted a resolution
    // comment, and stopped cleanly — then the engine marked BOTH
    // executions `failed` and left their panes lingering
    // ("Claude Not Detected").
    //
    // Mechanism: before this fix, a revision with an inconclusive
    // SHA-delta gate (here: no `pr_head_before` baseline was ever
    // captured) fell through to the cold-path "push to existing PR"
    // nudge. The worker — having already pushed — has nothing new to
    // push, so the SAME nudge fires on every subsequent Stop until the
    // auto-nudge circuit breaker trips (`NudgeBreakerParked`).
    // `on_stop`'s wrapper unconditionally routes any
    // `revision_implementation` Stop through
    // `finalize_conflict_resolution_attempt`, which — ONLY on
    // `NudgeBreakerParked` — marks the bound `conflict_resolutions`
    // ledger row `failed` (a false classification: the worker DID
    // push). At the time of the original incident,
    // `park_for_unproductive_nudges` never released the cube lease or
    // the pane, so the execution stayed `waiting_human` forever with a
    // stranded, unresponsive pane — exactly "lingering Claude Not
    // Detected panes." (`park_for_unproductive_nudges` now finalizes
    // via `finalize_idle_park`, releasing both — see
    // `record_worker_idle_abandonment` — but the false `failed`
    // ledger classification this test guards against is unrelated to
    // that leak and remains possible on the nudge-breaker path.)
    //
    // With the fix, the missing-baseline case never reaches the nudge
    // at all: it tries the satisfied-deliverable gate first. Here the
    // conflict is resolved and CI is clean, so it finalizes cleanly —
    // no nudge, no breaker trip, no false failure, no lingering pane.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1710";
    let head = "cccccccccccccccccccccccccccccccccccccccc";
    let (_dir, db, _product_id, _parent_chore_id, _revision_id, execution_id, attempt_id) =
        conflict_revision_fixture(workspace.path(), parent_pr_url, head);
    // Simulate the dispatch-time snapshot never landing (the actual
    // trigger observed in production): no SHA-delta baseline exists.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET pr_head_before = NULL WHERE id = ?1",
            rusqlite::params![execution_id],
        )
        .unwrap();
    }

    let detector = StubPrDetector::ok(None);
    // The merge commit resolving the conflict landed and CI is clean —
    // direct, SHA-independent evidence the deliverable is satisfied.
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(crate::merge_poller::PrLifecycleState::Open(
        crate::merge_poller::OpenPrStatus::clean(),
    )));

    let TestHarness {
        handler, cube, probes, ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_merge_probe(probe);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        !matches!(outcome, StopOutcome::NudgeBreakerParked { .. }),
        "a revision with no SHA-delta baseline must never be routed into the nudge-breaker \
         dead end; got {outcome:?}",
    );
    assert!(
        matches!(outcome, StopOutcome::DeliverableSatisfied { ref pr_url } if pr_url == parent_pr_url),
        "on_stop must finalize via the satisfied-deliverable gate; got {outcome:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "no push-to-existing-PR nudge must fire; got {:?}",
        probes.snapshot(),
    );
    // The conflict_resolutions ledger row must NOT be falsely marked
    // failed — the worker's push was real.
    let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
    assert_ne!(
        attempt.status, "failed",
        "a successful conflict-resolution push must never be recorded as a failed attempt",
    );
    // The execution must reach a terminal status with the lease
    // released — no lingering pane.
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::Completed,
        "execution must not be stranded in waiting_human",
    );
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "cube lease must be released — no lingering pane",
    );
}

#[tokio::test]
async fn conflict_revision_on_stop_no_baseline_with_unknown_mergeability_nudges_instead_of_stranding() {
    // Companion to `conflict_revision_on_stop_no_baseline_finalizes_without_false_failure`,
    // but with `mergeable: UNKNOWN` instead of `Clean`. A merge-conflict
    // revision that pushed its resolution commonly lands here while
    // GitHub's mergeability recompute is still in flight — the common
    // case, not a rare one. `mergeability_satisfies_deliverable` never
    // treats `Unknown` as satisfied for a merge-conflict revision, so
    // without the conflict-refusal gate running on this arm too, this
    // would silently return `AwaitingInput` with no nudge and no way to
    // recover (see `on_stop_inner`'s `Inapplicable` arm). With the gate
    // wired in, the run gets a targeted nudge instead of stranding.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1711";
    let head = "dddddddddddddddddddddddddddddddddddddddd";
    let (_dir, db, _product_id, _parent_chore_id, _revision_id, execution_id, _attempt_id) =
        conflict_revision_fixture(workspace.path(), parent_pr_url, head);
    // No SHA-delta baseline was ever captured.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET pr_head_before = NULL WHERE id = ?1",
            rusqlite::params![execution_id],
        )
        .unwrap();
    }

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_merge_probe(Arc::new(FixedMergeabilityProbe {
            mergeability: crate::merge_poller::OpenPrMergeability::Unknown,
        }))
        .with_conflict_unknown_backoff(std::time::Duration::ZERO);

    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "a persistent UNKNOWN mergeability must not be laundered into a silent, un-nudged \
         AwaitingInput; got {outcome:?}",
    );
    let queued = probes.snapshot();
    assert_eq!(
        queued.len(),
        1,
        "the conflict-refusal gate must fire from the no-baseline arm too; got {queued:?}",
    );
    assert!(
        queued[0].1.contains("UNKNOWN"),
        "must nudge with the UNKNOWN-specific probe text: {}",
        queued[0].1,
    );
}

#[tokio::test]
async fn conflict_revision_signal_still_active_nudges_as_before() {
    // Regression guard: if the conflict is STILL active when the worker
    // stops without pushing, the normal nudge path must still fire —
    // the signal-cleared gate must not suppress it.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/966";
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, _parent_chore_id, _revision_id, execution_id, attempt_id) =
        conflict_revision_fixture(workspace.path(), parent_pr_url, head);

    let detector = StubPrDetector::ok(None);

    // SHA-delta gate: head unchanged → NoContribution.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    // MergeProbe: PR is STILL conflicting.
    struct ConflictingMergeProbe;
    #[async_trait]
    impl MergeProbe for ConflictingMergeProbe {
        async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
            Ok(PrLifecycleProbe::builder()
                .url(url.to_owned())
                .state(PrLifecycleState::Open(
                    crate::merge_poller::OpenPrStatus::conflict_only(),
                ))
                .labels(Vec::new())
                .review(crate::merge_poller::PrReviewState::Unknown)
                .build())
        }
    }

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(ConflictingMergeProbe));

    let outcome = handler.on_stop(&execution_id).await;

    // Signal still active → normal nudge, NOT SignalAlreadyCleared.
    assert!(
        matches!(outcome, StopOutcome::AwaitingInput),
        "signal still active must fall through to normal nudge; got {outcome:?}",
    );

    // Conflict attempt must NOT be retired (still pending — Phase 3 style).
    let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
    assert_ne!(
        attempt.status, "succeeded",
        "conflict attempt must NOT be retired when signal is still active",
    );

    // Probe must have been queued (the nudge fired).
    let queued = probes.snapshot();
    assert_eq!(
        queued.len(),
        1,
        "exactly one nudge probe must be queued; got {queued:?}",
    );
}

#[tokio::test]
async fn conflict_revision_superseded_attempt_not_refused_on_no_contribution_path() {
    // Regression guard for the crz_id ownership check (finding #8) being
    // silently bypassed on the `NoContribution` call site — the exact
    // path the 2026-07-23 incident took. This execution's own attempt
    // (A, from `conflict_revision_fixture`) is retired/superseded by a
    // fresh attempt (B) minted for a later base move on the same parent
    // chore. `has_active_conflict_attempt` must recognize that A no
    // longer owns the parent's *active* attempt and refuse to fire the
    // GitHub-authoritative conflict-still-present nudge about B's
    // conflict — even though `try_retire_cleared_blocking_signal`'s
    // looser "any live attempt on the parent" prefetch sees B as active.
    // Before the fix, `conflict_revision_stop_refusal` trusted that
    // prefetched boolean directly on this call site and fired the
    // crz-specific refusal anyway, nudging this (wrong) execution about
    // a conflict it no longer owns.
    use crate::work::ConflictResolutionInsertInput;

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/966";
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, product_id, parent_chore_id, _revision_id, execution_id, attempt_id) =
        conflict_revision_fixture(workspace.path(), parent_pr_url, head);

    // Attempt A (this execution's owning attempt) is retired...
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE conflict_resolutions SET status = 'succeeded' WHERE id = ?1",
            rusqlite::params![attempt_id],
        )
        .unwrap();
    }

    // ...and superseded by a fresh attempt B for a later base move on
    // the same parent chore. B is the only `pending`/`running` row now,
    // so `active_conflict_resolution_for_work_item` returns B.
    let attempt_b = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.clone(),
            work_item_id: parent_chore_id.clone(),
            pr_url: parent_pr_url.to_owned(),
            pr_number: 966,
            head_branch: "my-feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base_sha_2".into()),
            head_sha_before: Some(head.into()),
        })
        .unwrap()
        .unwrap();
    assert_ne!(attempt_b.id, attempt_id, "attempt B must be a distinct, newer attempt");

    let detector = StubPrDetector::ok(None);

    // SHA-delta gate: head unchanged → NoContribution.
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    // MergeProbe: PR is STILL conflicting (B's conflict, not A's).
    struct ConflictingMergeProbe;
    #[async_trait]
    impl MergeProbe for ConflictingMergeProbe {
        async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
            Ok(PrLifecycleProbe::builder()
                .url(url.to_owned())
                .state(PrLifecycleState::Open(
                    crate::merge_poller::OpenPrStatus::conflict_only(),
                ))
                .labels(Vec::new())
                .review(crate::merge_poller::PrReviewState::Unknown)
                .build())
        }
    }

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(ConflictingMergeProbe));

    let outcome = handler.on_stop(&execution_id).await;

    assert!(
        matches!(outcome, StopOutcome::AwaitingInput),
        "must still nudge (generically) rather than finalize or crash; got {outcome:?}",
    );

    let queued = probes.snapshot();
    assert_eq!(
        queued.len(),
        1,
        "exactly one nudge probe must be queued; got {queued:?}"
    );
    assert!(
        queued[0].1.contains("Do NOT open a new PR"),
        "must fall through to the generic push-to-existing-PR nudge, NOT the crz-specific \
         conflict-still-present refusal — this execution's attempt (A) no longer owns the \
         parent's active attempt (B), so the ownership check must suppress the refusal gate; \
         got probe text: {}",
        queued[0].1,
    );
    assert!(
        !queued[0].1.contains("GitHub still reports"),
        "the crz-specific conflict-still-present probe must NOT fire for a non-owning \
         attempt; got probe text: {}",
        queued[0].1,
    );
}


#[tokio::test]
async fn conflict_revision_finalizes_on_mergeability_alone_even_when_ci_not_clean() {
    // 2026-07-03 incident (exec_18be836b10baae8_35 / T2154): the periodic
    // merge-poller sweep (`conflict_watch::on_resolved`) can retire the
    // `conflict_resolutions` ledger row to `succeeded` and snap the
    // parent chore back to `in_review` on its own schedule, independent
    // of this worker's Stop events. Once that has happened,
    // `try_retire_cleared_blocking_signal` finds no *active* attempt and
    // bails, so completion falls through to the generic
    // deliverable-satisfied gate — which historically also required CI
    // to be clean. A merge-conflict revision's job is only to clear the
    // conflict; CI is a separate concern it was never asked to fix, so
    // requiring it as well left this exact worker being nudged forever
    // while CI was merely in-flight. Mergeability alone must be enough.
    use crate::merge_poller::{OpenPrCiStatus, OpenPrMergeability, OpenPrStatus, PrLifecycleState};

    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1709";
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, parent_chore_id, revision_id, execution_id, attempt_id) =
        conflict_revision_fixture(workspace.path(), parent_pr_url, head);

    // Simulate the merge-poller's `on_resolved` having already retired
    // the ledger and unblocked the parent BEFORE this Stop — the race
    // that strands the ledger check in `try_retire_cleared_blocking_signal`.
    db.mark_conflict_resolution_succeeded(&attempt_id, None).unwrap();
    db.clear_chore_blocked_merge_conflict_for_attempt(&parent_chore_id, parent_pr_url, &attempt_id)
        .unwrap();

    let detector = StubPrDetector::ok(None);

    // SHA-delta gate: head unchanged → NoContribution (worker didn't
    // push this run — the conflict was already gone before it started).
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    // PR is mergeable, but CI is still in-flight — must NOT block
    // finalization for a merge-conflict-provenance revision.
    let probe: Arc<dyn MergeProbe> = Arc::new(FixedStateProbe(PrLifecycleState::Open(OpenPrStatus {
        mergeability: OpenPrMergeability::Clean,
        ci: OpenPrCiStatus::InFlight,
    })));

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier).with_merge_probe(probe);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::DeliverableSatisfied { ref pr_url } if pr_url == parent_pr_url),
        "a merge-conflict revision must finalize on mergeability alone, CI-in-flight \
         notwithstanding; got {outcome:?}",
    );

    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::Completed,
        "execution must be finalized, not left nudging",
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview, "revision must advance to in_review")
        }
        other => panic!("expected task, got {other:?}"),
    }
    assert!(
        probes.snapshot().is_empty(),
        "no nudge probe must fire once mergeability alone confirms the deliverable; got {:?}",
        probes.snapshot(),
    );
}

#[tokio::test]
async fn conflict_revision_parked_without_push_marks_attempt_failed() {
    // Defect #2a: a conflict-resolution revision worker that stops
    // without pushing and is parked by the auto-nudge breaker must
    // retire its `conflict_resolutions` ledger row as `failed`.
    // Otherwise the attempt strands `pending` forever (the "revision
    // task does nothing" stall the operator reports), and once `main`
    // moves again the detector mints a fresh conflict revision against
    // the new base SHA — the re-mint loop.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/966";
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, product_id, _parent_chore_id, _revision_id, execution_id, attempt_id) =
        conflict_revision_fixture(workspace.path(), parent_pr_url, head);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, publisher, .. } = TestHarness::new(db.clone(), detector);

    let execution = db.get_execution(&execution_id).unwrap();
    handler
        .finalize_conflict_resolution_attempt(
            &execution,
            &StopOutcome::NudgeBreakerParked {
                reason: "max unproductive nudges".into(),
            },
        )
        .await;

    let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
    assert_eq!(
        attempt.status, "failed",
        "a parked-without-push conflict attempt must be marked failed",
    );
    assert_eq!(
        attempt.failure_reason.as_deref(),
        Some(CONFLICT_NO_PUSH_REASON),
        "failure_reason must be the no-push catch-all",
    );

    let typed = publisher.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(pid, ev)| {
            pid == &product_id
                && matches!(
                    ev,
                    FrontendEvent::ConflictResolutionFailed { attempt_id: aid, failure_reason, .. }
                        if aid == &attempt_id && failure_reason == CONFLICT_NO_PUSH_REASON
                )
        }),
        "ConflictResolutionFailed must be published; typed events: {typed:?}",
    );
}

#[tokio::test]
async fn conflict_revision_awaiting_input_leaves_attempt_pending() {
    // The finalizer must NOT fire while the worker is still being
    // nudged (AwaitingInput): a worker that resumes and pushes must
    // never be prematurely failed, and the detector's in-flight dedup
    // must keep holding so no duplicate conflict revision is minted.
    // Only the genuine "parked, no push" terminal retires the attempt.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/966";
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, _parent_chore_id, _revision_id, execution_id, attempt_id) =
        conflict_revision_fixture(workspace.path(), parent_pr_url, head);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);

    let execution = db.get_execution(&execution_id).unwrap();
    for outcome in [StopOutcome::AwaitingInput, StopOutcome::DetectorFailed] {
        handler.finalize_conflict_resolution_attempt(&execution, &outcome).await;
        let attempt = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
        assert_eq!(
            attempt.status, "pending",
            "attempt must stay pending for non-parked outcome {outcome:?}",
        );
        assert!(attempt.failure_reason.is_none());
    }
}

#[tokio::test]
async fn ci_revision_target_check_cleared_retires_despite_other_failing() {
    // T57 / linkedin-multiproduct/rdev-base-image#440 regression: a
    // CI-remediation revision worker fixed the "Pull Request Description"
    // check via a metadata-only `gh pr edit` (NO commit → SHA-delta gate
    // returns NoContribution). The target check is now green, but the PR
    // has an UNRELATED failing required check. The old heuristic required
    // whole-PR `Clean`, so it re-nudged the worker forever. The fix: the
    // attempt's own targeted check is no longer failing, so it must be
    // retired as succeeded and the parent snapped back to in_review.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/440";
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let failed_checks = r#"[{"name":"Pull Request Description","conclusion":"FAILURE","target_url":"","provider":"other","provider_job_id":null}]"#;
    let (_dir, db, product_id, parent_chore_id, _revision_id, execution_id, attempt_id) =
        ci_revision_fixture(workspace.path(), parent_pr_url, head, failed_checks);

    let detector = StubPrDetector::ok(None);

    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    // The target check ("Pull Request Description") is green now; only an
    // unrelated check ("build") is failing.
    struct OtherFailingProbe;
    #[async_trait]
    impl MergeProbe for OtherFailingProbe {
        async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
            let mut p = ci_probe(crate::merge_poller::OpenPrCiStatus::Failing {
                failures: vec![failing_check("build")],
            });
            p.url = url.to_owned();
            Ok(p)
        }
    }

    let TestHarness {
        handler,
        publisher,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(OtherFailingProbe));

    let outcome = handler.on_stop(&execution_id).await;

    assert!(
        matches!(outcome, StopOutcome::SignalAlreadyCleared { ref pr_url } if pr_url == parent_pr_url),
        "targeted check cleared must retire the attempt; got {outcome:?}",
    );

    let attempt = db.get_ci_remediation(&attempt_id).unwrap().unwrap();
    assert_eq!(
        attempt.status, "succeeded",
        "ci_remediations attempt must be retired as succeeded",
    );

    let parent = match db.get_work_item(&parent_chore_id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(
        parent.status,
        TaskStatus::InReview,
        "parent chore must be snapped back to in_review",
    );

    assert!(
        probes.snapshot().is_empty(),
        "no nudge probe must fire when the targeted check is cleared; got {:?}",
        probes.snapshot(),
    );

    let typed = publisher.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(pid, ev)| {
            pid == &product_id
                && matches!(
                    ev,
                    FrontendEvent::CiRemediationSucceeded { work_item_id, .. }
                        if work_item_id == &parent_chore_id
                )
        }),
        "CiRemediationSucceeded must be published; typed events: {typed:?}",
    );
}

#[tokio::test]
async fn ci_revision_trunk_queue_eviction_stays_active_despite_clean_head_ci() {
    // A trunk_queue_eviction attempt's failure is a queue-side signal —
    // the head branch's own CI is Clean by construction (the eviction
    // never ran the PR's own CI at all). A Stop with a Clean head probe
    // must NOT auto-retire the attempt or clear the blocking chore
    // status: that would be exactly the bypass the queue-side-failure
    // guards elsewhere in this flow reject, done automatically by the
    // engine instead of by the worker.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/440";
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, parent_chore_id, _revision_id, execution_id, attempt_id) =
        ci_revision_fixture_with_kind(workspace.path(), parent_pr_url, head, "[]", "trunk_queue_eviction");

    let detector = StubPrDetector::ok(None);

    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    struct CleanProbe;
    #[async_trait]
    impl MergeProbe for CleanProbe {
        async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
            let mut p = ci_probe(crate::merge_poller::OpenPrCiStatus::Clean);
            p.url = url.to_owned();
            Ok(p)
        }
    }

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(CleanProbe));

    let outcome = handler.on_stop(&execution_id).await;

    assert!(
        !matches!(outcome, StopOutcome::SignalAlreadyCleared { .. }),
        "a queue-side trunk_queue_eviction attempt must never be auto-retired by a clean head CI probe; got {outcome:?}",
    );

    let attempt = db.get_ci_remediation(&attempt_id).unwrap().unwrap();
    assert_ne!(
        attempt.status, "succeeded",
        "trunk_queue_eviction attempt must remain active despite Clean head CI",
    );

    let parent = match db.get_work_item(&parent_chore_id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_ne!(
        parent.status,
        TaskStatus::InReview,
        "the block must not be silently cleared with no fix pushed",
    );
    let _ = probes;
}

#[tokio::test]
async fn ci_revision_target_check_still_failing_nudges_as_before() {
    // Regression guard: when the attempt's OWN targeted check is still
    // failing, the signal is NOT cleared — the normal nudge path must
    // still fire and the attempt must remain active.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/440";
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let failed_checks = r#"[{"name":"Pull Request Description","conclusion":"FAILURE","target_url":"","provider":"other","provider_job_id":null}]"#;
    let (_dir, db, _product_id, _parent_chore_id, _revision_id, execution_id, attempt_id) =
        ci_revision_fixture(workspace.path(), parent_pr_url, head, failed_checks);

    let detector = StubPrDetector::ok(None);

    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    // The target check is STILL failing.
    struct TargetFailingProbe;
    #[async_trait]
    impl MergeProbe for TargetFailingProbe {
        async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
            let mut p = ci_probe(crate::merge_poller::OpenPrCiStatus::Failing {
                failures: vec![failing_check("Pull Request Description")],
            });
            p.url = url.to_owned();
            Ok(p)
        }
    }

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(TargetFailingProbe));

    let outcome = handler.on_stop(&execution_id).await;

    assert!(
        matches!(outcome, StopOutcome::AwaitingInput),
        "target check still failing must fall through to the normal nudge; got {outcome:?}",
    );

    let attempt = db.get_ci_remediation(&attempt_id).unwrap().unwrap();
    assert_ne!(
        attempt.status, "succeeded",
        "attempt must NOT be retired while its targeted check is still failing",
    );

    assert_eq!(
        probes.snapshot().len(),
        1,
        "exactly one nudge probe must be queued; got {:?}",
        probes.snapshot(),
    );
}

#[tokio::test]
async fn ci_revision_target_check_inflight_does_not_retire() {
    // When CI is InFlight (some required check still non-terminal) we
    // cannot tell whether the targeted check specifically went green, so
    // we must stay conservative and NOT retire — the next sweep
    // re-evaluates once checks terminalize.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/440";
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let failed_checks = r#"[{"name":"Pull Request Description","conclusion":"FAILURE","target_url":"","provider":"other","provider_job_id":null}]"#;
    let (_dir, db, _product_id, _parent_chore_id, _revision_id, execution_id, attempt_id) =
        ci_revision_fixture(workspace.path(), parent_pr_url, head, failed_checks);

    let detector = StubPrDetector::ok(None);

    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;

    struct InFlightProbe;
    #[async_trait]
    impl MergeProbe for InFlightProbe {
        async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
            let mut p = ci_probe(crate::merge_poller::OpenPrCiStatus::InFlight);
            p.url = url.to_owned();
            Ok(p)
        }
    }

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(InFlightProbe));

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::AwaitingInput),
        "InFlight CI must not retire; got {outcome:?}",
    );
    let attempt = db.get_ci_remediation(&attempt_id).unwrap().unwrap();
    assert_ne!(attempt.status, "succeeded");
}

// ── ci_attempt_signal_cleared: predicate decision-table tests ────────────

#[test]
fn ci_signal_cleared_clean_always_clears() {
    // Clean clears any attempt, including one with no targeted-check info.
    assert!(ci_attempt_signal_cleared("[]", &OpenPrCiStatus::Clean));
    assert!(ci_attempt_signal_cleared(r#"[{"name":"x"}]"#, &OpenPrCiStatus::Clean));
}

#[test]
fn ci_signal_cleared_inflight_never_clears() {
    assert!(!ci_attempt_signal_cleared("[]", &OpenPrCiStatus::InFlight));
    assert!(!ci_attempt_signal_cleared(
        r#"[{"name":"x"}]"#,
        &OpenPrCiStatus::InFlight
    ));
}

#[test]
fn ci_signal_cleared_failing_clears_when_target_not_among_failures() {
    // Targeted "Pull Request Description" is green; only "build" fails.
    let ci = OpenPrCiStatus::Failing {
        failures: vec![failing_check("build")],
    };
    assert!(ci_attempt_signal_cleared(
        r#"[{"name":"Pull Request Description"}]"#,
        &ci
    ));
}

#[test]
fn ci_signal_cleared_failing_does_not_clear_when_target_still_failing() {
    let ci = OpenPrCiStatus::Failing {
        failures: vec![failing_check("Pull Request Description"), failing_check("build")],
    };
    assert!(!ci_attempt_signal_cleared(
        r#"[{"name":"Pull Request Description"}]"#,
        &ci
    ));
}

#[test]
fn ci_signal_cleared_failing_with_no_targeted_names_stays_conservative() {
    // No parseable targeted names → only Clean would clear; a Failing
    // status must not retire (preserves pre-change behaviour).
    let ci = OpenPrCiStatus::Failing {
        failures: vec![failing_check("build")],
    };
    assert!(!ci_attempt_signal_cleared("[]", &ci));
    assert!(!ci_attempt_signal_cleared("not json", &ci));
}

#[test]
fn ci_signal_cleared_multi_target_requires_all_targets_clear() {
    // An attempt targeting two checks clears only when BOTH are green.
    let targets = r#"[{"name":"a"},{"name":"b"}]"#;
    let one_left = OpenPrCiStatus::Failing {
        failures: vec![failing_check("b")],
    };
    assert!(!ci_attempt_signal_cleared(targets, &one_left));
    let unrelated = OpenPrCiStatus::Failing {
        failures: vec![failing_check("c")],
    };
    assert!(ci_attempt_signal_cleared(targets, &unrelated));
}

#[test]
fn targeted_check_names_parses_names() {
    assert_eq!(
        targeted_check_names(r#"[{"name":"a"},{"name":"b"}]"#),
        vec!["a".to_owned(), "b".to_owned()]
    );
    assert!(targeted_check_names("[]").is_empty());
    assert!(targeted_check_names("garbage").is_empty());
    assert!(targeted_check_names(r#"{"name":"a"}"#).is_empty());
}

// ── expected_branch_name: BranchNaming strategy tests ────────────────────

#[test]
fn boss_exec_prefix_produces_classic_branch_name() {
    let exec_id = "exec_18b44d2630b1df80_66";
    let branch = expected_branch_name(exec_id, &BranchNaming::BossExecPrefix, None);
    assert_eq!(branch, "boss/exec_18b44d2630b1df80_66");
    assert!(
        branch.contains(exec_id),
        "BossExecPrefix must embed the full execution id"
    );
}

#[test]
fn boss_exec_prefix_honors_product_worker_branch_prefix() {
    // Regression for #1141: a product configured with
    // `worker_branch_prefix = "bduff/"` must produce
    // `bduff/exec_<id>`, not the hardcoded `boss/exec_<id>`. The
    // prefix carries its own trailing `/` and is concatenated
    // verbatim, and the full execution id is preserved.
    let exec_id = "exec_18b44d2630b1df80_66";
    let branch = expected_branch_name(exec_id, &BranchNaming::BossExecPrefix, Some("bduff/"));
    assert_eq!(branch, "bduff/exec_18b44d2630b1df80_66");
}

#[test]
fn non_default_branch_naming_takes_precedence_over_worker_branch_prefix() {
    // A non-default editorial `branch_naming` is the richer, explicit
    // rule and wins over the plain `worker_branch_prefix` column, which
    // only shapes the default `BossExecPrefix` strategy.
    let exec_id = "exec_18b44d2630b1df80_66";
    let opaque = expected_branch_name(exec_id, &BranchNaming::OpaqueHash, Some("bduff/"));
    assert!(opaque.starts_with("boss/"), "OpaqueHash ignores worker_branch_prefix");
    let custom = expected_branch_name(
        exec_id,
        &BranchNaming::CustomPrefix {
            prefix: "lnkd".to_owned(),
        },
        Some("bduff/"),
    );
    assert!(custom.starts_with("lnkd/"), "CustomPrefix ignores worker_branch_prefix");
}

#[test]
fn opaque_hash_produces_8_hex_char_suffix_under_boss_prefix() {
    let exec_id = "exec_18b44d2630b1df80_66";
    let branch = expected_branch_name(exec_id, &BranchNaming::OpaqueHash, None);
    // Must start with "boss/" and have an 8-char hex suffix.
    assert!(branch.starts_with("boss/"), "OpaqueHash branch must start with boss/");
    let suffix = branch.strip_prefix("boss/").unwrap();
    assert_eq!(suffix.len(), 8, "OpaqueHash suffix must be 8 hex chars, got: {suffix}");
    assert!(
        suffix.chars().all(|c| c.is_ascii_hexdigit()),
        "OpaqueHash suffix must be hex digits, got: {suffix}",
    );
    // Must NOT expose the execution id.
    assert!(!branch.contains(exec_id), "OpaqueHash must not embed the execution id");
}

#[test]
fn opaque_hash_is_deterministic_for_same_execution_id() {
    let exec_id = "exec_18b44d2630b1df80_66";
    let a = expected_branch_name(exec_id, &BranchNaming::OpaqueHash, None);
    let b = expected_branch_name(exec_id, &BranchNaming::OpaqueHash, None);
    assert_eq!(a, b, "OpaqueHash must be deterministic for the same execution id");
}

#[test]
fn opaque_hash_differs_for_different_execution_ids() {
    let a = expected_branch_name("exec_aaaa0000_01", &BranchNaming::OpaqueHash, None);
    let b = expected_branch_name("exec_bbbb1111_02", &BranchNaming::OpaqueHash, None);
    assert_ne!(a, b, "distinct execution ids must produce distinct OpaqueHash branches");
}

#[test]
fn custom_prefix_uses_prefix_and_opaque_hash_suffix() {
    let exec_id = "exec_18b44d2630b1df80_66";
    let branch = expected_branch_name(
        exec_id,
        &BranchNaming::CustomPrefix {
            prefix: "bduff".to_owned(),
        },
        None,
    );
    assert!(
        branch.starts_with("bduff/"),
        "CustomPrefix branch must start with the given prefix"
    );
    let suffix = branch.strip_prefix("bduff/").unwrap();
    assert_eq!(
        suffix.len(),
        8,
        "CustomPrefix suffix must be 8 hex chars, got: {suffix}"
    );
    assert!(
        suffix.chars().all(|c| c.is_ascii_hexdigit()),
        "CustomPrefix suffix must be hex digits, got: {suffix}",
    );
    // Must NOT expose the execution id.
    assert!(
        !branch.contains(exec_id),
        "CustomPrefix must not embed the execution id"
    );
}

#[test]
fn custom_prefix_with_same_exec_id_differs_from_opaque_hash() {
    let exec_id = "exec_18b44d2630b1df80_66";
    let opaque = expected_branch_name(exec_id, &BranchNaming::OpaqueHash, None);
    let custom = expected_branch_name(
        exec_id,
        &BranchNaming::CustomPrefix {
            prefix: "bduff".to_owned(),
        },
        None,
    );
    // Same hash suffix but different prefix → different branch names.
    assert_ne!(opaque, custom);
    // The hash suffix is the same (both derive from the same execution id).
    let opaque_hash = opaque.strip_prefix("boss/").unwrap();
    let custom_hash = custom.strip_prefix("bduff/").unwrap();
    assert_eq!(opaque_hash, custom_hash, "same execution id → same hash suffix");
}

#[test]
fn branch_work_item_suffix_strips_the_prefix() {
    // `boss/` prefix → the execution id is the suffix.
    assert_eq!(
        branch_work_item_suffix("boss/exec_18b5023342a35418_18"),
        "exec_18b5023342a35418_18",
    );
    // A product `worker_branch_prefix` like `bduff/` → same suffix.
    assert_eq!(
        branch_work_item_suffix("bduff/exec_18b5023342a35418_18"),
        "exec_18b5023342a35418_18",
    );
    // OpaqueHash / CustomPrefix → the hash is the suffix.
    assert_eq!(branch_work_item_suffix("boss/a7f3e9c2"), "a7f3e9c2");
    assert_eq!(branch_work_item_suffix("bduff/a7f3e9c2"), "a7f3e9c2");
    // No slash → the whole string is the suffix.
    assert_eq!(branch_work_item_suffix("exec_x"), "exec_x");
    // Multi-segment → only the final segment counts.
    assert_eq!(branch_work_item_suffix("feature/x/exec_y"), "exec_y");
}

#[test]
fn parse_api_pr_tsv_parses_all_six_fields() {
    let pr = parse_api_pr_tsv("https://github.com/o/r/pull/7\topen\t2026-01-02T03:04:05Z\t3\t10\t4")
        .expect("a non-empty url yields Some");
    assert_eq!(pr.url, "https://github.com/o/r/pull/7");
    assert_eq!(pr.state, "open");
    assert_eq!(pr.merged_at.as_deref(), Some("2026-01-02T03:04:05Z"));
    assert_eq!(pr.changed_files, 3);
    assert_eq!(pr.additions, 10);
    assert_eq!(pr.deletions, 4);
}
