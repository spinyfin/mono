//! `bossctl dispatch diagnose`: who ended each execution by decision, and why.
//!
//! The dispatch timeline records what the engine did; whether an execution
//! was cancelled or abandoned, and by which surface, comes from the
//! provenance stamped on the execution row when it was cancelled. Reading it
//! straight from `state.db` means the answer is there even for a cancel path
//! that never wrote a dispatch event.

use std::collections::BTreeMap;

use boss_engine::work::WorkDb;
use boss_protocol::{CancelProvenance, ExecutionStatus};
use serde_json::{Value, json};

/// What the execution row says about how a decision-terminal execution ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionTerminal {
    pub status: ExecutionStatus,
    /// `None` for a row ended before provenance was recorded.
    pub provenance: Option<CancelProvenance>,
}

/// Provenance for each of `execution_ids` that was ended by decision
/// (`cancelled` / `abandoned`). Executions in any other status are omitted.
pub fn decision_terminals(db: &WorkDb, execution_ids: &[String]) -> BTreeMap<String, DecisionTerminal> {
    let mut out = BTreeMap::new();
    for id in execution_ids {
        let Ok(execution) = db.get_execution(id) else {
            continue;
        };
        if !matches!(
            execution.status,
            ExecutionStatus::Cancelled | ExecutionStatus::Abandoned
        ) {
            continue;
        }
        out.insert(
            id.clone(),
            DecisionTerminal {
                status: execution.status,
                provenance: execution.cancel_provenance,
            },
        );
    }
    out
}

/// The `cancel_provenance` object of the `--json` payload, keyed by
/// execution id.
pub fn to_json(terminals: &BTreeMap<String, DecisionTerminal>) -> Value {
    Value::Object(
        terminals
            .iter()
            .map(|(id, terminal)| {
                (
                    id.clone(),
                    json!({
                        "status": terminal.status.as_str(),
                        "source": terminal.provenance.as_ref().map(|p| p.source.as_str()),
                        "actor": terminal.provenance.as_ref().map(|p| &p.actor),
                        "reason": terminal.provenance.as_ref().map(|p| &p.reason),
                        "recorded": terminal.provenance.is_some(),
                    }),
                )
            })
            .collect(),
    )
}

/// The one-line human rendering for an execution's terminal row.
pub fn render_line(id: &str, terminal: &DecisionTerminal) -> String {
    match &terminal.provenance {
        Some(p) => format!("{id}: {} by {} ({}): {}", terminal.status, p.source, p.actor, p.reason),
        None => format!(
            "{id}: {} (ended before cancel provenance was recorded; who and why are not on the row)",
            terminal.status
        ),
    }
}

/// The human-readable section, or an empty string when nothing in scope was
/// ended by decision.
pub fn render_section(terminals: &BTreeMap<String, DecisionTerminal>) -> String {
    if terminals.is_empty() {
        return String::new();
    }
    let mut out = String::from("\ncancel provenance\n");
    for (id, terminal) in terminals {
        out.push_str("  ");
        out.push_str(&render_line(id, terminal));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::CancelSource;

    fn terminal(provenance: Option<CancelProvenance>) -> DecisionTerminal {
        DecisionTerminal {
            status: ExecutionStatus::Cancelled,
            provenance,
        }
    }

    #[test]
    fn recorded_provenance_names_source_actor_and_reason() {
        let p = CancelProvenance::new(CancelSource::Bossctl, "user brian", "superseded by manual fix").unwrap();
        let line = render_line("exec_1", &terminal(Some(p)));
        assert_eq!(
            line,
            "exec_1: cancelled by bossctl (user brian): superseded by manual fix"
        );
    }

    #[test]
    fn legacy_rows_say_provenance_was_not_recorded() {
        let line = render_line("exec_old", &terminal(None));
        assert!(line.contains("before cancel provenance was recorded"), "{line}");
    }

    #[test]
    fn json_carries_the_triple_and_a_recorded_flag() {
        let p = CancelProvenance::new(CancelSource::DependencyBlock, "work_dependencies", "gated").unwrap();
        let mut map = BTreeMap::new();
        map.insert("exec_1".to_owned(), terminal(Some(p)));
        map.insert("exec_0".to_owned(), terminal(None));
        let value = to_json(&map);
        assert_eq!(value["exec_1"]["source"], "dependency_block");
        assert_eq!(value["exec_1"]["actor"], "work_dependencies");
        assert_eq!(value["exec_1"]["reason"], "gated");
        assert_eq!(value["exec_1"]["recorded"], true);
        assert_eq!(value["exec_0"]["recorded"], false);
        assert!(value["exec_0"]["source"].is_null());
    }
}
