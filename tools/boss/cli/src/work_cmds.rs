//! product / project / task / chore / comment / github command handlers
//!
//! Extracted from the former monolithic `main.rs` (mechanical split; behavior unchanged).

use crate::*;

pub(crate) async fn run_product_command(command: ProductCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        ProductCommand::Create(args) => {
            let name = required_text(args.name, "Product name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let repo_remote_url = optional_text(args.repo_remote_url, "Repo remote URL", ctx)?;
            let design_repo = args.design_repo;
            let docs_repo = args.docs_repo;

            let product = create_product(
                &mut client,
                CreateProductInput::builder()
                    .name(name)
                    .maybe_description(description)
                    .maybe_repo_remote_url(repo_remote_url)
                    .maybe_design_repo(design_repo)
                    .maybe_docs_repo(docs_repo)
                    .maybe_worker_branch_prefix(args.worker_branch_prefix)
                    .build(),
            )
            .await?;

            print_entity(ctx, &serde_json::json!({ "product": product }), || {
                print_product_details("Created product", &product);
            })
        }
        ProductCommand::List => {
            let products = list_products(&mut client).await?;
            print_entity(ctx, &serde_json::json!({ "products": products }), || {
                print_products_table(&products);
            })
        }
        ProductCommand::Show(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            print_entity(ctx, &serde_json::json!({ "product": product }), || {
                print_product_details("Product", &product);
            })
        }
        ProductCommand::Update(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                name: args.name,
                description: args.description,
                status: args.status.map(|status| status.as_str().to_owned()),
                repo_remote_url: args.repo_remote_url,
                design_repo: args.design_repo,
                docs_repo: args.docs_repo,
                dispatch_preamble: args.dispatch_preamble,
                worker_branch_prefix: args.worker_branch_prefix,
                ..WorkItemPatch::default()
            };
            ensure_patch_present(
                &patch,
                "provide at least one field to update, such as --name or --status",
            )?;
            let item = update_work_item(&mut client, &product.id, patch).await?;
            let product = expect_product(item)?;
            print_entity(ctx, &serde_json::json!({ "product": product }), || {
                print_product_details("Updated product", &product);
            })
        }
        ProductCommand::Delete(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(ProductStatus::Archived.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let archived = expect_product(update_work_item(&mut client, &product.id, patch).await?)?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "product": archived,
                    "deleted": true,
                    "archived": true,
                }),
                || {
                    if !ctx.quiet {
                        println!(
                            "Archived product {} ({}) — products are not hard-deleted.",
                            archived.name, archived.slug,
                        );
                    }
                },
            )
        }
        ProductCommand::Move(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(args.target.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let moved = expect_product(update_work_item(&mut client, &product.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "product": moved }), || {
                print_product_details("Moved product", &moved);
            })
        }
        ProductCommand::SetDefaultModel(args) => {
            if !args.unset && args.model.is_none() {
                return Err(CliError::usage("provide either --model <slug> or --unset"));
            }
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let model = if args.unset { None } else { args.model };
            let updated = set_product_default_model(&mut client, &product.id, model).await?;
            print_entity(ctx, &serde_json::json!({ "product": updated }), || {
                print_product_details("Updated product", &updated);
            })
        }
        ProductCommand::SetDefaultDriver(args) => {
            if !args.unset && args.driver.is_none() {
                return Err(CliError::usage("provide either --driver <slug> or --unset"));
            }
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let driver = if args.unset { None } else { args.driver };
            let updated = set_product_default_driver(&mut client, &product.id, driver).await?;
            print_entity(ctx, &serde_json::json!({ "product": updated }), || {
                print_product_details("Updated product", &updated);
            })
        }
        ProductCommand::SetMergeMechanism(args) => {
            if !args.unset && args.mechanism.is_none() {
                return Err(CliError::usage(
                    "provide either --mechanism <direct|trunk_queue> or --unset",
                ));
            }
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let mechanism = if args.unset { None } else { args.mechanism };
            let updated = set_product_merge_mechanism(&mut client, &product.id, mechanism).await?;
            print_entity(ctx, &serde_json::json!({ "product": updated }), || {
                print_product_details("Updated product", &updated);
            })
        }
        ProductCommand::AuditEffort(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let response = client
                .send_request(&FrontendRequest::AuditProductEffort {
                    product_id: product.id.clone(),
                    window_days: args.window_days,
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::EffortAuditReport { report } => {
                    print_entity(ctx, &serde_json::json!({ "report": report }), || {
                        print_effort_audit_report(&report)
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("product audit-effort", &other)),
            }
        }
        ProductCommand::SetEditorialRules(args) => {
            if !args.unset && args.from_file.is_none() {
                return Err(CliError::usage("provide either --from-file <path> or --unset"));
            }
            let selector = args.selector.clone();
            let product = resolve_product(&mut client, Some(selector), ctx).await?;
            let rules: Option<EditorialRules> = if args.unset {
                None
            } else {
                let path = args.from_file.as_ref().unwrap();
                let contents = std::fs::read_to_string(path)
                    .map_err(|e| CliError::usage(format!("could not read {}: {e}", path.display())))?;
                let parsed: EditorialRules = serde_json::from_str(&contents)
                    .map_err(|e| CliError::usage(format!("invalid EditorialRules JSON in {}: {e}", path.display())))?;
                Some(parsed)
            };
            let input = SetProductEditorialRulesInput {
                product_id: product.id.clone(),
                rules,
            };
            let response = client
                .send_request(&FrontendRequest::SetProductEditorialRules { input })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::WorkItemUpdated { item } => {
                    let updated = expect_product(item)?;
                    print_entity(ctx, &serde_json::json!({ "product": updated }), || {
                        if args.unset {
                            if !ctx.quiet {
                                println!("Editorial rules cleared from product {}.", updated.slug);
                            }
                        } else {
                            print_product_details("Updated product", &updated);
                        }
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("product set-editorial-rules", &other)),
            }
        }
        ProductCommand::SetExternalTracker(args) => {
            if !args.unset && args.kind.is_none() {
                return Err(CliError::usage(
                    "provide either --kind (with kind-specific flags) or --unset",
                ));
            }
            let selector = args.selector.clone();
            let product = resolve_product(&mut client, Some(selector), ctx).await?;
            let (kind, config) = if args.unset {
                (None, None)
            } else {
                let kind = args.kind.as_deref().unwrap_or("github").to_owned();
                let config = build_external_tracker_config(&kind, &args)?;
                (Some(kind), Some(config))
            };
            let input = SetProductExternalTrackerInput {
                product_id: product.id.clone(),
                kind,
                config,
                unset: args.unset,
            };
            let response = client
                .send_request(&FrontendRequest::SetProductExternalTracker { input })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::WorkItemUpdated { item } => {
                    let updated = expect_product(item)?;
                    print_entity(ctx, &serde_json::json!({ "product": updated }), || {
                        if args.unset {
                            if !ctx.quiet {
                                println!("External tracker binding removed from product {}.", updated.slug);
                            }
                        } else {
                            print_product_details("Updated product", &updated);
                        }
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("product set-external-tracker", &other)),
            }
        }
        ProductCommand::SyncExternalTracker(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let response = client
                .send_request(&FrontendRequest::SyncProductExternalTracker {
                    product_id: product.id.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ExternalTrackerSyncStarted { product_id } => print_entity(
                    ctx,
                    &serde_json::json!({ "product_id": product_id, "synced": true }),
                    || {
                        if !ctx.quiet {
                            println!("External tracker sync complete for product {}.", product.slug);
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("product sync-external-tracker", &other)),
            }
        }
    }
}

pub(crate) async fn run_project_command(command: ProjectCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        ProjectCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let name = required_text(args.name, "Project name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let goal = optional_text(args.goal, "Goal", ctx)?;

            let project = create_project(
                &mut client,
                CreateProjectInput {
                    product_id: product.id.clone(),
                    name,
                    description,
                    goal,
                    // Project creation auto-creates a `kind = 'design'`
                    // task as the project's first row. The global
                    // `--no-autostart` flag, which already gates
                    // chore/task auto-dispatch, now also gates that
                    // design task so a single mental model covers
                    // every work-item kind.
                    autostart: !ctx.no_autostart,
                    no_design_task: args.no_design_task,
                },
            )
            .await?;

            // Surface the auto-spawned `kind=design` seed task so
            // callers (notably the coordinator) can write a design
            // brief onto it without a follow-up `task list` call.
            // `create_project` inserts the design task in the same
            // sqlite transaction, so it's always present by the
            // time we get the project back.
            let design_task = list_tasks(&mut client, &product.id, Some(&project.id), None, false)
                .await?
                .into_iter()
                .find(|t| t.kind == boss_protocol::TaskKind::Design)
                .map(with_display_status);

            print_entity(
                ctx,
                &serde_json::json!({
                    "project": project,
                    "design_task": design_task,
                }),
                || {
                    print_project_details("Created project", &project, None, false);
                    if let Some(task) = design_task.as_ref() {
                        println!(
                            "Design task: {} (autostart={}, status={})",
                            task.id, task.autostart, task.status
                        );
                    }
                },
            )
        }
        ProjectCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let dep_filter = args.dep.into_filter();
            let repo_selector = args.repo.as_deref().map(RepoSelector::parse).transpose()?;
            let projects = list_projects(&mut client, &product.id, dep_filter).await?;
            let projects = apply_project_list_filters(
                projects,
                &args.status,
                args.match_term.as_deref(),
                &args.id,
                args.limit,
                repo_selector.as_ref(),
                product.repo_remote_url.as_deref(),
            );
            print_entity(
                ctx,
                &serde_json::json!({ "product": product, "projects": projects }),
                || print_projects_table(&projects, args.with_primary_id),
            )
        }
        ProjectCommand::Show(args) => {
            let with_primary_id = args.with_primary_id;
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let detail = list_dependencies_detailed(
                &mut client,
                ListDependenciesInput {
                    work_item: project.id.clone(),
                    direction: Some(DependencyDirection::Both),
                },
            )
            .await?;
            let design_doc = resolve_project_design_doc(&mut client, &project.id).await?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "project": project,
                    "dependencies": detail,
                    "design_doc": design_doc,
                }),
                || {
                    print_project_details("Project", &project, Some(&product), with_primary_id);
                    if let Some(line) = format_project_design_doc_line(&design_doc.state) {
                        println!("Design doc: {line}");
                    }
                    print_dependency_section(&detail);
                },
            )
        }
        ProjectCommand::Update(args) => {
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                name: args.name,
                description: args.description,
                goal: args.goal,
                status: args.status.map(|status| status.as_str().to_owned()),
                priority: args.priority.map(|priority| priority.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            ensure_patch_present(
                &patch,
                "provide at least one field to update, such as --goal or --priority",
            )?;
            let item = update_work_item(&mut client, &project.id, patch).await?;
            let project = expect_project(item)?;
            print_entity(ctx, &serde_json::json!({ "project": project }), || {
                print_project_details("Updated project", &project, None, false);
            })
        }
        ProjectCommand::Delete(args) => {
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(ProjectStatusArg::Archived.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let archived = expect_project(update_work_item(&mut client, &project.id, patch).await?)?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "project": archived,
                    "deleted": true,
                    "archived": true,
                }),
                || {
                    if !ctx.quiet {
                        println!(
                            "Archived project {} ({}) — projects are not hard-deleted.",
                            archived.name, archived.slug,
                        );
                    }
                },
            )
        }
        ProjectCommand::Move(args) => {
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(args.target.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let moved = expect_project(update_work_item(&mut client, &project.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "project": moved }), || {
                print_project_details("Moved project", &moved, None, false);
            })
        }
        ProjectCommand::SetDesignDoc(args) => {
            if !args.unset && args.path.is_none() {
                return Err(CliError::usage(
                    "provide --path <p> (with optional --repo/--branch) or --unset",
                ));
            }
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let input = if args.unset {
                SetProjectDesignDocInput {
                    project_id: project.id.clone(),
                    unset: true,
                    ..SetProjectDesignDocInput::default()
                }
            } else {
                SetProjectDesignDocInput {
                    project_id: project.id.clone(),
                    design_doc_repo_remote_url: args.repo,
                    design_doc_branch: args.branch,
                    design_doc_path: args.path,
                    unset: false,
                }
            };
            let updated = set_project_design_doc(&mut client, input).await?;
            let resolved = resolve_project_design_doc(&mut client, &updated.id).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "project": updated, "design_doc": resolved }),
                || {
                    print_project_details("Updated project", &updated, None, false);
                    if let Some(line) = format_project_design_doc_line(&resolved.state) {
                        println!("Design doc: {line}");
                    }
                },
            )
        }
        ProjectCommand::OpenDesign(args) => {
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let resolved = resolve_project_design_doc(&mut client, &project.id).await?;
            let action = decide_open_design_action(&resolved.state, args.web)?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "project_id": project.id,
                    "design_doc": resolved,
                    "action": action.as_json(),
                }),
                || {
                    if !ctx.quiet {
                        println!("{}", action.human_summary());
                    }
                },
            )?;
            if !args.print {
                action.launch()?;
            }
            Ok(())
        }
        ProjectCommand::LintDesignDocs(args) => {
            let products = match args.product {
                Some(selector) => vec![resolve_product(&mut client, Some(selector), ctx).await?],
                None => list_products(&mut client).await?,
            };
            let mut entries: Vec<LintDesignDocEntry> = Vec::new();
            for product in &products {
                let projects = list_projects(&mut client, &product.id, None).await?;
                for project in projects {
                    let state = if project.design_doc_path.is_some() {
                        Some(resolve_project_design_doc(&mut client, &project.id).await?.state)
                    } else {
                        None
                    };
                    if let Some(entry) = classify_lint_finding(
                        product,
                        &project,
                        state.as_ref(),
                        check_design_doc_file_exists,
                        args.include_missing,
                        args.include_unverified,
                    ) {
                        entries.push(entry);
                    }
                }
            }
            entries.sort_by(|a, b| {
                a.product_slug
                    .cmp(&b.product_slug)
                    .then_with(|| a.project_slug.cmp(&b.project_slug))
            });
            let broken_count = entries
                .iter()
                .filter(|entry| entry.severity == LintSeverity::Broken)
                .count();
            print_entity(
                ctx,
                &serde_json::json!({
                    "entries": entries,
                    "scanned_products": products.iter().map(|p| &p.id).collect::<Vec<_>>(),
                    "broken_count": broken_count,
                }),
                || print_lint_design_docs_table(&entries),
            )?;
            if broken_count > 0 {
                Err(CliError::application(format!(
                    "{broken_count} project(s) have broken design-doc pointers"
                )))
            } else {
                Ok(())
            }
        }
        ProjectCommand::Plan(args) => {
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let result = plan_project(&mut client, &project.id, args.force, args.dry_run).await?;
            print_entity(ctx, &result, || print_plan_project_result(&result))
        }
        ProjectCommand::Release(args) => {
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let (run_id, released) = release_project(&mut client, &project.id).await?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "project_id": project.id,
                    "run_id": run_id,
                    "released": released,
                }),
                || {
                    if !ctx.quiet {
                        println!(
                            "Released {released} staged task(s) from planner run {run_id} — dispatch begins on \
                             the next reconcile pass."
                        );
                    }
                },
            )
        }
        ProjectCommand::Unpopulate(args) => {
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let (deleted, preserved) = unpopulate_project(&mut client, &project.id, &args.run).await?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "project_id": project.id,
                    "run_id": args.run,
                    "deleted": deleted,
                    "preserved": preserved,
                }),
                || {
                    if !ctx.quiet {
                        println!(
                            "Deleted {} staged task(s) from planner run {}.",
                            deleted.len(),
                            args.run
                        );
                        if !preserved.is_empty() {
                            println!(
                                "Preserved {} task(s) already released and dispatched (not deleted):",
                                preserved.len()
                            );
                            for task in &preserved {
                                println!("  {} ({})", task.name, task.id);
                            }
                        }
                    }
                },
            )
        }
        ProjectCommand::PlanRuns(args) => {
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let runs = list_planner_runs(&mut client, &project.id).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "project_id": project.id, "runs": runs }),
                || print_planner_runs_table(&runs),
            )
        }
        ProjectCommand::Depend { command } => run_depend_command(command, &mut client, ctx).await,
    }
}

