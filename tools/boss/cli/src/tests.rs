//! Unit tests for the boss CLI.

use clap::Parser;

use super::{
    AttentionGroupSelector, AutomationCommand, AutomationSelector, BindPrAction, BulkCreateItem, ChoreCommand, Cli,
    Commands, DependCommand, EffortLevelArg, LintSeverity, MoveTarget, OpenDesignAction, ProductCommand, ProductStatus,
    ProjectCommand, ProjectStatusArg, RepoSelector, RunContext, TaskCommand, TaskListCriteria, TaskPriority,
    TaskStatusArg, apply_project_list_filters, apply_task_list_filters, classify_bind_pr, classify_lint_finding,
    compile_schedule, decide_open_design_action, dependency_status_is_satisfied, ensure_explicit_product_matches,
    expect_leaf_work_item, format_project_design_doc_line, format_repo_line, is_typed_work_item_id, lint_summary_line,
    parse_attention_group_selector, parse_automation_selector, pick_by_index, split_shake_report, status_vocab,
    task_json_with_runtime, validate_github_pr_url, with_display_status,
};
use boss_protocol::{
    Product, Project, ProjectDesignDocState, ProjectStatus, ResolvedDesignDoc, ResolvedDesignDocKind, Task, TaskKind,
    TaskRuntime, TaskStatus, WorkItem,
};

#[test]
fn move_target_maps_board_names_to_stored() {
    assert_eq!(MoveTarget::Backlog.as_status(), "todo");
    assert_eq!(MoveTarget::Doing.as_status(), "active");
    assert_eq!(MoveTarget::Review.as_status(), "in_review");
    assert_eq!(MoveTarget::Done.as_status(), "done");
    assert_eq!(MoveTarget::Blocked.as_status(), "blocked");
    assert_eq!(MoveTarget::Archived.as_status(), "archived");
}

#[test]
fn task_status_arg_maps_board_names_to_stored() {
    // `--status`/filter values are the board names; the stored
    // string sent to the engine stays in the legacy vocabulary.
    assert_eq!(TaskStatusArg::Backlog.as_str(), "todo");
    assert_eq!(TaskStatusArg::Doing.as_str(), "active");
    assert_eq!(TaskStatusArg::Review.as_str(), "in_review");
    assert_eq!(TaskStatusArg::Done.as_str(), "done");
    assert_eq!(TaskStatusArg::Blocked.as_str(), "blocked");
    assert_eq!(TaskStatusArg::Archived.as_str(), "archived");
}

#[test]
fn archived_tasks_hidden_from_list_by_default_but_shown_on_request() {
    let archived = Task::builder()
        .id("task_archived")
        .product_id("prod_1")
        .kind(TaskKind::Chore)
        .name("n")
        .description("")
        .status(TaskStatus::Archived)
        .created_at("")
        .updated_at("")
        .build();
    let live = Task::builder()
        .id("task_live")
        .product_id("prod_1")
        .kind(TaskKind::Chore)
        .name("n")
        .description("")
        .status(TaskStatus::Todo)
        .created_at("")
        .updated_at("")
        .build();

    // Default view: archived is hidden, live rows still show.
    let visible = apply_task_list_filters(
        vec![archived.clone(), live.clone()],
        TaskListCriteria::builder()
            .statuses(&[])
            .priorities(&[])
            .ids(&[])
            .include_archived(false)
            .build(),
        None,
        None,
    );
    assert_eq!(visible.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["task_live"]);

    // `--include-archived` surfaces it alongside everything else.
    let visible = apply_task_list_filters(
        vec![archived.clone(), live.clone()],
        TaskListCriteria::builder()
            .statuses(&[])
            .priorities(&[])
            .ids(&[])
            .include_archived(true)
            .build(),
        None,
        None,
    );
    assert_eq!(visible.len(), 2);

    // An explicit `--status archived` filter also surfaces it, without
    // needing `--include-archived` too.
    let visible = apply_task_list_filters(
        vec![archived, live],
        TaskListCriteria::builder()
            .statuses(&[TaskStatusArg::Archived])
            .priorities(&[])
            .ids(&[])
            .include_archived(false)
            .build(),
        None,
        None,
    );
    assert_eq!(
        visible.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        ["task_archived"]
    );
}

/// Filter-test row: `dummy_task` with the fields the list filters
/// actually read dialled in. Everything else keeps the
/// `dummy_task` defaults.
fn filterable_task(id: &str, name: &str, description: &str, status: TaskStatus, priority: &str) -> Task {
    let mut task = dummy_task(id, TaskKind::Task);
    task.name = name.to_owned();
    task.description = description.to_owned();
    task.status = status;
    task.priority = priority.to_owned();
    task
}

/// Filter-test row for projects, mirroring [`filterable_task`].
fn filterable_project(id: &str, name: &str, description: &str, status: ProjectStatus) -> Project {
    Project::builder()
        .id(id)
        .product_id("prod_1")
        .name(name)
        .slug(name.to_lowercase())
        .description(description)
        .goal("")
        .status(status)
        .created_at("")
        .updated_at("")
        .build()
}

/// The filters promise a *set* of surviving rows, not an ordering —
/// sort so no test accidentally pins iteration order.
fn surviving_task_ids(tasks: &[Task]) -> Vec<&str> {
    let mut ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    ids.sort_unstable();
    ids
}

fn surviving_project_ids(projects: &[Project]) -> Vec<&str> {
    let mut ids: Vec<&str> = projects.iter().map(|p| p.id.as_str()).collect();
    ids.sort_unstable();
    ids
}

/// `TaskListCriteria` with every dimension off — the "no flags
/// passed" baseline each test narrows one field at a time.
fn unfiltered_task_criteria<'a>() -> TaskListCriteria<'a> {
    TaskListCriteria::builder()
        .statuses(&[])
        .priorities(&[])
        .ids(&[])
        .include_archived(false)
        .build()
}

/// `--priority` is an OR over the listed values; omitting it keeps
/// every priority.
#[test]
fn task_list_filters_by_priority() {
    let rows = || {
        vec![
            filterable_task("task_low", "n", "", TaskStatus::Todo, "low"),
            filterable_task("task_medium", "n", "", TaskStatus::Todo, "medium"),
            filterable_task("task_high", "n", "", TaskStatus::Todo, "high"),
        ]
    };

    let visible = apply_task_list_filters(
        rows(),
        TaskListCriteria::builder().priorities(&[TaskPriority::High]).build(),
        None,
        None,
    );
    assert_eq!(surviving_task_ids(&visible), ["task_high"]);

    // Several values OR together.
    let visible = apply_task_list_filters(
        rows(),
        TaskListCriteria::builder()
            .priorities(&[TaskPriority::Low, TaskPriority::High])
            .build(),
        None,
        None,
    );
    assert_eq!(surviving_task_ids(&visible), ["task_high", "task_low"]);

    // No `--priority` at all: nothing is filtered out on that axis.
    let visible = apply_task_list_filters(rows(), unfiltered_task_criteria(), None, None);
    assert_eq!(visible.len(), 3);
}

/// `--match` is a case-insensitive substring test against the name
/// *or* the description — a hit in either field keeps the row.
#[test]
fn task_list_match_term_hits_name_or_description_case_insensitively() {
    let rows = || {
        vec![
            filterable_task("task_name_hit", "Fix Nimbus deploy", "", TaskStatus::Todo, "medium"),
            filterable_task(
                "task_desc_hit",
                "unrelated",
                "rollout to NIMBUS",
                TaskStatus::Todo,
                "medium",
            ),
            filterable_task("task_miss", "unrelated", "nothing to see", TaskStatus::Todo, "medium"),
        ]
    };

    // Lowercase term matches the mixed-case name and the uppercase
    // description alike.
    let visible = apply_task_list_filters(
        rows(),
        TaskListCriteria::builder().match_term("nimbus").build(),
        None,
        None,
    );
    assert_eq!(surviving_task_ids(&visible), ["task_desc_hit", "task_name_hit"]);

    // ...and an uppercase term is folded the same way.
    let visible = apply_task_list_filters(
        rows(),
        TaskListCriteria::builder().match_term("NiMbUs").build(),
        None,
        None,
    );
    assert_eq!(surviving_task_ids(&visible), ["task_desc_hit", "task_name_hit"]);

    // A term nothing carries filters everything out.
    let visible = apply_task_list_filters(
        rows(),
        TaskListCriteria::builder().match_term("zeppelin").build(),
        None,
        None,
    );
    assert!(visible.is_empty());
}

/// An explicit id set restricts the listing to those rows; ids that
/// match nothing are simply absent rather than an error.
#[test]
fn task_list_restricts_to_requested_ids() {
    let rows = || {
        vec![
            filterable_task("task_1", "n", "", TaskStatus::Todo, "medium"),
            filterable_task("task_2", "n", "", TaskStatus::Todo, "medium"),
            filterable_task("task_3", "n", "", TaskStatus::Todo, "medium"),
        ]
    };

    let ids = vec!["task_1".to_owned(), "task_3".to_owned()];
    let visible = apply_task_list_filters(rows(), TaskListCriteria::builder().ids(&ids).build(), None, None);
    assert_eq!(surviving_task_ids(&visible), ["task_1", "task_3"]);

    // An id set naming rows that aren't present yields nothing.
    let ids = vec!["task_absent".to_owned()];
    let visible = apply_task_list_filters(rows(), TaskListCriteria::builder().ids(&ids).build(), None, None);
    assert!(visible.is_empty());
}

/// `--limit` caps how many rows come back, and it applies to what
/// *survived* the other filters — not to the raw input — so a
/// limited listing is never padded with rows the filters rejected.
#[test]
fn task_list_limit_caps_the_filtered_rows() {
    let rows = vec![
        filterable_task("task_done_1", "n", "", TaskStatus::Done, "medium"),
        filterable_task("task_todo_1", "n", "", TaskStatus::Todo, "medium"),
        filterable_task("task_todo_2", "n", "", TaskStatus::Todo, "medium"),
        filterable_task("task_todo_3", "n", "", TaskStatus::Todo, "medium"),
    ];

    // Limit alone: fewer rows than the input, all still real rows.
    let visible = apply_task_list_filters(rows.clone(), TaskListCriteria::builder().limit(2).build(), None, None);
    assert_eq!(visible.len(), 2);

    // Limit with a status filter: the two rows returned are both
    // `todo` — the leading `done` row does not consume a slot.
    let visible = apply_task_list_filters(
        rows.clone(),
        TaskListCriteria::builder()
            .statuses(&[TaskStatusArg::Backlog])
            .limit(2)
            .build(),
        None,
        None,
    );
    assert_eq!(visible.len(), 2);
    assert!(visible.iter().all(|t| t.status == TaskStatus::Todo));

    // A limit above the match count is not padding — you get what matched.
    let visible = apply_task_list_filters(
        rows,
        TaskListCriteria::builder()
            .statuses(&[TaskStatusArg::Done])
            .limit(10)
            .build(),
        None,
        None,
    );
    assert_eq!(surviving_task_ids(&visible), ["task_done_1"]);
}

