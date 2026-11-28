//! The experiment arms a caller hands `compose_worker_spawn` reach both the
//! prompt (as a plain option) and `SpawnConfig` (as the set the dispatch
//! site records), and the default is "nothing was running".

use super::*;
use crate::feature_flags::{BATCHED_SEARCH_EXPERIMENT, ExperimentArm, ExperimentArmSet};
use crate::work::Task;
use boss_protocol::{ExecutionStatus, TaskStatus};

const RULE: &str = "**Batch independent tool calls.**";

async fn compose_with(arms: Option<ExperimentArmSet>, workspace: &Path) -> ComposedWorkerSpawn {
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    let execution = WorkExecution::builder()
        .id("exec_arms_01")
        .work_item_id("task-arms")
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Running)
        .repo_remote_url("git@github.com:org/repo.git")
        .workspace_path(workspace.to_string_lossy())
        .created_at("2026-05-15T00:00:00Z")
        .build();
    let task = Task::builder()
        .id("task-arms")
        .product_id("prod-arms")
        .kind(TaskKind::Chore)
        .name("Widget import")
        .description("Implement widget import.")
        .status(TaskStatus::Todo)
        .created_at("2026-05-15T00:00:00Z")
        .updated_at("2026-05-15T00:00:00Z")
        .build();
    let opts = WorkerSpawnOpts::builder().review_signal_checks(|_| &[]);
    let opts = match arms {
        Some(arms) => opts.experiment_arms(arms).build(),
        None => opts.build(),
    };
    compose_worker_spawn(
        &db,
        "worker-arms",
        &execution,
        &WorkItem::Chore(task),
        workspace,
        None,
        opts,
    )
    .await
    .unwrap()
}

fn set(arm: ExperimentArm) -> ExperimentArmSet {
    let mut arms = ExperimentArmSet::new();
    arms.insert(BATCHED_SEARCH_EXPERIMENT, arm);
    arms
}

/// Compose a design or investigation task at `effort` under `arm` of the
/// `investigation_routing` experiment, and return what it launches with.
async fn compose_tiered(
    kind: TaskKind,
    reasoning: Option<boss_protocol::ReasoningMode>,
    effort: boss_protocol::EffortLevel,
    arm: Option<ExperimentArm>,
    workspace: &Path,
) -> crate::effort::SpawnConfig {
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    let execution = WorkExecution::builder()
        .id("exec_route_01")
        .work_item_id("task-route")
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Running)
        .repo_remote_url("git@github.com:org/repo.git")
        .workspace_path(workspace.to_string_lossy())
        .created_at("2026-05-15T00:00:00Z")
        .build();
    let task = Task::builder()
        .id("task-route")
        .product_id("prod-route")
        .kind(kind)
        .name("Investigate")
        .description("Investigate the thing.")
        .status(TaskStatus::Todo)
        .effort_level(effort)
        .maybe_reasoning(reasoning)
        .created_at("2026-05-15T00:00:00Z")
        .updated_at("2026-05-15T00:00:00Z")
        .build();
    let mut arms = ExperimentArmSet::new();
    if let Some(arm) = arm {
        arms.insert(crate::feature_flags::INVESTIGATION_ROUTING_EXPERIMENT, arm);
    }
    compose_worker_spawn(
        &db,
        "worker-route",
        &execution,
        &WorkItem::Chore(task),
        workspace,
        None,
        WorkerSpawnOpts::builder()
            .review_signal_checks(|_| &[])
            .experiment_arms(arms)
            .build(),
    )
    .await
    .unwrap()
    .spawn_config
}

