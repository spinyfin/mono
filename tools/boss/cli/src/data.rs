//! RPC data-access helpers, resolvers, and selectors

use crate::*;

pub(crate) async fn connect_for_work(ctx: &RunContext) -> Result<BossClient, CliError> {
    BossClient::connect(&ctx.discovery)
        .await
        .map_err(|err| CliError::engine_unavailable(err.to_string()))
}

pub(crate) async fn list_products(client: &mut BossClient) -> Result<Vec<Product>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListProducts,
        "products list",
        FrontendEvent::ProductsList { products } => products,
    )
}

pub(crate) async fn list_projects(
    client: &mut BossClient,
    product_id: &str,
    dep_filter: Option<DependencyFilter>,
) -> Result<Vec<Project>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListProjects {
            product_id: product_id.to_owned(),
            dep_filter,
        },
        "projects list",
        FrontendEvent::ProjectsList { projects, .. } => projects,
    )
}

pub(crate) async fn list_tasks(
    client: &mut BossClient,
    product_id: &str,
    project_id: Option<&str>,
    dep_filter: Option<DependencyFilter>,
    include_deleted: bool,
) -> Result<Vec<Task>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListTasks {
            product_id: product_id.to_owned(),
            project_id: project_id.map(str::to_owned),
            dep_filter,
            include_deleted,
        },
        "tasks list",
        FrontendEvent::TasksList { tasks, .. } => tasks,
    )
}

pub(crate) async fn list_chores(
    client: &mut BossClient,
    product_id: &str,
    dep_filter: Option<DependencyFilter>,
    include_deleted: bool,
) -> Result<Vec<Task>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListChores {
            product_id: product_id.to_owned(),
            dep_filter,
            include_deleted,
        },
        "chores list",
        FrontendEvent::ChoresList { chores, .. } => chores,
    )
}

pub(crate) async fn list_revisions(
    client: &mut BossClient,
    product_id: &str,
    dep_filter: Option<DependencyFilter>,
    include_deleted: bool,
    parent_id: Option<String>,
) -> Result<Vec<Task>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListRevisions {
            product_id: product_id.to_owned(),
            dep_filter,
            include_deleted,
            parent_id,
        },
        "revisions list",
        FrontendEvent::RevisionsList { revisions, .. } => revisions,
    )
}

pub(crate) async fn find_work_items_by_pr(
    client: &mut BossClient,
    pr_number: i64,
) -> Result<Vec<PrWorkItemMatch>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::FindWorkItemsByPr { pr_number },
        "work items by pr",
        FrontendEvent::WorkItemsByPrResult { matches, .. } => matches,
    )
}

pub(crate) async fn create_product(client: &mut BossClient, input: CreateProductInput) -> Result<Product, CliError> {
    rpc_call!(
        try client,
        FrontendRequest::CreateProduct { input },
        "product create",
        FrontendEvent::WorkItemCreated { item } => expect_product(item),
    )
}

pub(crate) async fn set_product_default_model(
    client: &mut BossClient,
    product_id: &str,
    model: Option<String>,
) -> Result<Product, CliError> {
    rpc_call!(
        try client,
        FrontendRequest::SetProductDefaultModel {
            product_id: product_id.to_owned(),
            model,
        },
        "set-default-model",
        FrontendEvent::WorkItemUpdated { item } => expect_product(item),
    )
}

pub(crate) async fn set_product_default_driver(
    client: &mut BossClient,
    product_id: &str,
    driver: Option<String>,
) -> Result<Product, CliError> {
    rpc_call!(
        try client,
        FrontendRequest::SetProductDefaultDriver {
            product_id: product_id.to_owned(),
            driver,
        },
        "set-default-driver",
        FrontendEvent::WorkItemUpdated { item } => expect_product(item),
    )
}

pub(crate) async fn set_product_merge_mechanism(
    client: &mut BossClient,
    product_id: &str,
    mechanism: Option<String>,
) -> Result<Product, CliError> {
    rpc_call!(
        try client,
        FrontendRequest::SetProductMergeMechanism {
            product_id: product_id.to_owned(),
            mechanism,
        },
        "set-merge-mechanism",
        FrontendEvent::WorkItemUpdated { item } => expect_product(item),
    )
}

/// Build the kind-specific JSON config for `set-external-tracker` from CLI args.
pub(crate) fn build_external_tracker_config(
    kind: &str,
    args: &ProductSetExternalTrackerArgs,
) -> Result<serde_json::Value, CliError> {
    match kind {
        "github" => {
            let org = args
                .org
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| CliError::usage("--org is required for --kind github"))?;
            let repo = args
                .repo
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| CliError::usage("--repo is required for --kind github"))?;
            let project_number = args
                .project
                .ok_or_else(|| CliError::usage("--project is required for --kind github"))?;
            Ok(serde_json::json!({
                "org": org,
                "repo": repo,
                "project_number": project_number,
                "reverse_close": args.reverse_close,
            }))
        }
        other => Err(CliError::usage(format!(
            "unknown tracker kind '{other}'; supported: github"
        ))),
    }
}

pub(crate) async fn create_project(client: &mut BossClient, input: CreateProjectInput) -> Result<Project, CliError> {
    rpc_call!(
        try client,
        FrontendRequest::CreateProject { input },
        "project create",
        FrontendEvent::WorkItemCreated { item } => expect_project(item),
    )
}

pub(crate) async fn set_project_design_doc(
    client: &mut BossClient,
    input: SetProjectDesignDocInput,
) -> Result<Project, CliError> {
    rpc_call!(
        try client,
        FrontendRequest::SetProjectDesignDoc { input },
        "set project design doc",
        FrontendEvent::WorkItemUpdated { item } => expect_project(item),
    )
}

pub(crate) async fn resolve_project_design_doc(
    client: &mut BossClient,
    project_id: &str,
) -> Result<ResolveProjectDesignDocOutput, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ResolveProjectDesignDoc {
            project_id: project_id.to_owned(),
        },
        "resolve project design doc",
        FrontendEvent::ProjectDesignDocResolved { output } => output,
    )
}

/// Response shape for `boss project plan` (both the real run and the
/// `--dry-run` preview) — a thin CLI-side mirror of
/// [`FrontendEvent::PlanProjectResult`], flattened for `print_entity`.
#[derive(Debug, Clone, Serialize, bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct PlanProjectResult {
    pub(crate) project_id: String,
    pub(crate) outcome: String,
    pub(crate) message: String,
    pub(crate) created: usize,
    pub(crate) edges: usize,
    pub(crate) skipped: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) proposal: Option<PlannerOutput>,
}

pub(crate) async fn plan_project(
    client: &mut BossClient,
    project_id: &str,
    force: bool,
    dry_run: bool,
) -> Result<PlanProjectResult, CliError> {
    rpc_call!(
        client,
        FrontendRequest::PlanProject {
            project_id: project_id.to_owned(),
            force,
            dry_run,
        },
        "plan project",
        FrontendEvent::PlanProjectResult {
            project_id,
            outcome,
            message,
            created,
            edges,
            skipped,
            run_id,
            proposal,
        } => PlanProjectResult::builder()
            .project_id(project_id)
            .outcome(outcome)
            .message(message)
            .created(created)
            .edges(edges)
            .skipped(skipped)
            .maybe_run_id(run_id)
            .maybe_proposal(proposal)
            .build(),
    )
}

pub(crate) async fn release_project(client: &mut BossClient, project_id: &str) -> Result<(String, usize), CliError> {
    rpc_call!(
        client,
        FrontendRequest::ReleaseProject {
            project_id: project_id.to_owned(),
        },
        "release project",
        FrontendEvent::ReleaseProjectResult { run_id, released, .. } => (run_id, released),
    )
}

pub(crate) async fn unpopulate_project(
    client: &mut BossClient,
    project_id: &str,
    run_id: &str,
) -> Result<(Vec<String>, Vec<UnpopulatePreservedTask>), CliError> {
    rpc_call!(
        client,
        FrontendRequest::UnpopulateProject {
            project_id: project_id.to_owned(),
            run_id: run_id.to_owned(),
        },
        "unpopulate project",
        FrontendEvent::UnpopulateProjectResult { deleted, preserved, .. } => (deleted, preserved),
    )
}

pub(crate) async fn list_planner_runs(client: &mut BossClient, project_id: &str) -> Result<Vec<PlannerRun>, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListPlannerRuns {
            project_id: project_id.to_owned(),
        },
        "list planner runs",
        FrontendEvent::PlannerRunsList { runs, .. } => runs,
    )
}

pub(crate) async fn create_task(client: &mut BossClient, input: CreateTaskInput) -> Result<Task, CliError> {
    match client
        .send_request(&FrontendRequest::CreateTask { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_task(item),
        FrontendEvent::WorkItemDuplicateBlocked {
            existing_id,
            existing_short_id,
            name,
            age_secs,
        } => Err(duplicate_blocked_error(
            "A task named",
            "to create another.",
            &existing_id,
            existing_short_id,
            &name,
            age_secs,
        )),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("task create", &other)),
    }
}