/// `--repo` matches a task's *resolved* repo: its own override
/// wins, otherwise the parent product's default. A task with
/// neither never matches (the selector is a positive filter).
#[test]
fn task_list_repo_selector_matches_override_and_inherited() {
    let mut overridden = filterable_task("task_override", "n", "", TaskStatus::Todo, "medium");
    overridden.repo_remote_url = Some("git@github.com:myorg/nimbus.git".to_owned());
    let inherited = filterable_task("task_inherited", "n", "", TaskStatus::Todo, "medium");

    let selector = RepoSelector::parse("nimbus").unwrap();

    // Product default is `mono`: only the overriding task resolves
    // to nimbus.
    let visible = apply_task_list_filters(
        vec![overridden.clone(), inherited.clone()],
        unfiltered_task_criteria(),
        Some(&selector),
        Some("git@github.com:spinyfin/mono.git"),
    );
    assert_eq!(surviving_task_ids(&visible), ["task_override"]);

    // Product default is nimbus: the non-overriding task inherits
    // it and matches too (design R10 / Q3).
    let visible = apply_task_list_filters(
        vec![overridden.clone(), inherited.clone()],
        unfiltered_task_criteria(),
        Some(&selector),
        Some("git@github.com:myorg/nimbus.git"),
    );
    assert_eq!(surviving_task_ids(&visible), ["task_inherited", "task_override"]);

    // No product repo to fall back on: the task that resolves to
    // nothing is filtered out rather than matching by default.
    let visible = apply_task_list_filters(
        vec![overridden, inherited],
        unfiltered_task_criteria(),
        Some(&selector),
        None,
    );
    assert_eq!(surviving_task_ids(&visible), ["task_override"]);
}

/// The filter dimensions compose as AND: a row must clear every
/// one that was supplied, not merely any of them.
#[test]
fn task_list_filters_compose_as_and() {
    let rows = vec![
        filterable_task("task_both", "Nimbus rollout", "", TaskStatus::Done, "high"),
        filterable_task("task_term_only", "Nimbus planning", "", TaskStatus::Todo, "high"),
        filterable_task("task_status_only", "unrelated", "", TaskStatus::Done, "high"),
    ];

    // Match term AND status: only the row satisfying both survives.
    let visible = apply_task_list_filters(
        rows.clone(),
        TaskListCriteria::builder()
            .statuses(&[TaskStatusArg::Done])
            .match_term("nimbus")
            .build(),
        None,
        None,
    );
    assert_eq!(surviving_task_ids(&visible), ["task_both"]);

    // Adding a third dimension the row fails (priority) empties the
    // result even though the other two still match.
    let visible = apply_task_list_filters(
        rows,
        TaskListCriteria::builder()
            .statuses(&[TaskStatusArg::Done])
            .priorities(&[TaskPriority::Low])
            .match_term("nimbus")
            .build(),
        None,
        None,
    );
    assert!(visible.is_empty());
}

/// `--status` on `project list` is an OR over the listed values;
/// unlike tasks, projects have no hidden-by-default status, so an
/// empty filter returns archived rows too.
#[test]
fn project_list_filters_by_status() {
    let rows = || {
        vec![
            filterable_project("proj_planned", "n", "", ProjectStatus::Planned),
            filterable_project("proj_active", "n", "", ProjectStatus::Active),
            filterable_project("proj_archived", "n", "", ProjectStatus::Archived),
        ]
    };

    let visible = apply_project_list_filters(rows(), &[ProjectStatusArg::Active], None, &[], None, None, None);
    assert_eq!(surviving_project_ids(&visible), ["proj_active"]);

    let visible = apply_project_list_filters(
        rows(),
        &[ProjectStatusArg::Planned, ProjectStatusArg::Archived],
        None,
        &[],
        None,
        None,
        None,
    );
    assert_eq!(surviving_project_ids(&visible), ["proj_archived", "proj_planned"]);

    // No status filter: everything, archived included.
    let visible = apply_project_list_filters(rows(), &[], None, &[], None, None, None);
    assert_eq!(visible.len(), 3);
}

/// An explicit id set restricts the project listing to those rows.
#[test]
fn project_list_restricts_to_requested_ids() {
    let rows = vec![
        filterable_project("proj_1", "n", "", ProjectStatus::Active),
        filterable_project("proj_2", "n", "", ProjectStatus::Active),
    ];

    let ids = vec!["proj_2".to_owned()];
    let visible = apply_project_list_filters(rows.clone(), &[], None, &ids, None, None, None);
    assert_eq!(surviving_project_ids(&visible), ["proj_2"]);

    let ids = vec!["proj_absent".to_owned()];
    let visible = apply_project_list_filters(rows, &[], None, &ids, None, None, None);
    assert!(visible.is_empty());
}

/// `--match` on projects behaves like the task one: case-insensitive
/// substring against the name *or* the description.
#[test]
fn project_list_match_term_hits_name_or_description_case_insensitively() {
    let rows = || {
        vec![
            filterable_project("proj_name_hit", "Nimbus Migration", "", ProjectStatus::Active),
            filterable_project("proj_desc_hit", "unrelated", "depends on NIMBUS", ProjectStatus::Active),
            filterable_project("proj_miss", "unrelated", "nothing to see", ProjectStatus::Active),
        ]
    };

    let visible = apply_project_list_filters(rows(), &[], Some("nimbus"), &[], None, None, None);
    assert_eq!(surviving_project_ids(&visible), ["proj_desc_hit", "proj_name_hit"]);

    let visible = apply_project_list_filters(rows(), &[], Some("NiMbUs"), &[], None, None, None);
    assert_eq!(surviving_project_ids(&visible), ["proj_desc_hit", "proj_name_hit"]);

    let visible = apply_project_list_filters(rows(), &[], Some("zeppelin"), &[], None, None, None);
    assert!(visible.is_empty());
}

/// `--limit` caps the surviving project rows, and applies after the
/// other filters rather than to the raw input.
#[test]
fn project_list_limit_caps_the_filtered_rows() {
    let rows = vec![
        filterable_project("proj_done", "n", "", ProjectStatus::Done),
        filterable_project("proj_active_1", "n", "", ProjectStatus::Active),
        filterable_project("proj_active_2", "n", "", ProjectStatus::Active),
        filterable_project("proj_active_3", "n", "", ProjectStatus::Active),
    ];

    let visible = apply_project_list_filters(rows.clone(), &[], None, &[], Some(2), None, None);
    assert_eq!(visible.len(), 2);

    // The leading `done` row does not consume one of the two slots.
    let visible = apply_project_list_filters(
        rows.clone(),
        &[ProjectStatusArg::Active],
        None,
        &[],
        Some(2),
        None,
        None,
    );
    assert_eq!(visible.len(), 2);
    assert!(visible.iter().all(|p| p.status == ProjectStatus::Active));

    // A limit above the match count returns just what matched.
    let visible = apply_project_list_filters(rows, &[ProjectStatusArg::Done], None, &[], Some(10), None, None);
    assert_eq!(surviving_project_ids(&visible), ["proj_done"]);
}

/// Projects carry no repo of their own — they resolve through the
/// parent product — so `--repo` is all-or-nothing for a product's
/// projects: it either keeps every one or none.
#[test]
fn project_list_repo_selector_gates_on_the_parent_product() {
    let rows = || {
        vec![
            filterable_project("proj_1", "n", "", ProjectStatus::Active),
            filterable_project("proj_2", "n", "", ProjectStatus::Active),
        ]
    };
    let selector = RepoSelector::parse("nimbus").unwrap();

    // Product resolves to nimbus: every project under it is kept.
    let visible = apply_project_list_filters(
        rows(),
        &[],
        None,
        &[],
        None,
        Some(&selector),
        Some("git@github.com:myorg/nimbus.git"),
    );
    assert_eq!(surviving_project_ids(&visible), ["proj_1", "proj_2"]);

    // Product resolves elsewhere: none of them are.
    let visible = apply_project_list_filters(
        rows(),
        &[],
        None,
        &[],
        None,
        Some(&selector),
        Some("git@github.com:spinyfin/mono.git"),
    );
    assert!(visible.is_empty());

    // Product has no repo at all: `--repo` is a positive filter, so
    // nothing matches.
    let visible = apply_project_list_filters(rows(), &[], None, &[], None, Some(&selector), None);
    assert!(visible.is_empty());

    // Without a selector the product's repo is irrelevant.
    let visible = apply_project_list_filters(rows(), &[], None, &[], None, None, None);
    assert_eq!(visible.len(), 2);
}

/// Q4 / Q10: a project prereq stops gating its dependent once it is
/// `done` *or* `archived` — archiving a project is an accepted way
/// to clear the dependency.
#[test]
fn project_dependency_is_satisfied_by_done_or_archived() {
    assert!(dependency_status_is_satisfied("proj_1", "done"));
    assert!(dependency_status_is_satisfied("proj_1", "archived"));
}

/// Q4 / Q10: tasks and chores satisfy on `done` only — archiving a
/// task does *not* clear it as a prereq, unlike a project.
#[test]
fn task_dependency_is_satisfied_only_by_done() {
    for id in ["task_1", "chore_1"] {
        assert!(dependency_status_is_satisfied(id, "done"));
        assert!(!dependency_status_is_satisfied(id, "archived"));
    }
}

/// Every other status leaves the prereq gating — this inverse is
/// what drives the `← INCOMPLETE` annotation on the dependent.
#[test]
fn unfinished_dependencies_are_never_satisfied() {
    for id in ["proj_1", "task_1", "chore_1"] {
        for status in ["todo", "active", "blocked", "in_review", "cancelled"] {
            assert!(
                !dependency_status_is_satisfied(id, status),
                "{id} @ {status} should still gate its dependent"
            );
        }
    }
}

#[test]
fn status_vocab_maps_stored_to_board_names() {
    assert_eq!(status_vocab::to_ui("todo"), "backlog");
    assert_eq!(status_vocab::to_ui("active"), "doing");
    assert_eq!(status_vocab::to_ui("in_review"), "review");
    // done / blocked are identical in both vocabularies.
    assert_eq!(status_vocab::to_ui("done"), "done");
    assert_eq!(status_vocab::to_ui("blocked"), "blocked");
    // Unknown values pass through unchanged.
    assert_eq!(status_vocab::to_ui("archived"), "archived");
}

#[test]
fn status_vocab_maps_board_names_to_stored() {
    // `to_stored` is the shared inverse of `to_ui` that both
    // `TaskStatusArg::as_str` and `MoveTarget::as_status` delegate to.
    assert_eq!(status_vocab::to_stored("backlog"), "todo");
    assert_eq!(status_vocab::to_stored("doing"), "active");
    assert_eq!(status_vocab::to_stored("review"), "in_review");
    // done / blocked are identical in both vocabularies.
    assert_eq!(status_vocab::to_stored("done"), "done");
    assert_eq!(status_vocab::to_stored("blocked"), "blocked");
    // Unknown values (incl. archived) pass through unchanged.
    assert_eq!(status_vocab::to_stored("archived"), "archived");
    assert_eq!(status_vocab::to_stored("unknown"), "unknown");

    // `to_stored` and `to_ui` round-trip on the renamed statuses.
    for stored in ["todo", "active", "in_review"] {
        assert_eq!(status_vocab::to_stored(status_vocab::to_ui(stored)), stored);
    }
}