pub(crate) async fn run_task_command(command: TaskCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        TaskCommand::Create(args) => {
            // `--automation`: the triage agent's create path. The produced
            // task is product-level (no project) and routed to the automations
            // pool; the engine owns provenance stamping + the cap re-check.
            if let Some(selector) = args.automation.clone() {
                let product = resolve_optional_product(&mut client, args.product.clone(), ctx).await?;
                let automation = resolve_automation(&mut client, &selector, product.as_ref()).await?;
                let name = required_text(args.name, "Task name", ctx)?;
                let description = optional_text(args.description, "Description", ctx)?;
                let task = create_automation_task(
                    &mut client,
                    &automation.id,
                    name,
                    description,
                    args.target_file,
                    args.target_symbol,
                )
                .await?;
                let task = with_display_status(task);
                return print_entity(ctx, &serde_json::json!({ "task": task }), || {
                    print_task_details("Created automation task", &task, None, false);
                });
            }
            if !args.target_file.is_empty() || !args.target_symbol.is_empty() {
                return Err(CliError::application(
                    "--target-file/--target-symbol are only valid with --automation",
                ));
            }
            let product = resolve_product_inferable(&mut client, args.product, args.project.as_deref(), ctx).await?;
            let project = resolve_project(&mut client, &product.id, args.project, ctx).await?;
            let name = required_text(args.name, "Task name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let prompt_text = compose_prompt_text(&name, description.as_deref());
            let resolved_repo = repo_resolution::resolve_repo_at_create_time(
                &mut client,
                &product,
                args.repo_remote_url.as_deref(),
                &prompt_text,
                ctx.allow_input,
            )
            .await?;
            // Only error on unresolved repo for multi-repo products (no product default).
            // Single-repo products return None intentionally; the engine inherits from the product.
            if product.repo_remote_url.is_none() && resolved_repo.is_none() && !ctx.allow_input {
                return Err(repo_resolution::unresolved_repo_error(&product.slug));
            }
            let model_override = normalize_non_empty(args.model);
            let driver = normalize_non_empty(args.driver);
            validate_driver_model_pair(driver.as_deref(), model_override.as_deref())?;
            let depends_on = resolve_depends_on(&mut client, ctx, &args.depends_on, &product.id).await?;
            let task = create_task(
                &mut client,
                CreateTaskInput::builder()
                    .product_id(product.id)
                    .project_id(project.id)
                    .name(name)
                    .maybe_description(description)
                    .autostart(!ctx.no_autostart)
                    .depends_on(depends_on)
                    .maybe_priority(args.priority.map(|priority| priority.as_str().to_owned()))
                    .created_via(CREATED_VIA_CLI)
                    .maybe_repo_remote_url(resolved_repo)
                    .maybe_effort_level(args.effort.map(EffortLevel::from))
                    .maybe_model_override(model_override)
                    .maybe_driver(driver)
                    .force_duplicate(args.force_duplicate)
                    .build(),
            )
            .await?;
            let task = with_display_status(task);
            print_entity(ctx, &serde_json::json!({ "task": task }), || {
                print_task_details("Created task", &task, None, false);
            })
        }
        TaskCommand::List(args) => {
            let product = resolve_product_inferable(&mut client, args.product, args.project.as_deref(), ctx).await?;
            let project = match args.project {
                Some(selector) => Some(resolve_project(&mut client, &product.id, Some(selector), ctx).await?),
                None => None,
            };
            let dep_filter = args.dep.into_filter();
            let repo_selector = args.repo.as_deref().map(RepoSelector::parse).transpose()?;
            let tasks = list_tasks(
                &mut client,
                &product.id,
                project.as_ref().map(|project| project.id.as_str()),
                dep_filter,
                args.include_deleted,
            )
            .await?;
            let tasks = apply_task_list_filters(
                tasks,
                TaskListCriteria::builder()
                    .statuses(&args.status)
                    .priorities(&args.priority)
                    .maybe_match_term(args.match_term.as_deref())
                    .ids(&args.id)
                    .maybe_limit(args.limit)
                    .include_archived(args.include_archived)
                    .build(),
                repo_selector.as_ref(),
                product.repo_remote_url.as_deref(),
            );
            // Status filtering above runs against the stored vocabulary;
            // remap to the board names only for output.
            let tasks: Vec<Task> = tasks.into_iter().map(with_display_status).collect();
            print_entity(ctx, &serde_json::json!({ "tasks": tasks }), || {
                print_tasks_table(&tasks, args.with_primary_id)
            })
        }
        TaskCommand::ByPr(args) => run_by_pr(&mut client, ctx, args).await,
        TaskCommand::ByExec(args) => run_by_exec(&mut client, ctx, args).await,
        TaskCommand::Show(args) => run_show_leaf(&mut client, ctx, args, false).await,
        TaskCommand::Update(args) => run_update_leaf(&mut client, ctx, args).await,
        TaskCommand::Move(args) => run_move_leaf(&mut client, ctx, args).await,
        TaskCommand::Delete(args) => run_delete_leaf(&mut client, ctx, args).await,
        TaskCommand::Restore(args) => run_restore_leaf(&mut client, ctx, args).await,
        TaskCommand::Reorder(args) => {
            let product = resolve_product_inferable(&mut client, args.product, args.project.as_deref(), ctx).await?;
            let project = resolve_project(&mut client, &product.id, args.project, ctx).await?;
            if args.ids.is_empty() {
                return Err(CliError::usage("provide at least one task id via --ids"));
            }
            reorder_project_tasks(&mut client, &project.id, &args.ids).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "project_id": project.id, "task_ids": args.ids }),
                || {
                    if !ctx.quiet {
                        println!("Reordered {} tasks for project {}", args.ids.len(), project.name);
                    }
                },
            )
        }
        TaskCommand::Depend { command } => run_depend_command(command, &mut client, ctx).await,
        TaskCommand::BindPr(args) => run_bind_pr(&mut client, ctx, args).await,
        TaskCommand::LinkExternal(args) => run_link_external(&mut client, ctx, args).await,
        TaskCommand::UnlinkExternal(args) => run_unlink_external(&mut client, ctx, args).await,
        TaskCommand::CreateMany(args) => run_task_create_many(&mut client, ctx, args).await,
        TaskCommand::CreateInvestigation(args) => run_create_investigation(&mut client, ctx, args).await,
        TaskCommand::CreateRevision(args) => run_create_revision(&mut client, ctx, args).await,
        TaskCommand::ListRevisions(args) => run_list_revisions(&mut client, ctx, args).await,
    }
}

