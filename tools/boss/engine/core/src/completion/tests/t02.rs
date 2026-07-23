//! Split out of `completion.rs`'s `#[cfg(test)] mod tests`.
//! Test functions only; shared fixtures, stubs, and helpers live
//! in the parent [`super`] module (`completion/tests.rs`).

use super::*;

#[tokio::test]
async fn merge_poller_recheck_binds_three_stuck_workers_when_detector_recovers() {
    use crate::merge_poller::{MergeProbe, PrLifecycleProbe, PrLifecycleState};

    // Three independent workspaces / chores / executions in
    // `waiting_human` with `pr_url=null`. Mirrors the 3-worker
    // dispatch wave (Worf/Crusher/Troi).
    let ws1 = tempdir().unwrap();
    let ws2 = tempdir().unwrap();
    let ws3 = tempdir().unwrap();
    let (_dir, db, _p1, c1, e1) = fixture(ws1.path());
    // Reuse the same DB for the next two so a single merge-poller
    // pass sees all three executions.
    let chore2 = db
        .create_chore(
            crate::work::CreateChoreInput::builder()
                .product_id({
                    let item = db.get_work_item(&c1).unwrap();
                    item.product_id().to_string()
                })
                .name("Crusher")
                .build(),
        )
        .unwrap();
    let exec2 = create_ready_chore_execution(&db, chore2.id.clone());
    let (exec2, run2) = db
        .start_execution_run(
            &exec2.id,
            "worker-2",
            "mono",
            "lease-2",
            "mono-agent-002",
            ws2.path().to_str().unwrap(),
        )
        .unwrap();
    finish_run_waiting_human(&db, &exec2.id, &run2.id, None);
    let chore3 = db
        .create_chore(
            crate::work::CreateChoreInput::builder()
                .product_id({
                    let item = db.get_work_item(&c1).unwrap();
                    item.product_id().to_string()
                })
                .name("Troi")
                .build(),
        )
        .unwrap();
    let exec3 = create_ready_chore_execution(&db, chore3.id.clone());
    let (exec3, run3) = db
        .start_execution_run(
            &exec3.id,
            "worker-3",
            "mono",
            "lease-3",
            "mono-agent-003",
            ws3.path().to_str().unwrap(),
        )
        .unwrap();
    finish_run_waiting_human(&db, &exec3.id, &run3.id, None);

    // Detector that returns Stale for every candidate — simulates
    // the failure mode where the worker's `@`/`@-` drifted from
    // the PR's head after push (`jj new main` after `jj git push`).
    // Keep a handle on the concrete stub so pass 2 can swap the
    // result without rebuilding the handler.
    let detector = StubPrDetector::ok_status(PrStatus::Stale {
        url: "https://github.com/spinyfin/mono/pull/433".into(),
        reason: "local commits do not match PR head abc1234".into(),
    });
    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = TestHarness::new(db.clone(), detector.clone());

    struct NoOpProbe;
    #[async_trait]
    impl MergeProbe for NoOpProbe {
        async fn probe(&self, _: &str) -> anyhow::Result<PrLifecycleProbe> {
            Ok(PrLifecycleProbe::builder()
                .url(String::new())
                .state(PrLifecycleState::Open(crate::merge_poller::OpenPrStatus::clean()))
                .labels(Vec::new())
                .review(crate::merge_poller::PrReviewState::Unknown)
                .build())
        }
    }
    let probe = NoOpProbe;

    let outcome =
        crate::merge_poller::run_one_pass(db.as_ref(), &probe, publisher.as_ref(), None, Some(&handler)).await;

    // Pass 1 — pre-fix behaviour: the recheck reaches all three
    // candidates but the detector still returns Stale on each. The
    // observability counter fires so the failure leaves a
    // breadcrumb on every sweep, but nothing transitions — exactly
    // the 2026-05-13 stuck-worker shape.
    assert_eq!(
        outcome.pr_recheck_unresolved, 3,
        "the sweep must count three unresolved recheck candidates, got {outcome:?}",
    );
    assert_eq!(
        outcome.pr_recheck_recovered, 0,
        "no transitions happen on the StalePr branch",
    );
    for chore_id in [c1.as_str(), chore2.id.as_str(), chore3.id.as_str()] {
        let item = db.get_work_item(chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Active);
                assert!(t.pr_url.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }
    for execution_id in [e1.as_str(), exec2.id.as_str(), exec3.id.as_str()] {
        let execution = db.get_execution(execution_id).unwrap();
        assert_eq!(
            execution.status,
            ExecutionStatus::WaitingHuman,
            "execution must stay in waiting_human after a Stale recheck",
        );
        assert!(
            execution.cube_lease_id.is_some(),
            "cube lease must NOT be released on the Stale branch — the worker is still alive",
        );
    }
    assert!(
        probes.snapshot().is_empty(),
        "recheck path stays quiet on Stale — no probes queued",
    );

    // Pass 2 — fix engaged: simulate the production path where
    // `jj_candidate_commit_shas`'s `committer_date(after:"<started_at>")`
    // gate has now expanded the candidate set with the worker's
    // pushed bookmark tip, so `gh api commits/{sha}/pulls` returns
    // a PR whose `head.sha` matches a local sha and `classify_pr`
    // accepts it as `Fresh`. The stub detector swaps to mirror
    // that real-world transition. All three stuck chores must
    // bind their `pr_url` and transition to `in_review` on this
    // pass — without coordinator backfill.
    *detector.result.lock().await = Ok(PrStatus::Fresh {
        url: "https://github.com/spinyfin/mono/pull/433".into(),
    });
    let outcome2 =
        crate::merge_poller::run_one_pass(db.as_ref(), &probe, publisher.as_ref(), None, Some(&handler)).await;
    assert_eq!(
        outcome2.pr_recheck_recovered, 3,
        "all three stuck workers must transition on the recovery pass, got {outcome2:?}",
    );
    assert_eq!(
        outcome2.pr_recheck_unresolved, 0,
        "no candidates should remain unresolved after the recovery pass",
    );
    // chore_implementation holds tasks in `active` while
    // reviewers are enqueued (not advanced to in_review yet).
    for chore_id in [c1.as_str(), chore2.id.as_str(), chore3.id.as_str()] {
        let item = db.get_work_item(chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(
                    t.status,
                    TaskStatus::Active,
                    "chore {chore_id} must be held in active (reviewer enqueued)",
                );
                assert_eq!(
                    t.pr_url.as_deref(),
                    Some("https://github.com/spinyfin/mono/pull/433"),
                    "chore {chore_id} must have pr_url bound on the recovery pass",
                );
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }
    for execution_id in [e1.as_str(), exec2.id.as_str(), exec3.id.as_str()] {
        let execution = db.get_execution(execution_id).unwrap();
        assert_eq!(
            execution.status,
            ExecutionStatus::Completed,
            "execution {execution_id} must finalise on the recovery pass",
        );
        assert!(
            execution.cube_lease_id.is_none(),
            "cube lease must be released on the recovery pass — worker has stopped",
        );
    }
    // Three leases released, three panes torn down: one per worker.
    assert_eq!(cube.release_calls.lock().await.len(), 3);
    assert_eq!(pane.calls.lock().await.len(), 3);
    // No probes queued even on the success path — recheck never
    // probes (that's a Stop-event-only side effect).
    assert!(probes.snapshot().is_empty());
}

#[test]
fn parse_repo_slug_handles_ssh_https_and_trailing_dotgit() {
    assert_eq!(
        parse_repo_slug("git@github.com:spinyfin/mono.git").unwrap(),
        "spinyfin/mono",
    );
    assert_eq!(
        parse_repo_slug("https://github.com/spinyfin/mono.git").unwrap(),
        "spinyfin/mono",
    );
    assert_eq!(
        parse_repo_slug("https://github.com/spinyfin/mono").unwrap(),
        "spinyfin/mono",
    );
    assert_eq!(
        parse_repo_slug("https://github.com/spinyfin/mono/").unwrap(),
        "spinyfin/mono",
    );
    // Anything not on github.com is rejected — we don't have a
    // generic resolver for self-hosted GitHub Enterprise yet, so
    // surfacing an explicit error keeps the failure mode obvious.
    assert!(parse_repo_slug("git@gitlab.com:foo/bar.git").is_err());
    assert!(parse_repo_slug("https://github.com/spinyfin").is_err());
}

/// AI #5 (incident 001): when the `detect_pr_cold_fallback` feature
/// flag is OFF the cold-path fallback must not call the detector
/// even for a `waiting_human` execution with an empty staged-URL
/// cache. The outcome is the new quiet `FallbackDisabledByFlag`,
/// no probe gets queued, the work item stays at its pre-Stop
/// state, and the lease/pane are NOT torn down — the human is
/// the next actor.
#[tokio::test]
async fn on_stop_skips_detector_when_feature_flag_is_off() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // Detector wired with a deliberately-wrong URL so any
    // accidental fall-through would surface as a wrong pr_url on
    // the chore.
    let detector = StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("detect_pr_cold_fallback", false).unwrap();

    let TestHarness {
        handler, pane, probes, ..
    } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_feature_flags(flags.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(outcome, StopOutcome::FallbackDisabledByFlag);
    assert_eq!(
        detector.call_count(),
        0,
        "feature-flag gate must short-circuit before the detector is consulted",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            // Chore stays put — no transition to `in_review` or anything else.
            assert_eq!(t.status, TaskStatus::Active);
            assert!(t.pr_url.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::WaitingHuman,
        "execution must remain `waiting_human` for the human to resolve",
    );
    assert!(
        execution.cube_lease_id.is_some(),
        "cube lease must be retained — the human may want to re-enter the workspace",
    );
    assert!(
        pane.calls.lock().await.is_empty(),
        "pane teardown must NOT fire when the fallback is suppressed by flag",
    );
    assert!(
        probes.snapshot().is_empty(),
        "no probe must be queued — the human is the next actor",
    );
}

/// AI #5 mirror: the merge-poller's `recheck_for_pr` sweep must
/// honour the same flag. The poller fires every ~60s, so a stuck
/// `detect_pr_cold_fallback=false` setting must keep the
/// fallback off on every sweep, not just the on-Stop path.
#[tokio::test]
async fn recheck_for_pr_skips_detector_when_feature_flag_is_off() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("detect_pr_cold_fallback", false).unwrap();

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_feature_flags(flags.clone());

    let outcome = handler.recheck_for_pr(&execution_id).await;
    assert_eq!(outcome, StopOutcome::FallbackDisabledByFlag);
    assert_eq!(detector.call_count(), 0);
}

/// Default-ON safety contract: with NO override file and no
/// explicit wiring (the typical test path), `detect_pr` MUST still
/// fire. This guards against a future regression where the flag's
/// default flips off by accident — the change would show up here
/// as the test going green on the wrong branch.
#[tokio::test]
async fn on_stop_calls_detector_when_feature_flag_defaults_on() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap(); // missing file → registry default (true)

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_feature_flags(flags);

    let outcome = handler.on_stop(&execution_id).await;
    // chore_implementation holds task and enqueues reviewer.
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "default-ON must still let `detect_pr` fire; got {outcome:?}",
    );
    assert_eq!(detector.call_count(), 1);
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected chore, got {other:?}"),
    }
}

