use super::*;

use boss_protocol::{CreateProjectInput, CreateRevisionInput, CreateTaskInput, ExecutionKind, RequestExecutionInput};

use crate::test_support::{
    create_test_chore_manual, create_test_product, insert_host_capability, open_db, seed_daily_automation,
    seed_review_guide_series_for_pr,
};
use crate::work::{FakePrStateChecker, PrOpenState};

/// Request and start an execution of `work_item_id` on `host_id`, so it
/// carries a `started_at`.
fn started_execution(db: &WorkDb, work_item_id: &str, host_id: &str) -> WorkExecution {
    let execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
        .unwrap();
    let (execution, _run) = db
        .start_execution_run_on_host_with_tmux_hosting(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "ws-1",
            "/tmp/ws-1",
            host_id,
            host_id == "local",
        )
        .unwrap();
    execution
}

fn create_project(db: &WorkDb, product_id: &str, name: &str) -> boss_protocol::Project {
    db.create_project(
        CreateProjectInput::builder()
            .product_id(product_id)
            .name(name)
            .no_design_task(true)
            .autostart(false)
            .build(),
    )
    .unwrap()
}

fn create_project_task(db: &WorkDb, product_id: &str, project_id: &str, name: &str) -> boss_protocol::Task {
    insert_host_capability(db, "local", "driver=claude", "auto");
    db.create_task(
        CreateTaskInput::builder()
            .product_id(product_id)
            .project_id(project_id)
            .name(name)
            .autostart(false)
            .build(),
    )
    .unwrap()
}

/// The same execution, relabelled as if it were dispatched as `kind`.
fn as_kind(execution: &WorkExecution, kind: ExecutionKind) -> WorkExecution {
    WorkExecution {
        kind,
        ..execution.clone()
    }
}

#[test]
fn project_task_is_attributed_to_its_project() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let project = create_project(&db, &product.id, "Dynamic Agents");
    let task = create_project_task(&db, &product.id, &project.id, "stamp metadata");
    let execution = started_execution(&db, &task.id, "local");

    let metadata = resolve(&db, &execution, "local");

    assert_eq!(metadata.project_id.as_deref(), Some(project.id.as_str()));
    assert_eq!(metadata.project_name.as_deref(), Some("Dynamic Agents"));
}

#[test]
fn project_work_item_is_attributed_to_itself() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let project = create_project(&db, &product.id, "Whole project");
    let execution = as_kind(
        &started_execution(&db, &create_test_chore_manual(&db, product.id.clone(), "x").id, "local"),
        ExecutionKind::ProjectDesign,
    );
    let execution = WorkExecution {
        work_item_id: project.id.clone(),
        ..execution
    };

    let metadata = resolve(&db, &execution, "local");

    assert_eq!(metadata.project_id.as_deref(), Some(project.id.as_str()));
    assert_eq!(metadata.project_name.as_deref(), Some("Whole project"));
    assert_eq!(metadata.agent_type.as_deref(), Some("design"));
}

#[test]
fn unfiled_chore_has_no_project() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "loose chore");
    let execution = started_execution(&db, &chore.id, "local");

    let metadata = resolve(&db, &execution, "local");

    assert_eq!(metadata.project_id, None);
    assert_eq!(metadata.project_name, None);
    assert_eq!(metadata.product_id.as_deref(), Some(product.id.as_str()));
    assert_eq!(metadata.work_item_short_id, chore.short_id);
    assert_eq!(metadata.pr_url, None);
}

#[test]
fn chore_with_pr_url_stamps_header_fields() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "open a pr");
    let execution = started_execution(&db, &chore.id, "local");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?1 WHERE id = ?2",
            rusqlite::params!["https://github.com/spinyfin/mono/pull/3356", chore.id],
        )
        .unwrap();

    let metadata = resolve(&db, &execution, "local");

    assert_eq!(metadata.product_id.as_deref(), Some(product.id.as_str()));
    assert_eq!(metadata.work_item_short_id, chore.short_id);
    assert!(chore.short_id.is_some());
    assert_eq!(
        metadata.pr_url.as_deref(),
        Some("https://github.com/spinyfin/mono/pull/3356")
    );
}