pub(crate) async fn run_chore_command(command: ChoreCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        ChoreCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let name = required_text(args.name, "Chore name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let prompt_text = compose_prompt_text(&name, description.as_deref());
            let resolved_repo = repo_resolution::resolve_repo_at_create_time(
                &mut client,
                &product,
                args.repo_remote_url.as_deref(),
                &prompt_text,
                ctx.allow_input,
            )
            .await?;
            // Only error on unresolved repo for multi-repo products (no product default).
            // Single-repo products return None intentionally; the engine inherits from the product.
            if product.repo_remote_url.is_none() && resolved_repo.is_none() && !ctx.allow_input {
                return Err(repo_resolution::unresolved_repo_error(&product.slug));
            }
            let model_override = normalize_non_empty(args.model);
            let driver = normalize_non_empty(args.driver);
            validate_driver_model_pair(driver.as_deref(), model_override.as_deref())?;
            let depends_on = resolve_depends_on(&mut client, ctx, &args.depends_on, &product.id).await?;
            let chore = create_chore(
                &mut client,
                CreateChoreInput::builder()
                    .product_id(product.id)
                    .name(name)
                    .maybe_description(description)
                    .autostart(!ctx.no_autostart)
                    .depends_on(depends_on)
                    .maybe_priority(args.priority.map(|priority| priority.as_str().to_owned()))
                    .created_via(CREATED_VIA_CLI)
                    .maybe_repo_remote_url(resolved_repo)
                    .maybe_effort_level(args.effort.map(EffortLevel::from))
                    .maybe_model_override(model_override)
                    .maybe_driver(driver)
                    .force_duplicate(args.force_duplicate)
                    .build(),
            )
            .await?;
            let chore = with_display_status(chore);
            print_entity(ctx, &serde_json::json!({ "chore": chore }), || {
                print_task_details("Created chore", &chore, None, false);
            })
        }
        ChoreCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let dep_filter = args.dep.into_filter();
            let repo_selector = args.repo.as_deref().map(RepoSelector::parse).transpose()?;
            let chores = list_chores(&mut client, &product.id, dep_filter, args.include_deleted).await?;
            let chores = apply_task_list_filters(
                chores,
                TaskListCriteria::builder()
                    .statuses(&args.status)
                    .priorities(&args.priority)
                    .maybe_match_term(args.match_term.as_deref())
                    .ids(&args.id)
                    .maybe_limit(args.limit)
                    .include_archived(args.include_archived)
                    .build(),
                repo_selector.as_ref(),
                product.repo_remote_url.as_deref(),
            );
            // Status filtering above runs against the stored vocabulary;
            // remap to the board names only for output.
            let chores: Vec<Task> = chores.into_iter().map(with_display_status).collect();
            print_entity(ctx, &serde_json::json!({ "chores": chores }), || {
                print_tasks_table(&chores, args.with_primary_id)
            })
        }
        ChoreCommand::Show(args) => run_show_leaf(&mut client, ctx, args, true).await,
        ChoreCommand::Update(args) => run_update_leaf(&mut client, ctx, args).await,
        ChoreCommand::Move(args) => run_move_leaf(&mut client, ctx, args).await,
        ChoreCommand::Delete(args) => run_delete_leaf(&mut client, ctx, args).await,
        ChoreCommand::Restore(args) => run_restore_leaf(&mut client, ctx, args).await,
        ChoreCommand::Depend { command } => run_depend_command(command, &mut client, ctx).await,
        ChoreCommand::BindPr(args) => run_bind_pr(&mut client, ctx, args).await,
        ChoreCommand::LinkExternal(args) => run_link_external(&mut client, ctx, args).await,
        ChoreCommand::UnlinkExternal(args) => run_unlink_external(&mut client, ctx, args).await,
        ChoreCommand::CreateMany(args) => run_chore_create_many(&mut client, ctx, args).await,
    }
}