#[tokio::test]
async fn investigation_routing_reaches_the_launch_only_for_a_treated_family_at_each_level() {
    use boss_protocol::{EffortLevel, ReasoningMode};
    let workspace = tempfile::tempdir().unwrap();
    // (level, treated model, treated effort, control model, control effort)
    let cases = [
        (EffortLevel::Small, "opus", "medium", "fable", "medium"),
        (EffortLevel::Medium, "opus", "high", "fable", "high"),
        (EffortLevel::Large, "fable", "high", "fable", "xhigh"),
    ];
    for (level, treated_model, treated_effort, control_model, control_effort) in cases {
        let treated = compose_tiered(
            TaskKind::Chore,
            Some(ReasoningMode::Investigation),
            level,
            Some(ExperimentArm::Treatment),
            workspace.path(),
        )
        .await;
        assert_eq!(treated.model, treated_model, "{level:?}");
        assert_eq!(treated.effort_value, Some(treated_effort), "{level:?}");

        for arm in [None, Some(ExperimentArm::Control), Some(ExperimentArm::Stopped)] {
            let untreated = compose_tiered(
                TaskKind::Chore,
                Some(ReasoningMode::Investigation),
                level,
                arm,
                workspace.path(),
            )
            .await;
            assert_eq!(untreated.model, control_model, "{level:?} {arm:?}");
            assert_eq!(untreated.effort_value, Some(control_effort), "{level:?} {arm:?}");
        }
    }
}

#[tokio::test]
async fn investigation_routing_does_not_touch_an_ordinary_task() {
    use boss_protocol::{EffortLevel, ReasoningMode};
    let workspace = tempfile::tempdir().unwrap();
    let treated = compose_tiered(
        TaskKind::Chore,
        Some(ReasoningMode::Standard),
        EffortLevel::Large,
        Some(ExperimentArm::Treatment),
        workspace.path(),
    )
    .await;
    assert_eq!(treated.model, "sonnet");
    assert_eq!(treated.effort_value, Some("xhigh"));
}

#[tokio::test]
async fn a_caller_that_resolves_no_arms_records_an_empty_set_and_is_never_treated() {
    // The remote spawn path builds its options without arms.
    let workspace = tempfile::tempdir().unwrap();
    let composed = compose_with(None, workspace.path()).await;
    assert_eq!(composed.spawn_config.experiment_arms.as_deref(), Some("{}"));
    assert!(!composed.prompt_text.contains(RULE));
}

