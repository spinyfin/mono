//! The `investigation_routing` treatment: a Claude design or investigation run
//! one step down, resolved per effort level.

use super::*;

fn resolve(kind: &TaskKind, reasoning: ReasoningMode, effort: Option<EffortLevel>, treated: bool) -> SpawnConfig {
    resolve_spawn_config(
        &SpawnResolutionInput::builder()
            .kind(kind)
            .reasoning(reasoning)
            .maybe_effort_level(effort)
            .investigation_routing(treated)
            .build(),
    )
    .unwrap()
}

/// Both populations the treatment can reach: a design (tier by kind) and an
/// investigation (tier by reasoning).
fn tiered_runs() -> [(TaskKind, ReasoningMode); 2] {
    [
        (TaskKind::Design, ReasoningMode::Standard),
        (TaskKind::Chore, ReasoningMode::Investigation),
    ]
}

#[test]
fn control_launches_on_the_design_model_at_the_levels_own_effort() {
    for (kind, reasoning) in tiered_runs() {
        for (level, effort) in [
            (EffortLevel::Trivial, "low"),
            (EffortLevel::Small, "medium"),
            (EffortLevel::Medium, "high"),
            (EffortLevel::Large, "xhigh"),
            (EffortLevel::Max, "max"),
        ] {
            let config = resolve(&kind, reasoning, Some(level), false);
            assert_eq!(config.model, "fable", "{kind:?} {level:?}");
            assert_eq!(config.model_source, ModelResolutionSource::DesignInvestigationTier);
            assert_eq!(config.effort_value, Some(effort), "{kind:?} {level:?}");
        }
    }
}

#[test]
fn treated_large_launches_at_high_on_the_design_model() {
    for (kind, reasoning) in tiered_runs() {
        let config = resolve(&kind, reasoning, Some(EffortLevel::Large), true);
        assert_eq!(config.effort_value, Some("high"), "{kind:?}");
        assert_eq!(config.model, "fable", "{kind:?}: only the effort moves");
        assert_eq!(config.model_source, ModelResolutionSource::DesignInvestigationTier);
        assert_eq!(
            config.effort_level,
            Some(EffortLevel::Large),
            "the row's level is unchanged"
        );
    }
}

#[test]
fn treated_small_and_medium_launch_on_opus_at_the_levels_own_effort() {
    for (kind, reasoning) in tiered_runs() {
        for (level, effort) in [(EffortLevel::Small, "medium"), (EffortLevel::Medium, "high")] {
            let config = resolve(&kind, reasoning, Some(level), true);
            assert_eq!(config.model, "opus", "{kind:?} {level:?}");
            assert_eq!(config.model_source, ModelResolutionSource::InvestigationRouting);
            assert_eq!(
                config.effort_value,
                Some(effort),
                "{kind:?} {level:?}: only the model moves"
            );
        }
    }
}

#[test]
fn treated_trivial_max_and_unlevelled_runs_are_not_routed() {
    for (kind, reasoning) in tiered_runs() {
        for (level, effort) in [
            (Some(EffortLevel::Trivial), Some("low")),
            (Some(EffortLevel::Max), Some("max")),
            (None, None),
        ] {
            let treated = resolve(&kind, reasoning, level, true);
            let control = resolve(&kind, reasoning, level, false);
            assert_eq!(treated, control, "{kind:?} {level:?}");
            assert_eq!(treated.model, "fable");
            assert_eq!(treated.effort_value, effort);
        }
    }
}

#[test]
fn the_explicit_design_xhigh_setting_outranks_the_effort_step() {
    let resolve_design = |treated| {
        resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .kind(&TaskKind::Design)
                .effort_level(EffortLevel::Large)
                .design_reasoning_effort_xhigh(true)
                .investigation_routing(treated)
                .build(),
        )
        .unwrap()
    };
    assert_eq!(resolve_design(true), resolve_design(false));
    assert_eq!(resolve_design(true).effort_value, Some("xhigh"));
}