// -----------------------------------------------------------
// Resume-bounce SHA-delta gate regressions.
//
// Reproduces the nudge-loop bug: when a chore was bounced back
// to a worker that already had a PR bound (`chore.pr_url`
// populated), the cold-path detector kept missing the bound PR
// (it searches by `boss/<execution_id>` branch, which is a
// FRESH name for the resume execution but the worker correctly
// pushed to the OLD branch where the PR lives). That false
// miss queued `PROBE_NO_PR`, the worker explained "PR exists",
// the runtime nudged again — loop.
//
// The fix: when `chore.pr_url` is bound, ignore the cold-path
// detector and verify contribution via SHA delta on the bound
// PR's head ref instead. The tests below pin three cases:
//   1. Resume + push (head moved) → no probe, chore finalized.
//   2. Resume + no push (head same) → probe fires.
//   3. No bound PR → existing branch detector still runs
//      (new-PR flow preserved).
// -----------------------------------------------------------

#[tokio::test]
async fn resume_push_to_bound_pr_finalizes_without_nudge() {
    // Resume scenario: chore already had PR 606 bound from a
    // prior run, this run pushed a fix commit so the bound PR's
    // head moved during the run. The cold-path detector would
    // miss the PR (it searches by the new execution's branch
    // name, not the OLD branch where the PR lives) — that's the
    // bug. The SHA-delta gate must intervene and finalize.
    let workspace = tempdir().unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/606";
    let head_before = "1111111111111111111111111111111111111111";
    let (_dir, db, product_id, chore_id, execution_id) = resume_fixture(workspace.path(), pr_url, head_before);
    // Cold-path detector reports None — this is what the live
    // engine sees on a resume because the detector searches by
    // `boss/<new-execution-id>`, which has no PR.
    let detector = StubPrDetector::ok(None);
    let verifier = StubBranchVerifier::ok("boss/exec_old");
    // Worker pushed a fix commit: head SHA moved.
    verifier
        .set_head_oid(Ok("2222222222222222222222222222222222222222".into()))
        .await;

    let TestHarness {
        handler,
        cube,
        publisher,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier);

    let outcome = handler.on_stop(&execution_id).await;
    // chore_implementation holds task and enqueues reviewer.
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { ref pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/606"),
        "SHA-delta gate must finalize the bound PR when the head moved; got {outcome:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "no probe must fire when the bound PR moved during this run; saw {:?}",
        probes.snapshot()
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            // Task held in active (reviewer enqueued); pr_url stamped.
            assert_eq!(t.status, TaskStatus::Active);
            assert_eq!(t.pr_url.as_deref(), Some(pr_url));
        }
        other => panic!("expected chore, got {other:?}"),
    }
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(execution.finished_at.is_some());
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "cube lease must be released after the SHA-delta finalize"
    );
    let work_events = publisher.events.lock().await.clone();
    assert!(
        work_events.iter().any(|(p, w, _)| p == &product_id && w == &chore_id),
        "work-item invalidation must fire for the chore",
    );
}

#[tokio::test]
async fn resume_without_push_to_bound_pr_still_probes() {
    // Resume bounce-back where the worker exited without pushing
    // any commit. The gate must NOT swallow this case — the
    // loop-catch nudge is load-bearing for genuinely-idle workers.
    //
    // A PR is already bound, though, so the nudge must point the
    // worker at the *existing* PR (never `gh pr create`): this is
    // this defect family (nudging with "create a PR" when one already exists). The first nudge fires under the
    // circuit-breaker cap.
    let workspace = tempdir().unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/606";
    let head = "1111111111111111111111111111111111111111";
    let (_dir, db, _product_id, chore_id, execution_id) = resume_fixture(workspace.path(), pr_url, head);
    let detector = StubPrDetector::ok(None);
    let verifier = StubBranchVerifier::ok("boss/exec_old");
    // Head SHA matches the snapshot — worker didn't push.
    verifier.set_head_oid(Ok(head.into())).await;

    let TestHarness {
        handler, cube, probes, ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_branch_verifier(verifier);

    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "unchanged head SHA means no contribution; probe must fire"
    );
    let queued = probes.snapshot();
    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued[0].1,
        probe_push_to_existing_pr(pr_url),
        "bound PR exists: nudge must target the existing PR, not `gh pr create`",
    );
    assert!(
        !queued[0].1.contains("gh pr create"),
        "a worker with a bound PR must never be told to `gh pr create`",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        // Chore stays put — no finalize.
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active);
            assert_eq!(t.pr_url.as_deref(), Some(pr_url));
        }
        other => panic!("expected chore, got {other:?}"),
    }
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "lease must stay held; the worker has unfinished business"
    );
}

