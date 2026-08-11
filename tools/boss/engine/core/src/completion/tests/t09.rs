//! Split out of `completion.rs`'s `#[cfg(test)] mod tests`.
//! Test functions only; shared fixtures, stubs, and helpers live
//! in the parent [`super`] module (`completion/tests.rs`).

use super::*;

/// Pins the wiring `dispatch_worker_event_fanout` depends on
/// (`app::worker_events::pre_pass_delivered_a_probe`'s pure predicate is
/// pinned separately) once it reaches [`WorkerCompletionHandler::on_stop_with_turn_end_deferrable`]:
/// when the pre-completion probe pass delivered a probe for this boundary,
/// the generic PR-detection/nudge/park decision must be withheld —
/// `DeferredForProbeTurn`, no `PROBE_NO_PR` nudge queued — but `stop_seen`
/// must still be stamped, because the merge poller's SHA-delta gate keys on
/// it regardless of whether this boundary's terminal decision ran.
///
/// Counterfactual: [`pre_pass_not_delivering_lets_the_stop_decision_run_and_still_stamps_stop_seen`].
#[tokio::test]
async fn pre_pass_delivering_a_probe_defers_the_stop_decision_but_still_stamps_stop_seen() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);

    assert!(
        !db.execution_stop_seen(&execution_id).unwrap(),
        "precondition: stop_seen must be unset before this execution's first Stop",
    );

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let outcome = handler
        .on_stop_with_turn_end_deferrable(&execution_id, None, /* defer_finalization */ true)
        .await;

    assert_eq!(
        outcome,
        StopOutcome::DeferredForProbeTurn,
        "a boundary whose pre-pass delivered a probe must withhold the terminal decision",
    );
    assert!(
        db.execution_stop_seen(&execution_id).unwrap(),
        "stop_seen must be stamped even when the terminal decision is deferred — the merge \
         poller's SHA-delta gate must see this boundary regardless",
    );
    assert!(
        probes.snapshot().is_empty(),
        "no PROBE_NO_PR nudge must be queued while the terminal decision is deferred, got {:?}",
        probes.snapshot(),
    );
}

/// Counterfactual to
/// [`pre_pass_delivering_a_probe_defers_the_stop_decision_but_still_stamps_stop_seen`]:
/// when the pre-pass did not deliver anything (`defer_finalization = false`,
/// mirroring `pre_pass_delivered_a_probe` returning `false`), the generic
/// PR-detection/nudge/park decision must run exactly as it always has —
/// `PROBE_NO_PR` queued, `AwaitingInput` — and `stop_seen` is stamped either
/// way.
#[tokio::test]
async fn pre_pass_not_delivering_lets_the_stop_decision_run_and_still_stamps_stop_seen() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let outcome = handler
        .on_stop_with_turn_end_deferrable(&execution_id, None, /* defer_finalization */ false)
        .await;

    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "a boundary whose pre-pass did not deliver must run the terminal decision as before",
    );
    assert!(
        db.execution_stop_seen(&execution_id).unwrap(),
        "stop_seen must be stamped on the normal (non-deferred) path too",
    );
    let queued = probes.snapshot();
    assert_eq!(
        queued.len(),
        1,
        "the produce-a-PR nudge must fire when the terminal decision is not deferred, got {queued:?}",
    );
    assert_eq!(queued[0].0, execution_id);
    assert_eq!(queued[0].1, PROBE_NO_PR);
}

#[tokio::test]
async fn background_children_recheck_does_not_probe_after_worker_resumes() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db, detector);
    let probe = WatermarkedDescendantProbe::new("before-stop");
    let handler = handler.with_background_activity_probe(probe.clone());

    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::BackgroundChildrenPending { .. }
    ));
    probe.set_watermark("new-worker-hook");

    assert_eq!(handler.recheck_background_nudge(&execution_id).await, None);
    assert!(
        probes.snapshot().is_empty(),
        "a resumed worker must not receive a recurring probe"
    );
    assert!(handler.pending_background_nudge_execution_ids().is_empty());
}

/// `activity_watermark` on demand: `None` simulates "no hook-only
/// evidence available right now" (e.g. a momentarily terminal or
/// re-registered live-state entry) independent of whatever
/// `live_delegated_descendant_count` reports.
struct ToggleWatermarkProbe {
    descendant_count: usize,
    watermark: std::sync::Mutex<Option<String>>,
}