#[tokio::test]
async fn treatment_changes_the_brief_and_is_recorded() {
    let workspace = tempfile::tempdir().unwrap();
    let composed = compose_with(Some(set(ExperimentArm::Treatment)), workspace.path()).await;
    assert_eq!(composed.prompt_text.matches(RULE).count(), 1);
    assert_eq!(
        composed.spawn_config.experiment_arms.as_deref(),
        Some(r#"{"batched_search":"treatment"}"#)
    );
}

#[tokio::test]
async fn control_and_stopped_keep_todays_brief_and_are_recorded() {
    let workspace = tempfile::tempdir().unwrap();
    let untreated = compose_with(None, workspace.path()).await;
    for (arm, recorded) in [
        (ExperimentArm::Control, r#"{"batched_search":"control"}"#),
        (ExperimentArm::Stopped, r#"{"batched_search":"stopped"}"#),
    ] {
        let composed = compose_with(Some(set(arm)), workspace.path()).await;
        assert_eq!(composed.prompt_text, untreated.prompt_text, "{arm:?}");
        assert_eq!(
            composed.spawn_config.experiment_arms.as_deref(),
            Some(recorded),
            "{arm:?}"
        );
    }
}

async fn compose_ci_fix_revision(
    created_via: &str,
    reasoning: boss_protocol::ReasoningMode,
    effort: boss_protocol::EffortLevel,
    arms: ExperimentArmSet,
    workspace: &Path,
) -> crate::effort::SpawnConfig {
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    let execution = WorkExecution::builder()
        .id("exec_cifix_01")
        .work_item_id("rev-cifix")
        .kind(ExecutionKind::RevisionImplementation)
        .status(ExecutionStatus::Running)
        .repo_remote_url("git@github.com:org/repo.git")
        .workspace_path(workspace.to_string_lossy())
        .created_at("2026-05-15T00:00:00Z")
        .build();
    let task = Task::builder()
        .id("rev-cifix")
        .product_id("prod-cifix")
        .kind(TaskKind::Revision)
        .name("CI fix revision")
        .description("Diagnose queue eviction.")
        .status(TaskStatus::Todo)
        .created_via(created_via)
        .effort_level(effort)
        .reasoning(reasoning)
        .created_at("2026-05-15T00:00:00Z")
        .updated_at("2026-05-15T00:00:00Z")
        .build();
    compose_worker_spawn(
        &db,
        "worker-cifix",
        &execution,
        &WorkItem::Task(task),
        workspace,
        None,
        WorkerSpawnOpts::builder()
            .review_signal_checks(|_| &[])
            .experiment_arms(arms)
            .build(),
    )
    .await
    .unwrap()
    .spawn_config
}

/// A no-evidence CI-fix revision belongs exclusively to `ci_fix_standard_tier`.
/// `investigation_routing` must never reroute it in either CI-fix arm, while
/// ordinary design/investigation work is still routed.
#[tokio::test]
async fn investigation_routing_does_not_reroute_no_evidence_ci_fix_revisions() {
    use crate::feature_flags::{CI_FIX_STANDARD_TIER_EXPERIMENT, INVESTIGATION_ROUTING_EXPERIMENT};
    use boss_protocol::{CREATED_VIA_CI_FIX_PREFIX, EffortLevel, ReasoningMode};

    let workspace = tempfile::tempdir().unwrap();
    let created_via = format!("{CREATED_VIA_CI_FIX_PREFIX}attempt-1");
    for level in [EffortLevel::Small, EffortLevel::Medium, EffortLevel::Large] {
        for ci_arm in [
            None,
            Some(ExperimentArm::Control),
            Some(ExperimentArm::Treatment),
            Some(ExperimentArm::Stopped),
        ] {
            let mut treated_arms = ExperimentArmSet::new();
            let mut control_arms = ExperimentArmSet::new();
            if let Some(arm) = ci_arm {
                treated_arms.insert(CI_FIX_STANDARD_TIER_EXPERIMENT, arm);
                control_arms.insert(CI_FIX_STANDARD_TIER_EXPERIMENT, arm);
            }
            treated_arms.insert(INVESTIGATION_ROUTING_EXPERIMENT, ExperimentArm::Treatment);
            control_arms.insert(INVESTIGATION_ROUTING_EXPERIMENT, ExperimentArm::Control);

            let under_treated = compose_ci_fix_revision(
                &created_via,
                ReasoningMode::Investigation,
                level,
                treated_arms,
                workspace.path(),
            )
            .await;
            let under_control = compose_ci_fix_revision(
                &created_via,
                ReasoningMode::Investigation,
                level,
                control_arms,
                workspace.path(),
            )
            .await;

            assert_eq!(under_treated.model, under_control.model, "{level:?} {ci_arm:?}");
            assert_eq!(
                under_treated.effort_value, under_control.effort_value,
                "{level:?} {ci_arm:?}"
            );
            assert_eq!(
                under_treated.model_source, under_control.model_source,
                "{level:?} {ci_arm:?}"
            );
            if ci_arm == Some(ExperimentArm::Treatment) {
                assert_eq!(under_treated.model, "sonnet", "{level:?}");
                assert_eq!(under_treated.effort_value, Some("high"), "{level:?}");
            } else {
                assert_eq!(under_treated.model, "fable", "{level:?}");
                assert_eq!(
                    under_treated.model_source,
                    crate::effort::ModelResolutionSource::DesignInvestigationTier,
                    "{level:?} {ci_arm:?}"
                );
            }
        }
    }
    // Ordinary design/investigation work is still routed.
    let design = compose_tiered(
        TaskKind::Design,
        Some(ReasoningMode::Investigation),
        EffortLevel::Small,
        Some(ExperimentArm::Treatment),
        workspace.path(),
    )
    .await;
    assert_eq!(design.model, "opus");
}

fn mint_no_evidence_ci_fix(db: &WorkDb, workspace: &Path) -> (Task, WorkExecution) {
    use crate::test_support::{create_product, create_test_chore};
    use crate::work::CreateExecutionInput;
    use boss_protocol::{CREATED_VIA_CI_FIX_PREFIX, CreateRevisionInput, ReasoningMode};

    let product_id = create_product(db);
    let parent = create_test_chore(db, product_id, "parent pr");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/org/repo/pull/9' WHERE id = ?1",
            [&parent.id],
        )
        .unwrap();
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent.id.clone())
                .description("Diagnose queue eviction.")
                .created_via(format!("{CREATED_VIA_CI_FIX_PREFIX}att-1"))
                .reasoning(ReasoningMode::Investigation)
                .build(),
            &crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open),
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .repo_remote_url("git@github.com:org/repo.git")
                .workspace_path(workspace.to_string_lossy().into_owned())
                .build(),
        )
        .unwrap();
    (revision, execution)
}

