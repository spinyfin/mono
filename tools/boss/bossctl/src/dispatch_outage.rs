//! `bossctl` reporting of a global dispatch outage (every host excluded for
//! every item), shared by `dispatch state` (live engine record) and `dispatch
//! diagnose` (rebuilt from the dispatch event stream, so it still answers when
//! the engine is wedged). Both print the same fields.

use boss_engine::coordinator::outage_elapsed_phrase;
use boss_engine::dispatch_events::DispatchEvent;
use boss_execution_contracts::{OutageReplay, replay_outage_events};
use boss_protocol::DispatchOutage;
use serde_json::Value;

/// What the dispatch event stream says about outages: the one still in
/// progress and the most recent recovery. The rules are the engine's own
/// (`replay_outage_events`), so this and `dispatch state` agree.
pub(super) fn from_events(events: &[DispatchEvent]) -> OutageReplay {
    replay_outage_events(events)
}

/// The text block both verbs print for an active outage.
pub(super) fn lines(outage: &DispatchOutage, now_epoch_s: i64) -> Vec<String> {
    let mut lines = vec![
        "DISPATCH OUTAGE: no host can accept a new worker".to_owned(),
        format!(
            "  started: epoch {} ({})",
            outage.started_epoch_s,
            outage_elapsed_phrase(outage.started_epoch_s, now_epoch_s)
        ),
        format!(
            "  refused: {} attempts across {} work items (latest: {} / {}, epoch {})",
            outage.refused_attempts,
            outage.refused_work_items,
            display_or_dash(&outage.latest_work_item_id),
            display_or_dash(&outage.latest_execution_id),
            outage.latest_refusal_epoch_s,
        ),
        "  hosts:".to_owned(),
    ];
    for exclusion in &outage.host_exclusions {
        lines.push(format!(
            "    {} [{}]: {}",
            exclusion.host_id, exclusion.kind, exclusion.cause
        ));
        lines.push(format!("      engine refusal: {}", exclusion.reason));
        if let Some(action) = &exclusion.recovery_action {
            lines.push(format!("      recovery: {action}"));
        }
    }
    lines.push("  ends when a new dispatch claims a host (not when a flag clears)".to_owned());
    lines
}