#[test]
fn task_json_with_runtime_escapes_control_chars_for_strict_json_parsers() {
    let task = Task::builder()
        .id("task_1")
        .product_id("prod_1")
        .name("n")
        .description("line1\tline2\rline3\x01end")
        .kind(TaskKind::Task)
        .status(TaskStatus::InReview)
        .created_at("t")
        .updated_at("t")
        .build();
    let runtime = TaskRuntime::builder().work_item_id("task_1").build();
    let value = task_json_with_runtime(&task, &runtime).unwrap();
    let mut buf = Vec::new();
    serde_json::to_writer_pretty(&mut buf, &value).unwrap();
    let text = String::from_utf8(buf).unwrap();
    let reparsed: serde_json::Value = serde_json::from_str(&text).expect("must be strictly valid JSON");
    assert_eq!(reparsed["description"].as_str().unwrap(), "line1\tline2\rline3\x01end");
}

#[test]
fn display_label_maps_stored_to_board_names() {
    // `with_display_status` is now an identity function; display
    // transformation happens at each display site via `display_label()`.
    let task = Task::builder()
        .id("task_1")
        .product_id("prod_1")
        .name("n")
        .description("d")
        .kind(TaskKind::Task)
        .status(TaskStatus::InReview)
        .created_at("t")
        .updated_at("t")
        .build();
    let shown = with_display_status(task);
    assert_eq!(shown.status.display_label(), "review");
    assert_eq!(shown.status, TaskStatus::InReview);
}

#[test]
fn task_status_accepts_legacy_aliases_on_input() {
    // Board name resolves to its variant...
    let cli = Cli::parse_from(["boss", "task", "list", "--status", "backlog"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::List(args),
        } => assert!(matches!(args.status.as_slice(), [TaskStatusArg::Backlog])),
        _ => panic!("expected task list command"),
    }
    // ...and so does the legacy stored name as an alias.
    let cli = Cli::parse_from(["boss", "task", "list", "--status", "in-review"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::List(args),
        } => assert!(matches!(args.status.as_slice(), [TaskStatusArg::Review])),
        _ => panic!("expected task list command"),
    }
}

#[test]
fn move_target_accepts_board_name_primary() {
    let cli = Cli::parse_from(["boss", "task", "move", "task_1", "--to", "backlog"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::Move(args),
        } => assert!(matches!(args.target, MoveTarget::Backlog)),
        _ => panic!("expected task move command"),
    }
}

#[test]
fn parses_task_depend_add_with_product() {
    let cli = Cli::parse_from(["boss", "task", "depend", "add", "T2075", "T2074", "--product", "boss"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::Depend {
                command: DependCommand::Add(args),
            },
        } => {
            assert_eq!(args.dependent, "T2075");
            assert_eq!(args.prerequisite, "T2074");
            assert_eq!(args.product.as_deref(), Some("boss"));
        }
        _ => panic!("expected task depend add command"),
    }
}

#[test]
fn parses_task_depend_rm_and_list_with_product() {
    let cli = Cli::parse_from(["boss", "task", "depend", "rm", "T2075", "T2074", "--product", "boss"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::Depend {
                command: DependCommand::Rm(args),
            },
        } => {
            assert_eq!(args.product.as_deref(), Some("boss"));
        }
        _ => panic!("expected task depend rm command"),
    }

    let cli = Cli::parse_from(["boss", "task", "depend", "list", "T2075", "--product", "boss"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::Depend {
                command: DependCommand::List(args),
            },
        } => {
            assert_eq!(args.selector, "T2075");
            assert_eq!(args.product.as_deref(), Some("boss"));
        }
        _ => panic!("expected task depend list command"),
    }
}

#[test]
fn parses_product_create_command() {
    let cli = Cli::parse_from(["boss", "product", "create", "--name", "Boss"]);
    match cli.command {
        Commands::Product {
            command: ProductCommand::Create(args),
        } => {
            assert_eq!(args.name.as_deref(), Some("Boss"));
        }
        _ => panic!("expected product create command"),
    }
}

#[test]
fn parses_task_move_command() {
    let cli = Cli::parse_from(["boss", "task", "move", "task_1", "--to", "review"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::Move(args),
        } => {
            assert_eq!(args.id, "task_1");
            assert!(matches!(args.target, MoveTarget::Review));
        }
        _ => panic!("expected task move command"),
    }
}

/// `boss task move <chore-id>` is the case from the work item: the
/// CLI used to error with "work item is a chore, not a task" even
/// though the engine already knew the kind from the id. After the
/// consolidation the parser still accepts it (parsing was never
/// the issue) and the runtime hands it to the same handler as a
/// task id; this test pins the parser shape.
#[test]
fn parses_task_move_command_with_chore_shaped_id() {
    let cli = Cli::parse_from(["boss", "task", "move", "task_18ad79226b0ca630_1a", "--to", "blocked"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::Move(args),
        } => {
            assert_eq!(args.id, "task_18ad79226b0ca630_1a");
            assert!(matches!(args.target, MoveTarget::Blocked));
        }
        _ => panic!("expected task move command"),
    }
}

/// `--depends-on` is repeatable and comma-splittable on `task create`,
/// carrying the create-time prerequisite selectors that the engine
/// turns into atomic `blocks` edges.
#[test]
fn parses_task_create_depends_on() {
    let cli = Cli::parse_from([
        "boss",
        "task",
        "create",
        "--name",
        "Dependent",
        "--depends-on",
        "T1908",
        "--depends-on",
        "task_abc,task_def",
    ]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::Create(args),
        } => {
            assert_eq!(args.depends_on, vec!["T1908", "task_abc", "task_def"]);
        }
        _ => panic!("expected task create command"),
    }
}

/// Same flag on `chore create`.
#[test]
fn parses_chore_create_depends_on() {
    let cli = Cli::parse_from([
        "boss",
        "chore",
        "create",
        "--name",
        "Dependent",
        "--depends-on",
        "T1908",
    ]);
    match cli.command {
        Commands::Chore {
            command: ChoreCommand::Create(args),
        } => {
            assert_eq!(args.depends_on, vec!["T1908"]);
        }
        _ => panic!("expected chore create command"),
    }
}

/// Same flag on `task create-revision`, added so revisions can gate on
/// an arbitrary prerequisite (not just the automatic chain-tail gate)
/// atomically at create time.
#[test]
fn parses_task_create_revision_depends_on() {
    let cli = Cli::parse_from([
        "boss",
        "task",
        "create-revision",
        "--parent",
        "T651",
        "--description",
        "fix the thing",
        "--depends-on",
        "T1908",
        "--depends-on",
        "task_abc,task_def",
    ]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::CreateRevision(args),
        } => {
            assert_eq!(args.depends_on, vec!["T1908", "task_abc", "task_def"]);
        }
        _ => panic!("expected task create-revision command"),
    }
}

/// `boss chore move` is now a thin alias for the same handler. The
/// legacy `active` value still parses (as an alias of the board name
/// `doing`), exercising backward compatibility.
#[test]
fn parses_chore_move_command() {
    let cli = Cli::parse_from(["boss", "chore", "move", "task_xyz", "--to", "active"]);
    match cli.command {
        Commands::Chore {
            command: ChoreCommand::Move(args),
        } => {
            assert_eq!(args.id, "task_xyz");
            assert!(matches!(args.target, MoveTarget::Doing));
        }
        _ => panic!("expected chore move command"),
    }
}

/// `--no-autostart` and `--no-engine-autostart` are distinct global
/// flags (issue #787). `--no-autostart` governs only worker
/// auto-dispatch; `--no-engine-autostart` governs transparent
/// engine startup. Pin that they parse into independent fields.
#[test]
fn no_autostart_and_no_engine_autostart_are_independent_flags() {
    let cli = Cli::parse_from(["boss", "--no-autostart", "engine", "status"]);
    assert!(cli.global.no_autostart);
    assert!(!cli.global.no_engine_autostart);

    let cli = Cli::parse_from(["boss", "--no-engine-autostart", "engine", "status"]);
    assert!(!cli.global.no_autostart);
    assert!(cli.global.no_engine_autostart);
}

/// Regression for #787: `--no-autostart` must NOT suppress
/// transparent engine startup — the engine is the system of record
/// and must stay reachable to service the request. Only
/// `--no-engine-autostart` flips `discovery.autostart` off.
#[test]
fn no_autostart_leaves_engine_autostart_enabled() {
    // `--no-autostart` alone: worker dispatch suppressed, engine
    // autostart still enabled.
    let cli = Cli::parse_from(["boss", "--no-autostart", "engine", "status"]);
    let ctx = RunContext::from_flags(&cli.global).expect("from_flags");
    assert!(ctx.no_autostart, "no_autostart should propagate");
    assert!(
        ctx.discovery.autostart,
        "--no-autostart must not disable transparent engine startup"
    );

    // `--no-engine-autostart` alone: engine autostart suppressed,
    // worker dispatch untouched.
    let cli = Cli::parse_from(["boss", "--no-engine-autostart", "engine", "status"]);
    let ctx = RunContext::from_flags(&cli.global).expect("from_flags");
    assert!(!ctx.no_autostart, "no_autostart should default to false");
    assert!(
        !ctx.discovery.autostart,
        "--no-engine-autostart must disable transparent engine startup"
    );

    // Neither flag: both default on/dispatching.
    let cli = Cli::parse_from(["boss", "engine", "status"]);
    let ctx = RunContext::from_flags(&cli.global).expect("from_flags");
    assert!(!ctx.no_autostart);
    assert!(ctx.discovery.autostart);
}

fn dummy_task(id: &str, kind: TaskKind) -> Task {
    Task::builder()
        .id(id)
        .product_id("prod_1")
        .kind(kind)
        .name("n")
        .description("")
        .status(TaskStatus::Todo)
        .created_at("")
        .updated_at("")
        .build()
}

#[test]
fn expect_leaf_accepts_task_and_chore() {
    let task = dummy_task("task_1", TaskKind::Task);
    let (unwrapped, label) = expect_leaf_work_item(WorkItem::Task(task.clone())).unwrap();
    assert_eq!(unwrapped.id, "task_1");
    assert_eq!(label, "task");

    let chore = dummy_task("task_2", TaskKind::Chore);
    let (unwrapped, label) = expect_leaf_work_item(WorkItem::Chore(chore)).unwrap();
    assert_eq!(unwrapped.id, "task_2");
    assert_eq!(label, "chore");
}

