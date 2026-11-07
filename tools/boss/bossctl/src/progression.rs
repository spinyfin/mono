//! What `bossctl dispatch diagnose` reports about progression for the work item
//! it is looking at: whether the engine would spend an administrative agent on
//! it, and if not, which clause of the candidate predicate says no.
//!
//! Read straight from `state.db`, like the rest of `diagnose`, so it works with
//! the engine down. The verdict comes from the same `progression_candidates`
//! views the sweep and the stuck report read, so it cannot disagree with them.

use anyhow::Result;
use boss_engine::work::{ProgressionClauses, WorkDb};

pub(crate) struct ProgressionDiagnosis {
    work_item_id: String,
    /// `None` when the item is not an open task: finished, deleted, a project, or unknown.
    clauses: Option<ProgressionClauses>,
}

impl ProgressionDiagnosis {
    pub(crate) fn load(db: &WorkDb, work_item_id: &str) -> Result<Self> {
        Ok(Self {
            work_item_id: work_item_id.to_owned(),
            clauses: db.progression_clauses(work_item_id)?,
        })
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        match &self.clauses {
            None => serde_json::json!({
                "work_item_id": self.work_item_id,
                "open_task": false,
                "candidate": false,
            }),
            Some(clauses) => serde_json::json!({
                "work_item_id": self.work_item_id,
                "open_task": true,
                "candidate": clauses.is_candidate(),
                "clauses": clauses,
                "failing_clauses": clauses
                    .failing_clauses()
                    .into_iter()
                    .map(|(number, reason)| serde_json::json!({"clause": number, "reason": reason}))
                    .collect::<Vec<_>>(),
            }),
        }
    }

    pub(crate) fn lines(&self, now_epoch_s: i64) -> Vec<String> {
        let Some(clauses) = &self.clauses else {
            return vec![format!(
                "progression: {} is not an open task (finished, deleted, a project, or unknown); \
                 progression only addresses open tasks",
                self.work_item_id
            )];
        };
        let mut lines = Vec::new();
        let kind = clauses.stuck_kind.as_deref().unwrap_or("none");
        let stuck_for = now_epoch_s.saturating_sub(clauses.stuck_since).max(0);
        lines.push(format!(
            "progression: {} — stuck state {kind}, in its current state for {}",
            self.work_item_id,
            human_duration(stuck_for),
        ));
        if clauses.is_candidate() {
            lines.push(format!(
                "  CANDIDATE: every clause holds{}; while the `progression_agents` feature flag is on, \
                 the sweep queues a progression run for it on its next pass",
                if clauses.grace_waived {
                    " (the stuck-after grace was waived)"
                } else {
                    ""
                },
            ));
        } else {
            lines.push("  not a candidate:".to_owned());
            for (number, reason) in clauses.failing_clauses() {
                lines.push(format!("    clause {number}: {reason}"));
            }
        }
        if clauses.progression_live {
            lines.push("  a progression run is live or queued for this item".to_owned());
        } else if clauses.infrastructure_failing {
            lines.push(
                "  the last progression runs ended in infrastructure failure, so the sweep no longer \
                 nominates this item"
                    .to_owned(),
            );
        } else if clauses.in_cooldown {
            lines.push("  a progression run finished recently; the item is inside the cooldown".to_owned());
        }
        lines
    }
}

fn human_duration(secs: i64) -> String {
    match secs {
        s if s >= 86_400 => format!("{}d {}h", s / 86_400, (s % 86_400) / 3_600),
        s if s >= 3_600 => format!("{}h {}m", s / 3_600, (s % 3_600) / 60),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clauses(candidate: bool) -> ProgressionClauses {
        ProgressionClauses::builder()
            .work_item_id("task_a")
            .stuck_kind("S2_churn_guard")
            .stuck_since(1_000)
            .grace_waived(false)
            .eligible_item(true)
            .no_live_work(candidate)
            .not_capacity_wait(true)
            .dependency_clear(true)
            .stuck_long_enough(true)
            .cooldown_and_budget_clear(true)
            .progression_live(false)
            .in_cooldown(false)
            .infrastructure_failing(false)
            .build()
    }

    #[test]
    fn a_candidate_says_the_sweep_will_queue_it() {
        let diagnosis = ProgressionDiagnosis {
            work_item_id: "task_a".into(),
            clauses: Some(clauses(true)),
        };
        let lines = diagnosis.lines(1_000 + 3 * 3_600).join("\n");
        assert!(
            lines.contains("stuck state S2_churn_guard, in its current state for 3h 0m"),
            "{lines}"
        );
        assert!(lines.contains("CANDIDATE"), "{lines}");
        assert_eq!(diagnosis.to_json()["candidate"], true);
    }

    #[test]
    fn a_non_candidate_names_the_failing_clause() {
        let diagnosis = ProgressionDiagnosis {
            work_item_id: "task_a".into(),
            clauses: Some(clauses(false)),
        };
        let lines = diagnosis.lines(2_000).join("\n");
        assert!(lines.contains("not a candidate"), "{lines}");
        assert!(lines.contains("clause 2: work is already live or pending"), "{lines}");
        let json = diagnosis.to_json();
        assert_eq!(json["candidate"], false);
        assert_eq!(json["failing_clauses"][0]["clause"], 2);
    }

    #[test]
    fn a_closed_or_unknown_item_is_not_an_open_task() {
        let diagnosis = ProgressionDiagnosis {
            work_item_id: "task_done".into(),
            clauses: None,
        };
        assert!(diagnosis.lines(0)[0].contains("not an open task"));
        assert_eq!(diagnosis.to_json()["open_task"], false);
    }
}