fn display_or_dash(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

/// One line for the most recent recovery recorded in the stream.
pub(super) fn recovery_line(details: &Value) -> String {
    format!(
        "last dispatch outage: lasted {}s (epoch {} to {}); recovered by {} on host {}; {} attempts refused",
        details.get("duration_secs").and_then(Value::as_i64).unwrap_or(0),
        details.get("started_epoch_s").and_then(Value::as_i64).unwrap_or(0),
        details.get("recovered_epoch_s").and_then(Value::as_i64).unwrap_or(0),
        details
            .get("claimed_by_execution_id")
            .and_then(Value::as_str)
            .unwrap_or("-"),
        details.get("claimed_host_id").and_then(Value::as_str).unwrap_or("-"),
        details.get("refused_attempts").and_then(Value::as_u64).unwrap_or(0),
    )
}

pub(super) fn to_json(outage: Option<&DispatchOutage>) -> Value {
    outage
        .and_then(|outage| serde_json::to_value(outage).ok())
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_engine::dispatch_events::{Outcome, Stage};

    fn event(stage: Stage, outcome: Outcome, execution: &str, item: &str, ts_ms: u128) -> DispatchEvent {
        let mut event = DispatchEvent::new(stage, outcome, execution).with_work_item(item);
        event.ts_epoch_ms = ts_ms;
        event
    }

    fn started(ts_ms: u128) -> DispatchEvent {
        event(Stage::DispatchOutageStarted, Outcome::Error, "engine", "task_1", ts_ms).with_details(serde_json::json!({
            "started_epoch_s": ts_ms / 1000,
            "triggering_execution_id": "exec_1",
            "host_exclusions": [{
                "host_id": "local",
                "kind": "local_quarantine",
                "cause": "held",
                "reason": "local dispatch quarantined: x",
                "recovery_action": "delete the row",
            }],
        }))
    }

    fn refusal(execution: &str, item: &str, ts_ms: u128) -> DispatchEvent {
        event(Stage::HostSelected, Outcome::Error, execution, item, ts_ms).with_details(serde_json::json!({
            "reason": "no_eligible_host",
            "global_outage": true,
            "outage_started_epoch_s": 1_000,
        }))
    }

    fn claim(execution: &str, ts_ms: u128) -> DispatchEvent {
        event(Stage::HostSelected, Outcome::Ok, execution, "task_9", ts_ms)
            .with_details(serde_json::json!({ "host_id": "anaplian", "existing_assignment": false }))
    }

    #[test]
    fn a_start_with_no_recovery_is_an_active_outage_with_refusals_counted() {
        let events = vec![
            refusal("exec_1", "task_1", 1_000_000),
            started(1_000_000),
            refusal("exec_2", "task_2", 1_100_000),
            refusal("exec_1", "task_1", 1_200_000),
        ];
        let active = from_events(&events).active.expect("outage still active");
        assert_eq!(active.started_epoch_s, 1_000);
        assert_eq!(
            active.refused_attempts, 3,
            "the three global refusals; the start event counts none itself"
        );
        assert_eq!(active.refused_work_items, 2);
        assert_eq!(active.latest_execution_id, "exec_1");
        assert_eq!(active.host_exclusions[0].host_id, "local");
        let text = lines(&active, 1_780).join("\n");
        assert!(text.contains("13 minutes ago"), "{text}");
        assert!(text.contains("engine refusal: local dispatch quarantined: x"), "{text}");
        assert!(text.contains("recovery: delete the row"), "{text}");
    }

    #[test]
    fn a_recovery_ends_the_outage_and_is_reported() {
        let recovered = event(
            Stage::DispatchOutageRecovered,
            Outcome::Ok,
            "engine",
            "task_3",
            1_780_000,
        )
        .with_details(serde_json::json!({
            "started_epoch_s": 1_000,
            "recovered_epoch_s": 1_780,
            "duration_secs": 780,
            "refused_attempts": 3,
            "claimed_by_execution_id": "exec_3",
            "claimed_host_id": "local",
        }));
        // Out of order on purpose: readers sort by time.
        let events = vec![recovered, refusal("exec_1", "task_1", 1_100_000), started(1_000_000)];
        let outages = from_events(&events);
        assert!(outages.active.is_none());
        let line = recovery_line(&outages.last_recovery.expect("recovery recorded"));
        assert!(line.contains("lasted 780s"), "{line}");
        assert!(line.contains("exec_3"), "{line}");
    }

    #[test]
    fn only_global_refusals_open_an_outage() {
        let not_global = event(Stage::HostSelected, Outcome::Error, "exec_1", "task_1", 5_000)
            .with_details(serde_json::json!({ "reason": "no_eligible_host" }));
        let outages = from_events(&[not_global]);
        assert!(outages.active.is_none());
        assert!(outages.last_recovery.is_none());
        // A global refusal is an outage even if its start event was lost.
        assert!(from_events(&[refusal("exec_1", "task_1", 5_000)]).active.is_some());
    }

    #[test]
    fn a_fresh_claim_closes_an_outage_whose_recovered_event_was_never_emitted() {
        // The engine restarted mid-outage and its first event was a launch.
        let events = vec![
            started(1_000_000),
            refusal("exec_1", "task_1", 1_100_000),
            claim("exec_9", 1_200_000),
        ];
        let outages = from_events(&events);
        assert!(outages.active.is_none(), "dispatch is healthy again");
        let line = recovery_line(&outages.last_recovery.expect("recovery derived from the claim"));
        assert!(line.contains("exec_9"), "{line}");
    }

    #[test]
    fn refusals_that_are_not_global_are_not_counted_and_exclusion_changes_are_followed() {
        let not_global = event(Stage::HostSelected, Outcome::Error, "exec_5", "task_5", 1_050_000)
            .with_details(serde_json::json!({ "reason": "no_eligible_host" }));
        let changed =
            event(Stage::HostSelected, Outcome::Error, "exec_2", "task_2", 1_100_000).with_details(serde_json::json!({
                "reason": "no_eligible_host",
                "global_outage": true,
                "outage_started_epoch_s": 1_000,
                "host_exclusions": [{
                    "host_id": "anaplian",
                    "kind": "host_disabled",
                    "cause": "host is disabled",
                    "reason": "disabled",
                    "recovery_action": null,
                }],
            }));
        let first = refusal("exec_1", "task_1", 1_000_000);
        let active = from_events(&[first, started(1_000_000), not_global, changed])
            .active
            .unwrap();
        assert_eq!(
            active.refused_attempts, 2,
            "two global refusals; the local one is not counted"
        );
        assert_eq!(active.refused_work_items, 2);
        assert_eq!(active.host_exclusions.len(), 1);
        assert_eq!(active.host_exclusions[0].host_id, "anaplian");
    }
}