#[test]
fn expect_leaf_rejects_product_and_project() {
    let product = Product::builder()
        .id("prod_1")
        .name("n")
        .slug("n")
        .description("")
        .status("active")
        .created_at("")
        .updated_at("")
        .build();
    assert!(expect_leaf_work_item(WorkItem::Product(product)).is_err());

    let project = Project::builder()
        .id("proj_1")
        .product_id("prod_1")
        .name("n")
        .slug("n")
        .description("")
        .goal("")
        .status(ProjectStatus::Planned)
        .created_at("")
        .updated_at("")
        .build();
    assert!(expect_leaf_work_item(WorkItem::Project(project)).is_err());
}

/// Helper for the `format_repo_line` golden tests: build a Product
/// with `repo_remote_url` set or unset and a fixed slug so the
/// inherited-line text is predictable.
fn dummy_product(slug: &str, repo: Option<&str>) -> Product {
    Product::builder()
        .id("prod_1")
        .name(slug)
        .slug(slug)
        .description("")
        .maybe_repo_remote_url(repo)
        .status("active")
        .created_at("")
        .updated_at("")
        .build()
}

/// Golden output: a work item with its own non-empty
/// `repo_remote_url` reports "(override on this work item)" — the
/// product's value is ignored in this branch even if it's also set.
#[test]
fn format_repo_line_override_on_work_item() {
    let product = dummy_product("boss", Some("git@github.com:spinyfin/mono.git"));
    let rendered = format_repo_line(Some("git@github.com:myorg/nimbus.git"), &product);
    assert_eq!(rendered, "git@github.com:myorg/nimbus.git (override on this work item)",);
}

/// Golden output: no override (or empty override) falls through to
/// the product's value, attributing via the product's slug.
#[test]
fn format_repo_line_inherits_from_product() {
    let product = dummy_product("boss", Some("git@github.com:spinyfin/mono.git"));
    let rendered = format_repo_line(None, &product);
    assert_eq!(
        rendered,
        "git@github.com:spinyfin/mono.git (inherited from product `boss`)",
    );

    // Empty-string override is treated as "no override" — mirrors
    // the `--repo ""` clear semantics on update.
    let rendered = format_repo_line(Some(""), &product);
    assert_eq!(
        rendered,
        "git@github.com:spinyfin/mono.git (inherited from product `boss`)",
    );
}

/// Golden output: neither row supplies a URL → the work item
/// cannot dispatch. Matches the engine's `resolve_repo_for_work_item`
/// returning `None`.
#[test]
fn format_repo_line_none_when_nothing_resolves() {
    let product = dummy_product("boss", None);
    let rendered = format_repo_line(None, &product);
    assert_eq!(rendered, "(none — work item cannot dispatch)");

    // Empty string on the product is equivalent to unset.
    let product = dummy_product("boss", Some(""));
    let rendered = format_repo_line(None, &product);
    assert_eq!(rendered, "(none — work item cannot dispatch)");

    // Empty override + empty product still falls through to none.
    let rendered = format_repo_line(Some(""), &product);
    assert_eq!(rendered, "(none — work item cannot dispatch)");
}

#[test]
fn parses_product_delete_command() {
    let cli = Cli::parse_from(["boss", "product", "delete", "boss"]);
    match cli.command {
        Commands::Product {
            command: ProductCommand::Delete(args),
        } => {
            assert_eq!(args.selector, "boss");
        }
        _ => panic!("expected product delete command"),
    }
}

#[test]
fn parses_task_restore_command() {
    let cli = Cli::parse_from(["boss", "task", "restore", "T43"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::Restore(args),
        } => assert_eq!(args.id, "T43"),
        _ => panic!("expected task restore command"),
    }
}

#[test]
fn parses_task_undelete_alias() {
    // `undelete` is an alias for `restore`.
    let cli = Cli::parse_from(["boss", "task", "undelete", "task_abc"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::Restore(args),
        } => assert_eq!(args.id, "task_abc"),
        _ => panic!("expected task restore command via undelete alias"),
    }
}

#[test]
fn parses_chore_restore_command() {
    let cli = Cli::parse_from(["boss", "chore", "restore", "T9"]);
    match cli.command {
        Commands::Chore {
            command: ChoreCommand::Restore(args),
        } => assert_eq!(args.id, "T9"),
        _ => panic!("expected chore restore command"),
    }
}

#[test]
fn parses_task_list_deleted_flag() {
    // Both `--deleted` and its `--include-deleted` alias flip the flag.
    for flag in ["--deleted", "--include-deleted"] {
        let cli = Cli::parse_from(["boss", "task", "list", "--product", "boss", flag]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::List(args),
            } => assert!(args.include_deleted, "expected include_deleted for {flag}"),
            _ => panic!("expected task list command"),
        }
    }
}

#[test]
fn parses_product_move_command() {
    let cli = Cli::parse_from(["boss", "product", "move", "boss", "--to", "paused"]);
    match cli.command {
        Commands::Product {
            command: ProductCommand::Move(args),
        } => {
            assert_eq!(args.selector, "boss");
            assert!(matches!(args.target, ProductStatus::Paused));
        }
        _ => panic!("expected product move command"),
    }
}

#[test]
fn parses_project_delete_command() {
    let cli = Cli::parse_from(["boss", "project", "delete", "work-cli", "--product", "boss"]);
    match cli.command {
        Commands::Project {
            command: ProjectCommand::Delete(args),
        } => {
            assert_eq!(args.selector, "work-cli");
            assert_eq!(args.product.as_deref(), Some("boss"));
        }
        _ => panic!("expected project delete command"),
    }
}

#[test]
fn parses_project_move_command() {
    let cli = Cli::parse_from([
        "boss",
        "project",
        "move",
        "work-cli",
        "--product",
        "boss",
        "--to",
        "done",
    ]);
    match cli.command {
        Commands::Project {
            command: ProjectCommand::Move(args),
        } => {
            assert_eq!(args.selector, "work-cli");
            assert_eq!(args.product.as_deref(), Some("boss"));
            assert!(matches!(args.target, ProjectStatusArg::Done));
        }
        _ => panic!("expected project move command"),
    }
}

#[test]
fn product_status_archived_serializes_to_archived() {
    assert_eq!(ProductStatus::Archived.as_str(), "archived");
    assert_eq!(ProductStatus::Active.as_str(), "active");
    assert_eq!(ProductStatus::Paused.as_str(), "paused");
}

#[test]
fn project_status_archived_serializes_to_archived() {
    assert_eq!(ProjectStatusArg::Archived.as_str(), "archived");
    assert_eq!(ProjectStatusArg::Done.as_str(), "done");
    assert_eq!(ProjectStatusArg::Planned.as_str(), "planned");
}

#[test]
fn numeric_selection_is_one_based() {
    let values = vec!["alpha".to_owned(), "beta".to_owned()];
    assert_eq!(pick_by_index(&values, "2").unwrap(), Some("beta".to_owned()));
    assert!(pick_by_index(&values, "0").is_err());
}

#[test]
fn validate_github_pr_url_accepts_canonical_form() {
    let url = "https://github.com/spinyfin/mono/pull/238";
    assert_eq!(validate_github_pr_url(url).unwrap(), url);
    // surrounding whitespace is trimmed
    assert_eq!(
        validate_github_pr_url("  https://github.com/a/b/pull/1\n").unwrap(),
        "https://github.com/a/b/pull/1"
    );
}

#[test]
fn validate_github_pr_url_rejects_malformed() {
    for bad in [
        "",
        "not-a-url",
        "http://github.com/a/b/pull/1",        // wrong scheme
        "https://gitlab.com/a/b/pull/1",       // wrong host
        "https://github.com/a/b/pulls/1",      // typo
        "https://github.com/a/b/issues/1",     // wrong noun
        "https://github.com/a/b/pull/",        // missing number
        "https://github.com/a/b/pull/abc",     // non-numeric
        "https://github.com/a/b/pull/1/files", // trailing path
        "https://github.com//repo/pull/1",     // empty org
        "https://github.com/org//pull/1",      // empty repo
    ] {
        assert!(validate_github_pr_url(bad).is_err(), "expected `{bad}` to be rejected");
    }
}

#[test]
fn classify_bind_pr_first_time_when_unset() {
    assert_eq!(
        classify_bind_pr(None, "https://github.com/a/b/pull/1"),
        BindPrAction::FirstTime
    );
    // Empty-string prior (engine normalizes empty → None, but defend
    // against the wire-format edge case) is treated as unset.
    assert_eq!(
        classify_bind_pr(Some(""), "https://github.com/a/b/pull/1"),
        BindPrAction::FirstTime
    );
}

#[test]
fn classify_bind_pr_idempotent_on_same_url() {
    let url = "https://github.com/a/b/pull/1";
    assert_eq!(classify_bind_pr(Some(url), url), BindPrAction::Idempotent);
}

#[test]
fn classify_bind_pr_overwrite_on_different_url() {
    let prior = "https://github.com/a/b/pull/1";
    let new = "https://github.com/a/b/pull/2";
    assert_eq!(
        classify_bind_pr(Some(prior), new),
        BindPrAction::Overwrite { previous: prior }
    );
}

#[test]
fn parses_task_bind_pr_command() {
    let cli = Cli::parse_from(["boss", "task", "bind-pr", "task_1", "https://github.com/a/b/pull/9"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::BindPr(args),
        } => {
            assert_eq!(args.id, "task_1");
            assert_eq!(args.pr_url, "https://github.com/a/b/pull/9");
        }
        _ => panic!("expected task bind-pr command"),
    }
}

#[test]
fn parses_chore_bind_pr_command() {
    let cli = Cli::parse_from(["boss", "chore", "bind-pr", "task_2", "https://github.com/a/b/pull/10"]);
    match cli.command {
        Commands::Chore {
            command: ChoreCommand::BindPr(args),
        } => {
            assert_eq!(args.id, "task_2");
            assert_eq!(args.pr_url, "https://github.com/a/b/pull/10");
        }
        _ => panic!("expected chore bind-pr command"),
    }
}

#[test]
fn parses_task_link_external_command() {
    let cli = Cli::parse_from([
        "boss",
        "task",
        "link-external",
        "task_1",
        "--kind",
        "github",
        "--id",
        "spinyfin/mono#560",
    ]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::LinkExternal(args),
        } => {
            assert_eq!(args.id, "task_1");
            assert_eq!(args.kind, "github");
            assert_eq!(args.upstream_id, "spinyfin/mono#560");
        }
        _ => panic!("expected task link-external command"),
    }
}

#[test]
fn parses_chore_link_external_command() {
    let cli = Cli::parse_from([
        "boss",
        "chore",
        "link-external",
        "task_2",
        "--kind",
        "github",
        "--id",
        "spinyfin/mono#561",
    ]);
    match cli.command {
        Commands::Chore {
            command: ChoreCommand::LinkExternal(args),
        } => {
            assert_eq!(args.id, "task_2");
            assert_eq!(args.kind, "github");
            assert_eq!(args.upstream_id, "spinyfin/mono#561");
        }
        _ => panic!("expected chore link-external command"),
    }
}

#[test]
fn parses_task_unlink_external_command() {
    let cli = Cli::parse_from(["boss", "task", "unlink-external", "task_3"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::UnlinkExternal(args),
        } => {
            assert_eq!(args.id, "task_3");
        }
        _ => panic!("expected task unlink-external command"),
    }
}