pub(crate) async fn run_comment_command(command: CommentCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        CommentCommand::Reply(args) => run_comment_reply(ctx, args).await,
    }
}

/// `boss comment reply --body <TEXT>` — the answer agent's sole write
/// action (P3b of `comment-triggered-document-revisions.md`). Reads the
/// target run from the process's own `BOSS_RUN_ID` env var (set by the
/// engine for the whole worker session, inherited by every Bash child,
/// including this one) rather than any CLI-supplied id — see
/// `CommentCommand::Reply`'s doc comment for why.
pub(crate) async fn run_comment_reply(ctx: &RunContext, args: CommentReplyArgs) -> Result<(), CliError> {
    let run_id = std::env::var("BOSS_RUN_ID").map_err(|_| {
        CliError::usage(
            "BOSS_RUN_ID is not set — `boss comment reply` only works inside a Boss \
             answer-agent worker session.",
        )
    })?;
    if args.body.trim().is_empty() {
        return Err(CliError::usage("--body may not be empty"));
    }
    let mut client = connect_for_work(ctx).await?;
    match client
        .send_request(&FrontendRequest::CommentsPostAnswer {
            run_id,
            body: args.body,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::CommentResult { comment } => {
            print_entity(ctx, &serde_json::json!({ "comment": comment }), || {
                if !ctx.quiet {
                    println!("Reply posted; comment {} is now '{}'.", comment.id, comment.status);
                }
            })
        }
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("comment reply", &other)),
    }
}