/// Revision task rows are inserted with `pr_url` NULL; the bound PR is on
/// the execution. The Agents pane header still has to show that PR.
#[test]
fn revision_falls_back_to_execution_pr_url_when_task_row_has_none() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let parent = create_test_chore_manual(&db, product.id.clone(), "Parent chore");
    const PARENT_PR: &str = "https://github.com/spinyfin/mono/pull/3356";
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?1 WHERE id = ?2",
            rusqlite::params![PARENT_PR, parent.id],
        )
        .unwrap();
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent.id.clone())
                .description("Address review findings")
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    assert!(revision.pr_url.is_none());

    let started = started_execution(&db, &revision.id, "local");
    let with_pr = WorkExecution {
        kind: ExecutionKind::RevisionImplementation,
        pr_url: Some(PARENT_PR.to_owned()),
        ..started.clone()
    };
    let metadata = resolve(&db, &with_pr, "local");
    assert_eq!(metadata.product_id.as_deref(), Some(product.id.as_str()));
    assert_eq!(metadata.work_item_short_id, revision.short_id);
    assert_eq!(metadata.pr_url.as_deref(), Some(PARENT_PR));

    let without_pr = WorkExecution {
        kind: ExecutionKind::RevisionImplementation,
        pr_url: None,
        ..started
    };
    assert_eq!(resolve(&db, &without_pr, "local").pr_url, None);
}

fn set_task_pr(db: &WorkDb, task_id: &str, pr_url: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?1 WHERE id = ?2",
            rusqlite::params![pr_url, task_id],
        )
        .unwrap();
}

/// Every pane kind carries the product's display name next to the short id,
/// and the project (when there is one) and PR once one exists.
#[test]
fn every_pane_kind_carries_product_project_short_id_and_pr() {
    const PR: &str = "https://github.com/spinyfin/mono/pull/3401";
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let project = create_project(&db, &product.id, "Platform");
    let task = create_project_task(&db, &product.id, &project.id, "Header metadata");
    let started = started_execution(&db, &task.id, "local");

    // No PR yet: the product, project and short id are present, the PR is not.
    let metadata = resolve(&db, &as_kind(&started, ExecutionKind::TaskImplementation), "local");
    assert_eq!(metadata.product_name.as_deref(), Some(product.name.as_str()));
    assert_eq!(metadata.project_name.as_deref(), Some("Platform"));
    assert_eq!(metadata.work_item_short_id, task.short_id);
    assert_eq!(metadata.pr_url, None);

    set_task_pr(&db, &task.id, PR);
    for kind in [
        ExecutionKind::TaskImplementation,
        ExecutionKind::PrReview,
        ExecutionKind::CiRemediation,
        ExecutionKind::ConflictResolution,
    ] {
        let metadata = resolve(&db, &as_kind(&started, kind.clone()), "local");
        assert_eq!(metadata.product_name.as_deref(), Some(product.name.as_str()), "{kind}");
        assert_eq!(metadata.project_name.as_deref(), Some("Platform"), "{kind}");
        assert_eq!(metadata.work_item_short_id, task.short_id, "{kind}");
        assert_eq!(metadata.pr_url.as_deref(), Some(PR), "{kind}");
    }

    // A chore with no project leaves the project out.
    let chore = create_test_chore_manual(&db, product.id.clone(), "loose");
    let metadata = resolve(&db, &started_execution(&db, &chore.id, "local"), "local");
    assert_eq!(metadata.product_name.as_deref(), Some(product.name.as_str()));
    assert_eq!(metadata.project_name, None);
}

#[test]
fn revision_pane_carries_product_and_project_of_its_chain() {
    const PR: &str = "https://github.com/spinyfin/mono/pull/3401";
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let project = create_project(&db, &product.id, "Platform");
    let parent = create_project_task(&db, &product.id, &project.id, "Parent");
    set_task_pr(&db, &parent.id, PR);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent.id.clone())
                .description("Address review findings")
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    let execution = WorkExecution {
        kind: ExecutionKind::RevisionImplementation,
        pr_url: Some(PR.to_owned()),
        ..started_execution(&db, &revision.id, "local")
    };

    let metadata = resolve(&db, &execution, "local");

    assert_eq!(metadata.product_name.as_deref(), Some(product.name.as_str()));
    assert_eq!(metadata.project_name.as_deref(), Some("Platform"));
    assert_eq!(metadata.work_item_short_id, revision.short_id);
    assert_eq!(metadata.pr_url.as_deref(), Some(PR));
}