#[tokio::test]
async fn new_pr_flow_still_falls_through_to_cold_detector() {
    // Regression guard: when `chore.pr_url` is empty (new-PR
    // flow, first run of the chore), the SHA-delta gate must
    // declare itself inapplicable and let the existing
    // branch-keyed detector run unchanged. Otherwise the fix
    // would regress the brand-new-PR path.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // No `chore.pr_url` set; no `pr_head_before` snapshot.
    let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    // chore_implementation holds task and enqueues reviewer.
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "expected ReviewerEnqueued; got {outcome:?}",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active);
            assert_eq!(t.pr_url.as_deref(), Some("https://github.com/foo/bar/pull/42"));
        }
        other => panic!("expected chore, got {other:?}"),
    }
    assert!(
        probes.snapshot().is_empty(),
        "new-PR flow must not probe; got {:?}",
        probes.snapshot()
    );
}

#[tokio::test]
async fn resume_with_missing_snapshot_nudges_to_existing_pr_not_create() {
    // Fail-safe: `chore.pr_url` is bound but `pr_head_before` was
    // never captured (e.g. the snapshot fetch failed at run start),
    // so the SHA-delta gate is inapplicable and the cold-path
    // branch detector runs — and misses the PR, because it searches
    // this execution's own branch. Pre-fix that false miss queued
    // `PROBE_NO_PR` ("create a PR"); the bound PR must be
    // resolved from the structured `pr_url` even with no snapshot,
    // so the worker is pointed at the existing PR instead.
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
    // Intentionally NOT calling `set_execution_pr_head_before`.
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "bound PR exists but worker idle: nudge fires (under the breaker cap)",
    );
    let queued = probes.snapshot();
    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued[0].1,
        probe_push_to_existing_pr(pr_url),
        "missing snapshot must NOT regress to `gh pr create`; resolve the bound PR from pr_url",
    );
    assert!(!queued[0].1.contains("gh pr create"));
}

// -----------------------------------------------------------
// Auto-nudge circuit breaker (the Worf incident).
//
// exec_18b3945c5b7d7e78_1b (ci_remediation on a chore) was sent
// the "produce a PR" nudge 20 times because the chore's PR #869 was
// bound on a sibling chore_implementation exec, not on the
// remediation exec's own row — the branch-keyed cold-path search
// missed it and concluded "no PR". The two guards below pin:
//   1. A ci_remediation exec whose chore has a bound PR is NEVER
//      told to `gh pr create`, and the breaker parks it after N
//      unproductive nudges instead of looping forever.
//   2. A genuine no-PR chore_implementation exec still gets the
//      "produce a PR" nudge (healthy case preserved), but the
//      breaker bounds even that after N.
// -----------------------------------------------------------

#[tokio::test]
async fn ci_remediation_with_bound_pr_never_creates_and_breaker_parks() {
    let workspace = tempdir().unwrap();
    let (_dir, db, product_id, _chore_id, execution_id, _attempt_id) = ci_remediation_fixture(workspace.path());
    let bound_pr = "https://github.com/spinyfin/mono/pull/88";
    // Cold-path detector finds no PR on the remediation exec's own
    // branch — exactly the Worf false miss.
    let detector = StubPrDetector::ok(None);

    // Default cap is 3: nudges 1..=3 fire, the 4th trips.
    let TestHarness {
        handler,
        publisher,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);

    let mut outcomes = Vec::new();
    for _ in 0..4 {
        outcomes.push(handler.on_stop(&execution_id).await);
    }

    // First three nudges fire; all target the existing PR, none say
    // `gh pr create`, none are the "produce a PR" probe.
    let queued = probes.snapshot();
    assert_eq!(queued.len(), 3, "exactly 3 nudges before the breaker trips");
    for (_, text) in &queued {
        assert_eq!(text, &probe_push_to_existing_pr(bound_pr));
        assert!(!text.contains("gh pr create"), "must never instruct create");
        assert_ne!(text, PROBE_NO_PR, "must never send the produce-a-PR nudge");
    }
    assert!(
        matches!(outcomes[0], StopOutcome::AwaitingInput),
        "first nudge fires; got {:?}",
        outcomes[0]
    );
    assert!(
        matches!(outcomes[3], StopOutcome::NudgeBreakerParked { .. }),
        "the 4th attempt must trip the breaker; got {:?}",
        outcomes[3]
    );

    // The execution is parked with a surfaced attention item.
    let items = db.list_attention_items(&execution_id).unwrap();
    let parked = items
        .iter()
        .find(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND)
        .expect("breaker must file an attention item");
    assert!(
        parked.body_markdown.contains(bound_pr),
        "parked reason should name the existing PR; got {:?}",
        parked.body_markdown
    );
    // Idempotent: only one attention item despite the repeated trips.
    assert_eq!(
        items.iter().filter(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND).count(),
        1,
        "repeated trips must not pile up duplicate attention items",
    );
    // Surfaced to the coordinator/UI.
    let typed = publisher.typed_events.lock().await.clone();
    assert!(
        typed
            .iter()
            .any(|(p, ev)| p == &product_id && matches!(ev, boss_protocol::FrontendEvent::AttentionItemCreated { .. })),
        "an AttentionItemCreated event must be published; got {typed:?}",
    );
    // Exactly one legacy AttentionItemCreated event, despite the repeated
    // trips above — this still-live event surface is distinct from the
    // newer AttentionCreated/attentions_created path the populator uses.
    assert_eq!(publisher.attention_items_created().await, 1);
}

#[tokio::test]
async fn ci_remediation_with_flaky_retrigger_signal_parks_without_nudging() {
    // Issue #1205: the worker diagnosed the CI failure as flaky/infra
    // and re-ran the job (`mark-retriggered`), which armed the
    // `ci_flaky_retriggered` signal. On the next Stop the completion
    // path must park the worker (no nudge, no diff probe) — the stuck
    // loop is the bug. It must also NOT mark the (already-terminal)
    // attempt failed.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id, attempt_id) = ci_remediation_fixture(workspace.path());
    // The worker's marker: flip the attempt terminal + arm the signal.
    db.mark_ci_remediation_retriggered(&attempt_id)
        .unwrap()
        .expect("retrigger flip");

    // Detector finds no PR on the remediation exec's own branch — the
    // same false miss that would otherwise drive the nudge loop.
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);

    // Probe it several times: every Stop must park, never nudge.
    let mut outcomes = Vec::new();
    for _ in 0..3 {
        outcomes.push(handler.on_stop(&execution_id).await);
    }

    assert!(
        probes.snapshot().is_empty(),
        "a flaky-retriggered worker must never be nudged; got {:?}",
        probes.snapshot(),
    );
    for outcome in &outcomes {
        assert!(
            matches!(outcome, StopOutcome::FlakyRetriggered { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/88"),
            "every Stop must park as FlakyRetriggered; got {outcome:?}",
        );
    }

    // The catch-all finalizer must NOT mark the attempt failed — it is
    // terminal `retriggered`, not a give-up.
    let attempt = db.get_ci_remediation(&attempt_id).unwrap().unwrap();
    assert_eq!(attempt.status, "retriggered");
    assert!(
        attempt.failure_reason.is_none(),
        "retrigger is not a failure; got {:?}",
        attempt.failure_reason,
    );

    // No breaker attention item is filed — parking here is the normal,
    // expected outcome, not a tripped circuit breaker.
    let items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        !items.iter().any(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND),
        "flaky park must not masquerade as a breaker trip",
    );
    let _ = chore_id;
}

