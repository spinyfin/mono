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
//! - **Pre-merge review-batch members.** The `delta_rereview` arm is decided
//!   once, at batch admission, and persisted atomically on
//!   `pr_review_batches.delta_rereview_arm` (`treatment`, `control`, or `NULL`
//!   when not enrolled). Every leaf, supervisor, member retry and restarted
//!   execution of a pre-merge batch resolves `delta_rereview` from that stored
//!   value, ignoring live flags for that key — so a supervisor never disagrees
//!   with its leaves, a control-admitted batch never flips to treatment or
//!   unrecorded while queued, and a not-enrolled batch never picks up the key
//!   mid-batch if the flags turn on.
//! - **Kill-switch policy for `delta_rereview`.** Switching `delta_rereview`
//!   (or `token_experiments`) off stops new pre-merge batches from enrolling.
//!   An already-admitted batch cannot change its shape mid-batch (its member
//!   rows and solo/full mode are immutable), so every member — including a
//!   restarted execution — completes under the batch's admitted behaviour and
//!   keeps its admission-time `delta_rereview` attribution rather than being
//!   rewritten to `stopped`. Other experiments on the same execution still
//!   follow `stop_experiments_not_running` as usual.
//!
//! Every failure to read resolves to the empty set, which is untreated and, at
//! worst, records "nothing was running". An experiment never changes a worker on
//! the strength of state this module could not read.

