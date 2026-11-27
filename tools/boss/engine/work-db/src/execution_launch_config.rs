use std::collections::BTreeMap;

use super::*;

/// The arm values an experiment arm set may hold, in the launch record's
/// canonical text (see `boss_feature_flags::ExperimentArm`). Kept here as
/// strings because this crate has no dependency on the flags crate; the
/// runner's tests round-trip every arm through the writer so the two cannot
/// drift apart unnoticed.
const ARM_VALUES: [&str; 3] = ["control", "treatment", "stopped"];
const ARM_STOPPED: &str = "stopped";

/// Everything the launch record stores for one execution, written together in
/// one transaction after the runner has launched the worker.
#[derive(Debug, Clone, Copy)]
pub struct ExecutionLaunchRecord<'a> {
    pub execution_id: &'a str,
    pub driver: &'a str,
    pub model: &'a str,
    pub effort_level: Option<EffortLevel>,
    /// Canonical text of the experiment arm set this launch ran under
    /// (`boss_feature_flags::ExperimentArmSet::encode`): a JSON object from
    /// experiment key to `control`, `treatment` or `stopped`. `{}` records that
    /// nothing was running. `None` leaves the column NULL, "not recorded".
    ///
    /// Written once, with the rest of the tuple. A later launch of the same
    /// execution changes it in one way only: a key it names as `stopped` is
    /// rewritten to `stopped` in the stored set, whichever arm it held.
    pub experiment_arms: Option<&'a str>,
}

/// What the launch record holds for an execution's experiment arms, for the
/// runner to read before it composes a later run of the same execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredExperimentArms {
    /// No launch has been recorded: the next launch is the execution's first
    /// and draws its arms from the flags as they are.
    NotLaunched,
    /// A launch was recorded without arms (before the column existed, or
    /// without a set). Later runs are not treated, and nothing is added.
    Unrecorded,
    /// The canonical text recorded at the first launch.
    Recorded(String),
}

/// Parse an arm set's canonical text, or `None` when it is not a JSON object
/// of known arms.
fn parse_arm_set(text: &str) -> Option<BTreeMap<String, String>> {
    let map: BTreeMap<String, String> = serde_json::from_str(text).ok()?;
    map.values()
        .all(|arm| ARM_VALUES.contains(&arm.as_str()))
        .then_some(map)
}