/// Shared handler for `boss task show <id>` and `boss chore show <id>`.
/// Routes any leaf work item id through the same path; the JSON key
/// and human-mode label match the actual kind of the returned item.
///
/// `chore_only`: when `true` (called from `boss chore show`), resolving
/// a friendly short id to a non-chore task-table row produces a
/// "wrong kind" error naming the correct verb.
pub(crate) async fn run_show_leaf(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskIdArg,
    chore_only: bool,
) -> Result<(), CliError> {
    let with_primary_id = args.with_primary_id;
    let work_item = match parse_work_item_selector(&args.id) {
        WorkItemSelector::ShortId(n) => {
            let item = resolve_short_id_item(client, ctx, args.product, n).await?;
            check_task_kind_for_verb(&item, n, chore_only)?;
            item
        }
        WorkItemSelector::ProductShortId { product_slug, n } => {
            let item = resolve_short_id_item(client, ctx, Some(product_slug), n).await?;
            check_task_kind_for_verb(&item, n, chore_only)?;
            item
        }
        WorkItemSelector::PrimaryId(id) | WorkItemSelector::Other(id) => get_work_item(client, &id).await?,
    };
    let (item, label) = expect_leaf_work_item(work_item)?;
    let item = with_display_status(item);
    let product = expect_product(get_work_item(client, &item.product_id).await?)?;
    let detail = list_dependencies_detailed(
        client,
        ListDependenciesInput {
            work_item: item.id.clone(),
            direction: Some(DependencyDirection::Both),
        },
    )
    .await?;
    let executions = list_executions_for_item(client, &item.id).await?;
    let runtime = get_task_runtime(client, &item.id).await?;
    let attention_items = list_attention_items_for_work_item(client, &item.id).await?;
    let attention_groups = list_attention_groups(client, &product.id, None, Some(item.id.clone()), None, None).await?;
    let task_json = task_json_with_runtime(&item, &runtime)?;
    print_entity(
        ctx,
        &serde_json::json!({
            label: task_json,
            "dependencies": detail,
            "executions": executions,
            "attention_items": attention_items,
            "attention_groups": attention_groups,
        }),
        || {
            print_task_details(label_titlecase(label), &item, Some(&product), with_primary_id);
            print_attention_items_section(&attention_items);
            print_attention_groups_section(&attention_groups);
            print_runtime_section(&runtime);
            print_dependency_section(&detail);
            print_executions_section(&executions);
        },
    )
}