#[test]
fn parses_chore_unlink_external_command() {
    let cli = Cli::parse_from(["boss", "chore", "unlink-external", "task_4"]);
    match cli.command {
        Commands::Chore {
            command: ChoreCommand::UnlinkExternal(args),
        } => {
            assert_eq!(args.id, "task_4");
        }
        _ => panic!("expected chore unlink-external command"),
    }
}

#[test]
fn parses_task_create_many_command() {
    let cli = Cli::parse_from([
        "boss",
        "task",
        "create-many",
        "--from-file",
        "tasks.json",
        "--product",
        "boss",
        "--project",
        "plan",
    ]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::CreateMany(args),
        } => {
            assert_eq!(args.from_file, "tasks.json");
            assert_eq!(args.product.as_deref(), Some("boss"));
            assert_eq!(args.project.as_deref(), Some("plan"));
        }
        _ => panic!("expected task create-many command"),
    }
}

#[test]
fn parses_chore_create_many_with_stdin() {
    let cli = Cli::parse_from(["boss", "chore", "create-many", "--from-file", "-", "--product", "boss"]);
    match cli.command {
        Commands::Chore {
            command: ChoreCommand::CreateMany(args),
        } => {
            assert_eq!(args.from_file, "-");
            assert_eq!(args.product.as_deref(), Some("boss"));
        }
        _ => panic!("expected chore create-many command"),
    }
}

#[test]
fn bulk_create_item_deserializes_full_form() {
    let raw = r#"{
            "name": "do thing",
            "description": "details",
            "autostart": false,
            "project_id": "proj_abc"
        }"#;
    let item: BulkCreateItem = serde_json::from_str(raw).unwrap();
    assert_eq!(item.name, "do thing");
    assert_eq!(item.description, "details");
    assert_eq!(item.autostart, Some(false));
    assert_eq!(item.project_id.as_deref(), Some("proj_abc"));
}

#[test]
fn bulk_create_item_rejects_unknown_fields() {
    let raw = r#"{ "name": "x", "description": "y", "autosatrt": true }"#;
    let err = serde_json::from_str::<BulkCreateItem>(raw).expect_err("typo must fail");
    assert!(err.to_string().contains("autosatrt"), "{err}");
}