#[tokio::test]
async fn genuine_no_pr_chore_still_nudges_then_breaker_parks() {
    // Healthy case: a chore_implementation exec with no bound PR and
    // no PR on its branch. The legitimate "produce a PR" nudge must
    // still fire — but the breaker bounds it too. Cap lowered to 2
    // to keep the test short.
    //
    // `exec_18b932df99d17658_475` incident: a worker that concludes
    // there's nothing left to do (whether or not it emits the sanctioned
    // NO_CHANGES_NEEDED marker) must not be left parked holding its cube
    // lease and worker slot forever once the breaker gives up on it —
    // the auto-remediation this test now also proves.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);

    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_max_unproductive_nudges(2);

    let o1 = handler.on_stop(&execution_id).await;
    let o2 = handler.on_stop(&execution_id).await;
    let o3 = handler.on_stop(&execution_id).await;

    let queued = probes.snapshot();
    assert_eq!(queued.len(), 2, "the legitimate produce-a-PR nudge fires up to the cap");
    assert_eq!(
        queued[0].1, PROBE_NO_PR,
        "healthy no-PR case must still nudge to create"
    );
    assert_eq!(queued[1].1, PROBE_NO_PR);
    assert!(matches!(o1, StopOutcome::AwaitingInput));
    assert!(matches!(o2, StopOutcome::AwaitingInput));
    assert!(
        matches!(o3, StopOutcome::NudgeBreakerParked { .. }),
        "breaker must bound the no-PR nudge after the cap; got {o3:?}",
    );
    // Chore is left exactly as it was — no false "done" finalize; a
    // human or a re-dispatch decides what happens next.
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active);
            assert!(t.pr_url.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
    // The execution itself, however, MUST be finalized — this is the
    // auto-remediation: the slot and lease are freed instead of being
    // held by a parked worker forever.
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::Abandoned,
        "a breaker-tripped no-op conclusion must finalize the execution, not leave it parked",
    );
    assert!(execution.cube_lease_id.is_none());
    assert!(execution.cube_workspace_id.is_none());
    assert!(execution.finished_at.is_some());
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "the breaker-tripped park must release the cube lease",
    );
    assert_eq!(
        pane.calls.lock().await.as_slice(),
        [execution_id.as_str()],
        "the breaker-tripped park must tear down the worker pane",
    );
    assert!(
        publisher
            .publish_calls
            .lock()
            .await
            .iter()
            .any(|(_, _, _, reason)| reason == "worker_idle_park_finalized"),
        "must publish a worker_idle_park_finalized event",
    );

    // Idempotent: a further Stop (hook re-fire) on the now-terminal
    // execution must not double-release or re-park.
    let o4 = handler.on_stop(&execution_id).await;
    assert!(
        matches!(o4, StopOutcome::AlreadyTerminal),
        "a re-fired Stop on an already-abandoned execution must be AlreadyTerminal; got {o4:?}",
    );
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "a re-fired Stop must not release the lease a second time",
    );

    let items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        items.iter().any(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND),
        "parking must file an attention item",
    );
}

// -----------------------------------------------------------
// Sanctioned no-op terminal for chore_implementation.
//
// A fresh chore_implementation worker whose work is already done on
// main (empty diff, no PR) must be able to terminate cleanly. When it
// emits the NO_CHANGES_NEEDED marker the engine closes the task as
// done WITHOUT a PR and sends NO nudge — replacing the produce-a-PR
// nudge loop. A worker that stops with no marker is still nudged.
// -----------------------------------------------------------

#[tokio::test]
async fn no_op_marker_closes_task_as_done_without_nudge() {
    // The incident path: a chore_implementation worker verified the
    // work was already done (empty diff, no PR) and emitted
    // NO_CHANGES_NEEDED. It must terminate ONCE as a clean no-op: task
    // → done (no pr_url), NO probe queued, lease + pane released, NO
    // breaker attention item.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "## Summary\nPRs #1559 and #1561 already cleaned all three breadcrumb patterns on \
         main; the working copy has no diff.\n\nNO_CHANGES_NEEDED\n",
    );
    let detector = StubPrDetector::ok(None);

    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::NoChangesNeeded { work_item_id } if work_item_id == &chore_id),
        "expected NoChangesNeeded for the chore; got {outcome:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "a sanctioned no-op must NOT queue a produce-a-PR probe; got {:?}",
        probes.snapshot(),
    );
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Done, "no-op must close the task as done");
            assert!(t.pr_url.is_none(), "no-op must not stamp a pr_url; got {:?}", t.pr_url);
        }
        other => panic!("expected chore, got {other:?}"),
    }
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(execution.cube_lease_id.is_none());
    assert!(execution.finished_at.is_some());
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "no-op completion must release the cube lease",
    );
    assert_eq!(
        pane.calls.lock().await.as_slice(),
        [execution_id.as_str()],
        "no-op completion must tear down the worker pane",
    );
    let items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        !items.iter().any(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND),
        "a clean no-op must not masquerade as a parked nudge-breaker trip",
    );
    assert!(
        publisher
            .publish_calls
            .lock()
            .await
            .iter()
            .any(|(_, _, _, reason)| reason == "worker_no_op_completed"),
        "no-op completion must publish a worker_no_op_completed event",
    );

    // Idempotent: a second Stop (hook re-fire) is a quiet terminal — no
    // second nudge, no re-loop.
    let outcome2 = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome2, StopOutcome::AlreadyTerminal),
        "a re-fired Stop on an already-closed no-op must be AlreadyTerminal; got {outcome2:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "the re-fired Stop must not queue a probe either",
    );
}

#[tokio::test]
async fn no_op_without_marker_still_nudges_to_produce_pr() {
    // Guardrail: a worker that stopped with NO PR and did NOT emit the
    // NO_CHANGES_NEEDED marker is "gave up / not done", not "verified
    // already done". The legitimate produce-a-PR nudge must still fire —
    // the no-op gate must NOT globally suppress it.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // A real transcript exists, but it does NOT contain the marker.
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "## Summary\nI made some progress but have not finished the change yet.\n",
    );
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::AwaitingInput),
        "no marker → normal produce-a-PR nudge; got {outcome:?}",
    );
    let queued = probes.snapshot();
    assert_eq!(
        queued.len(),
        1,
        "the no-PR worker without a marker is still nudged once"
    );
    assert_eq!(queued[0].1, PROBE_NO_PR, "the nudge is the produce-a-PR probe");
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(
                t.status,
                TaskStatus::Active,
                "no marker → task is NOT closed as a no-op"
            );
            assert!(t.pr_url.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

// -----------------------------------------------------------
// Worker escalation/blocker detection (incident 2026-07-02,
// exec_18b5243e65ff188_2d): a worker that emits an
// `[effort-escalation]` or `[blocked]` marker on its Stop boundary must
// get an attention item filed for the coordinator, and the "produce a
// PR" auto-nudge must be suppressed while it is unresolved. A
// coordinator probe on the run resolves it and resumes normal nudging.
// -----------------------------------------------------------

#[tokio::test]
async fn well_formed_effort_escalation_files_attention_and_suppresses_nudge() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "Hit a bazel toolchain error I can't resolve.\n\n\
         [effort-escalation] requested_level=large reason=\"multi-subsystem race; rule-3 missed\"\n",
    );
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::EscalationPending { .. }),
        "an unresolved escalation must suppress the auto-nudge; got {outcome:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "the produce-a-PR nudge must NOT fire while the escalation is unresolved; got {:?}",
        probes.snapshot(),
    );
    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == worker_escalation::WORKER_ESCALATION_ATTENTION_KIND)
            .count(),
        1,
        "exactly one worker_escalation attention item must be filed; got {items:?}",
    );
    let item = items
        .iter()
        .find(|i| i.kind == worker_escalation::WORKER_ESCALATION_ATTENTION_KIND)
        .unwrap();
    assert_eq!(item.status, "open");
    assert!(
        item.body_markdown.contains("requested_level=large"),
        "attention body must carry the marker verbatim; got: {}",
        item.body_markdown,
    );
    assert!(
        !item.body_markdown.contains("Parse warning"),
        "a well-formed marker must not carry a parse warning; got: {}",
        item.body_markdown,
    );
    // Task stays untouched — this is not a completion, just a pause.
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_effort_escalation_is_still_filed_with_a_parse_warning() {
    // O'Brien's incident marker: bare, no requested_level, no reason.
    // Malformed markers must still be surfaced, not silently dropped.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "I need guidance before proceeding.\n\n[effort-escalation]\n",
    );
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::EscalationPending { .. }),
        "got {outcome:?}"
    );
    assert!(probes.snapshot().is_empty());
    let items = db.list_attention_items(&execution_id).unwrap();
    let item = items
        .iter()
        .find(|i| i.kind == worker_escalation::WORKER_ESCALATION_ATTENTION_KIND)
        .expect("malformed marker must still file an attention item");
    assert!(
        item.body_markdown.contains("Parse warning"),
        "a malformed marker must be flagged with a parse warning; got: {}",
        item.body_markdown,
    );
}

