//! Cross-product task listing, reusing the engine's existing read surfaces.
use crate::*;

#[derive(Serialize)]
struct ListedTask<'a> {
    product_slug: &'a str,
    #[serde(flatten)]
    task: &'a Task,
}

#[derive(Serialize)]
struct TaskList<'a> {
    tasks: Vec<ListedTask<'a>>,
}

pub(crate) async fn run(client: &mut BossClient, ctx: &RunContext, args: TaskListArgs) -> Result<(), CliError> {
    let created_since = args
        .created_since
        .as_deref()
        .map(|raw| cost_cmds::parse_epoch_bound(raw, boss_engine_utils::epoch_time::now_epoch_secs()))
        .transpose()?;
    let repo = args.repo.as_deref().map(RepoSelector::parse).transpose()?;
    let products = list_products(client).await?;
    let cross_product = args.all_products
        || (args.product.is_none() && args.project.is_none() && (args.match_term.is_some() || !args.ids.is_empty()));
    let scope = if cross_product {
        None
    } else {
        Some(resolve_product_inferable(client, args.product.clone(), args.project.as_deref(), ctx).await?)
    };
    let project = match args.project.clone() {
        Some(selector) => Some(resolve_project(client, &scope.as_ref().unwrap().id, Some(selector), ctx).await?),
        None => None,
    };
    let parent = if let Some(selector) = args.parent.as_deref() {
        let resolved = resolve_create_revision_parent(client, selector).await?;
        Some(resolve_chain_root_id(client, &resolved).await)
    } else {
        None
    };
    let dep_filter = args.dep.clone().into_filter();
    // Batch status reads fetch only the requested items, including tombstones
    // for existence validation. Other filters are applied afterwards.
    let selected = if args.ids.is_empty() {
        None
    } else {
        Some(resolve_selected(client, &args.ids, &products, scope.as_ref()).await?)
    };
    let mut tasks = Vec::new();
    for product in &products {
        if scope.as_ref().is_some_and(|scope| scope.id != product.id) {
            continue;
        }
        let mut rows = if let Some(selected) = &selected {
            let mut rows: Vec<Task> = selected
                .iter()
                .filter(|task| task.product_id == product.id)
                .cloned()
                .collect();
            if rows.is_empty() {
                continue;
            }
            if dep_filter.is_some() {
                let eligible = list_tasks(client, &product.id, None, dep_filter.clone(), args.include_deleted).await?;
                let ids: std::collections::HashSet<_> = eligible.iter().map(|task| task.id.as_str()).collect();
                rows.retain(|task| ids.contains(task.id.as_str()));
            }
            rows.retain(|task| {
                (args.include_deleted || task.deleted_at.is_none())
                    && project
                        .as_ref()
                        .is_none_or(|project| task.project_id.as_deref() == Some(&project.id))
            });
            rows
        } else {
            list_tasks_query(
                client,
                &product.id,
                project.as_ref().map(|p| p.id.as_str()),
                dep_filter.clone(),
                args.include_deleted,
                boss_protocol::TaskListQuery {
                    selector: None,
                    match_term: args.match_term.clone(),
                    kinds: args.kind.clone(),
                    created_since,
                },
            )
            .await?
        };
        rows.sort_by(|a, b| {
            (a.ordinal.unwrap_or(0), &a.created_at, &a.id).cmp(&(b.ordinal.unwrap_or(0), &b.created_at, &b.id))
        });
        tasks.extend(apply_task_list_filters(
            rows,
            TaskListCriteria::builder()
                .statuses(&args.status)
                .priorities(&args.priority)
                .kinds(&args.kind)
                .maybe_created_since(created_since)
                .maybe_match_term(args.match_term.as_deref())
                .include_archived(args.include_archived)
                .parked(args.parked.filter())
                .maybe_parent_task_id(parent.as_deref())
                .build(),
            repo.as_ref(),
            product.repo_remote_url.as_deref(),
        ));
    }
    if cross_product {
        tasks.sort_by(|a, b| {
            let stamp = |task: &Task| boss_engine_utils::iso8601::parse_stored_timestamp(&task.created_at);
            (stamp(b), &b.id).cmp(&(stamp(a), &a.id))
        });
    }
    tasks.truncate(args.limit.unwrap_or(usize::MAX));
    let tasks: Vec<_> = tasks.into_iter().map(with_display_status).collect();
    let slugs: HashMap<_, _> = products.iter().map(|p| (p.id.as_str(), p.slug.as_str())).collect();
    // Borrow rows rather than allocating a full serde_json::Value tree.
    // Human rendering never serializes the (potentially large) descriptions.
    let output = TaskList {
        tasks: tasks
            .iter()
            .map(|task| ListedTask {
                product_slug: slugs[task.product_id.as_str()],
                task,
            })
            .collect(),
    };
    print_entity(ctx, &output, || {
        print_tasks_table_with_products(&tasks, args.with_primary_id, Some(&slugs))
    })
}