#[test]
fn bulk_create_item_deserializes_deferred_and_defaults_absent_to_none() {
    let with_deferred: BulkCreateItem =
        serde_json::from_str(r#"{ "name": "future work", "description": "d", "deferred": true }"#).unwrap();
    assert_eq!(with_deferred.deferred, Some(true));

    // Absent → None (the create path treats None as `false`).
    let without: BulkCreateItem = serde_json::from_str(r#"{ "name": "n", "description": "d" }"#).unwrap();
    assert_eq!(without.deferred, None);
}

#[test]
fn task_update_parses_deferred_flag() {
    // `--deferred false` is the operator-approval form; it must parse into
    // the shared TaskUpdateArgs.
    let cli = Cli::parse_from(["boss", "task", "update", "task_1", "--deferred", "false"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::Update(args),
        } => assert_eq!(args.deferred, Some(false)),
        _ => panic!("expected task update command"),
    }
}

#[test]
fn parses_project_set_design_doc_with_path() {
    let cli = Cli::parse_from([
        "boss",
        "project",
        "set-design-doc",
        "pointer",
        "--product",
        "boss",
        "--path",
        "tools/boss/docs/designs/foo.md",
    ]);
    match cli.command {
        Commands::Project {
            command: ProjectCommand::SetDesignDoc(args),
        } => {
            assert_eq!(args.selector, "pointer");
            assert_eq!(args.product.as_deref(), Some("boss"));
            assert_eq!(args.path.as_deref(), Some("tools/boss/docs/designs/foo.md"),);
            assert!(!args.unset);
            assert!(args.repo.is_none());
            assert!(args.branch.is_none());
        }
        _ => panic!("expected project set-design-doc command"),
    }
}

#[test]
fn parses_project_set_design_doc_with_repo_and_branch() {
    let cli = Cli::parse_from([
        "boss",
        "project",
        "set-design-doc",
        "pointer",
        "--product",
        "boss",
        "--path",
        "designs/foo.md",
        "--repo",
        "https://github.com/myorg/wiki.git",
        "--branch",
        "trunk",
    ]);
    match cli.command {
        Commands::Project {
            command: ProjectCommand::SetDesignDoc(args),
        } => {
            assert_eq!(args.repo.as_deref(), Some("https://github.com/myorg/wiki.git"),);
            assert_eq!(args.branch.as_deref(), Some("trunk"));
        }
        _ => panic!("expected project set-design-doc command"),
    }
}

#[test]
fn parses_project_set_design_doc_with_unset() {
    let cli = Cli::parse_from([
        "boss",
        "project",
        "set-design-doc",
        "pointer",
        "--product",
        "boss",
        "--unset",
    ]);
    match cli.command {
        Commands::Project {
            command: ProjectCommand::SetDesignDoc(args),
        } => {
            assert!(args.unset);
            assert!(args.path.is_none());
        }
        _ => panic!("expected project set-design-doc command"),
    }
}

/// Clap enforces the mutual exclusion between `--unset` and the
/// pointer-set flags so the engine never sees an ambiguous
/// request.
#[test]
fn rejects_unset_combined_with_path() {
    let err = Cli::try_parse_from([
        "boss",
        "project",
        "set-design-doc",
        "pointer",
        "--unset",
        "--path",
        "designs/foo.md",
    ])
    .expect_err("unset + path must conflict");
    let rendered = err.to_string();
    assert!(
        rendered.contains("--unset") || rendered.contains("--path"),
        "{rendered}"
    );
}

/// `--repo` without `--path` is meaningless — clap rejects it at
/// parse time so the user fixes the call rather than seeing a
/// confusing engine error.
#[test]
fn rejects_repo_without_path() {
    let err = Cli::try_parse_from([
        "boss",
        "project",
        "set-design-doc",
        "pointer",
        "--repo",
        "https://github.com/x/y.git",
    ])
    .expect_err("repo without path must error");
    assert!(err.to_string().contains("--path"), "{err}");
}

#[test]
fn parses_project_open_design_print_and_web() {
    let cli = Cli::parse_from([
        "boss",
        "project",
        "open-design",
        "pointer",
        "--product",
        "boss",
        "--web",
        "--print",
    ]);
    match cli.command {
        Commands::Project {
            command: ProjectCommand::OpenDesign(args),
        } => {
            assert_eq!(args.selector, "pointer");
            assert!(args.web);
            assert!(args.print);
        }
        _ => panic!("expected project open-design command"),
    }
}

fn resolved_state(kind: ResolvedDesignDocKind, local: bool) -> ProjectDesignDocState {
    ProjectDesignDocState::Resolved {
        resolved: ResolvedDesignDoc {
            repo_remote_url: "git@github.com:spinyfin/mono.git".to_owned(),
            branch: "main".to_owned(),
            path: "tools/boss/docs/designs/foo.md".to_owned(),
            kind,
        },
        workspace_path: local.then(|| "/tmp/mono-agent-007".to_owned()),
        web_url: "https://github.com/spinyfin/mono/blob/main/tools/boss/docs/designs/foo.md".to_owned(),
        raw_content_url: Some(
            "https://raw.githubusercontent.com/spinyfin/mono/main/tools/boss/docs/designs/foo.md".to_owned(),
        ),
    }
}

/// Same-product pointer with a leased workspace picks the
/// filesystem fast path (renderer / `$EDITOR`), not the web URL.
#[test]
fn open_design_same_product_with_workspace_uses_local_file() {
    let state = resolved_state(
        ResolvedDesignDocKind::SameProduct {
            product_id: "prod_1".into(),
        },
        true,
    );
    let action = decide_open_design_action(&state, false).unwrap();
    match action {
        OpenDesignAction::LocalFile { path, web_url } => {
            assert_eq!(path.to_string_lossy(), "tools/boss/docs/designs/foo.md");
            assert!(web_url.starts_with("https://github.com/"));
        }
        other => panic!("expected LocalFile, got {other:?}"),
    }
}

/// Without a leased workspace the fast path is unavailable —
/// fall through to the web URL even for same-product pointers.
#[test]
fn open_design_same_product_without_workspace_falls_back_to_web() {
    let state = resolved_state(
        ResolvedDesignDocKind::SameProduct {
            product_id: "prod_1".into(),
        },
        false,
    );
    let action = decide_open_design_action(&state, false).unwrap();
    assert!(matches!(action, OpenDesignAction::Web { .. }));
}

/// `--web` forces the web URL regardless of workspace state.
#[test]
fn open_design_force_web_overrides_local_path() {
    let state = resolved_state(
        ResolvedDesignDocKind::SameProduct {
            product_id: "prod_1".into(),
        },
        true,
    );
    let action = decide_open_design_action(&state, true).unwrap();
    assert!(matches!(action, OpenDesignAction::Web { .. }));
}

/// External pointers always open in the browser — there's no
/// workspace shortcut for repos Boss doesn't track.
#[test]
fn open_design_external_always_uses_web() {
    let state = resolved_state(ResolvedDesignDocKind::External, true);
    let action = decide_open_design_action(&state, false).unwrap();
    assert!(matches!(action, OpenDesignAction::Web { .. }));
}

#[test]
fn open_design_not_set_errors() {
    let err = decide_open_design_action(&ProjectDesignDocState::NotSet, false).expect_err("not-set must error");
    assert!(err.to_string().contains("no design-doc pointer"), "{err}");
}

#[test]
fn open_design_broken_errors() {
    let state = ProjectDesignDocState::Broken {
        reason: "missing repo".to_owned(),
    };
    let err = decide_open_design_action(&state, false).expect_err("broken must error");
    assert!(err.to_string().contains("broken"), "{err}");
}

#[test]
fn design_doc_line_omits_unset_state() {
    assert!(format_project_design_doc_line(&ProjectDesignDocState::NotSet).is_none());
}

#[test]
fn design_doc_line_renders_resolved_state() {
    let state = resolved_state(
        ResolvedDesignDocKind::SameProduct {
            product_id: "prod_1".into(),
        },
        false,
    );
    let line = format_project_design_doc_line(&state).expect("resolved → line");
    assert!(line.contains("tools/boss/docs/designs/foo.md"));
    assert!(line.contains("https://github.com/"));
}

#[test]
fn design_doc_line_flags_broken_state() {
    let state = ProjectDesignDocState::Broken {
        reason: "no repo".to_owned(),
    };
    let line = format_project_design_doc_line(&state).expect("broken → line");
    assert!(line.contains("(broken)"));
    assert!(line.contains("no repo"));
}

fn lint_product() -> Product {
    Product::builder()
        .id("prod_1")
        .name("Boss")
        .slug("boss")
        .description("")
        .repo_remote_url("git@github.com:spinyfin/mono.git")
        .status("active")
        .created_at("")
        .updated_at("")
        .build()
}

fn lint_project(slug: &str, path: Option<&str>) -> Project {
    Project {
        id: format!("proj_{slug}"),
        product_id: "prod_1".to_owned(),
        name: slug.to_owned(),
        slug: slug.to_owned(),
        description: String::new(),
        goal: String::new(),
        status: ProjectStatus::Planned,
        priority: "medium".to_owned(),
        created_at: String::new(),
        updated_at: String::new(),
        last_status_actor: "human".to_owned(),
        design_doc_repo_remote_url: None,
        design_doc_branch: None,
        design_doc_path: path.map(str::to_owned),
        short_id: None,
    }
}

/// A resolved pointer with a leased workspace whose file exists
/// on disk is healthy — the lint produces no entry.
#[test]
fn lint_skips_resolved_pointer_with_existing_file() {
    let product = lint_product();
    let project = lint_project("alpha", Some("tools/boss/docs/designs/alpha.md"));
    let state = ProjectDesignDocState::Resolved {
        resolved: ResolvedDesignDoc {
            repo_remote_url: "git@github.com:spinyfin/mono.git".to_owned(),
            branch: "main".to_owned(),
            path: "tools/boss/docs/designs/alpha.md".to_owned(),
            kind: ResolvedDesignDocKind::SameProduct {
                product_id: "prod_1".into(),
            },
        },
        workspace_path: Some("/tmp/mono-agent-007".to_owned()),
        web_url: "https://example.test/blob/main/x.md".to_owned(),
        raw_content_url: None,
    };
    let entry = classify_lint_finding(&product, &project, Some(&state), |_, _| true, false, false);
    assert!(entry.is_none(), "healthy pointer must not appear in lint");
}

/// A resolved pointer whose file is missing in the leased
/// workspace is the canonical stale-on-rename case. Always
/// flagged as `Broken`, regardless of opt-in flags.
#[test]
fn lint_flags_resolved_pointer_with_missing_file_as_broken() {
    let product = lint_product();
    let project = lint_project("alpha", Some("tools/boss/docs/designs/alpha-renamed.md"));
    let state = ProjectDesignDocState::Resolved {
        resolved: ResolvedDesignDoc {
            repo_remote_url: "git@github.com:spinyfin/mono.git".to_owned(),
            branch: "main".to_owned(),
            path: "tools/boss/docs/designs/alpha-renamed.md".to_owned(),
            kind: ResolvedDesignDocKind::SameProduct {
                product_id: "prod_1".into(),
            },
        },
        workspace_path: Some("/tmp/mono-agent-007".to_owned()),
        web_url: "https://example.test/blob/main/x.md".to_owned(),
        raw_content_url: None,
    };
    let entry = classify_lint_finding(
        &product,
        &project,
        Some(&state),
        |_, _| false,
        /*include_missing*/ false,
        /*include_unverified*/ false,
    )
    .expect("missing file must surface as a lint entry");
    assert_eq!(entry.severity, LintSeverity::Broken);
    assert!(entry.reason.contains("file not found"), "reason: {}", entry.reason);
    assert!(
        entry.suggested_fix.contains("boss project set-design-doc boss/alpha"),
        "fix template should pre-fill product/project selector: {}",
        entry.suggested_fix,
    );
}

/// The resolver's own `Broken` state — typically "path set but no
/// repo to resolve against" — is always reported, no flags
/// required.
#[test]
fn lint_flags_resolver_broken_state() {
    let product = lint_product();
    let project = lint_project("alpha", Some("designs/alpha.md"));
    let state = ProjectDesignDocState::Broken {
        reason: "no repo to resolve against".to_owned(),
    };
    let entry = classify_lint_finding(&product, &project, Some(&state), |_, _| true, false, false)
        .expect("broken resolver state must surface");
    assert_eq!(entry.severity, LintSeverity::Broken);
    assert!(entry.reason.contains("no repo"));
}

/// A resolved pointer with no leased workspace can't be probed.
/// Default behaviour: silently skip (we can't confirm it's
/// broken). With `--include-unverified`: surface as `Unverified`.
#[test]
fn lint_skips_unverified_pointer_by_default() {
    let product = lint_product();
    let project = lint_project("alpha", Some("designs/alpha.md"));
    let state = ProjectDesignDocState::Resolved {
        resolved: ResolvedDesignDoc {
            repo_remote_url: "https://github.com/myorg/wiki.git".to_owned(),
            branch: "main".to_owned(),
            path: "designs/alpha.md".to_owned(),
            kind: ResolvedDesignDocKind::External,
        },
        workspace_path: None,
        web_url: "https://example.test/blob/main/x.md".to_owned(),
        raw_content_url: None,
    };
    // The file_check callback must NOT be invoked when there's
    // no workspace — assert that by panicking from it.
    let entry = classify_lint_finding(
        &product,
        &project,
        Some(&state),
        |_, _| panic!("file_check must not run when no workspace is leased"),
        /*include_missing*/ false,
        /*include_unverified*/ false,
    );
    assert!(entry.is_none(), "unverified pointers are skipped by default");
}

#[test]
fn lint_includes_unverified_when_flag_set() {
    let product = lint_product();
    let project = lint_project("alpha", Some("designs/alpha.md"));
    let state = ProjectDesignDocState::Resolved {
        resolved: ResolvedDesignDoc {
            repo_remote_url: "https://github.com/myorg/wiki.git".to_owned(),
            branch: "main".to_owned(),
            path: "designs/alpha.md".to_owned(),
            kind: ResolvedDesignDocKind::External,
        },
        workspace_path: None,
        web_url: "https://example.test/blob/main/x.md".to_owned(),
        raw_content_url: None,
    };
    let entry = classify_lint_finding(
        &product,
        &project,
        Some(&state),
        |_, _| true,
        false,
        /*include_unverified*/ true,
    )
    .expect("--include-unverified must surface unverified pointers");
    assert_eq!(entry.severity, LintSeverity::Unverified);
    assert!(entry.reason.contains("no leased workspace"));
}

/// Projects with no pointer set are silently skipped unless
/// `--include-missing` is on; then they surface as `Missing`
/// (advisory, not counted as broken for the exit code).
#[test]
fn lint_skips_missing_pointer_by_default() {
    let product = lint_product();
    let project = lint_project("alpha", None);
    let entry = classify_lint_finding(&product, &project, None, |_, _| true, false, false);
    assert!(entry.is_none(), "missing pointers are skipped by default");
}

#[test]
fn lint_includes_missing_when_flag_set() {
    let product = lint_product();
    let project = lint_project("alpha", None);
    let entry = classify_lint_finding(
        &product,
        &project,
        None,
        |_, _| true,
        /*include_missing*/ true,
        false,
    )
    .expect("--include-missing must surface unset pointers");
    assert_eq!(entry.severity, LintSeverity::Missing);
    assert!(entry.design_doc_path.is_none());
    assert!(entry.suggested_fix.contains("set-design-doc boss/alpha"));
}

/// The footer tally lists each present severity with its count and
/// omits severities with no findings.
#[test]
fn lint_summary_line_breaks_down_present_severities() {
    let product = lint_product();
    let broken = ProjectDesignDocState::Broken {
        reason: "no repo".to_owned(),
    };
    let entries = vec![
        classify_lint_finding(
            &product,
            &lint_project("a", Some("a.md")),
            Some(&broken),
            |_, _| true,
            false,
            false,
        )
        .unwrap(),
        classify_lint_finding(
            &product,
            &lint_project("b", Some("b.md")),
            Some(&broken),
            |_, _| true,
            false,
            false,
        )
        .unwrap(),
        classify_lint_finding(&product, &lint_project("c", None), None, |_, _| true, true, false).unwrap(),
    ];
    assert_eq!(lint_summary_line(&entries), "3 finding(s): 2 broken, 1 missing");
}

#[test]
fn parses_project_lint_design_docs_defaults() {
    let cli = Cli::parse_from(["boss", "project", "lint-design-docs"]);
    match cli.command {
        Commands::Project {
            command: ProjectCommand::LintDesignDocs(args),
        } => {
            assert!(args.product.is_none());
            assert!(!args.include_missing);
            assert!(!args.include_unverified);
        }
        _ => panic!("expected project lint-design-docs command"),
    }
}

#[test]
fn parses_project_lint_design_docs_with_flags() {
    let cli = Cli::parse_from([
        "boss",
        "project",
        "lint-design-docs",
        "--product",
        "boss",
        "--include-missing",
        "--include-unverified",
    ]);
    match cli.command {
        Commands::Project {
            command: ProjectCommand::LintDesignDocs(args),
        } => {
            assert_eq!(args.product.as_deref(), Some("boss"));
            assert!(args.include_missing);
            assert!(args.include_unverified);
        }
        _ => panic!("expected project lint-design-docs command"),
    }
}

#[test]
fn parses_chore_create_with_repo_override() {
    let cli = Cli::parse_from([
        "boss",
        "chore",
        "create",
        "--product",
        "work",
        "--name",
        "fix it",
        "--repo",
        "git@github.com:myorg/nimbus.git",
    ]);
    match cli.command {
        Commands::Chore {
            command: ChoreCommand::Create(args),
        } => {
            assert_eq!(args.product.as_deref(), Some("work"));
            assert_eq!(args.repo_remote_url.as_deref(), Some("git@github.com:myorg/nimbus.git"));
        }
        _ => panic!("expected chore create command"),
    }
}

#[test]
fn parses_task_create_with_repo_override() {
    let cli = Cli::parse_from([
        "boss",
        "task",
        "create",
        "--product",
        "boss",
        "--project",
        "plan",
        "--name",
        "n",
        "--repo",
        "https://github.com/myorg/wiki.git",
    ]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::Create(args),
        } => {
            assert_eq!(
                args.repo_remote_url.as_deref(),
                Some("https://github.com/myorg/wiki.git")
            );
        }
        _ => panic!("expected task create command"),
    }
}

/// `--repo ""` on update is the canonical "clear the override"
/// form (mirrors `--pr-url ""`). Clap surfaces it as
/// `Some("")`; the engine canonicaliser turns the empty string
/// into `None` so the task inherits from the product again.
#[test]
fn parses_task_update_with_repo_clear() {
    let cli = Cli::parse_from(["boss", "task", "update", "task_1", "--repo", ""]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::Update(args),
        } => {
            assert_eq!(args.id, "task_1");
            assert_eq!(args.repo_remote_url.as_deref(), Some(""));
        }
        _ => panic!("expected task update command"),
    }
}

#[test]
fn parses_chore_create_with_effort_and_model() {
    let cli = Cli::parse_from([
        "boss",
        "chore",
        "create",
        "--product",
        "boss",
        "--name",
        "fix it",
        "--effort",
        "large",
        "--model",
        "claude-opus-4-7",
    ]);
    match cli.command {
        Commands::Chore {
            command: ChoreCommand::Create(args),
        } => {
            assert!(matches!(args.effort, Some(EffortLevelArg::Large)));
            assert_eq!(args.model.as_deref(), Some("claude-opus-4-7"));
        }
        _ => panic!("expected chore create command"),
    }
}