#[tokio::test]
async fn blocked_marker_files_attention_and_suppresses_nudge() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[blocked] reason=\"bazel E0583, survives clean --expunge; need explicit direction\"\n",
    );
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::EscalationPending { .. }),
        "got {outcome:?}"
    );
    assert!(probes.snapshot().is_empty());
    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == worker_escalation::WORKER_BLOCKED_ATTENTION_KIND)
            .count(),
        1,
        "exactly one worker_blocked attention item must be filed; got {items:?}",
    );
}

#[tokio::test]
async fn blocked_marker_files_attention_even_while_execution_still_running() {
    // A worker can emit the sanctioned `[blocked]` marker while
    // `execution.status` is still `running` — briefly between
    // `start_execution_run` and the coordinator's post-spawn-ack flip
    // to `waiting_human`, and unconditionally for a `pr_review`
    // reviewer pane (`RunWaitState::ReviewerPaneAlive` keeps it in
    // `running` for the pane's whole lifetime, by design). A
    // `[blocked]` marker emitted in either state must still be filed as
    // an attention item; it must not sit behind the `waiting_human`-only
    // gate that used to skip `detect_and_file_worker_signals` entirely
    // for any execution not in exactly that one status.
    let workspace = tempdir().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Detect worker stop while running");
    let execution = create_ready_chore_execution(&db, chore.id.clone());
    let (execution, _run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::Running,
        "fixture must leave the execution in `running` — the state under test",
    );
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution.id,
        "[blocked] reason=\"design doc marks this deferred; need coordinator confirmation before pulling it into scope\"\n",
    );
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution.id).await;
    assert!(
        matches!(outcome, StopOutcome::RunningNoStagedPr),
        "a `running` execution still falls through the PR-detection gate as a no-op; got {outcome:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "no produce-a-PR nudge may be queued for a running execution",
    );
    let items = db.list_attention_items(&execution.id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == worker_escalation::WORKER_BLOCKED_ATTENTION_KIND)
            .count(),
        1,
        "the [blocked] marker must be filed as an attention item even though \
         `execution.status` is `running`, not `waiting_human`; got {items:?}",
    );
}

#[tokio::test]
async fn blocked_marker_on_first_stop_clears_any_stale_queued_probe() {
    // The completion handler correctly refuses to *queue a new*
    // produce-a-PR nudge once `[blocked]` is detected (the case
    // above), but `dispatch_probe_on_stop` in the real event loop
    // pops and delivers whatever is already sitting in the run's
    // pending-probe queue on every Stop, independent of this Stop's
    // own completion outcome. A probe minted on an earlier Stop
    // (e.g. queued, then requeued for retry after a failed
    // `SendToPane`) would therefore still fire on the very Stop
    // where the worker reports `[blocked]`. `on_stop` must clear any
    // such stale probe via the `ProbeQueuer` the moment it finds an
    // unresolved worker signal, on the FIRST Stop that carries the
    // marker — not just refrain from adding a new one.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[blocked] reason=\"bazel E0583, survives clean --expunge; need explicit direction\"\n",
    );
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::EscalationPending { .. }),
        "got {outcome:?}"
    );
    assert!(
        probes.snapshot().is_empty(),
        "no new nudge should be queued on the blocked Stop"
    );
    assert_eq!(
        probes.clear_snapshot(),
        vec![execution_id.clone()],
        "on_stop must clear any stale queued probe for this run so dispatch_probe_on_stop \
         cannot deliver a nudge minted before the blocker was recognized",
    );
}

// -----------------------------------------------------------
// Worker-proposal seam (worker-proposal-api-replace-fragile-worker-to-engine-seams.md):
// `worker_signal_proposals_seam` makes `detect_and_file_worker_signals`
// read proposals-first, demoting the marker parsers to a counted
// fallback.
// -----------------------------------------------------------

#[tokio::test]
async fn proposals_first_flag_skips_legacy_marker_when_a_proposal_already_exists() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // The worker already called `boss propose effort-escalation` — this
    // auto-applies synchronously and files the attention item.
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::EffortEscalation,
        payload_json: r#"{"requested_level":"large","reason":"multi-subsystem race"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    // The final message also carries the legacy marker (e.g. an older
    // habit, or belt-and-suspenders) — proposals-first must not re-file
    // it as a second attention item.
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[effort-escalation] requested_level=large reason=\"multi-subsystem race\"\n",
    );

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("worker_signal_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::EscalationPending { .. }),
        "got {outcome:?}"
    );
    assert!(probes.snapshot().is_empty());

    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == worker_escalation::WORKER_ESCALATION_ATTENTION_KIND)
            .count(),
        1,
        "the proposal's synchronous apply already filed the attention item; the legacy \
         marker parser must not re-file it; got {items:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.effort_escalation"),
        Some(0),
        "no fallback hit expected — an existing proposal covered this signal",
    );
}

#[tokio::test]
async fn proposals_first_flag_falls_back_to_the_legacy_marker_and_counts_the_hit() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    // No proposal was ever submitted for this execution — only the
    // legacy marker.
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[blocked] reason=\"bazel E0583, survives clean --expunge; need explicit direction\"\n",
    );

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("worker_signal_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::EscalationPending { .. }),
        "got {outcome:?}"
    );
    assert!(probes.snapshot().is_empty());

    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == worker_escalation::WORKER_BLOCKED_ATTENTION_KIND)
            .count(),
        1,
        "no proposal existed, so the legacy parser must still file the attention item; \
         got {items:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.blocked"),
        Some(1),
        "the legacy path fired, so the seam's fallback-hit counter must increment",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.effort_escalation"),
        Some(0),
        "only the blocked seam fired in this test",
    );

    // The marker line never disappears from the transcript once emitted,
    // so a second terminal Stop against the same cumulative transcript
    // must not re-increment the exit-criterion counter — the attention
    // item is already filed and the fallback hit already counted.
    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::EscalationPending { .. }),
        "got {outcome:?}"
    );
    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == worker_escalation::WORKER_BLOCKED_ATTENTION_KIND)
            .count(),
        1,
        "the marker is already filed; a repeat Stop must not file a duplicate; got {items:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.blocked"),
        Some(1),
        "a repeat Stop against the same already-filed marker must not re-increment the \
         fallback-hit counter",
    );
}

#[tokio::test]
async fn proposals_first_flag_off_matches_pre_migration_behavior_exactly() {
    // Even with an existing proposal AND the legacy marker both present,
    // the flag defaulting off must reproduce the exact pre-seam
    // behavior: the legacy parser always runs, no proposals-first
    // check, no fallback counting.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::EffortEscalation,
        payload_json: r#"{"requested_level":"large","reason":"multi-subsystem race"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[effort-escalation] requested_level=large reason=\"multi-subsystem race\"\n",
    );

    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    // No `with_feature_flags` call — default store, flag at its
    // registry default (off).
    let handler = handler.with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::EscalationPending { .. }),
        "got {outcome:?}"
    );
    assert!(probes.snapshot().is_empty());

    // Pre-migration behavior: the proposal's own apply files one
    // attention item, and the (un-gated) legacy parser files its own —
    // `file_worker_signal_attention`'s content dedup only matches
    // identical marker-line text already present in an item's body, and
    // the proposal-authored item's body does not contain the literal
    // marker line, so both land. This is intentionally what "flag off
    // restores today's behavior exactly" means.
    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == worker_escalation::WORKER_ESCALATION_ATTENTION_KIND)
            .count(),
        2,
        "flag off: the proposal apply and the legacy parser each file independently, exactly \
         as they did before this seam existed; got {items:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.effort_escalation"),
        Some(0),
        "fallback counting only happens when the seam flag is on",
    );
}