/// Create the single task produced by an automation's triage phase
/// (`boss task create --automation`). The engine resolves provenance, the
/// open-task-cap re-check, the pre-file dedup gate, repo inheritance, and
/// execution dispatch; the CLI is a thin pass-through. A cap-reached or
/// duplicate-suspect rejection surfaces as a `WorkError`.
pub(crate) async fn create_automation_task(
    client: &mut BossClient,
    automation_id: &str,
    name: String,
    description: Option<String>,
    target_files: Vec<String>,
    target_symbols: Vec<String>,
) -> Result<Task, CliError> {
    rpc_call!(
        try client,
        FrontendRequest::CreateAutomationTask {
            automation_id: automation_id.to_owned(),
            name,
            description,
            target_files,
            target_symbols,
        },
        "automation task create",
        FrontendEvent::WorkItemCreated { item } => expect_chore(item),
    )
}

pub(crate) async fn create_chore(client: &mut BossClient, input: CreateChoreInput) -> Result<Task, CliError> {
    match client
        .send_request(&FrontendRequest::CreateChore { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_chore(item),
        FrontendEvent::WorkItemDuplicateBlocked {
            existing_id,
            existing_short_id,
            name,
            age_secs,
        } => Err(duplicate_blocked_error(
            "A chore named",
            "to create another.",
            &existing_id,
            existing_short_id,
            &name,
            age_secs,
        )),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("chore create", &other)),
    }
}

pub(crate) async fn create_investigation(
    client: &mut BossClient,
    input: CreateInvestigationInput,
) -> Result<Task, CliError> {
    match client
        .send_request(&FrontendRequest::CreateInvestigation { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_task(item),
        FrontendEvent::WorkItemDuplicateBlocked {
            existing_id,
            existing_short_id,
            name,
            age_secs,
        } => Err(duplicate_blocked_error(
            "An investigation named",
            "to create another.",
            &existing_id,
            existing_short_id,
            &name,
            age_secs,
        )),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("investigation create", &other)),
    }
}

pub(crate) async fn run_create_investigation(
    client: &mut BossClient,
    ctx: &RunContext,
    args: InvestigationCreateArgs,
) -> Result<(), CliError> {
    let product = resolve_product_inferable(client, args.product, args.project.as_deref(), ctx).await?;
    let project_id = if let Some(proj) = args.project {
        let project = resolve_project(client, &product.id, Some(proj), ctx).await?;
        Some(project.id)
    } else {
        None
    };
    let name = required_text(args.name, "Investigation name", ctx)?;
    let description = optional_text(args.description, "Description", ctx)?;
    let model_override = normalize_non_empty(args.model);
    let driver = normalize_non_empty(args.driver);
    validate_driver_model_pair(driver.as_deref(), model_override.as_deref())?;
    let task = create_investigation(
        client,
        CreateInvestigationInput::builder()
            .product_id(product.id)
            .maybe_project_id(project_id)
            .name(name.clone())
            .maybe_description(description)
            .autostart(!ctx.no_autostart)
            .maybe_priority(args.priority.map(|p| p.as_str().to_owned()))
            .created_via("cli")
            .maybe_repo_remote_url(args.repo_remote_url)
            .maybe_effort_level(args.effort.map(boss_protocol::EffortLevel::from))
            .maybe_model_override(model_override)
            .maybe_driver(driver)
            .force_duplicate(args.force_duplicate)
            .build(),
    )
    .await?;
    print_entity(ctx, &serde_json::json!({ "task": task }), || {
        if !ctx.quiet {
            println!("created investigation T{}: {}", task.short_id.unwrap_or(0), name);
        }
    })?;
    Ok(())
}

pub(crate) async fn create_revision_rpc(client: &mut BossClient, input: CreateRevisionInput) -> Result<Task, CliError> {
    rpc_call!(
        try client,
        FrontendRequest::CreateRevision { input },
        "revision create",
        FrontendEvent::WorkItemCreated { item } => expect_task(item),
    )
}

pub(crate) async fn run_list_revisions(
    client: &mut BossClient,
    ctx: &RunContext,
    args: RevisionListArgs,
) -> Result<(), CliError> {
    let product = resolve_product_inferable(client, args.product, None, ctx).await?;
    let dep_filter = args.dep.into_filter();
    // Resolve --parent to a canonical id if provided.
    let parent_id = if let Some(selector) = args.parent {
        Some(resolve_create_revision_parent(client, &selector).await?)
    } else {
        None
    };
    let revisions = list_revisions(client, &product.id, dep_filter, args.include_deleted, parent_id).await?;
    let revisions = apply_task_list_filters(
        revisions,
        TaskListCriteria::builder()
            .statuses(&args.status)
            .priorities(&args.priority)
            .maybe_match_term(args.match_term.as_deref())
            .ids(&args.id)
            .maybe_limit(args.limit)
            .include_archived(args.include_archived)
            .build(),
        None,
        product.repo_remote_url.as_deref(),
    );
    let revisions: Vec<Task> = revisions.into_iter().map(with_display_status).collect();
    print_entity(ctx, &serde_json::json!({ "revisions": revisions }), || {
        print_tasks_table(&revisions, args.with_primary_id)
    })
}

/// Resolve a work-item selector to a primary id without a product context.
///
/// Unlike the generic [`resolve_selector_to_primary_id`], this variant does
/// not require a product context: `T<n>` short ids are globally unique, so
/// we pass them straight to `GetWorkItem` which resolves them DB-globally
/// (via `get_work_item_resolving_short_id` in the engine). This is the only
/// product-free resolution we allow here; `#42` / `42` bare forms still need
/// a product and are rejected with a helpful message.
///
/// Used by `create-revision`'s `--parent` and `--depends-on` (neither of
/// which accepts a `--product` flag) and `list-revisions`' `--parent`.
pub(crate) async fn resolve_create_revision_parent(
    client: &mut BossClient,
    selector: &str,
) -> Result<String, CliError> {
    match parse_work_item_selector(selector) {
        // T-form short ids are globally unique — pass the friendly form
        // straight to GetWorkItem; the engine resolves it without a product.
        WorkItemSelector::ShortId(_) => {
            let item = get_work_item(client, selector).await?;
            Ok(item.primary_id().to_owned())
        }
        // Already a primary id or opaque slug — pass through unchanged.
        WorkItemSelector::PrimaryId(id) | WorkItemSelector::Other(id) => Ok(id),
        // Cross-product slug form (boss/42) — also unambiguous.
        WorkItemSelector::ProductShortId { .. } => {
            // Shouldn't normally appear given the --parent doc, but handle it
            // via the standard resolution with an empty product context.
            // This will fail clearly if the product slug can't be resolved.
            let item = get_work_item(client, selector).await?;
            Ok(item.primary_id().to_owned())
        }
    }
}

pub(crate) async fn run_create_revision(
    client: &mut BossClient,
    ctx: &RunContext,
    args: RevisionCreateArgs,
) -> Result<(), CliError> {
    // Resolve the --parent selector to a full task id before sending to
    // the engine, since the engine's CreateRevision RPC requires a full id.
    // We use a product-free resolver here: T<n> short ids are globally unique
    // so no --product flag is needed (or accepted) for create-revision.
    let parent_id = resolve_create_revision_parent(client, &args.parent).await?;
    let description = args.description.trim().to_owned();
    if description.is_empty() {
        return Err(CliError::usage("--description must be non-empty"));
    }
    let name = args
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let model_override = normalize_non_empty(args.model);
    let driver = normalize_non_empty(args.driver);
    validate_driver_model_pair(driver.as_deref(), model_override.as_deref())?;
    let depends_on = resolve_revision_depends_on(client, &args.depends_on).await?;
    let task = create_revision_rpc(
        client,
        CreateRevisionInput::builder()
            .parent_task_id(parent_id)
            .description(description.clone())
            .maybe_name(name)
            .maybe_priority(args.priority.map(|p| p.as_str().to_owned()))
            .maybe_effort_level(args.effort.map(boss_protocol::EffortLevel::from))
            .maybe_model_override(model_override)
            .maybe_driver(driver)
            .force_duplicate(args.force_duplicate)
            .depends_on(depends_on)
            .created_via(boss_protocol::CREATED_VIA_CLI)
            .autostart(!ctx.no_autostart)
            .build(),
    )
    .await?;
    print_entity(ctx, &serde_json::json!({ "task": task }), || {
        if !ctx.quiet {
            println!("created revision T{}: {}", task.short_id.unwrap_or(0), description);
        }
    })?;
    Ok(())
}

pub(crate) async fn get_work_item(client: &mut BossClient, id: &str) -> Result<WorkItem, CliError> {
    rpc_call!(
        client,
        FrontendRequest::GetWorkItem { id: id.to_owned() },
        "work item fetch",
        FrontendEvent::WorkItemResult { item } => item,
    )
}