/// Toggling `ci_fix_standard_tier` or its split between minting the revision
/// and its first launch cannot split the recorded arm from the tier that runs.
#[tokio::test]
async fn ci_fix_standard_tier_matches_recorded_arm_when_flags_toggle_before_first_launch() {
    use crate::feature_flags::{
        CI_FIX_STANDARD_TIER_AB_SPLIT_FLAG, CI_FIX_STANDARD_TIER_EXPERIMENT, FeatureFlagsStore, TREATMENT_BUCKET_LIMIT,
    };
    use crate::runner::experiment_arms::resolve_launch_experiment_arms;
    use boss_protocol::{EffortLevel, ReasoningMode};

    let workspace = tempfile::tempdir().unwrap();
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    let flags_dir = tempfile::tempdir().unwrap();
    let flags = FeatureFlagsStore::new(flags_dir.path().join("feature-flags.toml"));
    flags.load().unwrap();

    // Case 1: minted with key off, then key turned on (split off) before first launch.
    let (revision, execution) = mint_no_evidence_ci_fix(&db, workspace.path());
    flags.set(CI_FIX_STANDARD_TIER_EXPERIMENT, true).unwrap();
    flags.set(CI_FIX_STANDARD_TIER_AB_SPLIT_FLAG, false).unwrap();
    let arms = resolve_launch_experiment_arms(&flags, &db, &execution);
    assert_eq!(
        arms.get(CI_FIX_STANDARD_TIER_EXPERIMENT),
        Some(ExperimentArm::Treatment)
    );
    let composed = compose_worker_spawn(
        &db,
        "worker-1",
        &execution,
        &WorkItem::Task(revision),
        workspace.path(),
        None,
        WorkerSpawnOpts::builder()
            .review_signal_checks(|_| &[])
            .experiment_arms(arms)
            .build(),
    )
    .await
    .unwrap();
    assert_eq!(composed.spawn_config.model, "sonnet");
    assert_eq!(composed.spawn_config.reasoning, Some(ReasoningMode::Standard));
    assert_eq!(composed.spawn_config.effort_level, Some(EffortLevel::Medium));
    assert_eq!(composed.spawn_config.effort_value, Some("high"));

    // Case 2: minted with split off (all treated), then split turned on before first launch
    // for a family that hashes into control.
    let (control_rev, control_exec) = loop {
        let (rev, exec) = mint_no_evidence_ci_fix(&db, workspace.path());
        let family = db.experiment_family_root(&exec).unwrap();
        if crate::work::experiment_bucket(&family, CI_FIX_STANDARD_TIER_EXPERIMENT) >= TREATMENT_BUCKET_LIMIT {
            break (rev, exec);
        }
    };
    flags.set(CI_FIX_STANDARD_TIER_AB_SPLIT_FLAG, true).unwrap();
    let arms = resolve_launch_experiment_arms(&flags, &db, &control_exec);
    assert_eq!(arms.get(CI_FIX_STANDARD_TIER_EXPERIMENT), Some(ExperimentArm::Control));
    let composed = compose_worker_spawn(
        &db,
        "worker-1",
        &control_exec,
        &WorkItem::Task(control_rev),
        workspace.path(),
        None,
        WorkerSpawnOpts::builder()
            .review_signal_checks(|_| &[])
            .experiment_arms(arms)
            .build(),
    )
    .await
    .unwrap();
    assert_eq!(composed.spawn_config.model, "fable");
    assert_eq!(composed.spawn_config.reasoning, Some(ReasoningMode::Investigation));
}

