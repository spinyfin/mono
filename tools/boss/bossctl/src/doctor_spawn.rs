//! SIG-I: the worker pane was never even requested because a spawn
//! precondition was rejected.
//!
//! ## The incident this exists for
//!
//! Fifteen consecutive codex-driver spawns failed across 2026-08-06/07 with
//!
//! ```text
//! spawning worker pane for run <exec>: preparing progress ingress: <path>/sessions is not a real directory
//! ```
//!
//! `bossctl dispatch diagnose` on any of those executions matched **no**
//! signature and instead surfaced an unrelated stale SIG-H as its
//! top-priority result. The dispatch stream had carried the whole cause the
//! entire time; nothing was reading it.
//!
//! ## Why this is its own signature
//!
//! `spawn_failed` is emitted before any pane exists, and the classes it
//! covers are not interchangeable. A `progress_ingress` or `write_files`
//! rejection is a *deterministic host/config precondition*: the engine
//! checks it, refuses, and will refuse identically on every redispatch
//! forever — which is exactly why the outage ran for a day and a half. A
//! `send_spawn_request` / `app_rejected` / `tmux_host` failure is a
//! transport or engine/app-desync condition that normally self-heals and is
//! already covered by SIG-B (`SlotBusy`) and SIG-E (app NACK). So the
//! recovery text has to name the specific precondition that was rejected,
//! not offer generic spawn advice — a signature that said "the pane failed
//! to spawn, check the app" would have been no more actionable than the
//! silence it replaced.
//!
//! Structured `details.spawn_failure` (`{class, cause}`) is the primary
//! source; the `error_message` fallback exists so the signature still fires
//! on JSONL written before the engine started emitting that field, which is
//! precisely the historical data an operator diagnoses an outage from.

use std::collections::BTreeSet;

use boss_engine::dispatch_events::DispatchEvent;

use crate::doctor::{Finding, Severity, evidence_line, in_scope, work_item_of};

/// Marker the engine's `StartWorkerError::ProgressIngress` Display wraps its
/// cause in. Used only by the pre-`spawn_failure` fallback below.
const PROGRESS_INGRESS_MARKER: &str = "preparing progress ingress: ";

/// Spawn-failure classes that reject a *precondition* the engine evaluates
/// itself, and will therefore reject identically on every retry until a
/// human changes something on the host. These are the P0s: nothing in the
/// engine recovers from them, and no amount of redispatch helps.
///
/// Mirrors `StartWorkerError::class()` in the engine. A class this list does
/// not know still produces a finding — at P1, with whatever cause the event
/// carried — rather than being dropped, so a class added engine-side is
/// under-ranked, never invisible.
///
/// `response_kind_mismatch` is deliberately NOT in this list: unlike
/// `progress_ingress`/`write_files`, which are evaluated inside `spawn_flow`'s
/// pre-request path, before `SpawnWorkerPane` is sent, `StartWorkerError::ResponseKindMismatch`
/// is returned from the match on the app's reply to `SpawnWorkerPane` — i.e. only
/// after it was sent and the app replied with the wrong response variant. That is a
/// transport/protocol desync, not a host precondition — a pane may well have been
/// spawned and now needs reaping, so the P0 "no pane was ever requested;
/// do not grow the pool, retry, or reap panes" advice would be actively
/// wrong for it. It takes the transport branch below instead, which hands
/// off to SIG-B/SIG-E.
const DETERMINISTIC_PRECONDITION_CLASSES: &[&str] = &["progress_ingress", "write_files"];

/// What the spawn step refused, as `(class, cause)`.
///
/// Prefers the structured `details.spawn_failure` the engine emits; falls
/// back to the flattened `error_message` for older events. `class` is `None`
/// when neither source identified a step.
fn rejected_precondition(event: &DispatchEvent) -> (Option<String>, Option<String>) {
    if let Some(failure) = event.details.get("spawn_failure").filter(|v| !v.is_null()) {
        let class = failure.get("class").and_then(|v| v.as_str()).map(str::to_owned);
        let cause = failure.get("cause").and_then(|v| v.as_str()).map(str::to_owned);
        if class.is_some() || cause.is_some() {
            return (class, cause);
        }
    }
    // Fallback for JSONL predating `details.spawn_failure`. Only the
    // progress-ingress shape is recognised by message: it is the one whose
    // wrapper text is a stable engine constant, and inventing classes out of
    // free-form prose would be guessing.
    let message = event.error_message.as_deref().unwrap_or_default();
    match message.find(PROGRESS_INGRESS_MARKER) {
        Some(at) => (
            Some("progress_ingress".to_owned()),
            Some(message[at + PROGRESS_INGRESS_MARKER.len()..].trim().to_owned()),
        ),
        None => (None, None),
    }
}