#[tokio::test]
async fn proposals_first_flag_still_files_a_later_marker_with_a_distinct_reason() {
    // Regression test: an execution that proposed `blocked` early (reason
    // A) and later — after `boss propose` presumably stopped working —
    // fell back to the `[blocked]` bootstrap marker with a *different*
    // reason (B) must have that second, distinct signal filed. A
    // kind-scoped (content-blind) skip would silently drop it, since the
    // Stop-boundary transcript is cumulative and a `blocked` proposal
    // already exists for this execution.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::Blocked,
        payload_json: r#"{"reason":"reason A"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    write_assistant_transcript(&db, workspace.path(), &execution_id, "[blocked] reason=\"reason B\"\n");

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("worker_signal_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::EscalationPending { .. }),
        "got {outcome:?}"
    );
    assert!(probes.snapshot().is_empty());

    let items = db.list_attention_items(&execution_id).unwrap();
    let blocked_items: Vec<_> = items
        .iter()
        .filter(|i| i.kind == worker_escalation::WORKER_BLOCKED_ATTENTION_KIND)
        .collect();
    // Two items: one filed synchronously by the reason-A proposal's apply
    // pipeline (at submission time, above), one filed by the legacy
    // parser for the reason-B marker the content-aware check correctly
    // did NOT treat as covered by the reason-A proposal.
    assert_eq!(
        blocked_items.len(),
        2,
        "reason B is a distinct signal from reason A's proposal and must be filed \
         independently, not silently discarded because a same-kind proposal already exists; \
         got {items:?}",
    );
    assert!(
        blocked_items.iter().any(|i| i.body_markdown.contains("reason A")),
        "the reason-A proposal's own synchronous apply must still have filed its item; \
         got {items:?}",
    );
    assert!(
        blocked_items.iter().any(|i| i.body_markdown.contains("reason B")),
        "the reason-B marker must be filed by the legacy parser, not silently discarded; \
         got {items:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.blocked"),
        Some(1),
        "the legacy path fired for the unmatched marker, so the fallback-hit counter must \
         increment",
    );
}

#[tokio::test]
async fn proposals_first_flag_still_files_a_reasonless_marker_despite_an_existing_proposal() {
    // Regression test: a malformed bare `[blocked]` marker (no `reason=`)
    // must never be treated as covered by an existing same-kind
    // proposal — that would silently drop it, the exact failure mode the
    // content-aware rewrite exists to remove, just for unreasoned
    // markers specifically. It must still be filed (and still counted as
    // a fallback hit), relying on file_worker_signal_attention's own
    // marker-line dedup to prevent actual double-filing.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::Blocked,
        payload_json: r#"{"reason":"reason A"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    write_assistant_transcript(&db, workspace.path(), &execution_id, "[blocked]\n");

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("worker_signal_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::EscalationPending { .. }),
        "got {outcome:?}"
    );
    assert!(probes.snapshot().is_empty());

    let items = db.list_attention_items(&execution_id).unwrap();
    let blocked_items: Vec<_> = items
        .iter()
        .filter(|i| i.kind == worker_escalation::WORKER_BLOCKED_ATTENTION_KIND)
        .collect();
    assert_eq!(
        blocked_items.len(),
        2,
        "the bare reason-less marker must be filed independently of the reason-A proposal, \
         not silently discarded; got {items:?}",
    );
    assert!(
        blocked_items
            .iter()
            .any(|i| i.body_markdown.contains("```\n[blocked]\n```")),
        "the bare marker must be filed by the legacy parser; got {items:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.blocked"),
        Some(1),
        "the legacy path fired for the reason-less marker, so the fallback-hit counter must \
         increment",
    );
}

// -----------------------------------------------------------
// Deferred-scope declaration: a worker that emits a `[deferred-scope]`
// marker must get a durable audit line on the work item's description
// AND a coordinator-visible
// attention item — but, unlike escalation/blocker markers, must NOT
// suppress the "produce a PR" nudge: the worker already produced its
// (narrower) deliverable.
// -----------------------------------------------------------

#[tokio::test]
async fn well_formed_deferred_scope_records_audit_line_and_attention_item() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "Wired the auth and billing modules as asked.\n\n\
         [deferred-scope] summary=\"notifications wiring\" reason=\"needs new data plumbing, not just wiring\"\n",
    );
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);

    handler.on_stop(&execution_id).await;

    // Recorded on the work item's own description, grep-able even if the
    // transcript is later pruned.
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => {
            assert!(
                t.description.contains("[deferred-scope]"),
                "description must carry a [deferred-scope] audit line; got: {}",
                t.description,
            );
            assert!(
                t.description.contains("notifications wiring"),
                "audit line must carry the deferred summary; got: {}",
                t.description,
            );
            assert_eq!(
                t.description.matches("[deferred-scope]").count(),
                1,
                "the audit line must carry the tag exactly once; got: {}",
                t.description,
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }

    let items = db.list_attention_items(&execution_id).unwrap();
    let item = items
        .iter()
        .find(|i| i.kind == crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND)
        .expect("a deferred_scope attention item must be filed");
    assert_eq!(item.status, "open");
    assert!(
        item.body_markdown.contains("notifications wiring"),
        "attention body must carry the marker verbatim; got: {}",
        item.body_markdown,
    );
    assert!(
        !item.body_markdown.contains("Parse warning"),
        "a well-formed marker must not carry a parse warning; got: {}",
        item.body_markdown,
    );
}

#[tokio::test]
async fn malformed_deferred_scope_is_still_recorded_with_a_parse_warning() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(&db, workspace.path(), &execution_id, "[deferred-scope]\n");
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);

    handler.on_stop(&execution_id).await;

    let items = db.list_attention_items(&execution_id).unwrap();
    let item = items
        .iter()
        .find(|i| i.kind == crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND)
        .expect("malformed marker must still be recorded");
    assert!(
        item.body_markdown.contains("Parse warning"),
        "a malformed marker must be flagged with a parse warning; got: {}",
        item.body_markdown,
    );
}

#[tokio::test]
async fn deferred_scope_does_not_suppress_the_produce_a_pr_nudge() {
    // Unlike [effort-escalation]/[blocked], a [deferred-scope] marker is
    // a completeness record, not a stop-the-world signal — the worker
    // already produced its narrower deliverable, so the normal
    // produce-a-PR nudge must still fire.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[deferred-scope] summary=\"notifications wiring\" reason=\"needs new data plumbing\"\n",
    );
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        !matches!(outcome, StopOutcome::EscalationPending { .. }),
        "a deferred-scope marker alone must not suppress the nudge; got {outcome:?}",
    );
}

#[tokio::test]
async fn repeated_stops_do_not_duplicate_the_deferred_scope_record() {
    // The marker line never disappears from the cumulative transcript
    // once emitted, so repeated Stops must not re-append the audit line
    // or re-file the attention item.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[deferred-scope] summary=\"notifications wiring\" reason=\"needs new data plumbing\"\n",
    );
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);

    handler.on_stop(&execution_id).await;
    handler.on_stop(&execution_id).await;

    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND)
            .count(),
        1,
        "the same marker must only ever file one attention item; got {items:?}",
    );
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => assert_eq!(
            t.description.matches("[deferred-scope]").count(),
            1,
            "the same marker must only ever append one audit line; got: {}",
            t.description,
        ),
        other => panic!("expected chore, got {other:?}"),
    }
}

// -----------------------------------------------------------
// Worker-proposal seam (worker-proposal-api-replace-fragile-worker-to-engine-seams.md,
// implementation task 9): `deferred_scope_proposals_seam` makes
// `detect_and_record_deferred_scope` read proposals-first, demoting the
// `[deferred-scope]` marker parser to a counted fallback. Mirrors the
// `worker_signal_proposals_seam` tests above.
// -----------------------------------------------------------

