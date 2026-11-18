//! `bossctl` reporting of a global dispatch outage (every host excluded for
//! every item), shared by `dispatch state` (live engine record) and `dispatch
//! diagnose` (rebuilt from the dispatch event stream, so it still answers when
//! the engine is wedged). Both print the same fields.

use boss_engine::dispatch_events::DispatchEvent;
use boss_execution_contracts::format_outage_duration;
use boss_protocol::{DispatchOutage, DispatchOutageHostExclusion};
use serde_json::Value;

const STARTED: &str = "dispatch_outage_started";
const RECOVERED: &str = "dispatch_outage_recovered";

/// What the dispatch event stream says about outages: the one still in
/// progress (a start with no recovery after it) and the most recent recovery.
#[derive(Debug, Default)]
pub(super) struct OutageFromEvents {
    pub(super) active: Option<DispatchOutage>,
    pub(super) last_recovery: Option<Value>,
}

/// Rebuild outage state from `events` (any order). An outage runs from its
/// `dispatch_outage_started` event to the next `dispatch_outage_recovered`;
/// the refused-attempt counters are re-derived from the per-item
/// `host_selected` refusals in between, which the engine does not repeat as
/// outage events.
pub(super) fn from_events(events: &[DispatchEvent]) -> OutageFromEvents {
    let mut ordered: Vec<&DispatchEvent> = events.iter().collect();
    ordered.sort_by_key(|event| event.ts_epoch_ms);
    let mut out = OutageFromEvents::default();
    let mut work_items = std::collections::BTreeSet::new();
    for event in ordered {
        match event.stage.as_str() {
            STARTED => {
                work_items.clear();
                work_items.extend(event.work_item_id.clone());
                out.active = Some(started_record(event));
            }
            RECOVERED => {
                out.active = None;
                out.last_recovery = Some(event.details.clone());
            }
            "host_selected"
                if event.outcome == "error"
                    && event.details.get("reason").and_then(Value::as_str) == Some("no_eligible_host") =>
            {
                if let Some(active) = out.active.as_mut() {
                    active.refused_attempts += 1;
                    if let Some(item) = event.work_item_id.as_deref() {
                        work_items.insert(item.to_owned());
                        active.latest_work_item_id = item.to_owned();
                    }
                    active.refused_work_items = work_items.len() as u64;
                    active.latest_refusal_epoch_s = epoch_s(event);
                    active.latest_execution_id = event.execution_id.clone();
                }
            }
            _ => {}
        }
    }
    out
}

fn epoch_s(event: &DispatchEvent) -> i64 {
    i64::try_from(event.ts_epoch_ms / 1000).unwrap_or(i64::MAX)
}

fn started_record(event: &DispatchEvent) -> DispatchOutage {
    let exclusions = event
        .details
        .get("host_exclusions")
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<DispatchOutageHostExclusion>>(value).ok())
        .unwrap_or_default();
    let started = event
        .details
        .get("started_epoch_s")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| epoch_s(event));
    let triggering = event
        .details
        .get("triggering_execution_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    DispatchOutage::builder()
        .started_epoch_s(started)
        .host_exclusions(exclusions)
        // The refusal that raised the outage is the first attempt (its own
        // `host_selected` event precedes this one); the refusals the scan
        // sees after this event are the rest.
        .refused_attempts(1)
        .refused_work_items(u64::from(event.work_item_id.is_some()))
        .latest_refusal_epoch_s(epoch_s(event))
        .latest_work_item_id(event.work_item_id.clone().unwrap_or_default())
        .latest_execution_id(triggering)
        .build()
}

/// The text block both verbs print for an active outage.
pub(super) fn lines(outage: &DispatchOutage, now_epoch_s: i64) -> Vec<String> {
    let mut lines = vec![
        "DISPATCH OUTAGE: no host can accept a new worker".to_owned(),
        format!(
            "  started: epoch {} ({} ago)",
            outage.started_epoch_s,
            format_outage_duration(now_epoch_s.saturating_sub(outage.started_epoch_s))
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
        "last dispatch outage: lasted {} (epoch {} to {}); recovered by {} on host {}; {} attempts refused",
        format_outage_duration(details.get("duration_secs").and_then(Value::as_i64).unwrap_or(0)),
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
        event(Stage::HostSelected, Outcome::Error, execution, item, ts_ms)
            .with_details(serde_json::json!({ "reason": "no_eligible_host" }))
    }

    #[test]
    fn a_start_with_no_recovery_is_an_active_outage_with_refusals_counted() {
        let events = vec![
            started(1_000_000),
            refusal("exec_1", "task_1", 1_000_100),
            refusal("exec_2", "task_2", 1_100_000),
            refusal("exec_1", "task_1", 1_200_000),
        ];
        let active = from_events(&events).active.expect("outage still active");
        assert_eq!(active.started_epoch_s, 1_000);
        assert_eq!(
            active.refused_attempts, 4,
            "the starting refusal plus the three that followed"
        );
        assert_eq!(active.refused_work_items, 2);
        assert_eq!(active.latest_execution_id, "exec_1");
        assert_eq!(active.host_exclusions[0].host_id, "local");
        let text = lines(&active, 1_780).join("\n");
        assert!(text.contains("13 min ago"), "{text}");
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
        assert!(line.contains("lasted 13 min"), "{line}");
        assert!(line.contains("exec_3"), "{line}");
    }

    #[test]
    fn refusals_outside_an_outage_are_not_counted() {
        let outages = from_events(&[refusal("exec_1", "task_1", 5_000)]);
        assert!(outages.active.is_none());
        assert!(outages.last_recovery.is_none());
    }
}