impl ToggleWatermarkProbe {
    fn new(descendant_count: usize, watermark: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            descendant_count,
            watermark: std::sync::Mutex::new(watermark.map(str::to_owned)),
        })
    }

    fn set_watermark(&self, watermark: Option<&str>) {
        *self.watermark.lock().expect("watermark mutex poisoned") = watermark.map(str::to_owned);
    }
}

impl crate::background_children::BackgroundActivityProbe for ToggleWatermarkProbe {
    fn live_delegated_descendant_count(&self, _execution_id: &str) -> std::result::Result<usize, String> {
        Ok(self.descendant_count)
    }

    fn activity_watermark(&self, _execution_id: &str) -> Option<String> {
        self.watermark.lock().expect("watermark mutex poisoned").clone()
    }
}

#[tokio::test]
async fn recheck_does_not_retire_intent_when_current_watermark_is_unavailable() {
    // Regression: `LiveWorkerStateRegistry::activity_watermark_for_run`
    // can legitimately return `None` (the live-state entry momentarily
    // turned terminal, or was re-registered) even though the worker never
    // actually resumed. `recheck_background_nudge` must treat an
    // unavailable *current* watermark as "no evidence either way" and
    // keep the intent — retiring it here is the fail-closed bug this
    // pins down (a worker that never resumes would otherwise be
    // permanently dropped from the recheck-eligible set on engine-side
    // bookkeeping alone).
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db, detector);
    let probe = ToggleWatermarkProbe::new(1, Some("wm-1"));
    let handler = handler.with_background_activity_probe(probe.clone());

    let first = handler.on_stop(&execution_id).await;
    assert!(
        matches!(first, StopOutcome::BackgroundChildrenPending { .. }),
        "got {first:?}",
    );
    assert!(handler.pending_background_nudge_execution_ids().contains(&execution_id));

    // The watermark becomes unavailable while descendants are still
    // (independently) reported live.
    probe.set_watermark(None);

    let outcome = handler.recheck_background_nudge(&execution_id).await;
    assert!(
        matches!(outcome, Some(StopOutcome::BackgroundChildrenPending { .. })),
        "recheck must not retire on an unavailable current watermark; got {outcome:?}",
    );
    assert!(
        handler.pending_background_nudge_execution_ids().contains(&execution_id),
        "intent must survive an unavailable current watermark",
    );
}

#[tokio::test]
async fn debounced_nudge_with_indeterminate_probe_stays_pending_for_recheck() {
    // Regression for "NudgeDebounced retention is a no-op": every arm of
    // `nudge_or_park`'s descendant-probe match used to clear the tracked
    // intent unconditionally (`Ok(0)`/`Err` called `forget`, `Expired`
    // called `forget_intent`) before the breaker verdict was known, so a
    // `NudgeDecision::TooSoon` (-> `StopOutcome::NudgeDebounced`) could
    // never be retained for the recurring recheck sweep — the execution
    // was already gone from `pending_background_nudge_execution_ids()` by
    // the time the breaker even ran. Pin the clock so the second Stop
    // lands inside the debounce window and assert the execution is still
    // listed for recheck afterward.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db, detector);
    let handler = handler.with_background_activity_probe(Arc::new(FailingDescendantProbe));
    let fixed_now = std::time::Instant::now();
    let handler = handler.with_now_fn(std::sync::Arc::new(move || fixed_now));

    let first = handler.on_stop(&execution_id).await;
    let second = handler.on_stop(&execution_id).await;

    assert!(matches!(first, StopOutcome::AwaitingInput), "got {first:?}");
    assert!(matches!(second, StopOutcome::NudgeDebounced), "got {second:?}");
    assert!(
        handler.pending_background_nudge_execution_ids().contains(&execution_id),
        "a debounced Stop must not drop the execution from the recheck-eligible set",
    );
}