pub(crate) async fn update_work_item(
    client: &mut BossClient,
    id: &str,
    patch: WorkItemPatch,
) -> Result<WorkItem, CliError> {
    rpc_call!(
        client,
        FrontendRequest::UpdateWorkItem {
            id: id.to_owned(),
            patch,
        },
        "work item update",
        FrontendEvent::WorkItemUpdated { item } => item,
    )
}

/// Recover a repo's base URL from a PR URL by dropping the
/// `/pull/<n>` segment (and anything after it):
/// `https://github.com/owner/repo/pull/959` →
/// `https://github.com/owner/repo`. Returns the input unchanged when
/// no `/pull/` segment is present, so a non-PR-shaped URL still flows
/// through the repo matcher.
pub(crate) fn repo_url_from_pr_url(pr_url: &str) -> &str {
    pr_url.split_once("/pull/").map_or(pr_url, |(base, _)| base)
}

/// Friendly `T<n>` id, falling back to the canonical id when a row
/// somehow lacks a short_id.
pub(crate) fn friendly_task_id(task: &Task) -> String {
    boss_protocol::short_id_label(task.short_id).unwrap_or_else(|| task.id.clone())
}

/// Apply [`with_display_status`] to the owner and every revision in a
/// PR match so rendered statuses use the board vocabulary.
pub(crate) fn with_display_pr_match(m: PrWorkItemMatch) -> PrWorkItemMatch {
    PrWorkItemMatch {
        owner: with_display_status(m.owner),
        revisions: m.revisions.into_iter().map(with_display_status).collect(),
    }
}

/// Human-readable rendering of a single PR → work-item match: the
/// owning row plus any revisions in the PR's chain.
pub(crate) fn print_pr_match(m: &PrWorkItemMatch) {
    let owner = &m.owner;
    let repo = owner.pr_url.as_deref().map(repo_url_from_pr_url).unwrap_or("");
    println!(
        "{}  {}  [{}]  {}",
        friendly_task_id(owner),
        owner.kind,
        owner.status,
        owner.name,
    );
    if !repo.is_empty() {
        println!("Repo: {repo}");
    }
    if let Some(pr_url) = &owner.pr_url {
        println!("PR URL: {pr_url}");
    }
    if m.revisions.is_empty() {
        return;
    }
    println!("Revisions in this PR's chain:");
    for revision in &m.revisions {
        let seq = revision.revision_seq.map(|n| format!("R{n} ")).unwrap_or_default();
        println!(
            "  {seq}{}  [{}]  {}",
            friendly_task_id(revision),
            revision.status,
            revision.name,
        );
    }
}

/// Handler for `boss task by-pr <pr-number> [--repo <r>]`. Resolves a
/// PR number to the work item that owns it, spanning all kinds. When
/// `--repo` is given, matches are filtered by the repo parsed from
/// each owner's PR URL; ambiguity (the same number in >1 repo) and
/// not-found are surfaced as clear errors.
pub(crate) async fn run_by_pr(client: &mut BossClient, ctx: &RunContext, args: ByPrArgs) -> Result<(), CliError> {
    if args.pr_number <= 0 {
        return Err(CliError::usage("PR number must be a positive integer"));
    }
    let repo_selector = args.repo.as_deref().map(RepoSelector::parse).transpose()?;
    let matches = find_work_items_by_pr(client, args.pr_number).await?;

    // Repo filter (when given) matches against the repo parsed from the
    // owner's PR URL — the PR URL, not the work item's repo override, is
    // what authoritatively places the PR in a repo.
    let matches: Vec<PrWorkItemMatch> = match repo_selector.as_ref() {
        Some(selector) => matches
            .into_iter()
            .filter(|m| {
                m.owner
                    .pr_url
                    .as_deref()
                    .is_some_and(|url| selector.matches(Some(repo_url_from_pr_url(url))))
            })
            .collect(),
        None => matches,
    };

    match matches.len() {
        0 => {
            let scope = match args.repo.as_deref() {
                Some(repo) => format!(" in a repo matching {repo:?}"),
                None => String::new(),
            };
            Err(CliError::not_found(format!(
                "no work item bound to PR #{}{scope}",
                args.pr_number,
            )))
        }
        1 => {
            let matched = with_display_pr_match(matches.into_iter().next().expect("len checked == 1"));
            print_entity(
                ctx,
                &serde_json::json!({ "match": &matched, "matches": [&matched] }),
                || {
                    print_pr_match(&matched);
                },
            )
        }
        _ => {
            let matched: Vec<PrWorkItemMatch> = matches.into_iter().map(with_display_pr_match).collect();
            print_entity(ctx, &serde_json::json!({ "matches": &matched }), || {
                for (i, m) in matched.iter().enumerate() {
                    if i > 0 {
                        println!();
                    }
                    print_pr_match(m);
                }
            })
        }
    }
}

pub(crate) async fn get_execution(client: &mut BossClient, id: &str) -> Result<WorkExecution, CliError> {
    rpc_call!(
        client,
        FrontendRequest::GetExecution { id: id.to_owned() },
        "execution fetch",
        FrontendEvent::ExecutionResult { execution } => execution,
    )
}

/// Handler for `boss task by-exec <execution-id>`. Resolves an execution
/// id back to the task/chore that owns it — the inverse of the
/// execution → PR → work-item chain `by-pr` walks, useful when all you
/// have is an execution id (e.g. parsed out of an authoring branch name
/// `boss/exec_…`).
///
/// `answer_agent` and `automation_triage` executions don't bind a
/// task/chore/project: their `work_item_id` is actually a comment id or
/// automation id respectively (see `WorkDb::create_answer_agent_execution`
/// / `create_automation_triage_execution`). Those are reported directly
/// with a pointer to the right inspection verb rather than attempted
/// against `GetWorkItem`, which would only produce a confusing "unknown
/// work item" error.
pub(crate) async fn run_by_exec(client: &mut BossClient, ctx: &RunContext, args: ByExecArgs) -> Result<(), CliError> {
    let execution = get_execution(client, &args.execution_id).await?;
    match execution.kind {
        ExecutionKind::AnswerAgent => {
            return Err(CliError::application(format!(
                "execution {} is an answer-agent run bound to comment {} (not a task/chore) — inspect it with \
                 `bossctl comments show {}`",
                execution.id, execution.work_item_id, execution.work_item_id
            )));
        }
        ExecutionKind::AutomationTriage => {
            return Err(CliError::application(format!(
                "execution {} is an automation-triage run bound to automation {} (not a task/chore) — inspect it \
                 with `boss automation show {}`",
                execution.id, execution.work_item_id, execution.work_item_id
            )));
        }
        _ => {}
    }
    let item = get_work_item(client, &execution.work_item_id).await?;
    match item {
        WorkItem::Product(product) => print_entity(
            ctx,
            &serde_json::json!({ "product": &product, "execution_id": execution.id }),
            || {
                print_product_details("Product", &product);
            },
        ),
        WorkItem::Project(project) => print_entity(
            ctx,
            &serde_json::json!({ "project": &project, "execution_id": execution.id }),
            || {
                print_project_details("Project", &project, None, false);
            },
        ),
        item => {
            let (task, label) = expect_leaf_work_item(item)?;
            let task = with_display_status(task);
            print_entity(
                ctx,
                &serde_json::json!({ label: &task, "execution_id": execution.id }),
                || {
                    print_task_details(label_titlecase(label), &task, None, false);
                },
            )
        }
    }
}

/// Decide what bind-pr should do given the prior `pr_url` value on
/// the work item. Pure function so it can be unit-tested without an
/// engine round-trip.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BindPrAction<'a> {
    /// pr_url already matches `new`: skip the engine round-trip and
    /// report no-op without printing a warning.
    Idempotent,
    /// pr_url is unset (or empty): apply the update silently.
    FirstTime,
    /// pr_url is set to a different value: apply the update and emit
    /// a stderr warning naming the old URL.
    Overwrite { previous: &'a str },
}

pub(crate) fn classify_bind_pr<'a>(prior: Option<&'a str>, new: &str) -> BindPrAction<'a> {
    match prior {
        Some(p) if p == new => BindPrAction::Idempotent,
        Some("") => BindPrAction::FirstTime,
        Some(p) => BindPrAction::Overwrite { previous: p },
        None => BindPrAction::FirstTime,
    }
}

