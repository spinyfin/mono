//! The experiment arm set one worker launch runs under.
//!
//! Resolved before the prompt is composed, so what the worker is told and what
//! the launch record says come from the same set. The flags crate supplies the
//! pure rules (which experiments are running, which arm a bucket draws); this
//! module supplies the two facts those rules cannot know: the execution's PR
//! family, and whether the execution has launched before.
//!
//! - **First launch.** The arms are drawn from the flags as they are now, by the
//!   family's bucket for each running experiment.
//! - **Later run of an execution that recorded arms.** The stored set is reused,
//!   so the record stays true across a retry, with one change: a key whose
//!   experiment is no longer running becomes `stopped`, whichever arm it held,
//!   and is not treated. A key that started after the first launch is not added.
//! - **Later run of an execution that recorded no arms** (it launched before the
//!   column existed). Nothing is treated and nothing is added: a run that cannot
//!   be recorded must not be changed.
//!
//! Every failure to read resolves to the empty set, which is untreated and, at
//! worst, records "nothing was running". An experiment never changes a worker on
//! the strength of state this module could not read.

use crate::feature_flags::{ExperimentArmSet, FeatureFlagsStore};
use crate::work::{StoredExperimentArms, WorkDb, WorkExecution, experiment_bucket};

/// The arm set `execution` launches under now. See the module docs.
pub(crate) fn resolve_launch_experiment_arms(
    flags: &FeatureFlagsStore,
    work_db: &WorkDb,
    execution: &WorkExecution,
) -> ExperimentArmSet {
    let stored = match work_db.stored_experiment_arms(&execution.id) {
        Ok(stored) => stored,
        Err(error) => {
            tracing::warn!(
                execution_id = %execution.id,
                error = %format!("{error:#}"),
                "could not read the execution's experiment arms; launching without experiments",
            );
            return ExperimentArmSet::new();
        }
    };
    match stored {
        StoredExperimentArms::NotLaunched => draw_arms(flags, work_db, execution),
        StoredExperimentArms::Unrecorded => ExperimentArmSet::new(),
        StoredExperimentArms::Recorded(text) => match ExperimentArmSet::decode(&text) {
            Ok(mut arms) => {
                flags.stop_experiments_not_running(&mut arms);
                arms
            }
            Err(error) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    stored = %text,
                    error = %format!("{error:#}"),
                    "the execution's recorded experiment arms are unreadable; launching without experiments",
                );
                ExperimentArmSet::new()
            }
        },
    }
}

