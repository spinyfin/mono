//! Private spawn-composition tests, including controlled first-prompt measurements.

use super::*;
use crate::work::Task;
use crate::worker_setup::{WorkerSetupInput, render_claude_md, worker_kind_for_execution};
use boss_protocol::{ExecutionStatus, TaskStatus};

#[tokio::test]
async fn first_prompt_sizes_by_driver_and_kind() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("MODULE.bazel"), "").unwrap();
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    for driver_name in ["claude", "codex"] {
        for kind in [
            ExecutionKind::ChoreImplementation,
            ExecutionKind::RevisionImplementation,
            ExecutionKind::PrReview,
        ] {
            let execution = WorkExecution::builder()
                .id("exec_measure_01")
                .work_item_id("task-measure")
                .kind(kind.clone())
                .status(ExecutionStatus::Running)
                .repo_remote_url("git@github.com:org/repo.git")
                .workspace_path(workspace.path().to_string_lossy())
                .created_at("2026-05-15T00:00:00Z")
                .build();
            let task = Task::builder()
                .id("task-measure")
                .product_id("prod-measure")
                .kind(TaskKind::Chore)
                .name("Widget import")
                .description("Implement widget import. Preserve existing imports and test malformed input.")
                .status(TaskStatus::Todo)
                .created_at("2026-05-15T00:00:00Z")
                .updated_at("2026-05-15T00:00:00Z")
                .driver(driver_name)
                .effort_level(EffortLevel::Large)
                .maybe_pr_url(
                    (kind != ExecutionKind::ChoreImplementation)
                        .then(|| "https://github.com/org/repo/pull/42".to_owned()),
                )
                .build();
            let composed = compose_worker_spawn(
                &db,
                "worker-measure",
                &execution,
                &WorkItem::Chore(task),
                workspace.path(),
                None,
                WorkerSpawnOpts::builder().run_done_proposals_seam_enabled(true).build(),
            )
            .await
            .unwrap();
            assert_eq!(composed.spawn_config.driver, driver_name);
            let registry = crate::driver::DriverRegistry::default();
            let driver = registry.require(driver_name).unwrap();
            let input = WorkerSetupInput::builder()
                .run_id(execution.id.clone())
                .lease_id("lease-measure")
                .workspace_path(std::path::PathBuf::from("/tmp/prompt-measure"))
                .events_socket_path(std::path::PathBuf::from("/tmp/events.sock"))
                .boss_event_path(std::path::PathBuf::from("/tmp/boss-event"))
                .execution_kind(kind.as_str())
                .worker_kind(worker_kind_for_execution(&kind))
                .run_done_proposals_seam_enabled(true)
                .build();
            let rules = render_claude_md(&input, driver.agent_rules_preamble(), driver.descriptor().config_dir);
            let prompt = composed
                .prompt_text
                .replace(workspace.path().to_str().unwrap(), "/tmp/prompt-measure");
            let prompt = match composed.embedded_output_path {
                Some(path) => prompt.replace(&path, "/tmp/prompt-measure/output.json"),
                None => prompt,
            };
            assert!(prompt.contains("Execution recovery bookmark"));
            if kind == ExecutionKind::PrReview {
                assert!(prompt.starts_with("# PR review"));
            }
            println!(
                "PROMPT_BYTES {} {driver_name} {} RULES_BYTES {}",
                kind.as_str(),
                prompt.len(),
                rules.len()
            );
        }
    }
}

#[cfg(test)]
mod reviewer_pool_policy_tests {
    //! Verifies review/automation dispatch policy is decoupled from the
    //! reviewed/automated row's own `driver` column — the single resolution
    //! point is `boss_engine_storage_values::pool_policy::pool_dispatch_policy_for_worker_id`,
    //! and `SpawnResolutionInput::pool_policy_driver` applies it ahead of
    //! row and product driver sources.
    #[test]
    fn reviewer_policy_is_a_distinct_driver_source() {
        let policy = boss_engine_storage_values::pool_policy::pool_dispatch_policy_for_worker_id("review-1")
            .expect("review worker has a policy");
        for row_driver in [Some("codex"), Some("grok"), Some("claude"), None] {
            let input = crate::effort::SpawnResolutionInput::builder()
                .maybe_task_driver(row_driver)
                .pool_policy_driver(policy.driver)
                .build();
            let cfg = crate::effort::resolve_spawn_config(&input).unwrap();
            assert_eq!(cfg.driver, "claude");
            assert_eq!(cfg.driver_source, crate::effort::DriverResolutionSource::PoolPolicy);
        }
    }

    /// Resolves a review-pool spawn end to end (policy → effective driver →
    /// `resolve_spawn_config`) for a row whose own `driver` column is
    /// `row_driver`, mirroring exactly what `compose_worker_spawn` does.
    fn resolve_reviewer_spawn(
        row_driver: Option<&str>,
        kind: Option<&boss_protocol::TaskKind>,
        reasoning: Option<boss_protocol::ReasoningMode>,
        effort: Option<boss_protocol::EffortLevel>,
        design_reasoning_effort_xhigh: bool,
    ) -> crate::effort::SpawnConfig {
        let pool_policy =
            boss_engine_storage_values::pool_policy::pool_dispatch_policy_for_worker_id("review-1").unwrap();
        let registry = crate::driver::DriverRegistry::default();
        let input = crate::effort::SpawnResolutionInput::builder()
            .maybe_task_driver(row_driver)
            .pool_policy_driver(pool_policy.driver)
            .maybe_pool_model_override(Some(pool_policy.model_tier))
            .maybe_kind(kind)
            .maybe_reasoning(reasoning)
            .maybe_effort_level(effort)
            .design_reasoning_effort_xhigh(design_reasoning_effort_xhigh)
            .build();
        crate::effort::resolve_spawn_config_in(&registry, &input).unwrap()
    }

    #[test]
    fn codex_authored_row_gets_a_claude_reviewer_on_opus_not_a_codex_reviewer() {
        let cfg = resolve_reviewer_spawn(Some("codex"), None, None, None, false);
        assert_eq!(
            cfg.driver, "claude",
            "reviewer must dispatch on Claude, not the authored row's codex driver"
        );
        assert_eq!(cfg.model, "opus");
    }

    #[test]
    fn claude_authored_row_under_review_is_unchanged_claude_reviewer_on_opus() {
        let cfg = resolve_reviewer_spawn(Some("claude"), None, None, None, false);
        assert_eq!(cfg.driver, "claude");
        assert_eq!(cfg.model, "opus");
    }

    #[test]
    fn untagged_row_under_review_still_gets_a_claude_reviewer_on_opus() {
        let cfg = resolve_reviewer_spawn(None, None, None, None, false);
        assert_eq!(cfg.driver, "claude");
        assert_eq!(cfg.model, "opus");
    }