#[tokio::test]
async fn deferred_scope_proposals_first_flag_skips_legacy_marker_when_a_proposal_already_exists() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // The worker already called `boss propose deferred-scope` — this
    // auto-applies synchronously, appending the audit line and filing
    // the attention item.
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::DeferredScope,
        payload_json: r#"{"summary":"notifications wiring","reason":"needs new data plumbing, not just wiring"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    // The final message also carries the legacy marker with matching
    // fields — proposals-first must not re-record it a second time.
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[deferred-scope] summary=\"notifications wiring\" reason=\"needs new data plumbing, not just wiring\"\n",
    );

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("deferred_scope_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    handler.on_stop(&execution_id).await;

    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND)
            .count(),
        1,
        "the proposal's synchronous apply already filed the attention item; the legacy \
         marker parser must not re-record it; got {items:?}",
    );
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => assert_eq!(
            t.description.matches("[deferred-scope]").count(),
            1,
            "the proposal apply pipeline already appended the audit line; the legacy parser \
             must not append a second one; got: {}",
            t.description,
        ),
        other => panic!("expected chore, got {other:?}"),
    }
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.deferred_scope"),
        Some(0),
        "no fallback hit expected — an existing proposal covered this marker",
    );
}

#[tokio::test]
async fn deferred_scope_proposals_first_flag_falls_back_to_the_legacy_marker_and_counts_the_hit() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // No proposal was ever submitted for this execution — only the
    // legacy marker.
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[deferred-scope] summary=\"notifications wiring\" reason=\"needs new data plumbing\"\n",
    );

    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("deferred_scope_proposals_seam", true).unwrap();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    handler.on_stop(&execution_id).await;

    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND)
            .count(),
        1,
        "no proposal existed, so the legacy parser must still record the marker; got {items:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.deferred_scope"),
        Some(1),
        "the legacy path fired, so the seam's fallback-hit counter must increment",
    );

    // The marker line never disappears from the transcript once emitted,
    // so a second terminal Stop against the same cumulative transcript
    // must not re-increment the exit-criterion counter.
    handler.on_stop(&execution_id).await;
    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND)
            .count(),
        1,
        "the marker is already recorded; a repeat Stop must not record it again; got {items:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.deferred_scope"),
        Some(1),
        "a repeat Stop against the same already-recorded marker must not re-increment the \
         fallback-hit counter",
    );
}

#[tokio::test]
async fn deferred_scope_proposals_first_flag_off_matches_pre_migration_behavior_exactly() {
    // Even with an existing proposal AND the legacy marker both present,
    // the flag defaulting off must reproduce the exact pre-seam
    // behavior: the legacy parser always runs, no proposals-first
    // check, no fallback counting.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::DeferredScope,
        payload_json: r#"{"summary":"notifications wiring","reason":"needs new data plumbing"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[deferred-scope] summary=\"notifications wiring\" reason=\"needs new data plumbing\"\n",
    );

    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_metrics(metrics.clone());

    handler.on_stop(&execution_id).await;

    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND)
            .count(),
        1,
        "with the flag off the legacy parser must still record the marker unconditionally; \
         got {items:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.deferred_scope"),
        Some(0),
        "with the flag off nothing is counted",
    );
}

#[tokio::test]
async fn blocked_worker_is_never_reaped_across_repeated_stops() {
    // The other half of the auto-remediation contract: a worker with a
    // GENUINE pending question ([blocked]) must never be finalized by
    // the idle-park path, no matter how many Stops fire while it awaits
    // a coordinator decision. `nudge_or_park` short-circuits to
    // `EscalationPending` before the circuit breaker is ever consulted,
    // so `park_for_unproductive_nudges` (and its new
    // lease/pane-releasing finalizer) must never run for this case.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[blocked] reason=\"need a decision on approach A vs B before I can continue\"\n",
    );
    let detector = StubPrDetector::ok(None);
    let TestHarness {
        handler,
        cube,
        pane,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_max_unproductive_nudges(2); // lower than the number of Stops below

    // Fire more Stops than the (lowered) breaker cap. If the blocked
    // marker were not correctly suppressing the breaker, this would
    // trip `park_for_unproductive_nudges` and finalize the execution —
    // exactly the wrong behaviour for a genuine pending question.
    for _ in 0..5 {
        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::EscalationPending { .. }),
            "every Stop while [blocked] is unresolved must be EscalationPending; got {outcome:?}",
        );
    }

    assert!(
        probes.snapshot().is_empty(),
        "a genuinely blocked worker must never be nudged; got {:?}",
        probes.snapshot(),
    );
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "a genuinely blocked worker's lease must never be released",
    );
    assert!(
        pane.calls.lock().await.is_empty(),
        "a genuinely blocked worker's pane must never be torn down",
    );
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::WaitingHuman,
        "a genuinely blocked worker must stay live, not be finalized",
    );
    assert!(execution.cube_lease_id.is_some(), "lease must remain attached");
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected chore, got {other:?}"),
    }
    // No nudge-breaker attention item — only the worker_blocked one.
    let items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        !items.iter().any(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND),
        "a suppressed-nudge escalation must not also masquerade as a breaker trip",
    );
}

#[tokio::test]
async fn coordinator_resolution_resumes_normal_nudging() {
    // Fixture: a worker declares [blocked], suppressing the nudge. The
    // coordinator's ack (a probe on the run — mirrors
    // `bossctl probe <agent> "..."`, wired through
    // `resolve_worker_signal_attentions_for_execution` in the ProbeRun
    // RPC handler) resolves the attention item; the next Stop must
    // resume the normal produce-a-PR nudge even though the marker line
    // is still present in the cumulative transcript.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "[blocked] reason=\"need a decision on approach A vs B\"\n",
    );
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);

    let outcome1 = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome1, StopOutcome::EscalationPending { .. }),
        "got {outcome1:?}"
    );
    assert!(
        probes.snapshot().is_empty(),
        "nudge must be suppressed before resolution"
    );

    // Coordinator acks — resolve the attention item(s) for this execution.
    let resolved = db
        .resolve_worker_signal_attentions_for_execution(&execution_id)
        .unwrap();
    assert_eq!(resolved, 1, "exactly one open item should have been resolved");

    // Next Stop: the SAME cumulative transcript still contains the old
    // marker line, but it must not re-block the nudge — resolution
    // stuck.
    let outcome2 = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome2, StopOutcome::AwaitingInput),
        "resolution must resume the normal produce-a-PR nudge; got {outcome2:?}",
    );
    let queued = probes.snapshot();
    assert_eq!(
        queued.len(),
        1,
        "the produce-a-PR nudge must fire exactly once after resolution"
    );
    assert_eq!(queued[0].1, PROBE_NO_PR);

    // And no NEW attention item was re-filed for the stale marker line.
    let items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == worker_escalation::WORKER_BLOCKED_ATTENTION_KIND)
            .count(),
        1,
        "the stale marker line must not be re-filed as a fresh attention item; got {items:?}",
    );
    assert_eq!(
        items
            .iter()
            .find(|i| i.kind == worker_escalation::WORKER_BLOCKED_ATTENTION_KIND)
            .unwrap()
            .status,
        "resolved",
    );
}

// -----------------------------------------------------------
// Build-wait suppression (2026-07-14 log-volume incident:
// `exec_18c21add1416b5e8_3b`, `exec_18c21ba9b3fd2ef8_9e`). A worker
// narrating that it is legitimately waiting on a backgrounded
// build/test gate must not be nudged — each nudge manufactured the
// very Stop cadence that exhausted the auto-nudge circuit breaker in
// about two minutes and parked/abandoned a healthy worker mid-wait.
// -----------------------------------------------------------