/// `--effort` only accepts the five documented values; anything
/// else fails at parse time with a clear clap error listing the
/// valid set.
#[test]
fn rejects_invalid_effort_level_at_parse_time() {
    let result = Cli::try_parse_from([
        "boss",
        "chore",
        "create",
        "--product",
        "boss",
        "--name",
        "x",
        "--effort",
        "galaxybrain",
    ]);
    let err = result.expect_err("expected clap to reject the value");
    let rendered = err.to_string();
    // clap renders the allowed set; the exact framing changes
    // between clap versions but the level names are stable.
    assert!(rendered.contains("trivial"), "{rendered}");
    assert!(rendered.contains("max"), "{rendered}");
}

#[test]
fn parses_task_update_with_effort_clear_and_model_clear() {
    let cli = Cli::parse_from(["boss", "task", "update", "task_1", "--unset-effort", "--unset-model"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::Update(args),
        } => {
            assert!(args.unset_effort);
            assert!(args.unset_model);
            assert!(args.effort.is_none());
            assert!(args.model.is_none());
        }
        _ => panic!("expected task update command"),
    }
}

/// `--effort` and `--unset-effort` are mutually exclusive — the
/// `conflicts_with` attribute on the args struct makes clap
/// reject the combination.
#[test]
fn task_update_rejects_effort_and_unset_effort_together() {
    let result = Cli::try_parse_from([
        "boss",
        "task",
        "update",
        "task_1",
        "--effort",
        "small",
        "--unset-effort",
    ]);
    assert!(result.is_err(), "expected clap to reject mutually exclusive flags");
}

#[test]
fn parses_product_set_default_model_with_model() {
    let cli = Cli::parse_from(["boss", "product", "set-default-model", "boss", "--model", "sonnet"]);
    match cli.command {
        Commands::Product {
            command: ProductCommand::SetDefaultModel(args),
        } => {
            assert_eq!(args.selector, "boss");
            assert_eq!(args.model.as_deref(), Some("sonnet"));
            assert!(!args.unset);
        }
        _ => panic!("expected product set-default-model command"),
    }
}

#[test]
fn parses_product_set_default_model_with_unset() {
    let cli = Cli::parse_from(["boss", "product", "set-default-model", "boss", "--unset"]);
    match cli.command {
        Commands::Product {
            command: ProductCommand::SetDefaultModel(args),
        } => {
            assert!(args.unset);
            assert!(args.model.is_none());
        }
        _ => panic!("expected product set-default-model command"),
    }
}

/// `set-default-model` rejects `--model` and `--unset` together
/// at the parser; the "neither was supplied" case is caught in
/// the runtime handler (the selector positional sits outside the
/// mutual-exclusion group so the parser can still resolve it).
#[test]
fn product_set_default_model_rejects_model_with_unset() {
    let result = Cli::try_parse_from([
        "boss",
        "product",
        "set-default-model",
        "boss",
        "--model",
        "sonnet",
        "--unset",
    ]);
    assert!(result.is_err(), "expected clap to reject --model and --unset together",);
}

#[test]
fn parses_chore_update_with_repo_set() {
    let cli = Cli::parse_from([
        "boss",
        "chore",
        "update",
        "task_xyz",
        "--repo",
        "git@github.com:myorg/nimbus.git",
    ]);
    match cli.command {
        Commands::Chore {
            command: ChoreCommand::Update(args),
        } => {
            assert_eq!(args.repo_remote_url.as_deref(), Some("git@github.com:myorg/nimbus.git"));
        }
        _ => panic!("expected chore update command"),
    }
}

#[test]
fn parses_task_list_with_repo_filter() {
    let cli = Cli::parse_from(["boss", "task", "list", "--product", "work", "--repo", "nimbus"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::List(args),
        } => {
            assert_eq!(args.repo.as_deref(), Some("nimbus"));
        }
        _ => panic!("expected task list command"),
    }
}

#[test]
fn repo_url_from_pr_url_strips_pull_segment() {
    assert_eq!(
        super::repo_url_from_pr_url("https://github.com/spinyfin/mono/pull/959"),
        "https://github.com/spinyfin/mono",
    );
    // Query/fragment after the number stay attached to the dropped
    // tail, so the base is still clean.
    assert_eq!(
        super::repo_url_from_pr_url("https://github.com/foo/bar/pull/12?x=1#c"),
        "https://github.com/foo/bar",
    );
    // No /pull/ segment → returned unchanged.
    assert_eq!(
        super::repo_url_from_pr_url("https://github.com/foo/bar"),
        "https://github.com/foo/bar",
    );
}

/// The `--repo` short-name selector matches against the repo parsed
/// out of a PR URL, so `by-pr 959 --repo mono` resolves a
/// `…/spinyfin/mono/pull/959` owner.
#[test]
fn repo_selector_matches_repo_parsed_from_pr_url() {
    let sel = RepoSelector::parse("mono").unwrap();
    let base = super::repo_url_from_pr_url("https://github.com/spinyfin/mono/pull/959");
    assert!(sel.matches(Some(base)));
    let other = super::repo_url_from_pr_url("https://github.com/spinyfin/other/pull/959");
    assert!(!sel.matches(Some(other)));
}

#[test]
fn repo_selector_rejects_short_input() {
    assert!(RepoSelector::parse("").is_err());
    assert!(RepoSelector::parse("m").is_err());
    assert!(RepoSelector::parse("  m ").is_err(), "whitespace doesn't count");
    assert!(RepoSelector::parse("mo").is_ok());
}

#[test]
fn repo_selector_short_name_prefix_match() {
    let sel = RepoSelector::parse("nim").unwrap();
    assert!(sel.matches(Some("git@github.com:myorg/nimbus.git")));
    assert!(sel.matches(Some("https://github.com/other/nimbus-platform.git")));
    // Wrong product but same repo short-name → match (Q3).
    assert!(sel.matches(Some("https://github.com/foo/nimbus")));
    // Different repo, prefix doesn't match.
    assert!(!sel.matches(Some("git@github.com:myorg/mono.git")));
    // Unresolved row never matches.
    assert!(!sel.matches(None));
}

/// Inherited match: a task with no override but whose parent
/// product points at `nimbus` should match `--repo nimbus`. The
/// CLI resolves against the *effective* repo, not the raw column.
#[test]
fn repo_selector_matches_inherited_product_default() {
    let sel = RepoSelector::parse("nimbus").unwrap();
    let task = dummy_task("task_1", TaskKind::Task);
    assert!(task.repo_remote_url.is_none());
    let resolved = super::resolved_repo_for_task(&task, Some("git@github.com:myorg/nimbus.git"));
    assert!(sel.matches(resolved));
}

#[test]
fn repo_selector_full_url_form_is_exact_match() {
    let sel = RepoSelector::parse("git@github.com:myorg/nimbus.git").unwrap();
    // case-insensitive exact match
    assert!(sel.matches(Some("git@github.com:myorg/nimbus.git")));
    assert!(sel.matches(Some("GIT@GITHUB.COM:MYORG/NIMBUS.GIT")));
    // a different repo with the same short name does NOT match
    // when the selector is the URL form.
    assert!(!sel.matches(Some("git@github.com:other/nimbus.git")));
}

#[test]
fn typed_work_item_id_prefixes_are_recognized() {
    assert!(is_typed_work_item_id("prod_18ae0000_1"));
    assert!(is_typed_work_item_id("proj_18ae0000_1"));
    assert!(is_typed_work_item_id("task_18ae0000_1"));
    // whitespace is tolerated — the resolver trims before lookup.
    assert!(is_typed_work_item_id("  proj_abc  "));
    // slugs / arbitrary names are not typed ids.
    assert!(!is_typed_work_item_id("boss"));
    assert!(!is_typed_work_item_id("work-cli"));
    assert!(!is_typed_work_item_id(""));
    // chore_ is not used at the engine row level — chores share
    // the task_ prefix.
    assert!(!is_typed_work_item_id("chore_18ae0000_1"));
}

#[test]
fn friendly_tnnn_form_parses_as_short_id() {
    use super::{WorkItemSelector, parse_work_item_selector};
    // uppercase T
    assert!(matches!(
        parse_work_item_selector("T441"),
        WorkItemSelector::ShortId(441)
    ));
    // lowercase t
    assert!(matches!(
        parse_work_item_selector("t441"),
        WorkItemSelector::ShortId(441)
    ));
    // leading whitespace is trimmed
    assert!(matches!(
        parse_work_item_selector("  T12  "),
        WorkItemSelector::ShortId(12)
    ));
    // P-form (projects)
    assert!(matches!(parse_work_item_selector("P7"), WorkItemSelector::ShortId(7)));
    assert!(matches!(
        parse_work_item_selector("p100"),
        WorkItemSelector::ShortId(100)
    ));
    // zero is rejected (short_ids are positive)
    assert!(matches!(parse_work_item_selector("T0"), WorkItemSelector::Other(_)));
    // non-digit suffix is NOT a short id — falls through to Other
    assert!(matches!(parse_work_item_selector("Tabc"), WorkItemSelector::Other(_)));
    // plain primary id is still PrimaryId, not confused with T-form
    assert!(matches!(
        parse_work_item_selector("task_18ae0000_1"),
        WorkItemSelector::PrimaryId(_)
    ));
}

/// `boss project show proj_…` accepts a globally-unique typed id
/// without `--product`. The parser shape pin is the user-facing
/// half of the inference fix; the engine half is exercised by
/// the in-process integration test in `tests/infer_product.rs`.
#[test]
fn parses_project_show_with_typed_id_and_no_product() {
    let cli = Cli::parse_from(["boss", "project", "show", "proj_18aeacce8acf9140_27"]);
    match cli.command {
        Commands::Project {
            command: ProjectCommand::Show(args),
        } => {
            assert_eq!(args.selector, "proj_18aeacce8acf9140_27");
            assert!(args.product.is_none());
        }
        _ => panic!("expected project show command"),
    }
}

#[test]
fn parses_task_list_with_project_typed_id_and_no_product() {
    let cli = Cli::parse_from(["boss", "task", "list", "--project", "proj_18aeacce8acf9140_27"]);
    match cli.command {
        Commands::Task {
            command: TaskCommand::List(args),
        } => {
            assert_eq!(args.project.as_deref(), Some("proj_18aeacce8acf9140_27"));
            assert!(args.product.is_none());
        }
        _ => panic!("expected task list command"),
    }
}

fn product_with_id(id: &str, slug: &str) -> Product {
    Product::builder()
        .id(id)
        .name(slug)
        .slug(slug)
        .description("")
        .status("active")
        .created_at("")
        .updated_at("")
        .build()
}

#[test]
fn explicit_product_validator_accepts_omitted_explicit() {
    let products = vec![product_with_id("prod_1", "boss")];
    assert!(ensure_explicit_product_matches(&products, None, "prod_1", "proj_x").is_ok());
}