/// Shared handler for `boss task bind-pr` and `boss chore bind-pr`.
/// The kind is read from the actual item, not the noun the user
/// typed, so either invocation works against any leaf work item id.
pub(crate) async fn run_bind_pr(client: &mut BossClient, ctx: &RunContext, args: BindPrArgs) -> Result<(), CliError> {
    let new_url = validate_github_pr_url(&args.pr_url)?;

    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, None).await?;
    let (existing, label) = expect_leaf_work_item(get_work_item(client, &resolved_id).await?)?;
    let prior_url = existing.pr_url.clone();

    match classify_bind_pr(prior_url.as_deref(), new_url) {
        BindPrAction::Idempotent => {
            let id_for_print = existing.id.clone();
            return print_entity(
                ctx,
                &serde_json::json!({
                    label: existing,
                    "rebinding": false,
                    "previous_pr_url": prior_url,
                }),
                || {
                    if !ctx.quiet {
                        println!("{} {} already bound to {}", label, id_for_print, new_url);
                    }
                },
            );
        }
        BindPrAction::Overwrite { previous } => {
            eprintln!(
                "warning: replacing existing PR URL on {} {} (was {}, now {})",
                label, existing.id, previous, new_url,
            );
        }
        BindPrAction::FirstTime => {}
    }

    let patch = WorkItemPatch {
        pr_url: Some(new_url.to_owned()),
        ..WorkItemPatch::default()
    };
    let (updated, _) = expect_leaf_work_item(update_work_item(client, &resolved_id, patch).await?)?;

    let title = format!("Bound PR to {label}");
    print_entity(
        ctx,
        &serde_json::json!({
            label: updated,
            "rebinding": prior_url.is_some(),
            "previous_pr_url": prior_url,
        }),
        || print_task_details(&title, &updated, None, false),
    )
}

/// Shared handler for `boss task link-external` and `boss chore link-external`.
pub(crate) async fn run_link_external(
    client: &mut BossClient,
    ctx: &RunContext,
    args: LinkExternalArgs,
) -> Result<(), CliError> {
    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, None).await?;
    let item = match client
        .send_request(&FrontendRequest::LinkWorkItemExternalRef {
            input: LinkExternalRefInput {
                work_item_id: resolved_id,
                kind: args.kind,
                canonical_id: args.upstream_id,
            },
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemUpdated { item } => item,
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            return Err(CliError::application(message));
        }
        other => return Err(unexpected_event("link-external", &other)),
    };
    let (updated, label) = expect_leaf_work_item(item)?;
    let title = format!("Linked external ref on {label}");
    print_entity(ctx, &serde_json::json!({ label: updated }), || {
        print_task_details(&title, &updated, None, false)
    })
}

/// Shared handler for `boss task unlink-external` and `boss chore unlink-external`.
pub(crate) async fn run_unlink_external(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskIdArg,
) -> Result<(), CliError> {
    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, None).await?;
    let item = match client
        .send_request(&FrontendRequest::UnlinkWorkItemExternalRef {
            work_item_id: resolved_id,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemUpdated { item } => item,
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            return Err(CliError::application(message));
        }
        other => return Err(unexpected_event("unlink-external", &other)),
    };
    let (updated, label) = expect_leaf_work_item(item)?;
    let title = format!("Unlinked external ref on {label}");
    print_entity(ctx, &serde_json::json!({ label: updated }), || {
        print_task_details(&title, &updated, None, false)
    })
}

/// One entry in a bulk-create input file. Mirrors the documented
/// schema: `name` and `description` are required; `autostart` and
/// `project_id` (tasks only) are optional per-item overrides of the
/// top-level CLI defaults. Unknown fields are rejected so a typo
/// doesn't silently drop data on the floor.
#[derive(Debug, Clone, serde::Deserialize, bon::Builder)]
#[builder(on(String, into))]
#[serde(deny_unknown_fields)]
pub(crate) struct BulkCreateItem {
    pub(crate) name: String,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) autostart: Option<bool>,
    #[serde(default)]
    pub(crate) project_id: Option<String>,
    /// Per-item priority override. Omitted → engine default
    /// (`medium`). Accepts the same `low` / `medium` / `high`
    /// vocabulary as the `--priority` flag.
    #[serde(default)]
    pub(crate) priority: Option<String>,
    /// Per-item prerequisites, declared atomically with creation (see
    /// `--depends-on`). Each entry is a selector for an *already
    /// existing* work item in the same product; intra-batch references
    /// (one item depending on another created in the same call) are not
    /// supported — create the prerequisites first. Omitted → no gate.
    #[serde(default)]
    pub(crate) depends_on: Vec<String>,
}

pub(crate) fn read_bulk_input(from_file: &str) -> Result<Vec<BulkCreateItem>, CliError> {
    let raw = if from_file == "-" {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .map_err(|err| CliError::usage(format!("failed to read stdin: {err}")))?;
        buf
    } else {
        std::fs::read_to_string(from_file)
            .map_err(|err| CliError::usage(format!("failed to read {from_file}: {err}")))?
    };
    let items: Vec<BulkCreateItem> = serde_json::from_str(&raw).map_err(|err| {
        CliError::usage(format!(
            "failed to parse {} as a JSON array of items (line {}, column {}): {}",
            display_input_source(from_file),
            err.line(),
            err.column(),
            err,
        ))
    })?;
    if items.is_empty() {
        return Err(CliError::usage(format!(
            "{} contained an empty array; nothing to create",
            display_input_source(from_file),
        )));
    }
    for (index, item) in items.iter().enumerate() {
        if item.name.trim().is_empty() {
            return Err(CliError::usage(format!(
                "item {index}: `name` is required and must not be empty"
            )));
        }
    }
    Ok(items)
}

pub(crate) fn display_input_source(from_file: &str) -> String {
    if from_file == "-" {
        "stdin".to_owned()
    } else {
        from_file.to_owned()
    }
}

pub(crate) async fn run_task_create_many(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskCreateManyArgs,
) -> Result<(), CliError> {
    let items = read_bulk_input(&args.from_file)?;

    // Resolve --product / --project once; per-item project_id (if
    // present) is treated as an already-resolved engine id so we
    // don't pay an extra round-trip per row.
    let product = resolve_product_inferable(client, args.product, args.project.as_deref(), ctx).await?;
    let default_project = match args.project {
        Some(selector) => Some(resolve_project(client, &product.id, Some(selector), ctx).await?),
        None => None,
    };

    let default_autostart = !ctx.no_autostart;

    let mut inputs = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        let project_id = match item.project_id {
            Some(id) => id,
            None => match default_project.as_ref() {
                Some(project) => project.id.clone(),
                None => {
                    return Err(CliError::usage(format!(
                        "item {index}: no project specified — pass --project as a default or set `project_id` on the item"
                    )));
                }
            },
        };
        let depends_on = resolve_depends_on(client, ctx, &item.depends_on, &product.id)
            .await
            .map_err(|err| CliError::usage(format!("item {index}: {err}")))?;
        inputs.push(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project_id)
                .name(item.name)
                .maybe_description(normalize_non_empty(Some(item.description)))
                .autostart(item.autostart.unwrap_or(default_autostart))
                .depends_on(depends_on)
                .maybe_priority(item.priority)
                .created_via(CREATED_VIA_CLI)
                .build(),
        );
    }

    let count = inputs.len();
    let created = create_many_tasks(client, CreateManyTasksInput { items: inputs }).await?;

    print_entity(
        ctx,
        &serde_json::json!({ "tasks": created, "count": created.len() }),
        || {
            if !ctx.quiet {
                println!("Created {} tasks:", created.len());
                print_tasks_table(&created, false);
            }
        },
    )?;
    debug_assert_eq!(created.len(), count);
    Ok(())
}

pub(crate) async fn run_chore_create_many(
    client: &mut BossClient,
    ctx: &RunContext,
    args: ChoreCreateManyArgs,
) -> Result<(), CliError> {
    let items = read_bulk_input(&args.from_file)?;
    let product = resolve_product(client, args.product, ctx).await?;
    let default_autostart = !ctx.no_autostart;

    let mut inputs = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        if item.project_id.is_some() {
            return Err(CliError::usage(format!(
                "item {index}: chores do not have a project — remove `project_id`"
            )));
        }
        let depends_on = resolve_depends_on(client, ctx, &item.depends_on, &product.id)
            .await
            .map_err(|err| CliError::usage(format!("item {index}: {err}")))?;
        inputs.push(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name(item.name)
                .maybe_description(normalize_non_empty(Some(item.description)))
                .autostart(item.autostart.unwrap_or(default_autostart))
                .depends_on(depends_on)
                .maybe_priority(item.priority)
                .created_via(CREATED_VIA_CLI)
                .build(),
        );
    }

    let created = create_many_chores(client, CreateManyChoresInput { items: inputs }).await?;
    print_entity(
        ctx,
        &serde_json::json!({ "chores": created, "count": created.len() }),
        || {
            if !ctx.quiet {
                println!("Created {} chores:", created.len());
                print_tasks_table(&created, false);
            }
        },
    )
}

pub(crate) async fn create_many_tasks(
    client: &mut BossClient,
    input: CreateManyTasksInput,
) -> Result<Vec<Task>, CliError> {
    handle_create_many_response(
        client
            .send_request(&FrontendRequest::CreateManyTasks { input })
            .await
            .map_err(CliError::internal)?,
        "tasks create-many",
        |item| match item {
            WorkItem::Task(t) => Ok(t),
            other => Err(CliError::conflict(format!(
                "engine returned non-task item in tasks batch: {:?}",
                std::mem::discriminant(&other),
            ))),
        },
    )
}