#[tokio::test]
async fn build_wait_narration_suppresses_nudge_across_repeated_stops() {
    // The incident transcript, verbatim in spirit: the worker explains
    // it is waiting on an armed monitor for a backgrounded build/test
    // gate before it can push. Fire more Stops than the (lowered)
    // breaker cap would tolerate for an ordinary unproductive nudge —
    // if build-wait suppression did not short-circuit before the
    // breaker, this would trip `park_for_unproductive_nudges` and
    // discard the worker's in-progress session.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "the build gate requires an actual green build before I push, so I must let it finish. \
         The monitor (task b9qrwn8c7) is armed... I'm not going to push until the test run \
         comes back green. Waiting.",
    );
    let detector = StubPrDetector::ok(None);
    let TestHarness {
        handler,
        cube,
        pane,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_max_unproductive_nudges(2);

    for _ in 0..5 {
        let outcome = handler.on_stop(&execution_id).await;
        assert!(
            matches!(outcome, StopOutcome::BuildWaitPending { .. }),
            "every Stop while the worker narrates a build wait must be BuildWaitPending; got {outcome:?}",
        );
    }

    assert!(
        probes.snapshot().is_empty(),
        "a worker legitimately waiting on a build must never be nudged; got {:?}",
        probes.snapshot(),
    );
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "a healthy build-waiting worker's lease must never be released",
    );
    assert!(
        pane.calls.lock().await.is_empty(),
        "a healthy build-waiting worker's pane must never be torn down",
    );
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::WaitingHuman,
        "a healthy build-waiting worker must stay live, not be finalized",
    );
    assert!(execution.cube_lease_id.is_some(), "lease must remain attached");
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected chore, got {other:?}"),
    }
    // No nudge-breaker attention item was filed — the breaker was
    // never even consulted.
    let items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        !items.iter().any(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND),
        "build-wait suppression must not masquerade as a breaker trip",
    );
}

#[tokio::test]
async fn build_wait_horizon_expiry_falls_back_to_normal_nudge() {
    // Requirement: genuine wedge detection must keep working — a
    // worker that keeps narrating "waiting" without the horizon's
    // trust budget resets to normal nudging. A `0`-second horizon
    // means the very first detection is already past its budget,
    // deterministically exercising the fallback without a real
    // wall-clock wait (the elapsed-time arithmetic itself is covered
    // by `crate::build_wait_tracker`'s own unit tests).
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    write_assistant_transcript(&db, workspace.path(), &execution_id, "still building, waiting");
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_build_wait_horizon_secs(0);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::AwaitingInput),
        "an expired build-wait horizon must fall back to the normal produce-a-PR nudge; got {outcome:?}",
    );
    let queued = probes.snapshot();
    assert_eq!(
        queued.len(),
        1,
        "the normal nudge must fire once the horizon has elapsed"
    );
    assert_eq!(queued[0].1, PROBE_NO_PR);
}

// -----------------------------------------------------------
// revision_implementation stop-boundary fix (T-this).
//
// A `revision_implementation` execution must NEVER be told to
// `gh pr create` — the revision's job is to push a new commit to
// the parent task's EXISTING PR branch.  Two sub-cases pinned:
//   1. execution.pr_url was not stamped (older exec) but chain root
//      has a pr_url: chain-root lookup finds the bound PR. The
//      SHA-delta gate is Inapplicable (no `pr_head_before` snapshot
//      either), so the stuck-revision fix routes this
//      through the satisfied-deliverable gate instead of the old
//      "push to existing PR" nudge — see
//      `revision_on_stop_no_pr_head_before_snapshot_*` above for the
//      finalize/await coverage. This test pins the still-load-bearing
//      half of the original fix: PROBE_NO_PR must never fire.
//   2. No bound PR resolvable at all (anomalous data): park instead
//      of contradicting the worker with PROBE_NO_PR.
// -----------------------------------------------------------

#[tokio::test]
async fn revision_with_null_execution_pr_url_falls_back_to_chain_root_pr() {
    // T-this regression: a `revision_implementation` execution whose
    // `execution.pr_url` is NULL (created before reliable stamping)
    // must not receive PROBE_NO_PR ("open a new PR with `gh pr create`").
    // The chain-root lookup must find the parent chore's pr_url and
    // return `probe_push_to_existing_pr` instead.
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/966";
    let (_dir, db, _product_id, _revision_id, execution_id) =
        revision_fixture_no_execution_pr_url(workspace.path(), parent_pr_url);
    // Cold-path detector returns None — correct for revisions which
    // have no branch of their own.
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "revision with no execution.pr_url and no SHA-delta baseline must await quietly",
    );
    // Stuck-revision fix: with no `pr_head_before` snapshot to compare against and
    // no wired merge probe (satisfied-deliverable check is inconclusive),
    // the revision must NOT be nudged at all — in particular it must
    // never receive PROBE_NO_PR (there is a chain-root PR; `gh pr create`
    // would be wrong), and it must not receive the old "push to existing
    // PR" nudge either, since that nudge can never be satisfied once the
    // worker has already pushed (the stuck-revision dead end this fix
    // closes).
    let queued = probes.snapshot();
    assert!(
        queued.is_empty(),
        "revision with an inconclusive SHA-delta gate must not be nudged at all; got {queued:?}",
    );
}

#[tokio::test]
async fn revision_with_no_bound_pr_parks_instead_of_nudging_create() {
    // Safety net: if even the chain-root lookup yields no PR URL
    // (anomalous data — e.g. chain root never opened a PR), the
    // revision execution must be parked rather than nudged with
    // PROBE_NO_PR.  A parked revision surfaces as an attention item
    // for a human to investigate; PROBE_NO_PR would contradict the
    // worker's own task instructions.
    use crate::work::{FakePrStateChecker, PrOpenState};
    use boss_protocol::CreateRevisionInput;

    let workspace = tempdir().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product_named(&db, "Boss-revision-no-pr-test");
    // Parent chore with NO pr_url (never opened a PR).
    let parent = create_test_chore_manual(&db, product.id.clone(), "Parent chore no PR");
    // Manually set parent to in_review WITHOUT a pr_url so the
    // revision gate passes (bypassed via direct SQL) but the chain
    // root has no PR to resolve.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
            rusqlite::params![parent.id],
        )
        .unwrap();
        // Force the revision gate to see Open by setting a temporary
        // pr_url, create the revision, then clear it.
        conn.execute(
            "UPDATE tasks SET pr_url = 'https://github.com/spinyfin/mono/pull/999' WHERE id = ?1",
            rusqlite::params![parent.id],
        )
        .unwrap();
    }
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent.id.clone())
                .description("Fix conflict — no PR scenario")
                .build(),
            &checker,
        )
        .unwrap();
    // Clear the parent pr_url so the chain-root lookup yields None.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET pr_url = NULL WHERE id = ?1",
            rusqlite::params![parent.id],
        )
        .unwrap();
    }
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .prefer_is_soft(true)
                .build(),
        )
        .unwrap();
    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    finish_run_waiting_human(&db, &execution.id, &run.id, Some("spawned revision worker pane"));

    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution.id).await;
    assert!(
        matches!(outcome, StopOutcome::NudgeBreakerParked { .. }),
        "revision with no resolvable bound PR must park, not produce PROBE_NO_PR; got {outcome:?}",
    );
    let queued = probes.snapshot();
    assert!(
        queued.is_empty(),
        "no probe must be queued when parking a revision with no bound PR; got {queued:?}",
    );
    let items = db.list_attention_items(&execution.id).unwrap();
    assert!(
        items.iter().any(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND),
        "parking must file an attention item",
    );
    // Critical: PROBE_NO_PR must never be queued.
    assert!(
        queued.iter().all(|(_, t)| t != PROBE_NO_PR),
        "revision must never receive PROBE_NO_PR",
    );
}

#[tokio::test]
async fn nudge_breaker_resets_after_worker_finally_opens_pr() {
    // A worker that gets nudged a couple of times and THEN opens a
    // real PR must finalize cleanly — the accumulated nudge count is
    // reset on finalize, so it doesn't carry over to poison a later
    // cycle.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // First two stops find no PR; the third finds a fresh PR.
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());

    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingInput
    ));
    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingInput
    ));
    // The worker finally opens a real PR before the breaker trips.
    detector
        .set_result(PrStatus::Fresh {
            url: "https://github.com/foo/bar/pull/77".to_owned(),
        })
        .await;
    let final_outcome = handler.on_stop(&execution_id).await;
    // chore_implementation holds task and enqueues reviewer.
    assert!(
        matches!(final_outcome, StopOutcome::ReviewerEnqueued { .. }),
        "the worker's real PR must finalize; got {final_outcome:?}",
    );
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected chore, got {other:?}"),
    }
}