/// Serialise `item` and splice the runtime's `current_execution_id`
/// / `current_run_id` onto the resulting JSON object so a downstream
/// `jq .task.current_execution_id` resolves to the engine's view of
/// the dispatched execution. Both fields land as `null` when no
/// execution / run exists yet — the coordinator wants the keys
/// present so it can distinguish "engine returned null" from "this
/// client predates the field." Cloning into a `serde_json::Value`
/// keeps the wire shape of [`Task`] unchanged everywhere else.
pub(crate) fn task_json_with_runtime(item: &Task, runtime: &TaskRuntime) -> Result<serde_json::Value, CliError> {
    let mut value = serde_json::to_value(item).map_err(CliError::internal)?;
    if let serde_json::Value::Object(map) = &mut value {
        map.insert(
            "current_execution_id".to_owned(),
            runtime
                .execution_id
                .clone()
                .map_or(serde_json::Value::Null, serde_json::Value::String),
        );
        map.insert(
            "current_run_id".to_owned(),
            runtime
                .current_run_id
                .clone()
                .map_or(serde_json::Value::Null, serde_json::Value::String),
        );
    }
    Ok(value)
}

pub(crate) fn print_runtime_section(runtime: &TaskRuntime) {
    if runtime.execution_id.is_none() && runtime.current_run_id.is_none() {
        return;
    }
    println!();
    println!("Runtime:");
    println!(
        "  current_execution_id: {}",
        runtime.execution_id.as_deref().unwrap_or("-")
    );
    println!(
        "  current_run_id:       {}",
        runtime.current_run_id.as_deref().unwrap_or("-")
    );
    if let Some(status) = &runtime.execution_status {
        println!("  execution_status:     {status}");
    }
    if let Some(status) = &runtime.run_status {
        println!("  run_status:           {status}");
    }
}

/// Check whether a work item resolved from a short id matches the verb
/// context. When `chore_only` is true and the item is a non-chore task,
/// return a user-friendly error naming the right verb.
pub(crate) fn check_task_kind_for_verb(item: &WorkItem, short_id: i64, chore_only: bool) -> Result<(), CliError> {
    if !chore_only {
        return Ok(());
    }
    match item {
        WorkItem::Task(t) => Err(CliError::application(format!(
            "T{short_id} is a {} (kind={}), not a chore — use `boss task show {short_id}`",
            t.kind, t.kind
        ))),
        WorkItem::Project(_) => Err(CliError::application(format!(
            "P{short_id} is a project, not a chore — use `boss project show {short_id}`"
        ))),
        WorkItem::Chore(_) | WorkItem::Product(_) => Ok(()),
    }
}