pub(crate) async fn create_many_chores(
    client: &mut BossClient,
    input: CreateManyChoresInput,
) -> Result<Vec<Task>, CliError> {
    handle_create_many_response(
        client
            .send_request(&FrontendRequest::CreateManyChores { input })
            .await
            .map_err(CliError::internal)?,
        "chores create-many",
        |item| match item {
            WorkItem::Chore(t) => Ok(t),
            other => Err(CliError::conflict(format!(
                "engine returned non-chore item in chores batch: {:?}",
                std::mem::discriminant(&other),
            ))),
        },
    )
}

/// Build the `CliError::conflict` returned when the engine rejects a create
/// with `WorkItemDuplicateBlocked`. Shared by the single-item create paths and
/// the batch path so their wording stays in sync; `prefix` supplies the leading
/// noun phrase (e.g. `"A task named"`) and `hint` the trailing `--force-duplicate`
/// guidance. Callers keep the emitted messages byte-identical to the inlined
/// versions they replaced.
pub(crate) fn duplicate_blocked_error(
    prefix: &str,
    hint: &str,
    existing_id: &str,
    existing_short_id: i64,
    name: &str,
    age_secs: i64,
) -> CliError {
    CliError::conflict(format!(
        "{prefix} {name:?} was created {age_secs}s ago as T{existing_short_id} \
         ({existing_id}); pass --force-duplicate {hint}"
    ))
}

pub(crate) fn handle_create_many_response<F>(
    event: FrontendEvent,
    context: &str,
    extract: F,
) -> Result<Vec<Task>, CliError>
where
    F: Fn(WorkItem) -> Result<Task, CliError>,
{
    match event {
        FrontendEvent::WorkItemsCreated { items } => items.into_iter().map(extract).collect(),
        FrontendEvent::WorkItemDuplicateBlocked {
            existing_id,
            existing_short_id,
            name,
            age_secs,
        } => Err(duplicate_blocked_error(
            "Batch rejected: an item named",
            "to bypass.",
            &existing_id,
            existing_short_id,
            &name,
            age_secs,
        )),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event(context, &other)),
    }
}

/// Accept only `https://github.com/<org>/<repo>/pull/<n>`. Returns the
/// trimmed canonical form on success.
pub(crate) fn validate_github_pr_url(raw: &str) -> Result<&str, CliError> {
    let trimmed = raw.trim();
    match github_app::pr_url::parse_pr_url_parts(trimmed) {
        Some(_) => Ok(trimmed),
        None => Err(CliError::usage(format!(
            "PR URL must be of the form https://github.com/<org>/<repo>/pull/<n>, got `{trimmed}`"
        ))),
    }
}

pub(crate) async fn delete_work_item(client: &mut BossClient, id: &str) -> Result<(), CliError> {
    rpc_call!(
        client,
        FrontendRequest::DeleteWorkItem { id: id.to_owned() },
        "work item delete",
        FrontendEvent::WorkItemDeleted { .. } => (),
    )
}

pub(crate) async fn restore_work_item(client: &mut BossClient, id: &str) -> Result<WorkItem, CliError> {
    rpc_call!(
        client,
        FrontendRequest::RestoreWorkItem { id: id.to_owned() },
        "work item restore",
        FrontendEvent::WorkItemRestored { item } => item,
    )
}

pub(crate) async fn run_depend_command(
    command: DependCommand,
    client: &mut BossClient,
    ctx: &RunContext,
) -> Result<(), CliError> {
    match command {
        DependCommand::Add(args) => {
            let dependent = resolve_selector_to_primary_id(client, ctx, &args.dependent, args.product.clone()).await?;
            let prerequisite = resolve_selector_to_primary_id(client, ctx, &args.prerequisite, args.product).await?;
            let edge = add_dependency(
                client,
                AddDependencyInput {
                    dependent,
                    prerequisite,
                    relation: Some(args.relation),
                },
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "edge": edge }), || {
                if !ctx.quiet {
                    println!(
                        "Declared dependency: {} → {} ({})",
                        edge.dependent_id, edge.prerequisite_id, edge.relation
                    );
                }
            })
        }
        DependCommand::Rm(args) => {
            let dependent = resolve_selector_to_primary_id(client, ctx, &args.dependent, args.product.clone()).await?;
            let prerequisite = resolve_selector_to_primary_id(client, ctx, &args.prerequisite, args.product).await?;
            let removed = remove_dependency(
                client,
                RemoveDependencyInput {
                    dependent: dependent.clone(),
                    prerequisite: prerequisite.clone(),
                    relation: Some(args.relation.clone()),
                },
            )
            .await?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "dependent_id": dependent,
                    "prerequisite_id": prerequisite,
                    "relation": args.relation,
                    "removed": removed,
                }),
                || {
                    if !ctx.quiet {
                        if removed {
                            println!("Removed dependency: {} → {}", dependent, prerequisite,);
                        } else {
                            println!("No dependency {} → {} (no-op)", dependent, prerequisite,);
                        }
                    }
                },
            )
        }
        DependCommand::List(args) => {
            let selector = resolve_selector_to_primary_id(client, ctx, &args.selector, args.product).await?;
            let view = list_dependencies(
                client,
                ListDependenciesInput {
                    work_item: selector,
                    direction: Some(args.direction.into()),
                },
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "dependencies": view }), || {
                print_dependency_view(&view);
            })
        }
    }
}

pub(crate) async fn add_dependency(
    client: &mut BossClient,
    input: AddDependencyInput,
) -> Result<WorkItemDependency, CliError> {
    rpc_call!(
        client,
        FrontendRequest::AddDependency { input },
        "dependency add",
        FrontendEvent::DependencyAdded { edge } => edge,
    )
}

pub(crate) async fn remove_dependency(client: &mut BossClient, input: RemoveDependencyInput) -> Result<bool, CliError> {
    rpc_call!(
        client,
        FrontendRequest::RemoveDependency { input },
        "dependency remove",
        FrontendEvent::DependencyRemoved { removed, .. } => removed,
    )
}

pub(crate) async fn list_dependencies(
    client: &mut BossClient,
    input: ListDependenciesInput,
) -> Result<WorkItemDependencyView, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListDependencies { input },
        "dependency list",
        FrontendEvent::DependencyList { view } => view,
    )
}

pub(crate) async fn list_dependencies_detailed(
    client: &mut BossClient,
    input: ListDependenciesInput,
) -> Result<WorkItemDependencyDetail, CliError> {
    rpc_call!(
        client,
        FrontendRequest::ListDependenciesDetailed { input },
        "dependency detail",
        FrontendEvent::DependencyDetail { detail } => detail,
    )
}

pub(crate) async fn list_executions_for_item(
    client: &mut BossClient,
    work_item_id: &str,
) -> Result<Vec<WorkExecution>, CliError> {
    match client
        .send_request(&FrontendRequest::ListExecutions {
            work_item_id: Some(work_item_id.to_owned()),
            include_revision_chain: false,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ExecutionsList { mut executions, .. } => {
            executions.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
            executions.truncate(20);
            Ok(executions)
        }
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("executions list", &other)),
    }
}

/// Fetch the open + resolved operational attention items filed against a
/// work item directly (`work_attention_items.work_item_id`) — e.g. the
/// `churn_guard_parked` item an orphan-sweep or pr_review-recovery churn
/// guard trip files when it stops auto-redispatching an `active` item with
/// no live execution. Distinct from the `boss attention` noun, which covers
/// the newer agent-authored question/followup store.
pub(crate) async fn list_attention_items_for_work_item(
    client: &mut BossClient,
    work_item_id: &str,
) -> Result<Vec<WorkAttentionItem>, CliError> {
    match client
        .send_request(&FrontendRequest::ListAttentionItemsForWorkItem {
            work_item_id: work_item_id.to_owned(),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AttentionItemsForWorkItemList { items, .. } => Ok(items),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("attention items for work item", &other)),
    }
}

/// Print the Attention section appended by `boss <kind> show` — the
/// operator-visible surface for engine-raised operational alerts (see
/// `list_attention_items_for_work_item`). Only open items are shown;
/// resolved ones still ride along in the `--json` output but would just be
/// noise here. Prints nothing when there are none, matching
/// `print_dependency_section` / `print_executions_section`.
pub(crate) fn print_attention_items_section(items: &[WorkAttentionItem]) {
    let open: Vec<&WorkAttentionItem> = items.iter().filter(|item| item.status == "open").collect();
    if open.is_empty() {
        return;
    }
    println!();
    println!("Attention ({}):", open.len());
    for item in &open {
        println!("  [{}] {} (since {})", item.kind, item.title, item.created_at);
    }
}

/// Print the newer attention-groups surface (`attention_groups`/`attentions`,
/// the store `boss attention list` reads) for a task's detail view. Keeps
/// `boss task show` a complete picture of what's pending against a task even
/// though writers like `Populator::finish` moved off the legacy
/// `work_attention_items` table onto this store — see
/// `list_attention_items_for_work_item` for the older, still-live surface
/// this section complements. `groups` is pre-filtered to open/partially
/// answered by the caller's query, so nothing here needs re-filtering.
pub(crate) fn print_attention_groups_section(groups: &[AttentionGroup]) {
    if groups.is_empty() {
        return;
    }
    println!();
    println!("Attention groups ({}):", groups.len());
    for g in groups {
        let short = g.short_id.map(|n| format!("A{n}")).unwrap_or_else(|| g.id.clone());
        println!("  [{short}] {} (kind={}, since {})", g.state, g.kind, g.created_at);
    }
}