    #[test]
    fn design_and_investigation_reviews_keep_the_pool_strong_tier_and_row_effort() {
        use boss_protocol::{EffortLevel, ReasoningMode, TaskKind};

        let design = resolve_reviewer_spawn(None, Some(&TaskKind::Design), None, Some(EffortLevel::Medium), true);
        assert_eq!(design.model, "opus");
        assert_eq!(
            design.model_source,
            crate::effort::ModelResolutionSource::PoolStrongTier
        );
        assert_eq!(design.effort_value, Some("high"));

        let investigation = resolve_reviewer_spawn(
            None,
            Some(&TaskKind::Chore),
            Some(ReasoningMode::Investigation),
            Some(EffortLevel::Large),
            false,
        );
        assert_eq!(investigation.model, "opus");
        assert_eq!(
            investigation.model_source,
            crate::effort::ModelResolutionSource::PoolStrongTier
        );
        assert_eq!(investigation.effort_value, Some("xhigh"));
    }

    #[test]
    fn persisted_batch_member_overrides_the_legacy_claude_review_pool() {
        let registry = crate::driver::DriverRegistry::default();
        let member = boss_protocol::ReviewBatchMember::builder()
            .id("member")
            .batch_id("batch")
            .attempt(1)
            .created_at("1")
            .provider_effort("medium")
            .requested_driver("codex")
            .resolved_model("gpt-5.6-terra")
            .role(boss_protocol::ReviewBatchMemberRole::CodexReviewer)
            .status(boss_protocol::ReviewBatchMemberStatus::Pending)
            .updated_at("1")
            .execution_id("execution")
            .build();

        let config = super::resolve_batch_reviewer_spawn(&registry, &member, None).unwrap();
        assert_eq!(config.driver, "codex");
        assert_eq!(config.model, "gpt-5.6-terra");
        assert_eq!(config.effort_level, Some(boss_protocol::EffortLevel::Medium));
        // Effort resolution still produces the medium addendum; the prepend
        // gate (not the resolver) is what keeps it off a reviewer prompt.
        assert!(
            config
                .prompt_addendum
                .unwrap()
                .starts_with("Sketch a brief plan before you start editing."),
            "batch reviewer spawn must still resolve the medium addendum",
        );
        assert_eq!(
            super::prompt_addendum_to_prepend(&boss_protocol::ExecutionKind::PrReview, config.prompt_addendum),
            None,
        );
    }
}

#[cfg(test)]
mod model_driver_gate_tests {
    //! Targeted tests for [`check_model_driver_compatibility`] — the
    //! spawn-time gate that fails closed when the resolved model does not
    //! belong to the resolved driver's `ModelMenu`. Exercises the same
    //! `DriverRegistry::default()` the production spawn path builds, so a
    //! driver's real vocabulary (not a stub) is what gets checked.
    use super::*;

    #[test]
    fn rejects_a_claude_alias_dispatched_on_the_codex_driver() {
        // The exact bug this gate exists to catch: the review/automation
        // pool override used to hand "opus" straight to whatever driver the
        // row resolved to, so a codex-driver reviewer 400'd before emitting
        // a token. The gate must refuse to dispatch that pair.
        let registry = crate::driver::DriverRegistry::default();
        let codex = registry.get("codex").expect("codex is registered");
        let err = check_model_driver_compatibility(codex.descriptor(), "opus").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("opus"), "error must name the model, got: {msg:?}");
        assert!(msg.contains("codex"), "error must name the driver, got: {msg:?}");
    }

    #[test]
    fn rejects_a_codex_model_dispatched_on_the_claude_driver() {
        let registry = crate::driver::DriverRegistry::default();
        let claude = registry.get("claude").expect("claude is registered");
        let err = check_model_driver_compatibility(claude.descriptor(), "gpt-6-astra").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gpt-6-astra"));
        assert!(msg.contains("claude"));
    }

    #[test]
    fn accepts_every_registered_drivers_own_engine_default() {
        let registry = crate::driver::DriverRegistry::default();
        for slug in registry.slugs() {
            let driver = registry.get(slug).expect("slug came from registry.slugs()");
            let default_model = driver.descriptor().model_menu.engine_default;
            check_model_driver_compatibility(driver.descriptor(), default_model).unwrap_or_else(|e| {
                panic!("driver {slug:?}'s own engine_default {default_model:?} must pass its own gate: {e}")
            });
        }
    }

    #[test]
    fn accepts_the_reviewer_pools_strong_tier_for_every_registered_driver() {
        // Mirrors what `compose_worker_spawn` actually resolves for a
        // review/automation-pool row: `PoolModelTier::Strong` through each
        // driver's own `model_for_reasoning(Investigation)`.
        let registry = crate::driver::DriverRegistry::default();
        for slug in registry.slugs() {
            let driver = registry.get(slug).expect("slug came from registry.slugs()");
            let strong_model =
                (driver.descriptor().model_menu.model_for_reasoning)(boss_protocol::ReasoningMode::Investigation);
            check_model_driver_compatibility(driver.descriptor(), strong_model).unwrap_or_else(|e| {
                panic!("driver {slug:?}'s own strong-tier model {strong_model:?} must pass its own gate: {e}")
            });
        }
    }
}

#[cfg(test)]
mod compose_worker_spawn_tests {
    //! Targeted tests for `compose_worker_spawn` covering the `pr_review`
    //! branch: branch selection (PrReview vs. other kinds), the no-pr-url
    //! fallback to the generic implementer prompt, and the URL-only reviewer
    //! prompt rendered when the PR metadata fetch fails.

    fn assert_bazel_caching_rule(prompt: &str) {
        assert_eq!(prompt.matches("Trust Bazel's caching").count(), 1);
        assert!(prompt.contains("--remote_accept_cached=false"));
    }

    use super::*;
    use crate::work::Task;
    use boss_protocol::{EffortLevel, ExecutionKind, ExecutionStatus, TaskKind, TaskStatus};
    use tempfile::TempDir;

    const MEDIUM_ADDENDUM: &str = "Sketch a brief plan before you start editing.";

    #[tokio::test]
    async fn revision_spawn_places_advisory_notice_after_opening_and_recovery_instructions() {
        use crate::test_support::{create_test_chore_manual, create_test_product_named};
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let product = create_test_product_named(&db, "revision notice");
        let task = create_test_chore_manual(&db, product.id, "Revision notice");
        db.connect().unwrap().execute(
            "UPDATE tasks SET pr_mergeable_state = 'CONFLICTING', pr_url = 'https://github.com/org/repo/pull/77' WHERE id = ?1",
            [&task.id],
        ).unwrap();
        let mut execution = chore_execution();
        execution.kind = ExecutionKind::RevisionImplementation;
        execution.work_item_id = task.id.clone();
        execution.pr_url = Some("https://github.com/org/repo/pull/77".into());
        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &db.get_work_item(&task.id).unwrap(),
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .unwrap();
        let prompt = composed.prompt_text;
        let opening = prompt.find("You are a reusable worker").unwrap();
        let recovery = prompt.find("## Execution recovery bookmark").unwrap();
        let notice = prompt.find("Conflict known at dispatch").unwrap();
        assert!(opening < recovery && recovery < notice, "{prompt}");
    }
    const LARGE_ADDENDUM_PREFIX: &str = "Begin with a written plan";