#[tokio::test]
async fn ci_remediation_debounced_nudge_does_not_double_fail_or_republish() {
    // `nudge_or_park`'s `NudgeDecision::TooSoon` arm returns
    // `StopOutcome::NudgeDebounced` for every caller, not just the
    // background-children recheck path — previously it returned the
    // caller's `proceed_outcome` verbatim (e.g. `AwaitingInput`), which
    // the catch-all finalizer below treats as "mark the attempt failed".
    //
    // For `ci_remediation` specifically this is provably a no-op change:
    // `NudgeDecision::TooSoon` can only fire for a fingerprint that was
    // already nudged (`NudgeDecision::Proceed`) inside the debounce
    // window — there is no history to debounce against otherwise — and
    // that earlier Proceed already ran this same finalizer with the same
    // `proceed_outcome`. `WorkDb::mark_ci_remediation_failed` WHERE-guards
    // on `status IN ('pending', 'running')`, so by the time the debounced
    // repeat arrives the attempt is already terminal either way and a
    // second failed-mark attempt would have been an idempotent no-op even
    // under the old behaviour. This test pins that down: a debounced Stop
    // must not publish a second `CiRemediationFailed` event or otherwise
    // observably differ from the old (harmless) double-mark.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id, attempt_id) = ci_remediation_fixture(workspace.path());
    let detector = StubPrDetector::ok(None);

    let TestHarness {
        handler,
        probes,
        publisher,
        ..
    } = TestHarness::new(db.clone(), detector);
    // Pin the clock so the second `on_stop` lands inside
    // `nudge_breaker::MIN_RENUDGE_INTERVAL` of the first — the debounce
    // window this test exists to exercise. `TestHarness::new`'s default
    // auto-advancing clock exists precisely to avoid this, so override it.
    let fixed_now = std::time::Instant::now();
    let handler = handler.with_now_fn(std::sync::Arc::new(move || fixed_now));

    let first = handler.on_stop(&execution_id).await;
    let second = handler.on_stop(&execution_id).await;

    assert!(
        matches!(first, StopOutcome::AwaitingInput),
        "first Stop nudges normally; got {first:?}",
    );
    assert!(
        matches!(second, StopOutcome::NudgeDebounced),
        "second Stop inside the debounce window must be suppressed as NudgeDebounced; got {second:?}",
    );
    assert_eq!(
        probes.snapshot().len(),
        1,
        "only the first Stop should have queued a probe",
    );

    // The `ci_remediation` finalizer marks failed on the very first idle
    // Stop by design (see `finalize_ci_remediation_attempt`'s doc) — that
    // already happened on `first`, before debounce ever entered the
    // picture.
    let attempt = db.get_ci_remediation(&attempt_id).unwrap().unwrap();
    assert_eq!(attempt.status, "failed");
    assert_eq!(attempt.failure_reason.as_deref(), Some(CI_NO_PUSH_REASON));

    // Exactly one `CiRemediationFailed` publish — the debounced repeat
    // must not fire a second one now that it short-circuits to `false`
    // before ever calling `mark_ci_remediation_failed`.
    let typed = publisher.typed_events.lock().await.clone();
    let fail_events = typed
        .iter()
        .filter(|(_, ev)| matches!(ev, boss_protocol::FrontendEvent::CiRemediationFailed { attempt_id: a, .. } if a == &attempt_id))
        .count();
    assert_eq!(
        fail_events, 1,
        "a debounced Stop must not republish CiRemediationFailed; got {typed:?}",
    );

    // And the execution stays observable for a later recheck: the intent
    // is retained rather than dropped on the debounced Stop (see the
    // `nudge_or_park`/`recheck_background_nudge` retention fix) — even
    // though this particular execution has nothing left to recheck for,
    // the intent bookkeeping must behave uniformly across every
    // `nudge_or_park` caller.
    assert!(
        handler.pending_background_nudge_execution_ids().contains(&execution_id),
        "a debounced nudge must keep the execution tracked for the recurring recheck sweep",
    );
}

/// incident-004 AI-4: when `finalize_pr_transition` tears down a worker
/// whose live activity is still `Working` (mid-turn) must bump the aggregate
/// and per-source mid-turn-reap counters and file an operator-visible
/// attention item on the execution, so the prevalence the postmortem measured
/// only by an offline trace sweep is answerable from the running system.
#[tokio::test]
async fn finalize_pr_transition_records_mid_turn_reap_when_worker_is_working() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1344";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);

    let live_states = Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
    live_states.register_spawn(
        3,
        &execution_id,
        "claude-opus-5",
        std::process::id() as i32,
        Some(boss_protocol::WorkItemBinding {
            work_item_id: revision_id.clone(),
            work_item_name: "revision mid-turn metric".to_owned(),
            execution_id: execution_id.clone(),
        }),
    );
    live_states.apply_event(
        3,
        &boss_protocol::WorkerEvent::PreToolUse {
            session_id: "s".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
        },
    );
    assert_eq!(
        live_states.activity_for_run(&execution_id),
        Some(boss_protocol::WorkerActivity::Working),
        "fixture must model the mid-turn state observed at teardown",
    );

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_live_worker_states(live_states);

    assert_eq!(
        handler.metrics.counter_value("completion.mid_turn_reap.total"),
        Some(0),
        "must start at zero before any finalize",
    );

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
        "the finalization path must complete; got {outcome:?}",
    );

    assert_eq!(
        handler.metrics.counter_value("completion.mid_turn_reap.total"),
        Some(1),
        "a mid-turn finalize must bump the aggregate counter exactly once",
    );
    assert_eq!(
        handler
            .metrics
            .counter_value("completion.mid_turn_reap.pr_recheck_staged.count"),
        Some(1),
        "the per-source counter must be keyed on the finalize source that reaped this worker",
    );

    let items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        items.iter().any(|i| i.kind == MID_TURN_REAP_ATTENTION_KIND),
        "a mid-turn reap must file its own attention item; got {items:?}",
    );
}

