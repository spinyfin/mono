use super::*;

/// [`poll_tier_for_probe`] classification (doc §9 item 3): CI in
/// flight, a merge-queued PR, and an unresolved/conflicting
/// mergeability all count as "actively changing" (`Hot`); a clean,
/// mergeable, non-queued PR is steady-state (`Cold`); a terminal
/// (merged/closed) PR yields `None` so it is dropped from the adaptive
/// schedule instead of re-probed.
#[test]
fn poll_tier_classifies_open_pr_signals() {
    let base = |state: PrLifecycleState, in_merge_queue: bool| PrLifecycleProbe {
        url: "https://github.com/foo/bar/pull/1".to_owned(),
        state,
        base_ref_oid: None,
        head_ref_oid: None,
        head_ref_name: None,
        base_ref_name: None,
        labels: Vec::new(),
        review: PrReviewState::Unknown,
        in_merge_queue,
        merge_queue_entry_state: None,
        merge_queue_position: None,
        merge_queue_enqueued_at: None,
        raw_mergeable: String::new(),
        raw_merge_state_status: String::new(),
        auto_merge_enabled: false,
        auto_merge_enabled_at: None,
    };

    assert_eq!(
        poll_tier_for_probe(&base(PrLifecycleState::Open(OpenPrStatus::clean()), false)),
        Some(PollTier::Cold)
    );
    assert_eq!(
        poll_tier_for_probe(&base(PrLifecycleState::Open(OpenPrStatus::clean()), true)),
        Some(PollTier::Hot),
        "merge-queued PRs must poll fast",
    );
    assert_eq!(
        poll_tier_for_probe(&base(
            PrLifecycleState::Open(OpenPrStatus {
                mergeability: OpenPrMergeability::Clean,
                ci: OpenPrCiStatus::InFlight,
            }),
            false
        )),
        Some(PollTier::Hot),
        "in-flight CI must poll fast",
    );
    assert_eq!(
        poll_tier_for_probe(&base(PrLifecycleState::Open(OpenPrStatus::conflict_only()), false)),
        Some(PollTier::Hot),
        "conflicting mergeability must poll fast",
    );
    assert_eq!(
        poll_tier_for_probe(&base(
            PrLifecycleState::Open(OpenPrStatus::unknown_mergeability()),
            false
        )),
        Some(PollTier::Hot),
        "unresolved mergeability must poll fast",
    );
    assert_eq!(
        poll_tier_for_probe(&base(PrLifecycleState::Merged, false)),
        None,
        "merged PRs are terminal — drop them from the adaptive schedule",
    );
    assert_eq!(
        poll_tier_for_probe(&base(PrLifecycleState::ClosedUnmerged, false)),
        None,
        "closed PRs are terminal — drop them from the adaptive schedule",
    );
}

/// [`PrPollSchedule`]: `seed_defaults` must not clobber an entry
/// already scheduled by a real probe outcome, `drain_due` must
/// return exactly (and only) the entries due by the given instant,
/// and rescheduling with `None` must stop tracking a PR entirely.
#[test]
fn pr_poll_schedule_seed_drain_and_reschedule() {
    let mut schedule = PrPollSchedule::default();
    let now = Instant::now();

    // A real probe already scheduled pr1 as Cold (long interval).
    schedule.reschedule("pr1", Some(PollTier::Cold), now);
    // Seeding defaults must not override that — pr1 stays Cold-scheduled,
    // far in the future — while pr2 (never seen) gets a fresh Hot slot.
    schedule.seed_defaults(["pr1".to_owned(), "pr2".to_owned()], now);

    // Only pr2's Hot (40 s) slot should be due at now + 45s; pr1's Cold
    // (180 s) slot is not.
    let due = schedule.drain_due(now + Duration::from_secs(45));
    assert_eq!(due, vec!["pr2".to_owned()]);

    // pr1 is still tracked (its Cold slot hasn't arrived yet).
    assert!(schedule.next_due().is_some());
    let due_later = schedule.drain_due(now + Duration::from_secs(200));
    assert_eq!(due_later, vec!["pr1".to_owned()]);
    assert!(schedule.next_due().is_none());

    // Rescheduling with `None` stops tracking immediately.
    schedule.reschedule("pr3", Some(PollTier::Hot), now);
    assert!(schedule.next_due().is_some());
    schedule.reschedule("pr3", None, now);
    assert!(schedule.next_due().is_none());
}