    fn pr_review_execution() -> WorkExecution {
        WorkExecution::builder()
            .id("exec_rev123_01")
            .work_item_id("task-pr-1")
            .kind(ExecutionKind::PrReview)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build()
    }

    fn chore_execution() -> WorkExecution {
        WorkExecution::builder()
            .id("exec_chore123_01")
            .work_item_id("task-chore-1")
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build()
    }

    fn task_without_pr(task_id: &str) -> WorkItem {
        WorkItem::Chore(
            Task::builder()
                .id(task_id)
                .product_id("prod-1")
                .kind(TaskKind::Chore)
                .name("Add a new feature")
                .description("Feature description.")
                .status(TaskStatus::Todo)
                .created_at("2026-05-15T00:00:00Z")
                .updated_at("2026-05-15T00:00:00Z")
                .autostart(false)
                .build(),
        )
    }

    fn task_with_pr(task_id: &str, pr_url: &str) -> WorkItem {
        match task_without_pr(task_id) {
            WorkItem::Chore(mut task) => {
                task.pr_url = Some(pr_url.into());
                WorkItem::Chore(task)
            }
            other => other,
        }
    }

    fn task_with_pr_and_effort(task_id: &str, pr_url: &str, effort: EffortLevel) -> WorkItem {
        match task_with_pr(task_id, pr_url) {
            WorkItem::Chore(mut task) => {
                task.effort_level = Some(effort);
                WorkItem::Chore(task)
            }
            other => other,
        }
    }

    fn chore_with_effort(task_id: &str, effort: EffortLevel) -> WorkItem {
        match task_without_pr(task_id) {
            WorkItem::Chore(mut task) => {
                task.effort_level = Some(effort);
                WorkItem::Chore(task)
            }
            other => other,
        }
    }

    fn prompt_carries_implementation_addendum(prompt: &str) -> bool {
        prompt.starts_with(MEDIUM_ADDENDUM) || prompt.starts_with(LARGE_ADDENDUM_PREFIX)
    }

    fn open_memory_db() -> WorkDb {
        WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap()
    }