/// A retry after turning off the key or master switch records `stopped` and
/// reverts to the revision's original investigation tier.
#[tokio::test]
async fn ci_fix_standard_tier_retry_after_stop_reverts_to_investigation_tier() {
    use crate::feature_flags::{
        CI_FIX_STANDARD_TIER_AB_SPLIT_FLAG, CI_FIX_STANDARD_TIER_EXPERIMENT, FeatureFlagsStore, TOKEN_EXPERIMENTS_FLAG,
    };
    use crate::runner::experiment_arms::resolve_launch_experiment_arms;
    use crate::work::ExecutionLaunchRecord;
    use boss_protocol::ReasoningMode;

    let workspace = tempfile::tempdir().unwrap();
    for stop_flag in [CI_FIX_STANDARD_TIER_EXPERIMENT, TOKEN_EXPERIMENTS_FLAG] {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let flags_dir = tempfile::tempdir().unwrap();
        let flags = FeatureFlagsStore::new(flags_dir.path().join("feature-flags.toml"));
        flags.load().unwrap();
        flags.set(CI_FIX_STANDARD_TIER_EXPERIMENT, true).unwrap();
        flags.set(CI_FIX_STANDARD_TIER_AB_SPLIT_FLAG, false).unwrap();

        let (revision, execution) = mint_no_evidence_ci_fix(&db, workspace.path());
        let first_arms = resolve_launch_experiment_arms(&flags, &db, &execution);
        assert_eq!(
            first_arms.get(CI_FIX_STANDARD_TIER_EXPERIMENT),
            Some(ExperimentArm::Treatment)
        );
        let first = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &WorkItem::Task(revision.clone()),
            workspace.path(),
            None,
            WorkerSpawnOpts::builder()
                .review_signal_checks(|_| &[])
                .experiment_arms(first_arms)
                .build(),
        )
        .await
        .unwrap();
        assert_eq!(first.spawn_config.model, "sonnet");
        db.record_execution_launch(&ExecutionLaunchRecord {
            execution_id: &execution.id,
            driver: &first.spawn_config.driver,
            model: &first.spawn_config.model,
            effort_level: first.spawn_config.effort_level,
            experiment_arms: first.spawn_config.experiment_arms.as_deref(),
        })
        .unwrap();

        flags.set(stop_flag, false).unwrap();
        let retry_arms = resolve_launch_experiment_arms(&flags, &db, &execution);
        assert_eq!(
            retry_arms.get(CI_FIX_STANDARD_TIER_EXPERIMENT),
            Some(ExperimentArm::Stopped),
            "stop_flag={stop_flag}"
        );
        let retry = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &WorkItem::Task(revision),
            workspace.path(),
            None,
            WorkerSpawnOpts::builder()
                .review_signal_checks(|_| &[])
                .experiment_arms(retry_arms)
                .build(),
        )
        .await
        .unwrap();
        assert_eq!(retry.spawn_config.model, "fable", "stop_flag={stop_flag}");
        assert_eq!(
            retry.spawn_config.reasoning,
            Some(ReasoningMode::Investigation),
            "stop_flag={stop_flag}"
        );
        assert_eq!(
            retry.spawn_config.experiment_arms.as_deref(),
            Some(r#"{"ci_fix_standard_tier":"stopped"}"#),
            "stop_flag={stop_flag}"
        );
    }
}

#[tokio::test]
async fn ci_fix_standard_tier_leaves_evidence_ci_fix_and_non_ci_fix_revisions_untouched() {
    use crate::feature_flags::CI_FIX_STANDARD_TIER_EXPERIMENT;
    use boss_protocol::{CREATED_VIA_CI_FIX_PREFIX, EffortLevel, ReasoningMode};

    let workspace = tempfile::tempdir().unwrap();
    let mut treated = ExperimentArmSet::new();
    treated.insert(CI_FIX_STANDARD_TIER_EXPERIMENT, ExperimentArm::Treatment);

    // CI-fix revision with attached evidence (already Standard) keeps its own effort.
    let with_evidence = compose_ci_fix_revision(
        &format!("{CREATED_VIA_CI_FIX_PREFIX}att-2"),
        ReasoningMode::Standard,
        EffortLevel::Small,
        treated.clone(),
        workspace.path(),
    )
    .await;
    assert_eq!(with_evidence.model, "sonnet");
    assert_eq!(with_evidence.effort_level, Some(EffortLevel::Small));
    assert_eq!(with_evidence.effort_value, Some("medium"));

    // Non-CI-fix revision with Investigation reasoning is untouched.
    let review_rev = compose_ci_fix_revision(
        "pr-review:batch-1",
        ReasoningMode::Investigation,
        EffortLevel::Small,
        treated,
        workspace.path(),
    )
    .await;
    assert_eq!(review_rev.model, "fable");
    assert_eq!(review_rev.reasoning, Some(ReasoningMode::Investigation));
}