/// A review-guide run is bound to a comparison, not a task. Its header is the
/// series' root task (the chain root when the PR belongs to a revision chain)
/// and the comparison's canonical PR.
#[test]
fn review_guide_pane_resolves_to_the_root_task_and_canonical_pr() {
    const PR: &str = "https://github.com/spinyfin/mono/pull/3401";
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let project = create_project(&db, &product.id, "Platform");
    let root = create_project_task(&db, &product.id, &project.id, "Guided change");
    let started = started_execution(&db, &root.id, "local");
    set_task_pr(&db, &root.id, PR);
    // The PR belongs to a revision chain; the series is keyed on the root.
    db.create_revision(
        CreateRevisionInput::builder()
            .parent_task_id(root.id.clone())
            .description("Address review findings")
            .build(),
        &FakePrStateChecker::always(PrOpenState::Open),
    )
    .unwrap();
    let (_series, comparison) = seed_review_guide_series_for_pr(&db, &root.id, PR, "base", "head");
    let execution = WorkExecution {
        work_item_id: comparison,
        ..as_kind(&started, ExecutionKind::PrReviewGuide)
    };

    let metadata = resolve(&db, &execution, "local");

    assert_eq!(metadata.product_name.as_deref(), Some(product.name.as_str()));
    assert_eq!(metadata.product_id.as_deref(), Some(product.id.as_str()));
    assert_eq!(metadata.project_name.as_deref(), Some("Platform"));
    assert_eq!(metadata.owning_work_item_id.as_deref(), Some(root.id.as_str()));
    assert_eq!(metadata.owning_work_item_title.as_deref(), Some(root.name.as_str()));
    let registry = crate::live_worker_state::LiveWorkerStateRegistry::new();
    registry.register_spawn_with_capabilities(
        1,
        "run-guide",
        "model",
        0,
        Some(execution_binding(&db, &execution)),
        false,
        crate::live_worker_state::LiveSpawnRouting::none().with_metadata(metadata.clone()),
    );
    let state = registry.get(1).unwrap();
    assert_eq!(state.work_item_id.as_deref(), Some(execution.work_item_id.as_str()));
    assert_eq!(state.owning_work_item_id.as_deref(), Some(root.id.as_str()));
    assert_eq!(state.owning_work_item_title.as_deref(), Some(root.name.as_str()));
    assert_eq!(metadata.work_item_short_id, root.short_id);
    assert_eq!(metadata.pr_url.as_deref(), Some(PR));
}

/// A missing comparison has neither an owning task nor a canonical PR.
#[test]
fn review_guide_pane_with_a_missing_comparison_falls_back_cleanly() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "x");
    let execution = WorkExecution {
        work_item_id: "prgc_missing".to_owned(),
        ..as_kind(
            &started_execution(&db, &chore.id, "local"),
            ExecutionKind::PrReviewGuide,
        )
    };

    let metadata = resolve(&db, &execution, "local");

    assert_eq!(metadata.product_name, None);
    assert_eq!(metadata.project_name, None);
    assert_eq!(metadata.work_item_short_id, None);
    assert_eq!(metadata.pr_url, None);
}

#[test]
fn work_item_ids_that_are_not_tasks_are_unfiled_not_guessed() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let automation = seed_daily_automation(&db, &product.id);
    let chore = create_test_chore_manual(&db, product.id.clone(), "x");
    let base = started_execution(&db, &chore.id, "local");

    // Automation triage and review-guide executions point `work_item_id` at an
    // automation / comparison id, which is not a work item at all.
    let triage = WorkExecution {
        work_item_id: automation.id.clone(),
        ..as_kind(&base, ExecutionKind::AutomationTriage)
    };
    let guide = WorkExecution {
        work_item_id: "cmp_not_a_work_item".to_owned(),
        ..as_kind(&base, ExecutionKind::PrReviewGuide)
    };

    for execution in [triage, guide] {
        let metadata = resolve(&db, &execution, "local");
        assert_eq!(metadata.project_id, None, "{}", execution.kind);
        assert_eq!(metadata.project_name, None, "{}", execution.kind);
    }
}

#[test]
fn automation_sourced_project_task_keeps_its_project_but_is_typed_automation() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let project = create_project(&db, &product.id, "Maintenance");
    let task = create_project_task(&db, &product.id, &project.id, "automation produced");
    let automation = seed_daily_automation(&db, &product.id);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
            rusqlite::params![automation.id, task.id],
        )
        .unwrap();
    let execution = started_execution(&db, &task.id, "local");

    let coding = resolve(&db, &execution, "local");
    assert_eq!(coding.agent_type.as_deref(), Some("automation"));
    assert_eq!(coding.project_id.as_deref(), Some(project.id.as_str()));

    // Review precedence beats the automation source.
    let review = resolve(&db, &as_kind(&execution, ExecutionKind::PrReview), "local");
    assert_eq!(review.agent_type.as_deref(), Some("review"));
    assert_eq!(review.project_id.as_deref(), Some(project.id.as_str()));
}

#[test]
fn every_execution_kind_resolves_a_badge_type() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "x");
    let base = started_execution(&db, &chore.id, "local");

    let expected = [
        (ExecutionKind::AnswerAgent, "answer"),
        (ExecutionKind::AutomationTriage, "automation"),
        (ExecutionKind::ChoreImplementation, "coding"),
        (ExecutionKind::CiRemediation, "coding"),
        (ExecutionKind::ConflictResolution, "coding"),
        (ExecutionKind::InvestigationImplementation, "coding"),
        (ExecutionKind::PrReview, "review"),
        (ExecutionKind::PrReviewGuide, "review"),
        (ExecutionKind::ProductDesign, "design"),
        (ExecutionKind::ProjectDesign, "design"),
        (ExecutionKind::RevisionImplementation, "coding"),
        (ExecutionKind::TaskImplementation, "coding"),
    ];
    assert_eq!(expected.len(), 12);
    for (kind, badge) in expected {
        let metadata = resolve(&db, &as_kind(&base, kind.clone()), "local");
        assert_eq!(metadata.agent_type.as_deref(), Some(badge), "{kind}");
    }
}