/// Negative control for the test above: the worker's own `Stop` already landed
/// before finalization runs, so no mid-turn signal must be raised.
#[tokio::test]
async fn finalize_pr_transition_does_not_record_mid_turn_reap_when_worker_is_idle() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1345";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);

    let live_states = Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
    live_states.register_spawn(
        4,
        &execution_id,
        "claude-opus-5",
        std::process::id() as i32,
        Some(boss_protocol::WorkItemBinding {
            work_item_id: revision_id.clone(),
            work_item_name: "revision idle at finalize".to_owned(),
            execution_id: execution_id.clone(),
        }),
    );
    live_states.apply_event(
        4,
        &boss_protocol::WorkerEvent::PreToolUse {
            session_id: "s".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
        },
    );
    live_states.apply_event(
        4,
        &boss_protocol::WorkerEvent::Stop {
            session_id: "s".into(),
            stop_hook_active: false,
            stop_reason: boss_protocol::StopReason::Completed,
        },
    );
    assert_eq!(
        live_states.activity_for_run(&execution_id),
        Some(boss_protocol::WorkerActivity::Idle),
        "fixture must model the worker having already reached its own Stop boundary",
    );

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let handler = handler.with_live_worker_states(live_states);

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
        "finalization must still succeed for an idle worker; got {outcome:?}",
    );

    assert_eq!(
        handler.metrics.counter_value("completion.mid_turn_reap.total"),
        Some(0),
        "an idle finalize must not be counted as a mid-turn reap",
    );
    let items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        !items.iter().any(|i| i.kind == MID_TURN_REAP_ATTENTION_KIND),
        "an idle finalize must not file a mid-turn-reap attention item; got {items:?}",
    );
}

/// Mid-turn reaps from every execution kind are counted, but only revision
/// implementations create sticky attention: that class can lose a
/// reviewer-requested correction, whereas routine completion volume must not
/// accumulate an operator queue.
#[tokio::test]
async fn finalize_pr_transition_counts_non_revision_mid_turn_reap_without_attention() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let live_states = Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
    live_states.register_spawn(
        5,
        &execution_id,
        "claude-opus-5",
        std::process::id() as i32,
        Some(boss_protocol::WorkItemBinding {
            work_item_id: chore_id,
            work_item_name: "ordinary completion metric".to_owned(),
            execution_id: execution_id.clone(),
        }),
    );
    live_states.apply_event(
        5,
        &boss_protocol::WorkerEvent::PreToolUse {
            session_id: "s".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
        },
    );

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler.with_live_worker_states(live_states);
    handler
        .finalize_pr_transition(
            &execution_id,
            "https://github.com/spinyfin/mono/pull/1346".to_owned(),
            WorkerPrCompletionTarget::Done,
            "test_non_revision",
        )
        .await;

    assert_eq!(
        handler.metrics.counter_value("completion.mid_turn_reap.total"),
        Some(1),
        "non-revision reaps must remain visible in the aggregate metric",
    );
    assert_eq!(
        handler
            .metrics
            .counter_value("completion.mid_turn_reap.test_non_revision.count"),
        Some(1),
        "non-revision reaps must remain visible in the per-source metric",
    );
    assert!(
        !db.list_attention_items(&execution_id)
            .unwrap()
            .iter()
            .any(|i| i.kind == MID_TURN_REAP_ATTENTION_KIND),
        "routine completion reaps must not file sticky attention items",
    );
}