pub(crate) async fn get_task_runtime(client: &mut BossClient, work_item_id: &str) -> Result<TaskRuntime, CliError> {
    rpc_call!(
        client,
        FrontendRequest::GetTaskRuntime {
            work_item_id: work_item_id.to_owned(),
        },
        "task runtime",
        FrontendEvent::TaskRuntimeResult { runtime } => runtime,
    )
}

pub(crate) fn print_executions_section(executions: &[WorkExecution]) {
    if executions.is_empty() {
        return;
    }
    println!();
    println!("Executions ({}):", executions.len());
    for exec in executions {
        let started = exec.started_at.as_deref().unwrap_or("-");
        let finished = exec.finished_at.as_deref().unwrap_or("-");
        print!(
            "  {} [{}] started={} finished={}",
            exec.id, exec.status, started, finished
        );
        if let Some(pr) = &exec.pr_url {
            print!(" pr={pr}");
        }
        println!();
    }
}

pub(crate) fn print_dependency_view(view: &WorkItemDependencyView) {
    println!("Dependencies for {}:", view.work_item_id);
    if view.prerequisites.is_empty() && view.dependents.is_empty() {
        println!("  (none)");
        return;
    }
    if !view.prerequisites.is_empty() {
        println!("  Prerequisites ({}):", view.prerequisites.len());
        for edge in &view.prerequisites {
            println!("    {} ({})", edge.prerequisite_id, edge.relation);
        }
    }
    if !view.dependents.is_empty() {
        println!("  Dependents ({}):", view.dependents.len());
        for edge in &view.dependents {
            println!("    {} ({})", edge.dependent_id, edge.relation);
        }
    }
}

/// Print the Dependencies section appended by `boss <kind> show`
/// (Q6). Empty input prints nothing — the surrounding `show` already
/// rendered the rest of the row, and a noisy "Dependencies: (none)"
/// every time would drown out the common case. The body is composed
/// via [`format_dependency_section`] so unit tests can assert on the
/// text without capturing stdout.
pub(crate) fn print_dependency_section(detail: &WorkItemDependencyDetail) {
    for line in format_dependency_section(detail) {
        println!("{line}");
    }
}

/// Pure-function renderer for the Dependencies section. Returns the
/// human-mode lines that [`print_dependency_section`] would emit.
/// Empty result when both sides are empty so the caller can detect
/// "nothing to show" without parsing strings.
pub(crate) fn format_dependency_section(detail: &WorkItemDependencyDetail) -> Vec<String> {
    if detail.prerequisites.is_empty() && detail.dependents.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    lines.push("Dependencies:".to_owned());
    if !detail.prerequisites.is_empty() {
        lines.push(format!("  Prerequisites ({}):", detail.prerequisites.len()));
        for edge in &detail.prerequisites {
            lines.push(format_dependency_edge_line(edge, true));
        }
    }
    if !detail.dependents.is_empty() {
        lines.push(format!("  Dependents ({}):", detail.dependents.len()));
        for edge in &detail.dependents {
            lines.push(format_dependency_edge_line(edge, false));
        }
    }
    lines
}

pub(crate) fn format_dependency_edge_line(edge: &DependencyEdge, mark_incomplete: bool) -> String {
    let name = if edge.name.is_empty() {
        String::new()
    } else {
        format!(" \"{}\"", edge.name)
    };
    let suffix = if mark_incomplete && !dependency_status_is_satisfied(&edge.id, &edge.status) {
        "  ← INCOMPLETE"
    } else {
        ""
    };
    // Projects carry their own status taxonomy (planned/active/…); only
    // task/chore edges share the board vocabulary, so remap just those.
    let status = if edge.id.starts_with("proj_") {
        edge.status.as_str()
    } else {
        status_vocab::to_ui(&edge.status)
    };
    format!("    {id:<32}  {status:<10}{name}{suffix}", id = edge.id,)
}

/// Whether `status` counts as "this prereq is no longer gating its
/// dependent." Mirrors the engine-side rule (Q4 / Q10): tasks /
/// chores satisfy on `done`; projects also satisfy on `archived`.
/// The dependent annotator uses the inverse to print
/// `← INCOMPLETE`.
pub(crate) fn dependency_status_is_satisfied(id: &str, status: &str) -> bool {
    if id.starts_with("proj_") {
        matches!(status, "done" | "archived")
    } else {
        status == "done"
    }
}

pub(crate) async fn reorder_project_tasks(
    client: &mut BossClient,
    project_id: &str,
    task_ids: &[String],
) -> Result<(), CliError> {
    rpc_call!(
        client,
        FrontendRequest::ReorderProjectTasks {
            project_id: project_id.to_owned(),
            task_ids: task_ids.to_vec(),
        },
        "task reorder",
        FrontendEvent::ProjectTasksReordered { .. } => (),
    )
}

pub(crate) async fn resolve_product(
    client: &mut BossClient,
    selector: Option<String>,
    ctx: &RunContext,
) -> Result<Product, CliError> {
    let products = list_products(client).await?;
    if products.is_empty() {
        return Err(CliError::not_found("no products exist"));
    }

    let selector = match selector {
        Some(selector) => selector,
        None if products.len() == 1 => return Ok(products[0].clone()),
        None if ctx.allow_input => choose_product(&products)?,
        None => {
            return Err(CliError::usage(
                "product is required; pass --product or run interactively",
            ));
        }
    };

    match_products(&products, &selector)
}

/// Like [`resolve_product`] but returns `None` when no `--product` was
/// supplied and resolution is not needed (e.g. when the caller is about
/// to use a canonical `auto_…` id directly). Only resolves the product
/// when a `--product` flag is supplied or when there is exactly one
/// product (auto-selected). Does NOT prompt interactively.
pub(crate) async fn resolve_optional_product(
    client: &mut BossClient,
    selector: Option<String>,
    _ctx: &RunContext,
) -> Result<Option<Product>, CliError> {
    match selector {
        None => {
            // Try auto-select when exactly one product exists, so A<n>
            // selectors work without --product on single-product setups.
            let products = list_products(client).await?;
            if products.len() == 1 {
                Ok(Some(products.into_iter().next().unwrap()))
            } else {
                Ok(None)
            }
        }
        Some(sel) => {
            let products = list_products(client).await?;
            if products.is_empty() {
                return Err(CliError::not_found("no products exist"));
            }
            Ok(Some(match_products(&products, &sel)?))
        }
    }
}

/// True when `s` looks like a typed engine work-item id. The engine
/// stamps `prod_…` on products, `proj_…` on projects, and `task_…` on
/// both tasks and chores (chores share the task prefix at the row
/// level, so we don't enumerate `chore_` separately). Slugs are short
/// names like `boss` / `mono` and never collide with these prefixes
/// in practice.
pub(crate) fn is_typed_work_item_id(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("prod_") || s.starts_with("proj_") || s.starts_with("task_")
}

/// Parsed form of a task/chore/project selector.
///
/// Priority order per design Q5 (extended with friendly-id prefix forms):
/// 1. `#42` or `42` or `T441`/`t441`/`P7`/`p7` → short id
/// 2. `boss/42` or `boss/#42` → cross-product short id
/// 3. `task_…` / `proj_…` / `prod_…` → primary id (typed)
/// 4. anything else → slug / existing resolution
#[derive(Debug, Clone)]
pub(crate) enum WorkItemSelector {
    /// `42` or `#42` — short id within the context product.
    ShortId(i64),
    /// `boss/42` or `boss/#42` — short id in the named product slug.
    ProductShortId { product_slug: String, n: i64 },
    /// `task_…` / `proj_…` / `prod_…` — primary engine id.
    PrimaryId(String),
    /// Slug or other selector; fall through to existing resolution.
    Other(String),
}

/// Parse `s` into a [`WorkItemSelector`] per design Q5 grammar.
pub(crate) fn parse_work_item_selector(s: &str) -> WorkItemSelector {
    let s = s.trim();
    // Cross-product form: "boss/42" or "boss/#42"
    if let Some(slash) = s.find('/') {
        let product_slug = &s[..slash];
        let rest = s[slash + 1..].trim_start_matches('#');
        if !product_slug.is_empty()
            && let Ok(n) = rest.parse::<i64>()
            && n > 0
        {
            return WorkItemSelector::ProductShortId {
                product_slug: product_slug.to_owned(),
                n,
            };
        }
    }
    // `#42` form (explicit friendly-id prefix)
    if let Some(rest) = s.strip_prefix('#')
        && let Ok(n) = rest.parse::<i64>()
        && n > 0
    {
        return WorkItemSelector::ShortId(n);
    }
    // `T441` / `t441` / `P12` / `p12` — friendly kanban id (T for tasks/chores, P for projects).
    // Case-insensitive; the leading letter is just visual sugar for the short_id number.
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        if (first == b'T' || first == b't' || first == b'P' || first == b'p')
            && let Ok(n) = s[1..].parse::<i64>()
            && n > 0
        {
            return WorkItemSelector::ShortId(n);
        }
    }
    // Plain integer → short id (Q5 step 2: `#` is optional)
    if let Ok(n) = s.parse::<i64>()
        && n > 0
    {
        return WorkItemSelector::ShortId(n);
    }
    // Primary id prefixes
    if is_typed_work_item_id(s) {
        return WorkItemSelector::PrimaryId(s.to_owned());
    }
    WorkItemSelector::Other(s.to_owned())
}