#[test]
fn host_is_explicit_and_distinguishes_local_from_remote() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "x");
    let execution = started_execution(&db, &chore.id, "local");

    assert_eq!(resolve(&db, &execution, "local").host_id.as_deref(), Some("local"));
    assert_eq!(resolve(&db, &execution, "zakalwe").host_id.as_deref(), Some("zakalwe"));
}

#[test]
fn started_at_is_the_execution_start_as_sortable_iso8601() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "x");
    let execution = started_execution(&db, &chore.id, "local");
    let epoch = execution.started_epoch().expect("a started execution has started_at");

    let metadata = resolve(&db, &execution, "local");
    assert_eq!(
        metadata.started_at.as_deref(),
        Some(boss_engine_utils::iso8601::format_epoch_iso8601(epoch).as_str()),
    );

    let unstarted = WorkExecution {
        started_at: None,
        ..execution
    };
    assert_eq!(resolve(&db, &unstarted, "local").started_at, None);
}

#[test]
fn guide_recovery_prefers_execution_stamp_and_preserves_unknown_driver() {
    for (slug, model, capable) in [
        ("claude", "claude-opus-4-7", true),
        ("removed-driver", "retired-model", false),
    ] {
        let (_dir, db) = open_db();
        let execution = crate::test_support::unstamped_codex_guide(&db);
        let stamped = db
            .record_execution_launch_config(&execution.id, slug, model, None)
            .unwrap();
        let driver = crate::driver_transcript::driver_for_spawned_execution(&db, &execution.id);
        assert_eq!(
            driver.as_ref().map(|driver| driver.descriptor().name),
            if capable { Some("claude") } else { None }
        );
        assert_eq!(
            adopted_identity(&db, &stamped, driver.as_deref()),
            (model.to_owned(), capable)
        );
    }
}

#[test]
fn guide_recovery_does_not_replace_an_unknown_attempt_driver_with_live_policy() {
    let (_dir, db) = open_db();
    let execution = crate::test_support::unstamped_codex_guide(&db);
    let attempt = db
        .pr_review_guide_attempt_for_execution(&execution.id)
        .unwrap()
        .unwrap();
    db.record_pr_review_guide_attempt_spawn_config(&attempt.id, "removed-driver", "retired-model", "high")
        .unwrap();
    let driver = crate::driver_transcript::driver_for_spawned_execution(&db, &execution.id);
    assert!(driver.is_none());
    assert_eq!(
        adopted_identity(&db, &execution, driver.as_deref()),
        ("retired-model".to_owned(), false)
    );
}

/// A real external-PR comparison retains its PR without inventing task metadata.
#[test]
fn review_guide_pane_without_an_owning_task_falls_back_cleanly() {
    const PR: &str = "https://github.com/spinyfin/mono/pull/3401";
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "x");
    let (_, comparison) = seed_review_guide_series_for_pr(&db, "task_absent", PR, "base", "head");
    let execution = WorkExecution {
        work_item_id: comparison,
        ..as_kind(
            &started_execution(&db, &chore.id, "local"),
            ExecutionKind::PrReviewGuide,
        )
    };
    let metadata = resolve(&db, &execution, "local");
    assert_eq!(metadata.product_id, None);
    assert_eq!(metadata.product_name, None);
    assert_eq!(metadata.project_id, None);
    assert_eq!(metadata.project_name, None);
    assert_eq!(metadata.work_item_short_id, None);
    assert_eq!(metadata.owning_work_item_id, None);
    assert_eq!(metadata.owning_work_item_title, None);
    assert_eq!(metadata.pr_url.as_deref(), Some(PR));
    let title = boss_pane_summary::derived_title_summary(
        &execution.kind,
        None,
        metadata.owning_work_item_title.as_deref().unwrap_or_default(),
    );
    assert_eq!(title.flatten().as_deref(), Some("generating a review guide"));
    let label = crate::execution_label::label_for_execution(&db, &execution);
    assert_eq!(label.status, boss_protocol::ExecutionLabelStatus::NoWorkItem);
    assert_eq!(label.pr_url.as_deref(), Some(PR));
    assert_eq!(label.work_item_id, None);
    assert_eq!(label.work_item_short_id, None);
    assert_eq!(label.product_name, None);
    assert_eq!(label.project_name, None);
}