#[test]
fn explicit_product_validator_accepts_matching_id_or_slug() {
    let products = vec![product_with_id("prod_1", "boss")];
    assert!(ensure_explicit_product_matches(&products, Some("prod_1"), "prod_1", "proj_x").is_ok());
    assert!(ensure_explicit_product_matches(&products, Some("boss"), "prod_1", "proj_x").is_ok());
}

/// When the user passes `--product` AND a typed id whose product
/// disagrees, we surface a usage error naming both sides instead
/// of silently picking one. Same shape as the engine-side
/// "product/project disagree" check.
#[test]
fn explicit_product_validator_rejects_mismatch() {
    let products = vec![product_with_id("prod_1", "boss"), product_with_id("prod_2", "mono")];
    let err = ensure_explicit_product_matches(&products, Some("mono"), "prod_1", "proj_x")
        .expect_err("disagreement must error");
    let msg = format!("{err:?}");
    assert!(msg.contains("mono"), "{msg}");
    assert!(msg.contains("prod_1"), "{msg}");
}

#[test]
fn shake_report_takes_first_line_as_title() {
    let (title, body) = split_shake_report("Engine wedges on close\n\nrepro: …").unwrap();
    assert_eq!(title, "Engine wedges on close");
    assert_eq!(body, "repro: …");
}

#[test]
fn shake_report_strips_h1_marker_from_title() {
    let (title, body) = split_shake_report("# Engine wedges on close\n\nrepro: …\nstep two\n").unwrap();
    assert_eq!(title, "Engine wedges on close");
    assert_eq!(body, "repro: …\nstep two");
}

#[test]
fn shake_report_skips_leading_blank_lines() {
    let (title, body) = split_shake_report("\n\n  \nFirst line is title\nbody here").unwrap();
    assert_eq!(title, "First line is title");
    assert_eq!(body, "body here");
}

#[test]
fn shake_report_single_line_has_empty_body() {
    let (title, body) = split_shake_report("Only the title").unwrap();
    assert_eq!(title, "Only the title");
    assert_eq!(body, "");
}

#[test]
fn shake_report_rejects_blank_blob() {
    assert!(split_shake_report("").is_none());
    assert!(split_shake_report("\n\n  \n").is_none());
}

// --- boss automation CLI tests ---

#[test]
fn parses_automation_create_command() {
    let cli = Cli::parse_from([
        "boss",
        "automation",
        "create",
        "--product",
        "boss",
        "--name",
        "Fix clippy",
        "--instruction",
        "Look for clippy warnings",
        "--schedule",
        "weekday-2pm",
        "--timezone",
        "America/Los_Angeles",
    ]);
    match cli.command {
        Commands::Automation {
            command: AutomationCommand::Create(args),
        } => {
            assert_eq!(args.product.as_deref(), Some("boss"));
            assert_eq!(args.name.as_deref(), Some("Fix clippy"));
            assert_eq!(args.instruction.as_deref(), Some("Look for clippy warnings"));
            assert_eq!(args.schedule.as_deref(), Some("weekday-2pm"));
            assert_eq!(args.timezone, "America/Los_Angeles");
            assert!(!args.disabled);
            assert_eq!(args.open_task_limit, 1);
        }
        _ => panic!("expected automation create command"),
    }
}

#[test]
fn parses_automation_create_with_raw_cron_and_disabled() {
    let cli = Cli::parse_from([
        "boss",
        "automation",
        "create",
        "--product",
        "boss",
        "--name",
        "Weekly sweep",
        "--instruction",
        "Sweep old branches",
        "--schedule",
        "0 9 * * 1",
        "--disabled",
        "--open-task-limit",
        "3",
    ]);
    match cli.command {
        Commands::Automation {
            command: AutomationCommand::Create(args),
        } => {
            assert_eq!(args.schedule.as_deref(), Some("0 9 * * 1"));
            assert!(args.disabled);
            assert_eq!(args.open_task_limit, 3);
        }
        _ => panic!("expected automation create command"),
    }
}

#[test]
fn parses_automation_list_command() {
    let cli = Cli::parse_from(["boss", "automation", "list", "--product", "boss"]);
    match cli.command {
        Commands::Automation {
            command: AutomationCommand::List(args),
        } => {
            assert_eq!(args.product.as_deref(), Some("boss"));
        }
        _ => panic!("expected automation list command"),
    }
}

#[test]
fn parses_automation_show_command() {
    let cli = Cli::parse_from(["boss", "automation", "show", "A1", "--product", "boss"]);
    match cli.command {
        Commands::Automation {
            command: AutomationCommand::Show(args),
        } => {
            assert_eq!(args.selector, "A1");
            assert_eq!(args.product.as_deref(), Some("boss"));
        }
        _ => panic!("expected automation show command"),
    }
}

#[test]
fn parses_automation_show_with_canonical_id() {
    let cli = Cli::parse_from(["boss", "automation", "show", "auto_abc123"]);
    match cli.command {
        Commands::Automation {
            command: AutomationCommand::Show(args),
        } => {
            assert_eq!(args.selector, "auto_abc123");
            assert!(args.product.is_none());
        }
        _ => panic!("expected automation show command"),
    }
}

#[test]
fn parses_automation_update_command() {
    let cli = Cli::parse_from([
        "boss",
        "automation",
        "update",
        "A2",
        "--product",
        "boss",
        "--name",
        "New name",
        "--schedule",
        "nightly",
        "--open-task-limit",
        "2",
    ]);
    match cli.command {
        Commands::Automation {
            command: AutomationCommand::Update(args),
        } => {
            assert_eq!(args.selector, "A2");
            assert_eq!(args.product.as_deref(), Some("boss"));
            assert_eq!(args.name.as_deref(), Some("New name"));
            assert_eq!(args.schedule.as_deref(), Some("nightly"));
            assert_eq!(args.open_task_limit, Some(2));
        }
        _ => panic!("expected automation update command"),
    }
}

#[test]
fn parses_automation_enable_disable_commands() {
    let cli_enable = Cli::parse_from(["boss", "automation", "enable", "A1", "--product", "boss"]);
    let cli_disable = Cli::parse_from(["boss", "automation", "disable", "A1", "--product", "boss"]);
    assert!(matches!(
        cli_enable.command,
        Commands::Automation {
            command: AutomationCommand::Enable(_)
        }
    ));
    assert!(matches!(
        cli_disable.command,
        Commands::Automation {
            command: AutomationCommand::Disable(_)
        }
    ));
}

#[test]
fn parses_automation_run_command_with_force() {
    let cli = Cli::parse_from(["boss", "automation", "run", "A3", "--product", "boss", "--force"]);
    match cli.command {
        Commands::Automation {
            command: AutomationCommand::Run(args),
        } => {
            assert_eq!(args.selector, "A3");
            assert!(args.force);
        }
        _ => panic!("expected automation run command"),
    }
}

#[test]
fn parses_automation_runs_and_tasks_commands() {
    let cli_runs = Cli::parse_from(["boss", "automation", "runs", "A1", "--product", "boss"]);
    let cli_tasks = Cli::parse_from(["boss", "automation", "tasks", "A1", "--product", "boss"]);
    assert!(matches!(
        cli_runs.command,
        Commands::Automation {
            command: AutomationCommand::Runs(_)
        }
    ));
    assert!(matches!(
        cli_tasks.command,
        Commands::Automation {
            command: AutomationCommand::Tasks(_)
        }
    ));
}

#[test]
fn parses_automation_suppressions_command() {
    let cli = Cli::parse_from(["boss", "automation", "suppressions", "A1", "--product", "boss"]);
    assert!(matches!(
        cli.command,
        Commands::Automation {
            command: AutomationCommand::Suppressions(_)
        }
    ));
}

// --- cron validation tests ---

#[test]
fn compile_schedule_resolves_presets() {
    assert_eq!(compile_schedule("weekday-2pm").unwrap(), "0 14 * * 1-5");
    assert_eq!(compile_schedule("nightly").unwrap(), "0 2 * * *");
    assert_eq!(compile_schedule("weekly-mon-am").unwrap(), "0 9 * * 1");
    assert_eq!(compile_schedule("hourly").unwrap(), "0 * * * *");
    // Case-insensitive
    assert_eq!(compile_schedule("NIGHTLY").unwrap(), "0 2 * * *");
}

#[test]
fn compile_schedule_accepts_valid_raw_cron() {
    assert_eq!(compile_schedule("0 14 * * 1-5").unwrap(), "0 14 * * 1-5");
    assert_eq!(compile_schedule("*/15 * * * *").unwrap(), "*/15 * * * *");
    assert_eq!(compile_schedule("0 9 1,15 * *").unwrap(), "0 9 1,15 * *");
}

#[test]
fn compile_schedule_rejects_wrong_field_count() {
    assert!(compile_schedule("0 14 * *").is_err()); // 4 fields
    assert!(compile_schedule("0 14 * * 1-5 2026").is_err()); // 6 fields
    assert!(compile_schedule("").is_err());
}

#[test]
fn compile_schedule_rejects_invalid_chars() {
    assert!(compile_schedule("0 14 * * 1-5; echo hi").is_err());
    assert!(compile_schedule("0 14 * * 1$5").is_err());
}

// --- automation selector parsing tests ---

#[test]
fn parse_automation_selector_primary_id() {
    let sel = parse_automation_selector("auto_abc123").unwrap();
    assert!(matches!(sel, AutomationSelector::PrimaryId(id) if id == "auto_abc123"));
}

#[test]
fn parse_automation_selector_short_id_uppercase() {
    let sel = parse_automation_selector("A1").unwrap();
    assert!(matches!(sel, AutomationSelector::ShortId(1)));
}

#[test]
fn parse_automation_selector_short_id_lowercase() {
    let sel = parse_automation_selector("a42").unwrap();
    assert!(matches!(sel, AutomationSelector::ShortId(42)));
}

#[test]
fn parse_automation_selector_rejects_unknown_form() {
    assert!(parse_automation_selector("randomstring").is_err());
    assert!(parse_automation_selector("T42").is_err()); // task id — wrong namespace
}

// --- attention group selector parsing tests ---

#[test]
fn parse_attention_group_selector_primary_id() {
    let sel = parse_attention_group_selector("atg_abc123").unwrap();
    assert!(matches!(sel, AttentionGroupSelector::PrimaryId(id) if id == "atg_abc123"));
}

#[test]
fn parse_attention_group_selector_short_id_uppercase() {
    let sel = parse_attention_group_selector("A3").unwrap();
    assert!(matches!(sel, AttentionGroupSelector::ShortId(3)));
}

#[test]
fn parse_attention_group_selector_short_id_lowercase() {
    let sel = parse_attention_group_selector("a12").unwrap();
    assert!(matches!(sel, AttentionGroupSelector::ShortId(12)));
}

#[test]
fn parse_attention_group_selector_rejects_unknown_form() {
    assert!(parse_attention_group_selector("randomstring").is_err());
    assert!(parse_attention_group_selector("auto_abc").is_err()); // automation id — wrong namespace
    assert!(parse_attention_group_selector("T42").is_err()); // task id — wrong namespace
}