/// Call the engine's `GetWorkItemByShortId` RPC and return the result.
pub(crate) async fn get_work_item_by_short_id_rpc(
    client: &mut BossClient,
    product_id: &str,
    short_id: i64,
) -> Result<WorkItem, CliError> {
    rpc_call!(
        client,
        FrontendRequest::GetWorkItemByShortId {
            product_id: product_id.to_owned(),
            short_id,
        },
        "work item fetch by short id",
        FrontendEvent::WorkItemResult { item } => item,
    )
}

/// Resolve a short-id selector to its work item. `product` is the product
/// source: the caller's `--product` flag for a bare `ShortId`, or
/// `Some(product_slug)` for a cross-product `ProductShortId`. Shared by
/// [`run_show_leaf`] and [`resolve_selector_to_primary_id`], which differ
/// only in the per-site post-processing left at their call sites.
pub(crate) async fn resolve_short_id_item(
    client: &mut BossClient,
    ctx: &RunContext,
    product: Option<String>,
    short_id: i64,
) -> Result<WorkItem, CliError> {
    let product = resolve_product(client, product, ctx).await?;
    get_work_item_by_short_id_rpc(client, &product.id, short_id).await
}

/// Resolve any selector form (friendly `T441`, `#42`, plain `42`,
/// cross-product `boss/42`, or primary `task_…` id) to a primary engine
/// id. If the selector is already a primary id or an opaque slug, it is
/// returned unchanged so the engine can reject it with its own error.
/// Resolve a list of `--depends-on` selectors to canonical work-item
/// ids for the create-time dependency channel. Each selector resolves
/// within `product` (so bare `T<n>` short ids work), matching how
/// `depend add` resolves its endpoints. The resolved canonical ids
/// travel in `CreateTaskInput::depends_on` / `CreateChoreInput::depends_on`
/// and become `blocks` edges in the same transaction as the row insert.
pub(crate) async fn resolve_depends_on(
    client: &mut BossClient,
    ctx: &RunContext,
    selectors: &[String],
    product: &str,
) -> Result<Vec<String>, CliError> {
    let mut resolved = Vec::with_capacity(selectors.len());
    for selector in selectors {
        let trimmed = selector.trim();
        if trimmed.is_empty() {
            continue;
        }
        resolved.push(resolve_selector_to_primary_id(client, ctx, trimmed, Some(product.to_owned())).await?);
    }
    Ok(resolved)
}

/// Resolve `create-revision`'s `--depends-on` selectors to canonical ids
/// without a product context, mirroring `resolve_depends_on` but reusing
/// [`resolve_create_revision_parent`]'s product-free resolution since
/// `create-revision` takes no `--product` flag.
pub(crate) async fn resolve_revision_depends_on(
    client: &mut BossClient,
    selectors: &[String],
) -> Result<Vec<String>, CliError> {
    let mut resolved = Vec::with_capacity(selectors.len());
    for selector in selectors {
        let trimmed = selector.trim();
        if trimmed.is_empty() {
            continue;
        }
        resolved.push(resolve_create_revision_parent(client, trimmed).await?);
    }
    Ok(resolved)
}

pub(crate) async fn resolve_selector_to_primary_id(
    client: &mut BossClient,
    ctx: &RunContext,
    id: &str,
    product: Option<String>,
) -> Result<String, CliError> {
    match parse_work_item_selector(id) {
        WorkItemSelector::ShortId(n) => {
            let item = resolve_short_id_item(client, ctx, product, n).await?;
            Ok(item.primary_id().to_owned())
        }
        WorkItemSelector::ProductShortId { product_slug, n } => {
            let item = resolve_short_id_item(client, ctx, Some(product_slug), n).await?;
            Ok(item.primary_id().to_owned())
        }
        WorkItemSelector::PrimaryId(id) | WorkItemSelector::Other(id) => Ok(id),
    }
}

/// If `selector` is a typed engine work-item id, look it up and return
/// its product id. Returns `Ok(None)` when the selector isn't shaped
/// like a typed id; callers then fall back to slug / interactive
/// resolution against the existing [`resolve_product`] path.
pub(crate) async fn product_id_from_typed_selector(
    client: &mut BossClient,
    selector: &str,
) -> Result<Option<String>, CliError> {
    let trimmed = selector.trim();
    if !is_typed_work_item_id(trimmed) {
        return Ok(None);
    }
    let item = get_work_item(client, trimmed).await?;
    let product_id = match item {
        WorkItem::Product(p) => p.id,
        WorkItem::Project(p) => p.product_id,
        WorkItem::Task(t) | WorkItem::Chore(t) => t.product_id,
    };
    Ok(Some(product_id))
}

/// Pure validator extracted so the mismatch-handling can be unit-tested
/// without an engine. When `explicit` is `Some`, it must resolve to the
/// same product as `inferred_id`; on mismatch we return a usage error
/// that names both sides so the user can drop the redundant flag.
pub(crate) fn ensure_explicit_product_matches(
    products: &[Product],
    explicit: Option<&str>,
    inferred_id: &str,
    id_hint: &str,
) -> Result<(), CliError> {
    let Some(explicit) = explicit else {
        return Ok(());
    };
    let chosen = match_products(products, explicit)?;
    if chosen.id != inferred_id {
        return Err(CliError::usage(format!(
            "--product {explicit} resolves to {chosen} but {id_hint} belongs to {inferred_id} — drop the redundant --product flag",
            chosen = chosen.id,
        )));
    }
    Ok(())
}

/// Variant of [`resolve_product`] that infers the product from a
/// globally-unique typed work-item id (`proj_…` / `task_…` / `prod_…`)
/// already on the command line. When both an explicit `--product` and
/// a typed-id hint are supplied, the resolved products must agree —
/// mismatches surface as a usage error so the caller can drop the
/// redundant flag.
pub(crate) async fn resolve_product_inferable(
    client: &mut BossClient,
    explicit: Option<String>,
    typed_id_hint: Option<&str>,
    ctx: &RunContext,
) -> Result<Product, CliError> {
    let inferred_id = match typed_id_hint {
        Some(id) => product_id_from_typed_selector(client, id).await?,
        None => None,
    };

    let Some(inferred_id) = inferred_id else {
        return resolve_product(client, explicit, ctx).await;
    };

    let products = list_products(client).await?;
    let inferred = products.iter().find(|p| p.id == inferred_id).cloned().ok_or_else(|| {
        CliError::not_found(format!(
            "id {hint} references product {inferred_id}, but no such product exists",
            hint = typed_id_hint.unwrap_or("(typed id)"),
        ))
    })?;

    ensure_explicit_product_matches(
        &products,
        explicit.as_deref(),
        &inferred.id,
        typed_id_hint.unwrap_or("(typed id)"),
    )?;
    Ok(inferred)
}

pub(crate) async fn resolve_project(
    client: &mut BossClient,
    product_id: &str,
    selector: Option<String>,
    ctx: &RunContext,
) -> Result<Project, CliError> {
    let projects = list_projects(client, product_id, None).await?;
    if projects.is_empty() {
        return Err(CliError::not_found("no projects exist for the selected product"));
    }

    let selector = match selector {
        Some(selector) => selector,
        None if projects.len() == 1 => return Ok(projects[0].clone()),
        None if ctx.allow_input => choose_project(&projects)?,
        None => {
            return Err(CliError::usage(
                "project is required; pass --project or run interactively",
            ));
        }
    };

    match_projects(&projects, &selector)
}

pub(crate) fn match_products(products: &[Product], selector: &str) -> Result<Product, CliError> {
    if let Some(product) = pick_by_index(products, selector)? {
        return Ok(product);
    }

    let matches = products
        .iter()
        .filter(|product| product.id == selector || product.slug == selector)
        .cloned()
        .collect::<Vec<_>>();
    resolve_single_match(matches, format!("unknown product: {selector}"))
}

pub(crate) fn match_projects(projects: &[Project], selector: &str) -> Result<Project, CliError> {
    // Short id form: "42" or "#42" → match by short_id.
    // This takes priority over the 1-based index so that `boss project show 42`
    // consistently means "the project with short_id 42" everywhere.
    if let WorkItemSelector::ShortId(n) = parse_work_item_selector(selector) {
        let matches = projects
            .iter()
            .filter(|p| p.short_id == Some(n))
            .cloned()
            .collect::<Vec<_>>();
        return resolve_single_match(matches, format!("no project with id #{n}"));
    }

    if let Some(project) = pick_by_index(projects, selector)? {
        return Ok(project);
    }

    let matches = projects
        .iter()
        .filter(|project| project.id == selector || project.slug == selector)
        .cloned()
        .collect::<Vec<_>>();
    resolve_single_match(matches, format!("unknown project: {selector}"))
}