/// End-to-end check that `SUPERVISOR_STANDARD_TIER_EXPERIMENT` reaches the
/// supervisor batch-member branch through `compose_worker_spawn`.
#[tokio::test]
async fn supervisor_standard_tier_reaches_batch_supervisor_launch_through_compose() {
    use crate::feature_flags::SUPERVISOR_STANDARD_TIER_EXPERIMENT;
    use boss_protocol::{
        ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase, ReviewClassification, ReviewProfile,
    };

    let workspace = tempfile::tempdir().unwrap();
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    let product = crate::test_support::create_test_product(&db);
    let task = crate::test_support::create_test_chore_manual(&db, product.id, "Review target");
    let execution = db
        .create_execution(
            crate::work::CreateExecutionInput::builder()
                .work_item_id(task.id.clone())
                .kind(ExecutionKind::PrReview)
                .repo_remote_url("git@github.com:org/repo.git")
                .workspace_path(workspace.path().to_string_lossy().into_owned())
                .build(),
        )
        .unwrap();
    db.create_review_batch(
        crate::work::ReviewBatchCreateInput::builder()
            .cycle_root_id(&task.id)
            .base_sha("base")
            .classification(
                ReviewClassification::builder()
                    .changed_files(vec![])
                    .complexity_flags(vec![])
                    .has_production_code(true)
                    .metadata_missing(vec![])
                    .production_languages(vec![])
                    .profile(ReviewProfile::Deep)
                    .subsystem_buckets(vec![])
                    .additions(100)
                    .deletions(10)
                    .build(),
            )
            .phase(ReviewBatchPhase::PreMerge)
            .pr_number(42)
            .pr_url("https://github.com/example/repo/pull/42")
            .target_sha("head")
            .build(),
        &[crate::work::ReviewBatchMemberCreateInput::builder()
            .attempt(1)
            .provider_effort("medium")
            .requested_driver("claude")
            .resolved_model("opus")
            .role(ReviewBatchMemberRole::Supervisor)
            .status(ReviewBatchMemberStatus::Pending)
            .execution_id(execution.id.clone())
            .build()],
    )
    .unwrap();
    let work_item = db.get_work_item(&task.id).unwrap();

    let compose_arm = |arm: Option<ExperimentArm>| {
        let db = &db;
        let execution = &execution;
        let work_item = &work_item;
        let workspace_path = workspace.path();
        async move {
            let mut arms = ExperimentArmSet::new();
            if let Some(arm) = arm {
                arms.insert(SUPERVISOR_STANDARD_TIER_EXPERIMENT, arm);
            }
            compose_worker_spawn(
                db,
                "review-worker-1",
                execution,
                work_item,
                workspace_path,
                None,
                WorkerSpawnOpts::builder()
                    .review_signal_checks(|_| &[])
                    .review_batch_fanout_enabled(true)
                    .experiment_arms(arms)
                    .build(),
            )
            .await
            .unwrap()
            .spawn_config
        }
    };

    let treated = compose_arm(Some(ExperimentArm::Treatment)).await;
    assert_eq!(treated.driver, "claude");
    assert_eq!(treated.model, "sonnet");
    assert_eq!(
        treated.experiment_arms.as_deref(),
        Some(r#"{"supervisor_standard_tier":"treatment"}"#)
    );

    for arm in [None, Some(ExperimentArm::Control), Some(ExperimentArm::Stopped)] {
        let untreated = compose_arm(arm).await;
        assert_eq!(untreated.driver, "claude", "{arm:?}");
        assert_eq!(untreated.model, "opus", "{arm:?}");
    }
}
