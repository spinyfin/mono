use super::*;

#[test]
fn agy_generic_pool_automation_keeps_hydrogen() {
    let cfg = resolve_spawn_config(
        &SpawnResolutionInput::builder()
            .task_driver("agy")
            .execution_kind(&ExecutionKind::ChoreImplementation)
            .pool_model_override(PoolModelTier::Strong)
            .reasoning(ReasoningMode::Investigation)
            .effort_level(EffortLevel::Large)
            .build(),
    )
    .unwrap();
    assert_eq!(cfg.model, "gemini-4-hydrogen-xhigh");
}

#[test]
fn agy_execution_roles_override_inherited_reasoning_and_pool() {
    for (kind, family) in [
        (ExecutionKind::Progression, "hydrogen"),
        (ExecutionKind::AutomationTriage, "hydrogen"),
        (ExecutionKind::AnswerAgent, "hydrogen"),
        (ExecutionKind::ConflictResolution, "hydrogen"),
        (ExecutionKind::CiRemediation, "hydrogen"),
        (ExecutionKind::PrReview, "neon"),
        (ExecutionKind::PrReviewGuide, "neon"),
        (ExecutionKind::ProductDesign, "carbon"),
        (ExecutionKind::ProjectDesign, "carbon"),
        (ExecutionKind::InvestigationImplementation, "carbon"),
    ] {
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .task_driver("agy")
                .execution_kind(&kind)
                .kind(&TaskKind::Design)
                .reasoning(ReasoningMode::Investigation)
                .pool_model_override(PoolModelTier::Strong)
                .effort_level(EffortLevel::Large)
                .build(),
        )
        .unwrap();
        let rung = if family == "carbon" { "high" } else { "xhigh" };
        assert_eq!(cfg.model, format!("gemini-4-{family}-{rung}"), "{kind}");
        assert_eq!(cfg.effort_value, Some(rung));
        assert_eq!(cfg.model_source, ModelResolutionSource::ExecutionKind);
    }
}

#[test]
fn agy_revision_provenance_distinguishes_mechanical_and_review_work() {
    for (prefix, family) in [
        (boss_protocol::CREATED_VIA_CI_FIX_PREFIX, "hydrogen"),
        (boss_protocol::CREATED_VIA_MERGE_CONFLICT_PREFIX, "hydrogen"),
        (boss_protocol::CREATED_VIA_PR_REVIEW_PREFIX, "neon"),
    ] {
        let origin = format!("{prefix}123");
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .task_driver("agy")
                .execution_kind(&ExecutionKind::RevisionImplementation)
                .created_via(&origin)
                .kind(&TaskKind::Revision)
                .reasoning(ReasoningMode::Standard)
                .effort_level(EffortLevel::Medium)
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, format!("gemini-4-{family}-high"));
    }
}

#[test]
fn agy_design_and_investigation_rows_and_default() {
    for kind in [TaskKind::Design, TaskKind::Investigation] {
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .task_driver("agy")
                .execution_kind(&ExecutionKind::TaskImplementation)
                .kind(&kind)
                .reasoning(ReasoningMode::Standard)
                .effort_level(EffortLevel::Small)
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "gemini-4-carbon-medium");
    }
    for reasoning in [None, Some(ReasoningMode::Standard)] {
        let cfg = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .task_driver("agy")
                .maybe_reasoning(reasoning)
                .build(),
        )
        .unwrap();
        assert_eq!(cfg.model, "gemini-4-neon-medium");
    }
}

#[test]
fn work_type_input_does_not_change_other_driver_policies() {
    for driver in ["claude", "codex", "grok"] {
        for kind in [
            ExecutionKind::Progression,
            ExecutionKind::PrReview,
            ExecutionKind::CiRemediation,
            ExecutionKind::ProjectDesign,
        ] {
            for reasoning in [ReasoningMode::Standard, ReasoningMode::Investigation] {
                let input = SpawnResolutionInput::builder()
                    .task_driver(driver)
                    .reasoning(reasoning)
                    .effort_level(EffortLevel::Medium)
                    .build();
                let baseline = resolve_spawn_config(&input).map(|cfg| (cfg.model, cfg.effort_value, cfg.model_source));
                let with_kind = resolve_spawn_config(&SpawnResolutionInput {
                    execution_kind: Some(&kind),
                    ..input
                })
                .map(|cfg| (cfg.model, cfg.effort_value, cfg.model_source));
                assert_eq!(with_kind, baseline, "{driver} {kind}");
            }
        }
    }
}

#[test]
fn human_revision_with_investigation_reasoning_keeps_carbon() {
    let cfg = resolve_spawn_config(
        &SpawnResolutionInput::builder()
            .task_driver("agy")
            .execution_kind(&ExecutionKind::RevisionImplementation)
            .created_via("human")
            .kind(&TaskKind::Revision)
            .reasoning(ReasoningMode::Investigation)
            .effort_level(EffortLevel::Small)
            .build(),
    )
    .unwrap();
    assert_eq!(cfg.model, "gemini-4-carbon-medium");
}