/// Shared handler for `boss task update` and `boss chore update`.
pub(crate) async fn run_update_leaf(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskUpdateArgs,
) -> Result<(), CliError> {
    let effort_level = if args.unset_effort {
        Some(String::new())
    } else {
        args.effort.map(|e| e.as_str().to_owned())
    };
    let model_override = if args.unset_model {
        Some(String::new())
    } else {
        args.model
    };
    let driver = if args.unset_driver {
        Some(String::new())
    } else {
        args.driver.clone()
    };
    validate_driver_model_pair(
        args.driver.as_deref().filter(|_| !args.unset_driver),
        model_override.as_deref().filter(|s| !s.is_empty()),
    )?;
    let patch = WorkItemPatch {
        name: args.name,
        description: args.description,
        status: args.status.map(|status| status.as_str().to_owned()),
        priority: args.priority.map(|priority| priority.as_str().to_owned()),
        ordinal: args.ordinal,
        pr_url: args.pr_url,
        // Preserve the empty-string "clear" wire form: `--repo ""`
        // means the engine should clear the override (inherit from
        // the product). Don't `normalize_non_empty` here.
        repo_remote_url: args.repo_remote_url,
        effort_level,
        model_override,
        driver,
        autostart: args.autostart,
        // Preserve the empty-string "clear" wire form: `--blocked-reason ""`
        // maps to NULL in the engine (clears the field).
        blocked_reason: args.blocked_reason,
        // Preserve the empty-string "clear" wire form: `--blocked-detail ""`
        // maps to NULL in the engine (clears the field). The engine rejects
        // a non-empty detail with no accompanying blocked_reason.
        blocked_detail: args.blocked_detail,
        ..WorkItemPatch::default()
    };
    ensure_patch_present(
        &patch,
        "provide at least one field to update, such as --status, --priority, --pr-url, --repo, --effort, --model, --driver, --autostart, --blocked-reason, or --blocked-detail",
    )?;
    // Resolve the product from --product or --project (typed project id infers its product).
    let product_hint = match (args.product, args.project) {
        (Some(prod), _) => Some(prod),
        (None, Some(proj)) => product_id_from_typed_selector(client, &proj).await?,
        (None, None) => None,
    };
    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, product_hint).await?;
    let (item, label) = expect_leaf_work_item(update_work_item(client, &resolved_id, patch).await?)?;
    let item = with_display_status(item);
    print_entity(ctx, &serde_json::json!({ label: item }), || {
        print_task_details(&format!("Updated {label}"), &item, None, false);
    })
}

/// Shared handler for `boss task move` and `boss chore move`.
pub(crate) async fn run_move_leaf(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskMoveArgs,
) -> Result<(), CliError> {
    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, None).await?;
    let patch = WorkItemPatch {
        status: Some(args.target.as_status().to_owned()),
        ..WorkItemPatch::default()
    };
    let (item, label) = expect_leaf_work_item(update_work_item(client, &resolved_id, patch).await?)?;
    let item = with_display_status(item);
    print_entity(ctx, &serde_json::json!({ label: item }), || {
        print_task_details(&format!("Moved {label}"), &item, None, false);
    })
}

/// Shared handler for `boss task delete` and `boss chore delete`. The
/// engine doesn't need the kind to delete; we read it back from the
/// pre-delete fetch only so the human-mode message names the right
/// noun.
pub(crate) async fn run_delete_leaf(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskDeleteArgs,
) -> Result<(), CliError> {
    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, None).await?;
    let label = match get_work_item(client, &resolved_id).await {
        Ok(item) => expect_leaf_work_item(item).map(|(_, l)| l).unwrap_or("item"),
        Err(_) => "item",
    };
    delete_work_item(client, &resolved_id).await?;
    print_entity(ctx, &serde_json::json!({ "id": resolved_id, "deleted": true }), || {
        if !ctx.quiet {
            println!("Deleted {label} {resolved_id}");
        }
    })
}

pub(crate) async fn run_restore_leaf(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskRestoreArgs,
) -> Result<(), CliError> {
    // Restore resolution is intentionally not routed through
    // `resolve_selector_to_primary_id`: a soft-deleted row is hidden
    // from the per-product short-id resolver, so bare `#43` / `boss/43`
    // can't reach it. The engine resolves the globally-unique `T43`
    // form (and canonical `task_…` ids) against tombstoned rows itself,
    // so we pass the raw selector straight through.
    let item = work_item_with_display_status(restore_work_item(client, args.id.trim()).await?);
    let (label, friendly) = match &item {
        WorkItem::Task(t) => ("Task", boss_protocol::short_id_label(t.short_id)),
        WorkItem::Chore(t) => ("Chore", boss_protocol::short_id_label(t.short_id)),
        _ => ("Item", None),
    };
    let friendly = friendly.unwrap_or_else(|| item.primary_id().to_owned());
    print_entity(ctx, &serde_json::json!({ "item": item }), || {
        if !ctx.quiet {
            println!("Restored {label} {friendly}");
        }
    })
}

/// "task" -> "Task". The label set comes from
/// [`expect_leaf_work_item`], so `&'static str` in / out is enough.
pub(crate) fn label_titlecase(label: &str) -> &'static str {
    match label {
        "task" => "Task",
        "chore" => "Chore",
        _ => "Item",
    }
}

pub(crate) async fn run_github_command(command: GithubCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        GithubCommand::Auth { command } => run_github_auth_command(command, ctx).await,
    }
}

pub(crate) async fn run_github_auth_command(command: GithubAuthCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        GithubAuthCommand::Login => run_github_auth_login(ctx).await,
        GithubAuthCommand::Status => run_github_auth_status(ctx).await,
        GithubAuthCommand::Logout => run_github_auth_logout(ctx).await,
    }
}