#[test]
fn a_model_pin_or_pool_tier_is_never_routed() {
    for level in [EffortLevel::Small, EffortLevel::Medium, EffortLevel::Large] {
        let pinned = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .kind(&TaskKind::Design)
                .effort_level(level)
                .model_override("sonnet")
                .investigation_routing(true)
                .build(),
        )
        .unwrap();
        assert_eq!(pinned.model, "sonnet", "{level:?}");
        assert_eq!(pinned.model_source, ModelResolutionSource::TaskOverride);

        // A first-round Deep reviewer is a Strong-tier pool run: Opus either way.
        let pooled = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .kind(&TaskKind::Design)
                .reasoning(ReasoningMode::Investigation)
                .effort_level(level)
                .pool_model_override(PoolModelTier::Strong)
                .investigation_routing(true)
                .build(),
        )
        .unwrap();
        assert_eq!(pooled.model, "opus", "{level:?}");
        assert_eq!(pooled.model_source, ModelResolutionSource::PoolStrongTier);
        assert_eq!(
            pooled.effort_value,
            menu_effort(level),
            "{level:?}: a pool run keeps the level's own effort",
        );
    }
}

fn menu_effort(level: EffortLevel) -> Option<&'static str> {
    boss_engine_driver::DriverRegistry::default()
        .get("claude")
        .and_then(|driver| (driver.descriptor().model_menu.effort_value_for_level)(level))
}

#[test]
fn ordinary_runs_are_never_routed() {
    for level in [EffortLevel::Small, EffortLevel::Medium, EffortLevel::Large] {
        let treated = resolve(&TaskKind::Chore, ReasoningMode::Standard, Some(level), true);
        let control = resolve(&TaskKind::Chore, ReasoningMode::Standard, Some(level), false);
        assert_eq!(treated, control, "{level:?}");
        assert_eq!(treated.model, "sonnet");
    }
    // The post-mortem is structurally excluded from the tier, so from the treatment.
    let postmortem = resolve(
        &TaskKind::DesignPostmortem,
        ReasoningMode::Investigation,
        Some(EffortLevel::Medium),
        true,
    );
    assert_ne!(postmortem.model_source, ModelResolutionSource::InvestigationRouting);
}

#[test]
fn other_drivers_are_never_routed() {
    for driver in ["codex", "agy"] {
        for level in [EffortLevel::Small, EffortLevel::Medium, EffortLevel::Large] {
            let resolve_on = |treated| {
                resolve_spawn_config(
                    &SpawnResolutionInput::builder()
                        .task_driver(driver)
                        .kind(&TaskKind::Design)
                        .effort_level(level)
                        .investigation_routing(treated)
                        .build(),
                )
                .unwrap()
            };
            assert_eq!(resolve_on(true), resolve_on(false), "{driver} {level:?}");
        }
    }
}

#[test]
fn ci_fix_revisions_are_excluded_from_investigation_routing() {
    let created_via = format!("{}attempt-1", boss_protocol::CREATED_VIA_CI_FIX_PREFIX);
    for level in [EffortLevel::Small, EffortLevel::Medium, EffortLevel::Large] {
        let resolve_ci_fix = |treated| {
            resolve_spawn_config(
                &SpawnResolutionInput::builder()
                    .execution_kind(&ExecutionKind::RevisionImplementation)
                    .created_via(created_via.as_str())
                    .kind(&TaskKind::Revision)
                    .reasoning(ReasoningMode::Investigation)
                    .effort_level(level)
                    .investigation_routing(treated)
                    .build(),
            )
            .unwrap()
        };
        let treated = resolve_ci_fix(true);
        let control = resolve_ci_fix(false);
        assert_eq!(treated, control, "{level:?}");
        assert_eq!(treated.model, "fable", "{level:?}");
        assert_eq!(treated.model_source, ModelResolutionSource::DesignInvestigationTier);
    }
}