fn draw_arms(flags: &FeatureFlagsStore, work_db: &WorkDb, execution: &WorkExecution) -> ExperimentArmSet {
    match work_db.experiment_family_root(execution) {
        Ok(family) => flags.resolve_experiment_arms(|key| experiment_bucket(&family, key)),
        Err(error) => {
            tracing::warn!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                error = %format!("{error:#}"),
                "could not resolve the execution's PR family; launching without experiments",
            );
            ExperimentArmSet::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature_flags::{
        BATCHED_SEARCH_AB_SPLIT_FLAG, BATCHED_SEARCH_EXPERIMENT, ExperimentArm, TOKEN_EXPERIMENTS_FLAG,
    };
    use crate::test_support::{create_product, create_ready_chore_execution, create_test_chore, open_db};
    use crate::work::ExecutionLaunchRecord;
    use tempfile::TempDir;

    const KEY: &str = BATCHED_SEARCH_EXPERIMENT;

    fn flags() -> (TempDir, FeatureFlagsStore) {
        let dir = TempDir::new().unwrap();
        let store = FeatureFlagsStore::new(dir.path().join("feature-flags.toml"));
        store.load().unwrap();
        (dir, store)
    }

    fn execution(db: &WorkDb) -> WorkExecution {
        let task = create_test_chore(db, create_product(db), "arms");
        create_ready_chore_execution(db, &task.id)
    }

    /// Record `arms` as `execution`'s first launch, the way the dispatch site does.
    fn launch(db: &WorkDb, execution: &WorkExecution, arms: &ExperimentArmSet) {
        db.record_execution_launch(&ExecutionLaunchRecord {
            execution_id: &execution.id,
            driver: "claude",
            model: "opus",
            effort_level: None,
            experiment_arms: Some(&arms.encode()),
        })
        .unwrap();
    }

    fn stored(db: &WorkDb, execution: &WorkExecution) -> ExperimentArmSet {
        match db.stored_experiment_arms(&execution.id).unwrap() {
            StoredExperimentArms::Recorded(text) => ExperimentArmSet::decode(&text).unwrap(),
            other => panic!("expected recorded arms, got {other:?}"),
        }
    }

    fn drawn_arm(execution: &WorkExecution) -> ExperimentArm {
        // A new work item is its own family.
        if experiment_bucket(&execution.work_item_id, KEY) < 50 {
            ExperimentArm::Treatment
        } else {
            ExperimentArm::Control
        }
    }

    #[test]
    fn nothing_is_drawn_while_every_experiment_is_off() {
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        let execution = execution(&db);
        assert!(resolve_launch_experiment_arms(&flags, &db, &execution).is_empty());
    }

    #[test]
    fn a_first_launch_draws_its_familys_arm_from_the_live_flags() {
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        flags.set(KEY, true).unwrap();
        for _ in 0..8 {
            let execution = execution(&db);
            let arms = resolve_launch_experiment_arms(&flags, &db, &execution);
            assert_eq!(arms.get(KEY), Some(drawn_arm(&execution)), "{}", execution.work_item_id);
            assert_eq!(arms.is_treated(KEY), drawn_arm(&execution) == ExperimentArm::Treatment);
        }
    }

    #[test]
    fn with_the_split_off_every_first_launch_is_treated() {
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        flags.set(KEY, true).unwrap();
        flags.set(BATCHED_SEARCH_AB_SPLIT_FLAG, false).unwrap();
        for _ in 0..8 {
            let arms = resolve_launch_experiment_arms(&flags, &db, &execution(&db));
            assert_eq!(arms.get(KEY), Some(ExperimentArm::Treatment));
        }
    }

    #[test]
    fn the_master_switch_leaves_every_first_launch_untouched() {
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        flags.set(KEY, true).unwrap();
        flags.set(TOKEN_EXPERIMENTS_FLAG, false).unwrap();
        assert!(resolve_launch_experiment_arms(&flags, &db, &execution(&db)).is_empty());
    }

    #[test]
    fn a_later_run_reuses_the_stored_arms_whatever_the_flags_say_now() {
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        flags.set(KEY, true).unwrap();
        let execution = execution(&db);
        let first = resolve_launch_experiment_arms(&flags, &db, &execution);
        launch(&db, &execution, &first);

        // Every family treated now, which would flip a control execution.
        flags.set(BATCHED_SEARCH_AB_SPLIT_FLAG, false).unwrap();
        assert_eq!(resolve_launch_experiment_arms(&flags, &db, &execution), first);
        // And with the split back on.
        flags.set(BATCHED_SEARCH_AB_SPLIT_FLAG, true).unwrap();
        assert_eq!(resolve_launch_experiment_arms(&flags, &db, &execution), first);
    }

    #[test]
    fn an_experiment_started_after_the_first_launch_is_not_picked_up() {
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        let execution = execution(&db);
        let first = resolve_launch_experiment_arms(&flags, &db, &execution);
        assert!(first.is_empty());
        launch(&db, &execution, &first);

        flags.set(KEY, true).unwrap();
        flags.set(BATCHED_SEARCH_AB_SPLIT_FLAG, false).unwrap();
        let later = resolve_launch_experiment_arms(&flags, &db, &execution);
        assert!(later.is_empty());
        assert!(!later.is_treated(KEY));
    }

    #[test]
    fn a_later_run_of_an_execution_that_recorded_no_arms_is_not_treated() {
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        flags.set(KEY, true).unwrap();
        flags.set(BATCHED_SEARCH_AB_SPLIT_FLAG, false).unwrap();
        let execution = execution(&db);
        // Launched before arms existed: the four-argument writer records none.
        db.record_execution_launch_config(&execution.id, "claude", "opus", None)
            .unwrap();
        assert!(resolve_launch_experiment_arms(&flags, &db, &execution).is_empty());
    }

    #[test]
    fn turning_an_experiment_off_stops_both_arms_of_a_later_run() {
        for arm in [ExperimentArm::Treatment, ExperimentArm::Control] {
            let (_db_dir, db) = open_db();
            let (_flags_dir, flags) = flags();
            flags.set(KEY, true).unwrap();
            let execution = execution(&db);
            let mut first = ExperimentArmSet::new();
            first.insert(KEY, arm);
            launch(&db, &execution, &first);

            flags.set(KEY, false).unwrap();
            let later = resolve_launch_experiment_arms(&flags, &db, &execution);
            assert_eq!(later.get(KEY), Some(ExperimentArm::Stopped), "{arm:?} becomes stopped");
            assert!(!later.is_treated(KEY), "a stopped run is not treated");
        }
    }

    #[test]
    fn the_master_switch_stops_a_later_run_too() {
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        flags.set(KEY, true).unwrap();
        let execution = execution(&db);
        let mut first = ExperimentArmSet::new();
        first.insert(KEY, ExperimentArm::Treatment);
        launch(&db, &execution, &first);

        flags.set(TOKEN_EXPERIMENTS_FLAG, false).unwrap();
        assert_eq!(
            resolve_launch_experiment_arms(&flags, &db, &execution).get(KEY),
            Some(ExperimentArm::Stopped)
        );
    }

    /// The stop reaches the launch record through the writer. This pins the arm
    /// vocabulary the flags crate encodes against the one the writer accepts, so
    /// the two cannot drift apart without a failing test.
    #[test]
    fn a_stop_is_written_back_to_the_launch_record_in_both_arms() {
        for arm in [ExperimentArm::Treatment, ExperimentArm::Control] {
            let (_db_dir, db) = open_db();
            let (_flags_dir, flags) = flags();
            flags.set(KEY, true).unwrap();
            let execution = execution(&db);
            let mut first = ExperimentArmSet::new();
            first.insert(KEY, arm);
            first.insert("another_experiment", ExperimentArm::Treatment);
            launch(&db, &execution, &first);
            assert_eq!(stored(&db, &execution), first);

            // Both are no longer running: batched_search is off, and the other
            // is not registered at all.
            flags.set(KEY, false).unwrap();
            let later = resolve_launch_experiment_arms(&flags, &db, &execution);
            launch(&db, &execution, &later);

            let recorded = stored(&db, &execution);
            assert_eq!(recorded.get(KEY), Some(ExperimentArm::Stopped), "{arm:?}");
            assert_eq!(recorded.get("another_experiment"), Some(ExperimentArm::Stopped));
        }
    }

    #[test]
    fn every_arm_the_flags_crate_encodes_is_stored_as_written() {
        let (_db_dir, db) = open_db();
        let mut arms = ExperimentArmSet::new();
        arms.insert("alpha", ExperimentArm::Control);
        arms.insert("beta", ExperimentArm::Treatment);
        arms.insert("gamma", ExperimentArm::Stopped);
        let execution = execution(&db);
        launch(&db, &execution, &arms);
        assert_eq!(
            db.stored_experiment_arms(&execution.id).unwrap(),
            StoredExperimentArms::Recorded(arms.encode()),
            "the writer keeps the flags crate's canonical text byte for byte"
        );
        assert_eq!(stored(&db, &execution), arms);
    }

    #[test]
    fn unreadable_stored_arms_launch_untreated() {
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        flags.set(KEY, true).unwrap();
        flags.set(BATCHED_SEARCH_AB_SPLIT_FLAG, false).unwrap();
        let execution = execution(&db);
        launch(&db, &execution, &ExperimentArmSet::new());
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET experiment_arms = 'not json' WHERE id = ?1",
                [&execution.id],
            )
            .unwrap();
        assert!(resolve_launch_experiment_arms(&flags, &db, &execution).is_empty());
    }
}