/// Match `spawn_failed: error` — the stage the engine emits when
/// `ExecutionRunner::run_execution` returned before any pane existed.
///
/// One finding per event rather than one aggregated per execution: a
/// recurring spawn failure must keep being visible, and collapsing repeats
/// into a single row is how this class of outage stayed quiet in the first
/// place.
pub(crate) fn match_sig_i_spawn_precondition(events: &[DispatchEvent], scope: &BTreeSet<String>) -> Vec<Finding> {
    let mut out = Vec::new();
    for event in events {
        if !in_scope(&event.execution_id, scope) {
            continue;
        }
        if event.stage != "spawn_failed" || event.outcome != "error" {
            continue;
        }
        let (class, cause) = rejected_precondition(event);
        let deterministic = class
            .as_deref()
            .is_some_and(|c| DETERMINISTIC_PRECONDITION_CLASSES.contains(&c));

        let named = match (class.as_deref(), cause.as_deref()) {
            (Some(class), Some(cause)) => format!("The `{class}` step rejected: {cause}"),
            (Some(class), None) => format!("The `{class}` step refused the spawn."),
            (None, Some(cause)) => format!("The spawn step refused: {cause}"),
            (None, None) => "The spawn step refused; the event carries no classified cause — \
                             read `error_message` on the evidence line below."
                .to_owned(),
        };

        let advice = if deterministic {
            "This is a precondition the ENGINE evaluates before it asks the app for a pane, so no \
             pane was ever requested and every redispatch will be rejected identically until the \
             precondition itself is fixed. Fix the named condition on this host (create/repair the \
             path, fix the permissions, correct the driver's configured root); do not grow the \
             pool, retry, or reap panes. The run row's `error_text` carries the same flattened \
             cause (`boss task show --json`), and a `pane_spawn_failed` attention item is filed \
             against the work item."
        } else {
            "The spawn round-trip itself failed rather than a locally-checked precondition. Check \
             the co-occurring SIG-B (`SlotBusy` desync) and SIG-E (app NACK / libghostty surface) \
             findings first — those name the app-side condition. A `pane_spawn_failed` attention \
             item is filed against the work item with the same flattened cause."
        };

        let title = if deterministic {
            "Worker pane never requested — spawn precondition rejected"
        } else {
            "Worker pane spawn failed — spawn round-trip rejected"
        };

        out.push(Finding {
            sig_id: "SIG-I".into(),
            absence_based: false,
            severity: if deterministic { Severity::P0 } else { Severity::P1 },
            title: title.into(),
            execution_id: Some(event.execution_id.clone()),
            work_item_id: work_item_of(event),
            count: 1,
            evidence: vec![evidence_line(event)],
            recovery: format!("{named} {advice}"),
            details: serde_json::json!({
                "class": class,
                "cause": cause,
                "deterministic_precondition": deterministic,
            }),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_engine::dispatch_events::{Outcome, Stage};

    fn scope_all() -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn spawn_failed_event(details: serde_json::Value, error: &str) -> DispatchEvent {
        let err = anyhow::anyhow!("{}", error.to_owned());
        DispatchEvent::new(Stage::SpawnFailed, Outcome::Error, "exec_spawn_1")
            .with_work_item("task_1")
            .with_error(&err)
            .with_details(details)
    }

    #[test]
    fn structured_progress_ingress_failure_is_p0_and_names_the_precondition() {
        let event = spawn_failed_event(
            serde_json::json!({
                "run_id": "run_1",
                "spawn_failure": {
                    "class": "progress_ingress",
                    "cause": "/Users/x/.codex/sessions is not a real directory",
                },
            }),
            "spawning worker pane for run exec_spawn_1: preparing progress ingress: \
             /Users/x/.codex/sessions is not a real directory",
        );

        let findings = match_sig_i_spawn_precondition(&[event], &scope_all());
        assert_eq!(findings.len(), 1, "expected exactly one SIG-I finding: {findings:#?}");
        let finding = &findings[0];
        assert_eq!(finding.sig_id, "SIG-I");
        assert_eq!(finding.severity, Severity::P0);
        assert_eq!(finding.execution_id.as_deref(), Some("exec_spawn_1"));
        assert!(
            finding
                .recovery
                .contains("/Users/x/.codex/sessions is not a real directory"),
            "recovery must name the rejected precondition verbatim; got {:?}",
            finding.recovery,
        );
        assert!(
            finding.recovery.contains("progress_ingress"),
            "recovery must name the failing step; got {:?}",
            finding.recovery,
        );
        assert_eq!(finding.details["deterministic_precondition"], true);
    }

    #[test]
    fn falls_back_to_the_error_message_for_events_without_structured_details() {
        // Exactly the shape the 2026-08-06/07 outage left on disk: no
        // `spawn_failure` key, cause only in the flattened message.
        let event = spawn_failed_event(
            serde_json::json!({ "run_id": "run_1", "slot_id": 3 }),
            "spawning worker pane for run exec_spawn_1: preparing progress ingress: \
             /Users/x/.codex/sessions is not a real directory",
        );

        let findings = match_sig_i_spawn_precondition(&[event], &scope_all());
        assert_eq!(findings.len(), 1, "historical events must still match: {findings:#?}");
        assert_eq!(findings[0].severity, Severity::P0);
        assert_eq!(findings[0].details["class"], "progress_ingress");
        assert!(
            findings[0]
                .recovery
                .contains("/Users/x/.codex/sessions is not a real directory"),
            "recovery must name the precondition even on the fallback path; got {:?}",
            findings[0].recovery,
        );
    }

    #[test]
    fn transport_class_is_p1_and_points_at_the_app_side_signatures() {
        let event = spawn_failed_event(
            serde_json::json!({
                "spawn_failure": { "class": "app_rejected", "cause": "app reported spawn error: SlotBusy" },
            }),
            "spawning worker pane for run exec_spawn_1: app reported spawn error: SlotBusy",
        );

        let findings = match_sig_i_spawn_precondition(&[event], &scope_all());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::P1);
        assert!(
            findings[0].recovery.contains("SIG-B"),
            "a transport-class spawn failure should hand off to the app-side signatures; got {:?}",
            findings[0].recovery,
        );
    }

    #[test]
    fn response_kind_mismatch_is_p1_not_p0_because_the_app_already_replied() {
        // The app WAS contacted and DID respond (just with the wrong
        // variant) — this must take the transport branch, not the
        // deterministic-precondition one, since a pane may need reaping.
        let event = spawn_failed_event(
            serde_json::json!({
                "spawn_failure": {
                    "class": "response_kind_mismatch",
                    "cause": "app responded with unexpected response variant",
                },
            }),
            "spawning worker pane for run exec_spawn_1: app responded with unexpected response variant",
        );

        let findings = match_sig_i_spawn_precondition(&[event], &scope_all());
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].severity,
            Severity::P1,
            "response_kind_mismatch is a protocol desync after the app replied, not a locally-evaluated \
             precondition — it must not get the 'no pane was ever requested' P0 advice"
        );
        assert!(
            findings[0].recovery.contains("SIG-B"),
            "must hand off to the app-side signatures, not claim no pane was ever requested; got {:?}",
            findings[0].recovery,
        );
        assert_eq!(findings[0].details["deterministic_precondition"], false);
        assert!(
            !findings[0].title.contains("never requested"),
            "the transport branch must not carry the deterministic-precondition headline; got {:?}",
            findings[0].title,
        );
    }

    #[test]
    fn unrelated_stages_and_out_of_scope_executions_do_not_match() {
        let other_stage = DispatchEvent::new(Stage::PaneSpawned, Outcome::Ok, "exec_spawn_1");
        assert!(match_sig_i_spawn_precondition(&[other_stage], &scope_all()).is_empty());

        let event = spawn_failed_event(serde_json::json!({}), "boom");
        let mut scope = BTreeSet::new();
        scope.insert("exec_other".to_owned());
        assert!(
            match_sig_i_spawn_precondition(&[event], &scope).is_empty(),
            "scoped diagnose must not report another execution's spawn failure",
        );
    }

    #[test]
    fn every_repeat_occurrence_produces_its_own_finding() {
        // The outage was fifteen consecutive failures. Collapsing repeats
        // into one row is the behaviour that let it stay quiet.
        let events: Vec<DispatchEvent> = (0..3)
            .map(|_| {
                spawn_failed_event(
                    serde_json::json!({
                        "spawn_failure": { "class": "progress_ingress", "cause": "/p/sessions is not a real directory" },
                    }),
                    "spawning worker pane for run exec_spawn_1: preparing progress ingress: \
                     /p/sessions is not a real directory",
                )
            })
            .collect();

        let findings = match_sig_i_spawn_precondition(&events, &scope_all());
        assert_eq!(findings.len(), 3, "each occurrence must stay visible: {findings:#?}");
    }
}