/// Apply the one change a later launch may make to a stored arm set: every key
/// `incoming` names as `stopped` becomes `stopped`. Keys `incoming` lacks, and
/// keys it holds under any other arm, are left as stored; keys only `incoming`
/// has are not added, because an experiment that started after the first launch
/// is not picked up by a later run.
fn apply_stopped_keys(
    stored: &BTreeMap<String, String>,
    incoming: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    stored
        .iter()
        .map(|(key, arm)| {
            let stopped = incoming.get(key).is_some_and(|arm| arm == ARM_STOPPED);
            (key.clone(), if stopped { ARM_STOPPED.to_owned() } else { arm.clone() })
        })
        .collect()
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Persist the resolved worker launch configuration exactly once per
    /// execution. This is deliberately stamped after the runner successfully
    /// launches the worker, not inferred later from current routing/model
    /// policy, so execution history remains an honest record of what ran.
    ///
    /// Four positional arguments, no experiment arms: the launch tuple as it
    /// was before arms existed. [`Self::record_execution_launch`] is the
    /// writer; this wrapper stays so callers that do not run experiments need
    /// no change.
    pub fn record_execution_launch_config(
        &self,
        execution_id: &str,
        driver: &str,
        model: &str,
        effort_level: Option<EffortLevel>,
    ) -> Result<WorkExecution> {
        self.record_execution_launch(&ExecutionLaunchRecord {
            execution_id,
            driver,
            model,
            effort_level,
            experiment_arms: None,
        })
    }

    /// The single launch-record writer: driver, model, effort level and the
    /// experiment arm set, in one transaction.
    ///
    /// The first launch of an execution writes all of them. A later launch
    /// leaves the tuple as the first one wrote it, and changes the arm set only
    /// by marking keys `stopped` (see [`ExecutionLaunchRecord::experiment_arms`]).
    /// An arm set that is not canonical text is not stored, because a malformed
    /// value would poison every comparison that reads it; the launch tuple is
    /// still recorded and the arms stay "not recorded".
    pub fn record_execution_launch(&self, record: &ExecutionLaunchRecord<'_>) -> Result<WorkExecution> {
        let execution_id = record.execution_id;
        let arms = record.experiment_arms.and_then(|text| {
            let parsed = parse_arm_set(text);
            if parsed.is_none() {
                tracing::error!(
                    execution_id,
                    experiment_arms = text,
                    "experiment arm set is not canonical text; recording the launch without it",
                );
            }
            parsed
        });
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing = query_execution(&tx, execution_id).require("execution", execution_id)?;
        // Seed the selected launch model even when no usage arrives. Observed
        // transcript models remain authoritative, including mixed-model usage.
        tx.execute(
            "UPDATE work_runs SET model = ?2
             WHERE execution_id = ?1 AND status = 'active' AND model IS NULL
               AND input_tokens IS NULL AND output_tokens IS NULL
               AND cache_creation_tokens IS NULL AND cache_read_tokens IS NULL",
            params![execution_id, record.model],
        )?;
        if existing.driver.is_some() || existing.model.is_some() {
            if let Some(incoming) = arms.as_ref() {
                stop_recorded_experiment_arms(&tx, execution_id, incoming)?;
            }
            tx.commit()?;
            return Ok(existing);
        }

        tx.execute(
            "UPDATE work_executions
             SET driver = ?2,
                 model = ?3,
                 effort_level = ?4,
                 experiment_arms = ?5
             WHERE id = ?1",
            params![
                execution_id,
                record.driver,
                record.model,
                record.effort_level.map(|level| level.as_str()),
                arms.as_ref().map(canonical_arm_text),
            ],
        )?;
        tx.execute(
            "UPDATE execution_driver_decisions SET model = ?2 WHERE execution_id = ?1",
            params![execution_id, record.model],
        )?;
        let updated = query_execution(&tx, execution_id)?
            .with_context(|| format!("missing execution after launch-config update: {execution_id}"))?;
        tx.commit()?;
        Ok(updated)
    }

    /// What the launch record holds for `execution_id`'s experiment arms.
    pub fn stored_experiment_arms(&self, execution_id: &str) -> Result<StoredExperimentArms> {
        let conn = self.connect()?;
        let (driver, model, arms): (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT driver, model, experiment_arms FROM work_executions WHERE id = ?1",
                [execution_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .with_context(|| format!("execution {execution_id} not found reading its experiment arms"))?;
        // The same test the writer uses to tell a first launch from a later one.
        Ok(match (driver.is_some() || model.is_some(), arms) {
            (false, _) => StoredExperimentArms::NotLaunched,
            (true, None) => StoredExperimentArms::Unrecorded,
            (true, Some(text)) => StoredExperimentArms::Recorded(text),
        })
    }

    /// The PR family `execution` draws its experiment arms in: the chain root
    /// of its work item (see [`experiment_family_root`]).
    pub fn experiment_family_root(&self, execution: &WorkExecution) -> Result<String> {
        let conn = self.connect()?;
        experiment_family_root(&conn, &execution.kind, &execution.work_item_id)
    }
}

fn canonical_arm_text(arms: &BTreeMap<String, String>) -> String {
    serde_json::to_string(arms).expect("a map of strings always serialises")
}

/// Mark keys of the stored arm set `stopped` where `incoming` says so. A stored
/// value that is NULL or unreadable is left alone: a later launch never fills in
/// arms the first one did not record.
fn stop_recorded_experiment_arms(
    tx: &rusqlite::Transaction<'_>,
    execution_id: &str,
    incoming: &BTreeMap<String, String>,
) -> Result<()> {
    let stored_text: Option<String> = tx.query_row(
        "SELECT experiment_arms FROM work_executions WHERE id = ?1",
        [execution_id],
        |row| row.get(0),
    )?;
    let Some(stored) = stored_text.as_deref().and_then(parse_arm_set) else {
        return Ok(());
    };
    let stopped = apply_stopped_keys(&stored, incoming);
    if stopped != stored {
        tx.execute(
            "UPDATE work_executions SET experiment_arms = ?2 WHERE id = ?1",
            params![execution_id, canonical_arm_text(&stopped)],
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "execution_launch_config_tests.rs"]
mod tests;
