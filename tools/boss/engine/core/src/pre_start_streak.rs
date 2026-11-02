//! Health-report presentation for the shared pre-start failure tracker.

use boss_protocol::{EngineHealthIssue, SpawnFailureStreak};

pub use boss_execution_contracts::{
    FailureEffect, PRE_START_FAILURE_STREAK_THRESHOLD, PreStartStreakTracker, StreakKey, UNRESOLVED_DRIVER_LABEL,
};

/// `EngineHealthIssue::kind` of the alert in the engine health report.
pub const PRE_START_FAILURE_STREAK_ISSUE_KIND: &str = "pre_start_spawn_failure_streak";

/// Render one active alert as the engine-health issue the app banner and
/// `bossctl` show. The engine owns the wording; the app renders it verbatim.
///
/// The title carries the combination, the live count and how long the
/// streak has run; the body carries the latest error in full. Times are
/// relative phrases ("3 hours ago"), matching the pause issues — never a
/// raw Zulu timestamp in banner text.
pub fn health_issue_for(alert: &SpawnFailureStreak, now_epoch_secs: i64) -> EngineHealthIssue {
    let phrase = |epoch_s: i64| {
        let phrase = boss_engine_utils::iso8601::format_paused_since_phrase(epoch_s, now_epoch_secs);
        if phrase.is_empty() {
            "just now".to_owned()
        } else {
            phrase
        }
    };
    EngineHealthIssue {
        kind: PRE_START_FAILURE_STREAK_ISSUE_KIND.to_owned(),
        severity: "error".to_owned(),
        title: format!(
            "{driver} {kind} workers are failing to start: {count} consecutive failures, none succeeded \
             (first {first})",
            driver = alert.driver,
            kind = alert.worker_kind,
            count = alert.consecutive_failures,
            first = phrase(alert.first_failure_epoch_s),
        ),
        body: format!(
            "Latest error ({latest}, execution {execution}):\n{error}\n\n\
             Every {driver} {kind} spawn since the first failure {first} has failed during spawn or \
             spawn confirmation (no worker started its first turn). This alert updates on each further failure and clears \
             itself on the next successful {driver} {kind} spawn. Dispatch is not paused by it. Each \
             failed execution also has its own `spawn_failed` entry in its dispatch event log.",
            latest = phrase(alert.latest_failure_epoch_s),
            execution = alert.latest_execution_id,
            error = alert.latest_error,
            driver = alert.driver,
            kind = alert.worker_kind,
            first = phrase(alert.first_failure_epoch_s),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::ExecutionKind;

    fn codex_guide() -> StreakKey {
        StreakKey::for_execution(Some("codex"), &ExecutionKind::PrReviewGuide)
    }

    #[test]
    fn one_failure_raises_nothing() {
        let tracker = PreStartStreakTracker::new();
        assert_eq!(
            tracker.record_failure(codex_guide(), "exec_1", "refused", 100),
            FailureEffect::BelowThreshold
        );
        assert!(tracker.active_alerts().is_empty());
    }

    #[test]
    fn threshold_raises_one_alert_and_later_failures_update_it_in_place() {
        let tracker = PreStartStreakTracker::new();
        tracker.record_failure(codex_guide(), "exec_1", "first refusal", 100);
        assert_eq!(
            tracker.record_failure(codex_guide(), "exec_2", "second refusal", 640),
            FailureEffect::Raised
        );
        let alerts = tracker.active_alerts();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].driver, "codex");
        assert_eq!(alerts[0].worker_kind, "review-guide");
        assert_eq!(alerts[0].consecutive_failures, 2);
        assert_eq!(alerts[0].first_failure_epoch_s, 100);
        assert_eq!(alerts[0].latest_failure_epoch_s, 640);
        assert_eq!(alerts[0].latest_error, "second refusal");
        assert_eq!(alerts[0].latest_execution_id, "exec_2");

        // The incident's shape: the failures keep coming. Still exactly one
        // alert, carrying the live count and the newest cause — never a
        // second alert, never a frozen count.
        for n in 3..=28 {
            assert_eq!(
                tracker.record_failure(codex_guide(), &format!("exec_{n}"), &format!("refusal {n}"), 100 + n),
                FailureEffect::Updated
            );
        }
        let alerts = tracker.active_alerts();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].consecutive_failures, 28);
        assert_eq!(alerts[0].first_failure_epoch_s, 100, "first-failure time must not move");
        assert_eq!(alerts[0].latest_error, "refusal 28");
        assert_eq!(alerts[0].latest_execution_id, "exec_28");
    }

    #[test]
    fn success_resolves_the_alert_and_restarts_the_count() {
        let tracker = PreStartStreakTracker::new();
        tracker.record_failure(codex_guide(), "exec_1", "refused", 100);
        tracker.record_failure(codex_guide(), "exec_2", "refused", 200);
        let resolved = tracker
            .record_success(&codex_guide())
            .expect("an active alert resolves");
        assert_eq!(resolved.consecutive_failures, 2);
        assert!(tracker.active_alerts().is_empty());

        // A success in between means the next failure starts a new streak.
        assert_eq!(
            tracker.record_failure(codex_guide(), "exec_3", "refused", 300),
            FailureEffect::BelowThreshold
        );
        assert!(tracker.active_alerts().is_empty());
    }

    #[test]
    fn success_below_threshold_resets_without_reporting_a_resolution() {
        let tracker = PreStartStreakTracker::new();
        tracker.record_failure(codex_guide(), "exec_1", "refused", 100);
        assert!(tracker.record_success(&codex_guide()).is_none());
        assert_eq!(
            tracker.record_failure(codex_guide(), "exec_2", "refused", 200),
            FailureEffect::BelowThreshold,
            "failure, success, failure is not two consecutive failures"
        );
    }

    #[test]
    fn combinations_do_not_merge() {
        let tracker = PreStartStreakTracker::new();
        let claude_guide = StreakKey::for_execution(Some("claude"), &ExecutionKind::PrReviewGuide);
        let codex_standard = StreakKey::for_execution(Some("codex"), &ExecutionKind::TaskImplementation);

        // One failure each for three combinations: three failures in total,
        // but no combination has two, so nothing is raised.
        tracker.record_failure(codex_guide(), "exec_1", "refused", 100);
        tracker.record_failure(claude_guide.clone(), "exec_2", "refused", 110);
        tracker.record_failure(codex_standard.clone(), "exec_3", "refused", 120);
        assert!(tracker.active_alerts().is_empty());

        tracker.record_failure(codex_guide(), "exec_4", "refused again", 130);
        let alerts = tracker.active_alerts();
        assert_eq!(alerts.len(), 1);
        assert_eq!(
            (alerts[0].driver.as_str(), alerts[0].worker_kind.as_str()),
            ("codex", "review-guide")
        );
        assert_eq!(alerts[0].consecutive_failures, 2);

        // A success for a different combination must not resolve it.
        assert!(tracker.record_success(&codex_standard).is_none());
        assert!(tracker.record_success(&claude_guide).is_none());
        assert_eq!(tracker.active_alerts().len(), 1);
    }

    #[test]
    fn unresolved_driver_is_still_counted() {
        let key = StreakKey::for_execution(None, &ExecutionKind::PrReview);
        assert_eq!(key.driver, UNRESOLVED_DRIVER_LABEL);
        assert_eq!(key.worker_kind.label(), "reviewer");
        assert_eq!(StreakKey::for_execution(Some("  "), &ExecutionKind::PrReview), key);
    }

    #[test]
    fn subscribers_wake_on_raise_update_and_resolve_only() {
        let tracker = PreStartStreakTracker::new();
        let mut changes = tracker.subscribe();

        tracker.record_failure(codex_guide(), "exec_1", "refused", 100);
        assert!(
            !changes.has_changed().unwrap(),
            "a sub-threshold failure changes no alert"
        );

        tracker.record_failure(codex_guide(), "exec_2", "refused", 200);
        assert!(changes.has_changed().unwrap(), "raise");
        changes.mark_unchanged();

        tracker.record_failure(codex_guide(), "exec_3", "refused", 300);
        assert!(changes.has_changed().unwrap(), "in-place update");
        changes.mark_unchanged();

        tracker.record_success(&codex_guide());
        assert!(changes.has_changed().unwrap(), "resolve");
        changes.mark_unchanged();

        tracker.record_success(&codex_guide());
        assert!(
            !changes.has_changed().unwrap(),
            "a success with no active alert changes nothing"
        );
    }

    #[test]
    fn health_issue_carries_count_combination_and_full_latest_error() {
        let error = "hook-trust gate refused the spawn\nno hook entries; silence is not success";
        let alert = SpawnFailureStreak::builder()
            .driver("codex")
            .worker_kind("review-guide")
            .consecutive_failures(28)
            .first_failure_epoch_s(1_000)
            .latest_failure_epoch_s(1_000 + 31 * 3600)
            .latest_error(error)
            .latest_execution_id("exec_28")
            .build();
        let issue = health_issue_for(&alert, 1_000 + 31 * 3600 + 120);
        assert_eq!(issue.kind, PRE_START_FAILURE_STREAK_ISSUE_KIND);
        assert_eq!(issue.severity, "error");
        assert!(issue.title.contains("codex review-guide"), "{}", issue.title);
        assert!(issue.title.contains("28 consecutive failures"), "{}", issue.title);
        assert!(issue.title.contains("ago"), "{}", issue.title);
        assert!(
            issue.body.contains(error),
            "body must carry the full error: {}",
            issue.body
        );
        assert!(issue.body.contains("exec_28"), "{}", issue.body);
        assert!(
            !issue.title.contains('Z') && !issue.title.contains("1970"),
            "{}",
            issue.title
        );
    }
}