    #[tokio::test]
    async fn derived_worker_receives_origin_backlink_and_body_composition_instruction() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = chore_execution();
        for origin in [None, Some(2685)] {
            let WorkItem::Chore(mut task) = task_without_pr("task-chore-1") else {
                unreachable!();
            };
            if origin.is_some() {
                task.kind = TaskKind::Followup;
                task.created_via = "pr_review:exec_source".into();
                task.origin_pr_number = origin;
            }
            let composed = compose_worker_spawn(
                &db,
                "worker-1",
                &execution,
                &WorkItem::Chore(task),
                workspace.path(),
                None,
                WorkerSpawnOpts::default(),
            )
            .await
            .unwrap();
            assert_bazel_caching_rule(&composed.prompt_text);
            let prompt = composed.prompt_text;
            assert_eq!(prompt.contains("## Origin PR backlink"), origin.is_some(), "{prompt}");
            if origin.is_some() {
                assert!(
                    prompt.contains("When writing this run's PR body, include the following backlink"),
                    "{prompt}"
                );
                assert!(prompt.contains("body file you pass to cube"), "{prompt}");
                assert!(prompt.contains("This `review findings` follow-up derives from [the origin PR](https://github.com/org/repo/pull/2685)."), "{prompt}");
            }
        }
    }

    /// A follow-up minted from a `PostMerge` review batch carries a durable
    /// `pr_review:post_merge:` created_via sub-prefix (see
    /// `review_verdict_apply.rs`), which must reach the worker-facing
    /// `## Origin PR backlink` block the worker is told to paste verbatim
    /// into its PR body — not just the separate free-text provenance
    /// paragraph in the task description.
    #[tokio::test]
    async fn post_merge_followup_gets_a_post_merge_specific_origin_backlink() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = chore_execution();
        let WorkItem::Chore(mut task) = task_without_pr("task-chore-1") else {
            unreachable!();
        };
        task.kind = TaskKind::Followup;
        task.created_via = "pr_review:post_merge:exec_source".into();
        task.origin_pr_number = Some(2685);

        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &WorkItem::Chore(task),
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .unwrap();
        assert_bazel_caching_rule(&composed.prompt_text);
        let prompt = composed.prompt_text;
        assert!(
            prompt.contains("This `post-merge review findings` follow-up derives from [the origin PR](https://github.com/org/repo/pull/2685)."),
            "{prompt}"
        );
    }

    /// A restricted-kind execution (here `PrReview`) never writes a PR body,
    /// so the origin-PR backlink instruction must not be appended even when
    /// the work item's static provenance (a `Followup` task with
    /// `origin_pr_number` set) would otherwise produce one for a Standard
    /// worker. This guards the `worker_kind_for_execution` gate on
    /// `origin_pr_backlink` in `compose_worker_spawn`.
    #[tokio::test]
    async fn restricted_kind_execution_never_receives_origin_backlink() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let mut execution = pr_review_execution();
        execution.repo_remote_url = "git@github.com:org/repo.git".into();
        let WorkItem::Chore(mut task) = task_with_pr("task-pr-1", "https://github.com/org/repo/pull/99") else {
            unreachable!();
        };
        task.kind = TaskKind::Followup;
        task.created_via = "pr_review:exec_source".into();
        task.origin_pr_number = Some(2685);

        let composed = compose_worker_spawn(
            &db,
            "review-1",
            &execution,
            &WorkItem::Chore(task),
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .unwrap();
        assert_bazel_caching_rule(&composed.prompt_text);

        assert!(
            !composed.prompt_text.contains("## Origin PR backlink"),
            "restricted-kind execution must not receive the origin PR backlink instruction:\n{}",
            composed.prompt_text,
        );
    }

    #[tokio::test]
    async fn derived_worker_still_refuses_missing_or_invalid_origin_provenance() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        for (origin, remote, expected) in [
            (None, "git@github.com:org/repo.git", "has no origin PR number"),
            (Some(0), "git@github.com:org/repo.git", "invalid origin PR number"),
            (
                Some(42),
                "git@example.com:org/repo.git",
                "could not resolve origin PR URL",
            ),
        ] {
            let mut execution = chore_execution();
            execution.repo_remote_url = remote.into();
            let WorkItem::Chore(mut task) = task_without_pr("task-chore-1") else {
                unreachable!();
            };
            task.kind = TaskKind::Followup;
            task.origin_pr_number = origin;
            let error = compose_worker_spawn(
                &db,
                "worker-1",
                &execution,
                &WorkItem::Chore(task),
                workspace.path(),
                None,
                WorkerSpawnOpts::default(),
            )
            .await
            .err()
            .expect("invalid provenance must refuse dispatch");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    /// The provenance hard-fail in `followup_pr_backlink_for_work_item` must
    /// run for every execution kind, not just `WorkerKind::Standard`: a
    /// `Followup` work item with missing or invalid origin provenance is a
    /// dispatch error regardless of which kind of worker it is dispatched
    /// as. This guards against re-introducing the regression where gating
    /// the whole `followup_pr_backlink_for_work_item` call on worker kind
    /// (rather than gating only whether its result is appended to the
    /// prompt) let a restricted-kind execution (`PrReview` here) dispatch
    /// successfully against a `Followup` work item with no origin PR number.
    #[tokio::test]
    async fn restricted_kind_execution_still_refuses_missing_or_invalid_origin_provenance() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        for (origin, remote, expected) in [
            (None, "git@github.com:org/repo.git", "has no origin PR number"),
            (Some(0), "git@github.com:org/repo.git", "invalid origin PR number"),
            (
                Some(42),
                "git@example.com:org/repo.git",
                "could not resolve origin PR URL",
            ),
        ] {
            let mut execution = pr_review_execution();
            execution.repo_remote_url = remote.into();
            let WorkItem::Chore(mut task) = task_with_pr("task-pr-1", "https://github.com/org/repo/pull/99") else {
                unreachable!();
            };
            task.kind = TaskKind::Followup;
            task.origin_pr_number = origin;
            let error = compose_worker_spawn(
                &db,
                "review-1",
                &execution,
                &WorkItem::Chore(task),
                workspace.path(),
                None,
                WorkerSpawnOpts::default(),
            )
            .await
            .err()
            .expect("invalid provenance must refuse dispatch even for a restricted-kind execution");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    /// When a `pr_review` execution's producing task has no `pr_url`, the
    /// branch falls back to the generic implementer prompt rather than
    /// rendering a reviewer prompt with no target PR.
    #[tokio::test]
    async fn pr_review_no_pr_url_falls_back_to_generic_prompt() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = pr_review_execution();
        let work_item = task_without_pr("task-pr-1");

        let composed = compose_worker_spawn(
            &db,
            "review-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            WorkerSpawnOpts {
                run_done_proposals_seam_enabled: true,
                ..WorkerSpawnOpts::default()
            },
        )
        .await
        .unwrap();
        assert_bazel_caching_rule(&composed.prompt_text);

        assert!(
            !composed.prompt_text.contains("# PR review"),
            "pr_review with no pr_url must not render the reviewer prompt:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains("exec_rev123_01"),
            "fallback generic prompt must contain the execution id:\n{}",
            composed.prompt_text,
        );
        assert_eq!(composed.embedded_output_path, None);
        assert!(
            !composed
                .prompt_text
                .contains("Follow the question-writing contract in the generated worker rules."),
            "pr_review fallback must not point at a Standard-only rules section:\n{}",
            composed.prompt_text,
        );
        assert!(
            !composed.prompt_text.contains("--question"),
            "pr_review fallback must not teach question flags it cannot file:\n{}",
            composed.prompt_text,
        );
        let input = crate::worker_setup::WorkerSetupInput::builder()
            .run_id("review-1")
            .execution_kind(execution.kind.as_str())
            .lease_id("lease-1")
            .workspace_path(workspace.path().to_path_buf())
            .events_socket_path(workspace.path().join("events.sock"))
            .boss_event_path(workspace.path().join("boss-event"))
            .worker_kind(crate::worker_setup::worker_kind_for_execution(&execution.kind))
            .run_done_proposals_seam_enabled(true)
            .build();
        let rules = crate::worker_setup::render_claude_md(&input, "", ".claude");
        assert!(!rules.contains("## Write questions for someone arriving cold"));
    }

    /// A missing automation row degrades an automation-triage execution to
    /// the generic prompt, which must not advertise the triage artifact path.
    #[tokio::test]
    async fn automation_triage_without_automation_row_omits_embedded_output_path() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = WorkExecution::builder()
            .id("exec_triage123_01")
            .work_item_id("automation-missing")
            .kind(ExecutionKind::AutomationTriage)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build();

        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &task_without_pr("task-triage-1"),
            workspace.path(),
            None,
            WorkerSpawnOpts {
                run_done_proposals_seam_enabled: true,
                ..WorkerSpawnOpts::default()
            },
        )
        .await
        .unwrap();
        assert_bazel_caching_rule(&composed.prompt_text);

        let triage_path =
            crate::structured_output::default_path_string(&execution.id, StructuredOutputKind::TriageDecision);
        assert_eq!(composed.embedded_output_path, None);
        assert!(!composed.prompt_text.contains(&triage_path));
        assert!(
            !composed
                .prompt_text
                .contains("Follow the question-writing contract in the generated worker rules."),
            "triage fallback must not point at a Standard-only rules section:\n{}",
            composed.prompt_text,
        );
        let input = crate::worker_setup::WorkerSetupInput::builder()
            .run_id("worker-1")
            .execution_kind(execution.kind.as_str())
            .lease_id("lease-1")
            .workspace_path(workspace.path().to_path_buf())
            .events_socket_path(workspace.path().join("events.sock"))
            .boss_event_path(workspace.path().join("boss-event"))
            .worker_kind(crate::worker_setup::worker_kind_for_execution(&execution.kind))
            .run_done_proposals_seam_enabled(true)
            .build();
        let rules = crate::worker_setup::render_claude_md(&input, "", ".claude");
        assert!(!rules.contains("## Write questions for someone arriving cold"));
    }

    /// When a `pr_review` execution has a `pr_url`, `compose_worker_spawn`
    /// calls `render_reviewer_initial_prompt` even when the upstream
    /// `fetch_pr_review_context` fails (no real `gh` in tests) — the
    /// URL-only reviewer prompt is still correctly formatted.
    #[tokio::test]
    async fn pr_review_with_pr_url_renders_reviewer_prompt() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = pr_review_execution();
        let pr_url = "https://github.com/org/repo/pull/42";
        let work_item = task_with_pr("task-pr-1", pr_url);

        let composed = compose_worker_spawn(
            &db,
            "review-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .unwrap();
        assert_bazel_caching_rule(&composed.prompt_text);

        assert!(
            composed.prompt_text.contains("# PR review"),
            "pr_review with pr_url must render the reviewer prompt header:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains("independent PR reviewer"),
            "reviewer prompt must identify the agent role:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains(pr_url),
            "reviewer prompt must include the PR URL:\n{}",
            composed.prompt_text,
        );
        let path = composed
            .embedded_output_path
            .expect("reviewer prompt must embed its output path");
        assert!(composed.prompt_text.contains(&path));
        assert!(
            composed.prompt_text.contains("Feature description."),
            "reviewer prompt must embed the work-item brief:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains("Brief conformance — CRITICAL"),
            "reviewer prompt must require the brief-conformance check:\n{}",
            composed.prompt_text,
        );
    }

    fn batch_classification() -> boss_protocol::ReviewClassification {
        boss_protocol::ReviewClassification::builder()
            .changed_files(vec!["tools/boss/engine/pr-review/src/brief.rs".to_owned()])
            .complexity_flags(vec![])
            .has_production_code(true)
            .metadata_missing(vec![])
            .production_languages(vec![boss_protocol::ReviewLanguageBucket::Rust])
            .profile(boss_protocol::ReviewProfile::Light)
            .subsystem_buckets(vec!["tools/boss/engine".to_owned()])
            .additions(12)
            .deletions(3)
            .build()
    }

    /// The pre-merge batch path creates reviewer executions on the batch's
    /// `cycle_root_id` (the chain root a revision's review-cycle accounting
    /// collapses to — see `pr_transition::review_batch_input_from_metadata`),
    /// not on the revision itself. `compose_worker_spawn` resolves the
    /// packet from the batch's persisted `producing_work_item_id`, so a
    /// batch created from a completed revision still renders the revision
    /// ask and the chain-root brief.
    #[tokio::test]
    async fn batch_reviewer_prompt_for_a_revision_includes_revision_ask_and_chain_root_brief() {
        use crate::test_support::{create_test_product, insert_host_capability};
        use crate::work::{FakePrStateChecker, PrOpenState};
        use boss_protocol::{CreateChoreInput, CreateRevisionInput, ReviewBatchPhase, WorkItemPatch};

        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        insert_host_capability(&db, "local", "driver=claude", "auto");
        let product = create_test_product(&db);
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Run durable Astra-high guide jobs")
                    .description("Implement a revision-aware broker.")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let pr_url = "https://github.com/spinyfin/mono/pull/3010";
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(chore.id.clone())
                    .description("Restore the broker the first pass inlined away.")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();

        let input = crate::work::ReviewBatchCreateInput::builder()
            .cycle_root_id(chore.id.clone())
            .base_sha("base-sha")
            .classification(batch_classification())
            .phase(ReviewBatchPhase::PreMerge)
            .pr_number(3010)
            .pr_url(pr_url)
            .target_sha("target-sha")
            .legacy_task_id(revision.id.clone())
            .build();
        let dispatch = db
            .create_pre_merge_review_batch(input, "git@github.com:spinyfin/mono.git")
            .unwrap();
        let executions = match dispatch {
            crate::work::ReviewBatchDispatch::Created { executions, .. } => executions,
            other => panic!("expected a freshly created batch, got {other:?}"),
        };
        let leaf_execution = &executions[0];
        assert_eq!(
            leaf_execution.work_item_id, chore.id,
            "leaf executions run on the cycle root"
        );

        let root_work_item = db.get_work_item(&chore.id).unwrap();
        let composed = compose_worker_spawn(
            &db,
            "review-1",
            leaf_execution,
            &root_work_item,
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .unwrap();
        assert_bazel_caching_rule(&composed.prompt_text);

        assert!(
            composed
                .prompt_text
                .contains("Restore the broker the first pass inlined away."),
            "batch leaf reviewer prompt must include the revision ask, not just the chain root's \
             own description:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains("Implement a revision-aware broker."),
            "batch leaf reviewer prompt must still include the chain-root brief:\n{}",
            composed.prompt_text,
        );
        assert!(composed.prompt_text.contains("Revision ask"));
        assert!(composed.prompt_text.contains("Chain-root brief"));

        // The other two leaves, and the consolidating supervisor once it is
        // dispatched, are members of the same batch and go through the same
        // `member_and_batch` → `producing_work_item_id` resolution in
        // `compose_worker_spawn` before rendering diverges by role.
        for other_leaf in &executions[1..] {
            let composed_other = compose_worker_spawn(
                &db,
                "review-1",
                other_leaf,
                &root_work_item,
                workspace.path(),
                None,
                WorkerSpawnOpts::default(),
            )
            .await
            .unwrap();
            assert!(
                composed_other
                    .prompt_text
                    .contains("Restore the broker the first pass inlined away."),
                "every leaf in the batch must resolve the same revision brief:\n{}",
                composed_other.prompt_text,
            );
        }
    }

    /// When the batch's persisted `producing_work_item_id` cannot be loaded,
    /// the cycle-root brief is still assembled as partial context and the
    /// missing revision ask is recorded as an unresolved brief input.
    #[tokio::test]
    async fn batch_reviewer_prompt_records_unresolved_when_producing_item_cannot_be_loaded() {
        use crate::test_support::{create_test_product, insert_host_capability};
        use crate::work::{FakePrStateChecker, PrOpenState};
        use boss_protocol::{CreateChoreInput, CreateRevisionInput, ReviewBatchPhase, WorkItemPatch};

        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        insert_host_capability(&db, "local", "driver=claude", "auto");
        let product = create_test_product(&db);
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Run durable Astra-high guide jobs")
                    .description("Implement a revision-aware broker.")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let pr_url = "https://github.com/spinyfin/mono/pull/3013";
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(chore.id.clone())
                    .description("Restore the broker the first pass inlined away.")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();

        let input = crate::work::ReviewBatchCreateInput::builder()
            .cycle_root_id(chore.id.clone())
            .base_sha("base-sha")
            .classification(batch_classification())
            .phase(ReviewBatchPhase::PreMerge)
            .pr_number(3013)
            .pr_url(pr_url)
            .target_sha("target-sha")
            .legacy_task_id(revision.id.clone())
            .build();
        let dispatch = db
            .create_pre_merge_review_batch(input, "git@github.com:spinyfin/mono.git")
            .unwrap();
        let executions = match dispatch {
            crate::work::ReviewBatchDispatch::Created { executions, .. } => executions,
            other => panic!("expected a freshly created batch, got {other:?}"),
        };
        let leaf_execution = &executions[0];
        assert_eq!(
            leaf_execution.work_item_id, chore.id,
            "leaf executions run on the cycle root"
        );

        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET deleted_at = '2026-01-01T00:00:00Z' WHERE id = ?1",
                rusqlite::params![revision.id],
            )
            .unwrap();

        let root_work_item = db.get_work_item(&chore.id).unwrap();
        let composed = compose_worker_spawn(
            &db,
            "review-1",
            leaf_execution,
            &root_work_item,
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .unwrap();
        assert_bazel_caching_rule(&composed.prompt_text);

        assert!(
            composed.prompt_text.contains("Implement a revision-aware broker."),
            "cycle-root brief must still be present as partial context:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains("Unresolved review inputs"),
            "missing producing item must render as an unresolved-input requirement:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains(&format!(
                "revision ask for producing work item {} could not be loaded",
                revision.id
            )),
            "unresolved reason must name the producing work item:\n{}",
            composed.prompt_text,
        );
        assert!(
            !composed
                .prompt_text
                .contains("Restore the broker the first pass inlined away."),
            "deleted producing item must not silently supply a revision ask:\n{}",
            composed.prompt_text,
        );
    }

    /// A non-`pr_review` execution kind (e.g. `ChoreImplementation`) must not
    /// enter the `pr_review` branch at all and must produce the generic
    /// implementer prompt.
    #[tokio::test]
    async fn non_pr_review_execution_routes_to_generic_prompt() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = chore_execution();
        let work_item = task_without_pr("task-chore-1");

        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .unwrap();
        assert_bazel_caching_rule(&composed.prompt_text);

        assert!(
            !composed.prompt_text.contains("# PR review"),
            "non-pr_review execution must not render the reviewer prompt:\n{}",
            composed.prompt_text,
        );
        assert!(
            !composed.prompt_text.contains("independent PR reviewer"),
            "non-pr_review execution must not contain reviewer role text:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains("exec_chore123_01"),
            "generic prompt must contain the execution id:\n{}",
            composed.prompt_text,
        );
    }

    /// The reviewer prompt must not include implementer-only directives like
    /// "expected branch name" — reviewers must not commit or push anything.
    #[tokio::test]
    async fn pr_review_prompt_omits_branch_push_directives() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = pr_review_execution();
        let pr_url = "https://github.com/org/repo/pull/99";
        let work_item = task_with_pr("task-pr-1", pr_url);

        let composed = compose_worker_spawn(
            &db,
            "review-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .unwrap();
        assert_bazel_caching_rule(&composed.prompt_text);

        assert!(
            !composed.prompt_text.contains("expected branch name"),
            "reviewer prompt must not include the expected branch name directive:\n{}",
            composed.prompt_text,
        );
    }

    /// The effort addendum is implementation-only. Restricted kinds
    /// (reviewer / triage / answer-agent) must never receive it, even
    /// when spawn-config resolution still populates the field from the
    /// effort level. Exhaustive over WorkerKind via
    /// `worker_kind_for_execution`.
    #[test]
    fn prompt_addendum_is_prepended_only_for_implementing_kinds() {
        let addendum = Some(MEDIUM_ADDENDUM);
        for kind in [
            ExecutionKind::PrReview,
            ExecutionKind::AutomationTriage,
            ExecutionKind::AnswerAgent,
        ] {
            assert_eq!(
                prompt_addendum_to_prepend(&kind, addendum),
                None,
                "{kind:?} must not receive the implementation addendum",
            );
        }
        for kind in [
            ExecutionKind::ChoreImplementation,
            ExecutionKind::CiRemediation,
            ExecutionKind::ConflictResolution,
            ExecutionKind::InvestigationImplementation,
            ExecutionKind::ProductDesign,
            ExecutionKind::ProjectDesign,
            ExecutionKind::RevisionImplementation,
            ExecutionKind::TaskImplementation,
        ] {
            assert_eq!(
                prompt_addendum_to_prepend(&kind, addendum),
                addendum,
                "{kind:?} is an implementing kind and must keep the addendum",
            );
        }
        assert_eq!(
            prompt_addendum_to_prepend(&ExecutionKind::PrReview, None),
            None,
            "None stays None even for restricted kinds",
        );
    }

    /// A reviewer-kind spawn must not receive the effort addendum at any
    /// effort level. Medium/Large/Max still *resolve* an addendum (effort
    /// mapping is unchanged); it just must not be prepended. After the
    /// gate, the rendered prompt starts with the reviewer header.
    #[tokio::test]
    async fn pr_review_spawn_omits_effort_addendum_at_every_level() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = pr_review_execution();
        let pr_url = "https://github.com/org/repo/pull/42";

        for level in EffortLevel::ALL {
            let work_item = task_with_pr_and_effort("task-pr-1", pr_url, *level);
            let composed = compose_worker_spawn(
                &db,
                "review-1",
                &execution,
                &work_item,
                workspace.path(),
                None,
                WorkerSpawnOpts::default(),
            )
            .await
            .unwrap();
            assert_bazel_caching_rule(&composed.prompt_text);

            match level {
                EffortLevel::Trivial | EffortLevel::Small => {
                    assert_eq!(
                        composed.spawn_config.prompt_addendum, None,
                        "{level:?}: trivial/small resolve no addendum",
                    );
                }
                EffortLevel::Medium | EffortLevel::Large | EffortLevel::Max => {
                    assert!(
                        composed.spawn_config.prompt_addendum.is_some(),
                        "{level:?}: effort resolution must still populate prompt_addendum",
                    );
                }
            }
            assert!(
                composed.prompt_text.starts_with("# PR review"),
                "{level:?}: reviewer prompt must start with the review header, got first line: {:?}",
                composed.prompt_text.lines().next(),
            );
            assert!(
                !prompt_carries_implementation_addendum(&composed.prompt_text),
                "{level:?}: reviewer prompt must not prepend the implementation addendum:\n{}",
                composed.prompt_text,
            );
            assert!(
                !composed.prompt_text.contains(MEDIUM_ADDENDUM),
                "{level:?}: reviewer prompt must not contain the medium addendum",
            );
            assert!(
                !composed.prompt_text.contains(LARGE_ADDENDUM_PREFIX),
                "{level:?}: reviewer prompt must not contain the large addendum",
            );
        }
    }

    /// An implementation worker at the same effort levels still receives
    /// the addendum — the gate is kind-keyed, not a deletion of the
    /// addendum itself.
    #[tokio::test]
    async fn implementation_spawn_still_prepends_effort_addendum() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = chore_execution();

        for (level, expected_prefix) in [
            (EffortLevel::Medium, MEDIUM_ADDENDUM),
            (EffortLevel::Large, LARGE_ADDENDUM_PREFIX),
            (EffortLevel::Max, LARGE_ADDENDUM_PREFIX),
        ] {
            let work_item = chore_with_effort("task-chore-1", level);
            let composed = compose_worker_spawn(
                &db,
                "worker-1",
                &execution,
                &work_item,
                workspace.path(),
                None,
                WorkerSpawnOpts::default(),
            )
            .await
            .unwrap();
            assert_bazel_caching_rule(&composed.prompt_text);

            assert!(
                composed.spawn_config.prompt_addendum.is_some(),
                "{level:?}: implementation spawn must still resolve an addendum",
            );
            assert!(
                composed.prompt_text.starts_with(expected_prefix),
                "{level:?}: implementation prompt must start with the effort addendum, got first line: {:?}",
                composed.prompt_text.lines().next(),
            );
        }
    }

    /// Triage and answer-agent sit in the same non-editing bucket as
    /// reviewers. Even when the work-item row carries a medium/large
    /// effort (so resolution populates `prompt_addendum`), the prepend
    /// must not land.
    #[tokio::test]
    async fn triage_and_answer_agent_spawns_omit_effort_addendum() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let work_item = chore_with_effort("task-restricted-1", EffortLevel::Large);

        for (kind, worker_id, exec_id, work_item_id) in [
            (
                ExecutionKind::AutomationTriage,
                "worker-1",
                "exec_triage_addendum_01",
                "automation-missing",
            ),
            (
                ExecutionKind::AnswerAgent,
                "worker-1",
                "exec_answer_addendum_01",
                "comment-missing",
            ),
        ] {
            let execution = WorkExecution::builder()
                .id(exec_id)
                .work_item_id(work_item_id)
                .kind(kind.clone())
                .status(ExecutionStatus::Running)
                .repo_remote_url("git@github.com:org/repo.git")
                .workspace_path("/tmp/workspace")
                .created_at("2026-05-15T00:00:00Z")
                .build();
            let composed = compose_worker_spawn(
                &db,
                worker_id,
                &execution,
                &work_item,
                workspace.path(),
                None,
                WorkerSpawnOpts::default(),
            )
            .await
            .unwrap();
            assert_bazel_caching_rule(&composed.prompt_text);

            assert!(
                composed.spawn_config.prompt_addendum.is_some(),
                "{kind:?}: resolution still populates the addendum from the row's large effort",
            );
            assert!(
                !prompt_carries_implementation_addendum(&composed.prompt_text),
                "{kind:?}: must not prepend the implementation addendum; first line: {:?}",
                composed.prompt_text.lines().next(),
            );
        }
    }

    /// A `Product`-scoped execution picks up the product's
    /// `dispatch_preamble` but not its `default_model` / `default_driver` /
    /// `editorial_rules`, which stay scoped to Task/Chore executions.
    #[tokio::test]
    async fn product_scoped_execution_gets_preamble_but_not_default_model_or_driver() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let product = db
            .create_product(
                crate::work::CreateProductInput::builder()
                    .name("Widget Co")
                    .repo_remote_url("git@github.com:org/widget.git")
                    .build(),
            )
            .unwrap();
        let patch = crate::work::WorkItemPatch::builder()
            .default_model("sonnet")
            .default_driver("codex")
            .dispatch_preamble("house style: terse commit messages")
            .build();
        db.update_product(&product.id, patch, "human").unwrap();
        let product = db.get_product(&product.id).unwrap().expect("product exists");
        assert_eq!(product.default_model.as_deref(), Some("sonnet"));

        let execution = WorkExecution::builder()
            .id("exec_prod123_01")
            .work_item_id(product.id.clone())
            .kind(ExecutionKind::ProductDesign)
            .status(boss_protocol::ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/widget.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build();
        let work_item = WorkItem::Product(product);

        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .unwrap();
        assert_bazel_caching_rule(&composed.prompt_text);

        assert!(
            composed.prompt_text.contains("house style: terse commit messages"),
            "product-scoped execution must still receive dispatch_preamble:\n{}",
            composed.prompt_text,
        );
        assert_ne!(
            composed.spawn_config.model, "sonnet",
            "product's default_model must not apply to a Product-scoped execution"
        );
        assert_ne!(
            composed.spawn_config.driver, "codex",
            "product's default_driver must not apply to a Product-scoped execution"
        );
    }

    fn conflict_resolution_execution(work_item_id: &str) -> WorkExecution {
        WorkExecution::builder()
            .id("exec_conflict_01")
            .work_item_id(work_item_id)
            .kind(ExecutionKind::ConflictResolution)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build()
    }

    fn chore_with_driver(task_id: &str, driver: Option<&str>, product_id: &str) -> WorkItem {
        let mut task = Task::builder()
            .id(task_id)
            .product_id(product_id)
            .kind(TaskKind::Chore)
            .name("Resolve merge conflict")
            .description("Conflict-resolution chore.")
            .status(TaskStatus::Todo)
            .created_at("2026-05-15T00:00:00Z")
            .updated_at("2026-05-15T00:00:00Z")
            .autostart(false)
            .build();
        task.driver = driver.map(str::to_owned);
        WorkItem::Chore(task)
    }

    fn design_with_product(task_id: &str, product_id: &str) -> WorkItem {
        WorkItem::Task(
            Task::builder()
                .id(task_id)
                .product_id(product_id)
                .kind(TaskKind::Design)
                .name("Design a new feature")
                .description("Design task.")
                .status(TaskStatus::Todo)
                .created_at("2026-05-15T00:00:00Z")
                .updated_at("2026-05-15T00:00:00Z")
                .autostart(false)
                .build(),
        )
    }

    #[tokio::test]
    async fn design_with_grok_product_default_spawns_on_engine_default() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let product = db
            .create_product(
                crate::work::CreateProductInput::builder()
                    .name("Grok Product")
                    .repo_remote_url("git@github.com:org/grok-product.git")
                    .build(),
            )
            .unwrap();
        db.update_product(
            &product.id,
            crate::work::WorkItemPatch::builder().default_driver("grok").build(),
            "human",
        )
        .unwrap();
        let execution = WorkExecution::builder()
            .id("exec_design_grok_01")
            .work_item_id("task-design-grok")
            .kind(ExecutionKind::TaskImplementation)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/grok-product.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build();

        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &design_with_product("task-design-grok", &product.id),
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .expect("grok product default must yield rather than wedge a design spawn");

        assert_eq!(composed.spawn_config.driver, crate::effort::ENGINE_DEFAULT_DRIVER);
    }

    /// A codex task pin must not hard-fail ConflictResolution spawn: the pin
    /// yields to the engine default (claude), which clears the capability gate.
    #[tokio::test]
    async fn conflict_resolution_with_codex_task_pin_spawns_on_claude() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = conflict_resolution_execution("task-conflict-1");
        let work_item = chore_with_driver("task-conflict-1", Some("codex"), "prod-1");

        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .expect("codex pin must yield, not fail the capability gate");

        assert_eq!(
            composed.spawn_config.driver, "claude",
            "ConflictResolution must spawn on claude when the codex pin fails the gate",
        );
    }

    /// Same yield via `products.default_driver = codex` — the documented way
    /// to run a whole product on a non-default driver.
    #[tokio::test]
    async fn conflict_resolution_with_codex_product_pin_spawns_on_claude() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let product = db
            .create_product(
                crate::work::CreateProductInput::builder()
                    .name("Codex Product")
                    .repo_remote_url("git@github.com:org/codex-product.git")
                    .build(),
            )
            .unwrap();
        db.update_product(
            &product.id,
            crate::work::WorkItemPatch::builder().default_driver("codex").build(),
            "human",
        )
        .unwrap();

        let execution = conflict_resolution_execution("task-conflict-prod");
        let work_item = chore_with_driver("task-conflict-prod", None, &product.id);

        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .expect("product codex pin must yield, not fail the capability gate");

        assert_eq!(
            composed.spawn_config.driver, "claude",
            "ConflictResolution must spawn on claude when the product codex pin fails the gate",
        );
    }

    /// CiRemediation shares the CommandOutcomeObservation gate with
    /// ConflictResolution — a codex pin must yield there too.
    #[tokio::test]
    async fn ci_remediation_with_codex_task_pin_spawns_on_claude() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = WorkExecution::builder()
            .id("exec_ci_01")
            .work_item_id("task-ci-1")
            .kind(ExecutionKind::CiRemediation)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build();
        let work_item = chore_with_driver("task-ci-1", Some("codex"), "prod-1");

        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .expect("codex pin must yield for CiRemediation");

        assert_eq!(composed.spawn_config.driver, "claude");
    }

    #[tokio::test]
    async fn review_guide_prompt_omits_jj_recovery_bookmark_instructions() {
        let workspace = TempDir::new().unwrap();
        let (_db_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_product(&db);
        let root = crate::test_support::create_active_chore(&db, &product, "Review changes");
        let packet = boss_pr_review_sources::SourcePacket {
            schema_version: 3,
            canonical_pr_url: "https://github.com/org/repo/pull/42".into(),
            pr_number: 42,
            title: "Review changes".into(),
            body: None,
            base_repository: "org/repo".into(),
            head_repository: "org/repo".into(),
            observed_base_sha: "base".into(),
            probe_base_sha: None,
            merge_base_sha: "merge-base".into(),
            head_sha: "head".into(),
            files: vec![],
            omissions: vec![],
        };
        let crate::work::PrSourceCapturePersistOutcome::Stored(capture) = db
            .persist_pr_review_guide_source_capture(&root, 1, crate::work::PrSourceCaptureTrigger::Creation, &packet)
            .unwrap()
        else {
            panic!("capture must persist")
        };
        let git_dir = workspace.path().join(".git").display().to_string();
        let metadata = boss_review_guide::PromptMetadata::builder()
            .pr_url(&packet.canonical_pr_url)
            .repository(&packet.base_repository)
            .pr_title(&packet.title)
            .base_sha(&packet.merge_base_sha)
            .head_sha(&packet.head_sha)
            .git_dir(&git_dir)
            .build();
        let template = boss_review_guide::render_prompt(&metadata);
        let execution = WorkExecution::builder()
            .id("exec_guide_01")
            .work_item_id(&capture.comparison_id)
            .kind(ExecutionKind::PrReviewGuide)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build();
        let work_item = task_without_pr("cmp_missing");
        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .expect("review-guide spawn must compose");
        assert_bazel_caching_rule(&composed.prompt_text);
        assert!(composed.prompt_text.starts_with(&template));
        assert!(
            !composed.prompt_text.contains("jj bookmark set"),
            "review-guide workers cannot run jj bookmark commands: {}",
            composed.prompt_text
        );
        assert!(
            !composed.prompt_text.contains("## Execution recovery bookmark"),
            "{}",
            composed.prompt_text
        );
    }

    /// Unit coverage for the yield helper itself: a pin that clears the gate
    /// is preserved; one that does not is dropped.
    #[test]
    fn yield_pins_helper_drops_codex_for_conflict_resolution_keeps_it_for_chore() {
        let (task, product) = yield_pins_that_fail_capability_gate(
            "exec_x",
            &ExecutionKind::ConflictResolution,
            Some(&TaskKind::Chore),
            Some("codex"),
            Some("codex"),
        );
        assert_eq!(task, None);
        assert_eq!(product, None);

        let (task, product) = yield_pins_that_fail_capability_gate(
            "exec_y",
            &ExecutionKind::ChoreImplementation,
            Some(&TaskKind::Chore),
            Some("codex"),
            Some("grok"),
        );
        assert_eq!(task, Some("codex"));
        // Task pin is present, so product pin is left as-is for the resolver
        // (which will not consult it).
        assert_eq!(product, Some("grok"));
    }

    #[tokio::test]
    async fn restore_report_reaches_the_initial_prompt_only_when_recorded() {
        use boss_engine_recovery::execution_bookmark::RestoreReport;
        let workspace = TempDir::new().unwrap();
        let db = WorkDb::open(workspace.path().join("boss.db")).unwrap();
        let product = crate::test_support::create_test_product(&db);
        let chore = crate::test_support::create_test_chore_manual(&db, product.id, "Resume me");
        let execution = db
            .create_execution(
                crate::work::CreateExecutionInput::builder()
                    .work_item_id(&chore.id)
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url("git@github.com:org/repo.git")
                    .build(),
            )
            .unwrap();
        let item = WorkItem::Chore(chore);
        let compose = || async {
            compose_worker_spawn(
                &db,
                "worker-1",
                &execution,
                &item,
                workspace.path(),
                None,
                WorkerSpawnOpts::default(),
            )
            .await
            .unwrap()
            .prompt_text
        };

        let without = compose().await;
        assert!(!without.contains("## EXECUTION BOOKMARK RECOVERY"), "{without}");

        db.record_execution_bookmark(&boss_engine_recovery::execution_bookmark::ExecutionBookmark {
            execution_id: execution.id.clone(),
            repo_path: workspace.path().to_path_buf(),
            host_id: "local".into(),
            base_commit: None,
        })
        .unwrap();
        db.record_bookmark_recovery(&execution.id, "exec_prior", false).unwrap();

        for (conflicts, first_task) in [("", false), ("Commit abc:\nbase.txt\n", true)] {
            db.record_execution_restore_report(
                &execution.id,
                &RestoreReport::builder()
                    .pointer("boss-recovery/exec_prior + pr/99")
                    .commits("abc1234 Preserved work\n")
                    .base_sha("0123456789abcdef")
                    .conflicts(conflicts)
                    .pr_bound(true)
                    .build(),
            )
            .unwrap();
            let prompt = compose().await;
            assert_eq!(prompt.matches("## EXECUTION BOOKMARK RECOVERY").count(), 1, "{prompt}");
            assert!(!prompt.contains("positioned this lease normally"));
            assert!(prompt.contains("boss-recovery/exec_prior + pr/99"), "{prompt}");
            assert!(prompt.contains("0123456789abcdef"), "{prompt}");
            assert!(prompt.contains("abc1234 Preserved work"), "{prompt}");
            assert_eq!(prompt.contains("FIRST TASK"), first_task, "{prompt}");
        }
    }
}