use crate::feature_flags::{ExperimentArm, ExperimentArmSet, FeatureFlagsStore};
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
        StoredExperimentArms::NotLaunched => {
            let mut arms = draw_arms(flags, work_db, execution);
            align_delta_rereview_arm(work_db, execution, &mut arms);
            arms
        }
        StoredExperimentArms::Unrecorded => ExperimentArmSet::new(),
        StoredExperimentArms::Recorded(text) => match ExperimentArmSet::decode(&text) {
            Ok(mut arms) => {
                flags.stop_experiments_not_running(&mut arms);
                align_delta_rereview_arm(work_db, execution, &mut arms);
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

/// Resolve a pre-merge review-batch member's `delta_rereview` arm from the
/// batch's persisted admission-time assignment (`pr_review_batches.delta_rereview_arm`).
/// See the module docs. Any read failure leaves `arms` unchanged.
fn align_delta_rereview_arm(work_db: &WorkDb, execution: &WorkExecution, arms: &mut ExperimentArmSet) {
    let key = crate::feature_flags::DELTA_REREVIEW_EXPERIMENT;
    let Ok(Some(member)) = work_db.review_batch_member_for_execution(&execution.id) else {
        return;
    };
    let Ok(Some(batch)) = work_db.review_batch(&member.batch_id) else {
        return;
    };
    if batch.phase != boss_protocol::ReviewBatchPhase::PreMerge {
        return;
    }
    match batch.delta_rereview_arm {
        Some(boss_protocol::DeltaRereviewBatchArm::Treatment) => {
            arms.insert(key, ExperimentArm::Treatment);
        }
        Some(boss_protocol::DeltaRereviewBatchArm::Control) => {
            arms.insert(key, ExperimentArm::Control);
        }
        None if batch.single_reviewer => {
            arms.insert(key, ExperimentArm::Treatment);
        }
        None => {
            arms.remove(key);
        }
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

    /// Admit a pre-merge review batch for a fresh chore and return its batch id and executions.
    fn review_batch_with_arm(
        db: &WorkDb,
        solo: bool,
        arm: Option<boss_protocol::DeltaRereviewBatchArm>,
    ) -> (String, Vec<WorkExecution>) {
        use crate::work::{ReviewBatchCreateInput, ReviewBatchDispatch};
        let task = create_test_chore(db, create_product(db), "batch target");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/o/r/pull/9' WHERE id = ?1",
                [&task.id],
            )
            .unwrap();
        let input = ReviewBatchCreateInput::builder()
            .cycle_root_id(task.id.clone())
            .base_sha("base")
            .classification(
                boss_protocol::ReviewClassification::builder()
                    .changed_files(vec!["src/lib.rs".to_owned()])
                    .complexity_flags(vec![])
                    .has_production_code(true)
                    .metadata_missing(vec![])
                    .production_languages(vec![boss_protocol::ReviewLanguageBucket::Rust])
                    .profile(boss_protocol::ReviewProfile::Light)
                    .subsystem_buckets(vec!["src".to_owned()])
                    .build(),
            )
            .phase(boss_protocol::ReviewBatchPhase::PreMerge)
            .pr_number(9)
            .pr_url("https://github.com/o/r/pull/9")
            .target_sha("head")
            .single_reviewer_light(solo)
            .maybe_delta_rereview_arm(arm)
            .build();
        match db
            .create_pre_merge_review_batch(input, "git@github.com:o/r.git")
            .unwrap()
        {
            ReviewBatchDispatch::Created { batch, executions } => (batch.id, executions),
            other => panic!("expected a new batch, got {other:?}"),
        }
    }

    /// Attach a pending supervisor execution to `batch_id` so tests can check
    /// supervisor arm resolution alongside leaf members.
    fn attach_supervisor(db: &WorkDb, batch_id: &str) -> WorkExecution {
        let batch = db.review_batch(batch_id).unwrap().expect("batch exists");
        let exec = db
            .create_execution(
                boss_protocol::CreateExecutionInput::builder()
                    .work_item_id(batch.cycle_root_id)
                    .kind(boss_protocol::ExecutionKind::PrReview)
                    .repo_remote_url("git@github.com:o/r.git")
                    .build(),
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO pr_review_batch_members (
                    id, batch_id, attempt, created_at, provider_effort,
                    requested_driver, resolved_model, role, status, updated_at,
                    execution_id, report_proposal_id, terminal_at
                 ) VALUES ('rvm_sup', ?1, 1, '1', 'high', 'claude', 'opus', 'supervisor', 'pending', '1', ?2, NULL, NULL)",
                rusqlite::params![batch_id, exec.id],
            )
            .unwrap();
        exec
    }

    #[test]
    fn a_solo_delta_batch_launches_as_treatment_even_after_the_experiment_is_switched_off() {
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        let (_, executions) = review_batch_with_arm(&db, true, Some(boss_protocol::DeltaRereviewBatchArm::Treatment));
        assert_eq!(executions.len(), 1);

        // The flags changed between admission and launch: nothing is running now.
        let arms = resolve_launch_experiment_arms(&flags, &db, &executions[0]);

        assert_eq!(
            arms.get(crate::feature_flags::DELTA_REREVIEW_EXPERIMENT),
            Some(ExperimentArm::Treatment)
        );
    }

    #[test]
    fn a_control_admitted_full_batch_stays_control_when_flags_flip_before_any_member_launches() {
        let key = crate::feature_flags::DELTA_REREVIEW_EXPERIMENT;
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        let (batch_id, executions) =
            review_batch_with_arm(&db, false, Some(boss_protocol::DeltaRereviewBatchArm::Control));
        let supervisor = attach_supervisor(&db, &batch_id);

        // Flip 1: ab_split turns off before any member launches — live flags would draw Treatment.
        flags.set("review_batch_fanout", true).unwrap();
        flags.set(TOKEN_EXPERIMENTS_FLAG, true).unwrap();
        flags.set(key, true).unwrap();
        flags
            .set(crate::feature_flags::DELTA_REREVIEW_AB_SPLIT_FLAG, false)
            .unwrap();
        for exec in [&executions[0], &executions[1], &supervisor] {
            let arms = resolve_launch_experiment_arms(&flags, &db, exec);
            assert_eq!(
                arms.get(key),
                Some(ExperimentArm::Control),
                "ab_split off must not flip a control-admitted batch"
            );
        }

        // Flip 2: experiment turns off completely before any member launches — live flags would omit the key.
        flags.set(key, false).unwrap();
        for exec in [&executions[0], &executions[1], &supervisor] {
            let arms = resolve_launch_experiment_arms(&flags, &db, exec);
            assert_eq!(
                arms.get(key),
                Some(ExperimentArm::Control),
                "turning the flag off must not erase a control-admitted batch's arm"
            );
        }
    }

    #[test]
    fn a_not_enrolled_full_batch_records_no_arm_for_later_members_after_flags_turn_on() {
        let key = crate::feature_flags::DELTA_REREVIEW_EXPERIMENT;
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        let (batch_id, executions) = review_batch_with_arm(&db, false, None);
        let supervisor = attach_supervisor(&db, &batch_id);

        // Leaf 1 launches while the experiment is off and records no `delta_rereview` key.
        let leaf1_arms = resolve_launch_experiment_arms(&flags, &db, &executions[0]);
        assert_eq!(leaf1_arms.get(key), None);
        launch(&db, &executions[0], &leaf1_arms);

        // Now turn `delta_rereview` on with split off (live flags would draw Treatment).
        flags.set("review_batch_fanout", true).unwrap();
        flags.set(TOKEN_EXPERIMENTS_FLAG, true).unwrap();
        flags.set(key, true).unwrap();
        flags
            .set(crate::feature_flags::DELTA_REREVIEW_AB_SPLIT_FLAG, false)
            .unwrap();

        // Leaf 2 and the supervisor must still record no `delta_rereview` key.
        assert_eq!(
            resolve_launch_experiment_arms(&flags, &db, &executions[1]).get(key),
            None
        );
        assert_eq!(resolve_launch_experiment_arms(&flags, &db, &supervisor).get(key), None);
    }

    #[test]
    fn a_recorded_solo_execution_that_restarts_keeps_treatment_while_other_experiments_stop() {
        let delta_key = crate::feature_flags::DELTA_REREVIEW_EXPERIMENT;
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        let (_, executions) = review_batch_with_arm(&db, true, Some(boss_protocol::DeltaRereviewBatchArm::Treatment));
        let solo = &executions[0];

        // First launch records both `delta_rereview` and an unrelated experiment.
        let mut first = ExperimentArmSet::new();
        first.insert(delta_key, ExperimentArm::Treatment);
        first.insert(KEY, ExperimentArm::Treatment);
        launch(&db, solo, &first);

        // Both experiments are switched off before the solo execution restarts.
        flags.set(delta_key, false).unwrap();
        flags.set(KEY, false).unwrap();
        let restarted = resolve_launch_experiment_arms(&flags, &db, solo);

        // The solo batch still runs the delta treatment, so attribution stays `treatment`,
        // while the unrelated experiment transitions to `stopped` as usual.
        assert_eq!(restarted.get(delta_key), Some(ExperimentArm::Treatment));
        assert_eq!(restarted.get(KEY), Some(ExperimentArm::Stopped));
    }

    #[test]
    fn a_recorded_control_execution_that_restarts_after_kill_switch_keeps_control() {
        let delta_key = crate::feature_flags::DELTA_REREVIEW_EXPERIMENT;
        let (_db_dir, db) = open_db();
        let (_flags_dir, flags) = flags();
        let (_, executions) = review_batch_with_arm(&db, false, Some(boss_protocol::DeltaRereviewBatchArm::Control));
        let mut first = ExperimentArmSet::new();
        first.insert(delta_key, ExperimentArm::Control);
        launch(&db, &executions[0], &first);

        flags.set(TOKEN_EXPERIMENTS_FLAG, false).unwrap();
        let restarted = resolve_launch_experiment_arms(&flags, &db, &executions[0]);
        assert_eq!(restarted.get(delta_key), Some(ExperimentArm::Control));
    }
}