pub(crate) async fn run_github_auth_login(ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;

    let response = client
        .send_request(&FrontendRequest::GitHubAuthStart)
        .await
        .map_err(CliError::internal)?;

    let mut state = match response {
        FrontendEvent::GitHubAuthState { state } => state,
        other => {
            return Err(CliError::internal(anyhow::anyhow!(
                "unexpected response to GitHubAuthStart: {other:?}"
            )));
        }
    };

    let mut code_shown = false;

    loop {
        let poll_secs: u64 = match &state {
            GitHubAuthStateDto::Authorized {
                login,
                granted_scopes,
                org_state,
            } => {
                let json = serde_json::json!({
                    "status": "authorized",
                    "login": login,
                    "granted_scopes": granted_scopes,
                    "org_state": org_state,
                });
                let (login, granted_scopes, org_state) = (login.clone(), granted_scopes.clone(), org_state.clone());
                return print_entity(ctx, &json, move || {
                    println!("Authorized as @{login}");
                    println!("Scopes: {}", granted_scopes.join(", "));
                    print_org_state_human(&org_state);
                });
            }
            GitHubAuthStateDto::Expired => {
                return Err(CliError::application(
                    "Device code expired. Run `boss github auth login` again to start over.",
                ));
            }
            GitHubAuthStateDto::Denied => {
                return Err(CliError::application(
                    "Authorization denied. Run `boss github auth login` again to start over.",
                ));
            }
            GitHubAuthStateDto::Error { message } => {
                return Err(CliError::application(format!("Auth error: {message}")));
            }
            GitHubAuthStateDto::PendingUserAuth {
                user_code,
                verification_uri,
                verification_uri_complete,
                interval_seconds,
                ..
            } => {
                if !code_shown && matches!(ctx.output_mode, OutputMode::Human) {
                    println!("Open this URL in a browser to authorize Boss:");
                    if let Some(complete) = verification_uri_complete {
                        println!("  {complete}");
                        println!("Or visit {} and enter code: {user_code}", verification_uri);
                    } else {
                        println!("  {verification_uri}");
                        println!("Enter code: {user_code}");
                    }
                    println!("Waiting for authorization...");
                }
                code_shown = true;
                *interval_seconds as u64
            }
            GitHubAuthStateDto::RequestingCode | GitHubAuthStateDto::Disconnected => 2,
        };

        tokio::time::sleep(std::time::Duration::from_secs(poll_secs)).await;

        let response = client
            .send_request(&FrontendRequest::GitHubAuthStatus)
            .await
            .map_err(CliError::internal)?;
        state = match response {
            FrontendEvent::GitHubAuthState { state } => state,
            other => {
                return Err(CliError::internal(anyhow::anyhow!(
                    "unexpected response to GitHubAuthStatus: {other:?}"
                )));
            }
        };
    }
}

pub(crate) async fn run_github_auth_status(ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    let response = client
        .send_request(&FrontendRequest::GitHubAuthStatus)
        .await
        .map_err(CliError::internal)?;
    match response {
        FrontendEvent::GitHubAuthState { state } => {
            let json = serde_json::to_value(&state).unwrap_or(serde_json::Value::Null);
            print_entity(ctx, &json, || print_auth_state_human(&state))
        }
        other => Err(CliError::internal(anyhow::anyhow!(
            "unexpected response to GitHubAuthStatus: {other:?}"
        ))),
    }
}

pub(crate) async fn run_github_auth_logout(ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    let response = client
        .send_request(&FrontendRequest::GitHubAuthDisconnect)
        .await
        .map_err(CliError::internal)?;
    match response {
        FrontendEvent::GitHubAuthState { .. } => {
            print_entity(ctx, &serde_json::json!({ "status": "disconnected" }), || {
                if !ctx.quiet {
                    println!(
                        "Disconnected. Token removed from keychain. Issue sync will fall back \
                         to ambient `gh auth` credentials."
                    );
                }
            })
        }
        other => Err(CliError::internal(anyhow::anyhow!(
            "unexpected response to GitHubAuthDisconnect: {other:?}"
        ))),
    }
}

pub(crate) fn print_auth_state_human(state: &GitHubAuthStateDto) {
    match state {
        GitHubAuthStateDto::Disconnected => {
            println!("Not connected. Run `boss github auth login` to authenticate.");
        }
        GitHubAuthStateDto::RequestingCode => {
            println!("Requesting device code from GitHub...");
        }
        GitHubAuthStateDto::PendingUserAuth {
            user_code,
            verification_uri,
            verification_uri_complete,
            ..
        } => {
            println!("Pending authorization. Open this URL in a browser:");
            if let Some(complete) = verification_uri_complete {
                println!("  {complete}");
                println!("Or visit {} and enter code: {user_code}", verification_uri);
            } else {
                println!("  {verification_uri}");
                println!("Enter code: {user_code}");
            }
        }
        GitHubAuthStateDto::Authorized {
            login,
            granted_scopes,
            org_state,
        } => {
            println!("Authorized as @{login}");
            println!("Scopes: {}", granted_scopes.join(", "));
            print_org_state_human(org_state);
        }
        GitHubAuthStateDto::Expired => {
            println!("Device code expired. Run `boss github auth login` to start over.");
        }
        GitHubAuthStateDto::Denied => {
            println!("Authorization denied. Run `boss github auth login` to start over.");
        }
        GitHubAuthStateDto::Error { message } => {
            println!("Auth error: {message}");
        }
    }
}

pub(crate) fn print_org_state_human(org_state: &OrgAuthState) {
    match org_state {
        OrgAuthState::Ok => println!("Org access: OK"),
        OrgAuthState::NeedsOrgApproval { request_url } => {
            println!("Org access: needs org-owner approval");
            println!("  Approval page: {request_url}");
        }
        OrgAuthState::NeedsSso { sso_url } => {
            println!("Org access: needs SAML SSO authorization");
            println!("  Authorize: {sso_url}");
        }
        OrgAuthState::Unknown => println!("Org access: unknown (probe failed)"),
    }
}