#[tokio::test]
async fn activation_kick_quiesce_absorbs_rapid_repeats() {
    use tokio::time::timeout;

    let kick = Arc::new(Notify::new());
    let quiesce_window = Duration::from_millis(200); // short for tests
    let interval = Duration::from_secs(3600); // never fires

    // Simulate: last run just finished.
    let last_run_at = Instant::now();

    // Fire a kick immediately (well within the quiesce window).
    kick.notify_one();

    // The 'wait loop should absorb the kick and NOT break out within
    // a short window. We run one iteration of the select: if kick
    // fires and elapsed < quiesce_window, the loop should continue
    // (not break). We test this by trying to break out within 50 ms
    // using only the kick arm; the timer is infinite so only the kick
    // arm can fire.
    let broke_out = timeout(Duration::from_millis(50), async {
        loop {
            let elapsed = last_run_at.elapsed();
            let remaining = interval.saturating_sub(elapsed);
            tokio::select! {
                _ = tokio::time::sleep(remaining) => { return true; }
                _ = kick.notified() => {
                    let since_last = last_run_at.elapsed();
                    if since_last >= quiesce_window {
                        return true;
                    }
                    // absorbed — continue waiting
                }
            }
        }
    })
    .await;

    // The timeout must fire (broke_out = Err) because the kick was
    // absorbed and the periodic timer (3600 s) never elapsed.
    assert!(
        broke_out.is_err(),
        "kick within quiesce window must be absorbed, not break out of wait",
    );
}

/// Acceptance test: a kick that arrives after the quiesce window
/// has elapsed triggers an immediate pass (breaks out of the wait).
#[tokio::test]
async fn activation_kick_after_quiesce_window_triggers_pass() {
    use tokio::time::timeout;

    let kick = Arc::new(Notify::new());
    let quiesce_window = Duration::from_millis(1); // essentially instant
    let interval = Duration::from_secs(3600);

    // Simulate: last run finished a long time ago (100 ms > 1 ms quiesce).
    let last_run_at = Instant::now() - Duration::from_millis(100);

    // Fire a kick.
    kick.notify_one();

    // The 'wait loop should break out immediately because elapsed > quiesce.
    let broke_out = timeout(Duration::from_millis(500), async {
        loop {
            let elapsed = last_run_at.elapsed();
            let remaining = interval.saturating_sub(elapsed);
            tokio::select! {
                _ = tokio::time::sleep(remaining) => { return true; }
                _ = kick.notified() => {
                    let since_last = last_run_at.elapsed();
                    if since_last >= quiesce_window {
                        return true; // break out — trigger pass
                    }
                }
            }
        }
    })
    .await;

    assert!(
        broke_out.is_ok(),
        "kick after quiesce window must break out of wait loop",
    );
}

/// [`PrReconcilerTargetedKick::kick`] records the PR that requested the
/// pass and wakes anyone awaiting [`PrReconcilerTargetedKick::notified`];
/// [`PrReconcilerTargetedKick::drain_pending`] returns exactly what was
/// recorded and clears it so a second drain sees nothing new.
#[tokio::test]
async fn targeted_kick_records_and_drains_pr_urls() {
    use tokio::time::timeout;

    let targeted_kick = PrReconcilerTargetedKick::new();
    targeted_kick.kick("https://github.com/spinyfin/mono/pull/1");
    targeted_kick.kick("https://github.com/spinyfin/mono/pull/2");

    timeout(Duration::from_millis(500), targeted_kick.notified())
        .await
        .expect("kick() must notify a waiter");

    assert_eq!(
        targeted_kick.drain_pending(),
        vec![
            "https://github.com/spinyfin/mono/pull/1".to_owned(),
            "https://github.com/spinyfin/mono/pull/2".to_owned(),
        ],
    );
    assert!(
        targeted_kick.drain_pending().is_empty(),
        "drain_pending must clear the queue",
    );
}

/// `is_failure_conclusion` / `is_pass_conclusion` partition GitHub's
/// conclusion tokens into the gating buckets. Each is a closed set:
/// the failure set is FAILURE/ERROR/TIMED_OUT/CANCELLED/STARTUP_FAILURE/
/// ACTION_REQUIRED/STALE; the pass set is SUCCESS/NEUTRAL/SKIPPED.
/// Matching is case-insensitive, and an unknown token is in neither set
/// (so the caller keeps waiting rather than mis-routing remediation).
#[test]
fn conclusion_predicates_partition_closed_sets() {
    let failures = [
        "FAILURE",
        "ERROR",
        "TIMED_OUT",
        "CANCELLED",
        "STARTUP_FAILURE",
        "ACTION_REQUIRED",
        "STALE",
    ];
    for c in failures {
        assert!(super::is_failure_conclusion(c), "{c} should be a failure");
        assert!(!super::is_pass_conclusion(c), "{c} must not also be a pass",);
        // Case-insensitive: the lowercase form classifies identically.
        let lower = c.to_ascii_lowercase();
        assert!(
            super::is_failure_conclusion(&lower),
            "{lower} (lowercase) should be a failure",
        );
    }

    let passes = ["SUCCESS", "NEUTRAL", "SKIPPED"];
    for c in passes {
        assert!(super::is_pass_conclusion(c), "{c} should be a pass");
        assert!(!super::is_failure_conclusion(c), "{c} must not also be a failure",);
        let lower = c.to_ascii_lowercase();
        assert!(
            super::is_pass_conclusion(&lower),
            "{lower} (lowercase) should be a pass",
        );
    }

    // An unknown conclusion is in neither set.
    for unknown in ["", "WAT", "in_progress", "queued"] {
        assert!(
            !super::is_failure_conclusion(unknown),
            "{unknown:?} must not be a failure",
        );
        assert!(!super::is_pass_conclusion(unknown), "{unknown:?} must not be a pass",);
    }
}