async fn resolve_selected(
    client: &mut BossClient,
    selectors: &[String],
    products: &[Product],
    scope: Option<&Product>,
) -> Result<Vec<Task>, CliError> {
    let selectors: Vec<_> = selectors.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if selectors.is_empty() {
        return Err(CliError::usage("--ids: no ids given (empty value)"));
    }
    let mut tasks = Vec::new();
    let mut unknown = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for selector in selectors {
        let parsed = parse_work_item_selector(selector);
        let (mut product_scope, wire_selector) = match parsed {
            WorkItemSelector::ProductShortId { product_slug, n } => {
                let product = match_products(products, &product_slug).map_err(|err| {
                    let message = format!("--ids: could not resolve {selector}: {err}");
                    match err {
                        CliError::Conflict(_) => CliError::conflict(message),
                        _ => CliError::not_found(message),
                    }
                })?;
                (Some(product.id), n.to_string())
            }
            WorkItemSelector::ShortId(n) => (scope.map(|p| p.id.clone()), n.to_string()),
            WorkItemSelector::PrimaryId(id) | WorkItemSelector::Other(id) => (scope.map(|p| p.id.clone()), id),
        };
        let primary_id = wire_selector.starts_with("task_");
        if primary_id && product_scope.is_none() {
            // Use the direct read to locate live primary ids, then fetch the
            // list projection from that product so JSON rows stay identical.
            match get_work_item(client, &wire_selector).await {
                Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => product_scope = Some(task.product_id),
                Err(CliError::Application(_)) => {} // Tombstones use targeted list reads below.
                Err(err) => return Err(err),
                Ok(_) => {
                    unknown.push(selector.to_owned());
                    continue;
                }
            }
        }
        let mut matches = Vec::new();
        for product in products
            .iter()
            .filter(|p| product_scope.as_ref().is_none_or(|id| *id == p.id))
        {
            matches.extend(
                list_tasks_query(
                    client,
                    &product.id,
                    None,
                    None,
                    true,
                    boss_protocol::TaskListQuery {
                        selector: Some(wire_selector.clone()),
                        ..Default::default()
                    },
                )
                .await?
                .into_iter()
                .filter(|task| matches_selector(task, &wire_selector)),
            );
            if primary_id && !matches.is_empty() {
                break;
            }
        }
        if matches.len() > 1 {
            return Err(CliError::conflict(format!(
                "--ids: ambiguous selector {selector}: {}",
                matches.iter().map(|t| t.id.as_str()).collect::<Vec<_>>().join(", ")
            )));
        }
        match matches.pop() {
            Some(task) if scope.is_none_or(|product| product.id == task.product_id) => {
                if seen.insert(task.id.clone()) {
                    tasks.push(task);
                }
            }
            _ => unknown.push(selector.to_owned()),
        }
    }
    if !unknown.is_empty() {
        return Err(CliError::not_found(format!(
            "--ids: no matching row for {}",
            unknown.join(", ")
        )));
    }
    Ok(tasks)
}

fn matches_selector(task: &Task, selector: &str) -> bool {
    task.id == selector || selector.parse::<i64>().ok().is_some_and(|n| task.short_id == Some(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_filters_rows_from_engines_that_ignore_query_predicates() {
        let task = Task::builder()
            .id("task_selected")
            .short_id(42)
            .product_id("prod_test")
            .kind(boss_protocol::TaskKind::Chore)
            .name("Example")
            .description("")
            .status(boss_protocol::TaskStatus::Todo)
            .created_at("100")
            .updated_at("100")
            .created_via("cli")
            .build();
        assert!(matches_selector(&task, "task_selected"));
        assert!(matches_selector(&task, "42"));
        for selector in ["task_other", "43", "invalid"] {
            assert!(!matches_selector(&task, selector), "{selector}");
        }
        let mut without_short_id = task.clone();
        without_short_id.short_id = None;
        assert!(!matches_selector(&without_short_id, "42"));
        assert!(!matches_selector(&without_short_id, "invalid"));
    }

    #[test]
    fn created_since_is_inclusive_and_handles_stored_timestamp_formats() {
        let tasks: Vec<_> = ["99", "100", "101", "1970-01-01T00:01:40Z", "invalid"]
            .into_iter()
            .enumerate()
            .map(|(index, created)| {
                Task::builder()
                    .id(format!("task_{index}"))
                    .product_id("prod_test")
                    .kind(boss_protocol::TaskKind::Chore)
                    .name("Example")
                    .description("")
                    .status(boss_protocol::TaskStatus::Todo)
                    .created_at(created)
                    .updated_at(created)
                    .created_via("cli")
                    .build()
            })
            .collect();
        let result = apply_task_list_filters(
            tasks,
            TaskListCriteria::builder().created_since(100).build(),
            None,
            None,
        );
        assert_eq!(
            result.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["task_1", "task_2", "task_3"]
        );
    }
}