pub(crate) fn resolve_single_match<T>(matches: Vec<T>, not_found_message: String) -> Result<T, CliError> {
    match matches.len() {
        0 => Err(CliError::not_found(not_found_message)),
        1 => Ok(matches.into_iter().next().expect("len checked")),
        _ => Err(CliError::conflict("selector resolved to multiple work items")),
    }
}

pub(crate) fn pick_by_index<T: Clone>(items: &[T], selector: &str) -> Result<Option<T>, CliError> {
    let Ok(index) = selector.parse::<usize>() else {
        return Ok(None);
    };
    if !(1..=items.len()).contains(&index) {
        return Err(CliError::usage(format!(
            "selection {index} is out of range; choose a value between 1 and {}",
            items.len()
        )));
    }
    Ok(Some(items[index - 1].clone()))
}

pub(crate) fn choose_product(products: &[Product]) -> Result<String, CliError> {
    println!("Select a product:");
    for (index, product) in products.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, product.name, product.slug);
    }
    prompt_index_or_selector("Product", products.len()).map_err(CliError::internal)
}

pub(crate) fn choose_project(projects: &[Project]) -> Result<String, CliError> {
    println!("Select a project:");
    for (index, project) in projects.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, project.name, project.slug);
    }
    prompt_index_or_selector("Project", projects.len()).map_err(CliError::internal)
}

pub(crate) fn required_text(value: Option<String>, label: &str, ctx: &RunContext) -> Result<String, CliError> {
    if let Some(value) = normalize_non_empty(value) {
        return Ok(value);
    }
    if !ctx.allow_input {
        return Err(CliError::usage(format!(
            "{label} is required; pass it explicitly or omit --no-input"
        )));
    }
    loop {
        let input = prompt_text(label, None).map_err(CliError::internal)?;
        if let Some(value) = normalize_non_empty(Some(input)) {
            return Ok(value);
        }
        eprintln!("{label} cannot be empty.");
    }
}

pub(crate) fn optional_text(value: Option<String>, label: &str, ctx: &RunContext) -> Result<Option<String>, CliError> {
    if value.is_some() || !ctx.allow_input {
        return Ok(normalize_non_empty(value));
    }
    let input = prompt_text(label, Some("")).map_err(CliError::internal)?;
    Ok(normalize_non_empty(Some(input)))
}

pub(crate) fn prompt_text(label: &str, default: Option<&str>) -> Result<String> {
    let mut stdout = io::stdout();
    match default {
        Some(default) if !default.is_empty() => write!(stdout, "{label} [{default}]: ")?,
        _ => write!(stdout, "{label}: ")?,
    }
    stdout.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim_end().to_owned();
    if input.is_empty() {
        Ok(default.unwrap_or_default().to_owned())
    } else {
        Ok(input)
    }
}

pub(crate) fn prompt_index_or_selector(label: &str, count: usize) -> Result<String> {
    loop {
        let input = prompt_text(label, None)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            eprintln!("{label} cannot be empty.");
            continue;
        }
        if let Ok(index) = trimmed.parse::<usize>() {
            if (1..=count).contains(&index) {
                return Ok(index.to_string());
            }
            eprintln!("{label} must be between 1 and {count}.");
            continue;
        }
        return Ok(trimmed.to_owned());
    }
}

/// Glue together the name and optional description into the
/// "prompt text" fed to the repo parser. Mirrors what the engine
/// will eventually see as the chore's contents; the parser only does
/// case-insensitive substring search, so the simple `name\n\ndesc`
/// shape is exactly enough.
pub(crate) fn compose_prompt_text(name: &str, description: Option<&str>) -> String {
    match description.and_then(|d| {
        let trimmed = d.trim();
        if trimmed.is_empty() { None } else { Some(d) }
    }) {
        Some(desc) => format!("{name}\n\n{desc}"),
        None => name.to_owned(),
    }
}

/// Reject `--driver <non-claude> --model <claude-slug>` combinations at CLI
/// parse time (agent-driver design §Mix-and-match). A Claude-specific slug is
/// one that starts with `"claude-"` or is one of the family aliases
/// `"opus"`, `"sonnet"`, `"haiku"`. When the driver is `"claude"` (or absent,
/// which resolves to `"claude"`), any model slug is accepted.
pub(crate) fn validate_driver_model_pair(driver: Option<&str>, model: Option<&str>) -> Result<(), CliError> {
    let Some(d) = driver else { return Ok(()) };
    let d_lower = d.trim().to_ascii_lowercase();
    if d_lower == "claude" || d_lower.is_empty() {
        return Ok(());
    }
    let Some(m) = model else { return Ok(()) };
    let m_lower = m.trim().to_ascii_lowercase();
    let is_claude_slug = m_lower.starts_with("claude-")
        || m_lower == "opus"
        || m_lower == "sonnet"
        || m_lower == "haiku"
        || m_lower == "fable";
    if is_claude_slug {
        return Err(CliError::usage(format!(
            "model slug `{m}` is for the Claude driver but `--driver {d}` was specified; \
             pass a model slug valid for `{d}` or omit `--model`"
        )));
    }
    Ok(())
}

pub(crate) fn normalize_non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

pub(crate) fn ensure_patch_present(patch: &WorkItemPatch, message: &str) -> Result<(), CliError> {
    let has_fields = patch.name.is_some()
        || patch.description.is_some()
        || patch.status.is_some()
        || patch.goal.is_some()
        || patch.priority.is_some()
        || patch.repo_remote_url.is_some()
        || patch.pr_url.is_some()
        || patch.ordinal.is_some()
        || patch.effort_level.is_some()
        || patch.model_override.is_some()
        || patch.driver.is_some()
        || patch.default_model.is_some()
        || patch.dispatch_preamble.is_some()
        || patch.worker_branch_prefix.is_some()
        || patch.autostart.is_some()
        || patch.blocked_reason.is_some()
        || patch.blocked_detail.is_some();

    if has_fields {
        Ok(())
    } else {
        Err(CliError::usage(message))
    }
}

pub(crate) fn expect_product(item: WorkItem) -> Result<Product, CliError> {
    match item {
        WorkItem::Product(product) => Ok(product),
        _ => Err(CliError::conflict("work item is not a product")),
    }
}

pub(crate) fn expect_project(item: WorkItem) -> Result<Project, CliError> {
    match item {
        WorkItem::Project(project) => Ok(project),
        _ => Err(CliError::conflict("work item is not a project")),
    }
}

pub(crate) fn expect_task(item: WorkItem) -> Result<Task, CliError> {
    match item {
        WorkItem::Task(task) => Ok(task),
        WorkItem::Chore(_) => Err(CliError::conflict("work item is a chore, not a task")),
        _ => Err(CliError::conflict("work item is not a task")),
    }
}

pub(crate) fn expect_chore(item: WorkItem) -> Result<Task, CliError> {
    match item {
        WorkItem::Chore(task) => Ok(task),
        WorkItem::Task(_) => Err(CliError::conflict("work item is a task, not a chore")),
        _ => Err(CliError::conflict("work item is not a chore")),
    }
}

/// Permissive counterpart of [`expect_task`] / [`expect_chore`]: the
/// kind-agnostic verbs (`show`, `update`, `move`, `delete`, `bind-pr`)
/// accept any leaf work item, so they unwrap the inner [`Task`] and
/// return the kind label (`"task"` or `"chore"`) for user-facing
/// labelling. Products and projects still error — those have their
/// own command surface.
pub(crate) fn expect_leaf_work_item(item: WorkItem) -> Result<(Task, &'static str), CliError> {
    match item {
        WorkItem::Task(task) => Ok((task, "task")),
        WorkItem::Chore(task) => Ok((task, "chore")),
        WorkItem::Product(_) | WorkItem::Project(_) => Err(CliError::conflict(
            "work item is not a task or chore (use `boss product`/`boss project` for those kinds)",
        )),
    }
}

pub(crate) fn unexpected_event(context: &str, event: &FrontendEvent) -> CliError {
    // A worker-tier refusal can arrive in reply to *any* verb — the engine's
    // gate runs ahead of dispatch across the whole surface — so it reaches
    // the hand-rolled `match` sites that don't go through `rpc_call!` as
    // well. Rendering it here rather than adding an arm to each of those
    // means one place knows how, and none of them can forget: a denial is an
    // application error with an actionable message, not an engine bug worth
    // dumping raw JSON for.
    if let FrontendEvent::WorkerTierDenied { denial } = event {
        return CliError::application(denial.message.clone());
    }
    CliError::internal(anyhow::anyhow!(
        "unexpected engine event for {context}: {}",
        serde_json::to_string(event).unwrap_or_else(|_| "<unserializable>".to_owned())
    ))
}
