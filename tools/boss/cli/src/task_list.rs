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
    } else if let Some(selector) = &args.product {
        Some(match_products(&products, selector)?)
    } else {
        Some(resolve_product_inferable(client, None, args.project.as_deref(), ctx).await?)
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
            list_tasks(
                client,
                &product.id,
                project.as_ref().map(|p| p.id.as_str()),
                dep_filter.clone(),
                args.include_deleted,
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
    let mut tombstone_lists = HashMap::new();
    for selector in selectors {
        let mut item = match parse_work_item_selector(selector) {
            WorkItemSelector::ProductShortId { product_slug, n } => {
                let product = match_products(products, &product_slug).map_err(|err| {
                    let message = format!("--ids: could not resolve {selector}: {err}");
                    match err {
                        CliError::Conflict(_) => CliError::conflict(message),
                        _ => CliError::not_found(message),
                    }
                })?;
                get_work_item_by_short_id_rpc(client, &product.id, n).await
            }
            WorkItemSelector::ShortId(n) => match scope {
                Some(product) => get_work_item_by_short_id_rpc(client, &product.id, n).await,
                None => get_work_item(client, &boss_protocol::short_id_wire_form(n)).await,
            },
            WorkItemSelector::PrimaryId(id) | WorkItemSelector::Other(id) => get_work_item(client, &id).await,
        };
        if matches!(item, Err(CliError::Application(_))) {
            // Single-item reads hide non-archived tombstones. Only on a
            // failed lookup, consult the include-deleted list to distinguish
            // a filtered-out row from a nonexistent selector.
            if let Some(task) = find_tombstone(client, selector, products, scope, &mut tombstone_lists).await? {
                item = Ok(WorkItem::Task(task));
            }
        }
        match item {
            Ok(WorkItem::Task(task) | WorkItem::Chore(task))
                if scope.is_none_or(|product| product.id == task.product_id) =>
            {
                if seen.insert(task.id.clone()) {
                    tasks.push(task);
                }
            }
            Ok(_) => unknown.push(selector.to_owned()),
            Err(CliError::Application(message)) => unknown.push(format!("{selector} ({message})")),
            Err(err) => return Err(err),
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

async fn find_tombstone(
    client: &mut BossClient,
    selector: &str,
    products: &[Product],
    scope: Option<&Product>,
    lists: &mut HashMap<String, Vec<Task>>,
) -> Result<Option<Task>, CliError> {
    let parsed = parse_work_item_selector(selector);
    let product = match &parsed {
        WorkItemSelector::ProductShortId { product_slug, .. } => Some(match_products(products, product_slug)?.id),
        _ => scope.map(|p| p.id.clone()),
    };
    let mut matches = Vec::new();
    for p in products
        .iter()
        .filter(|p| product.as_ref().is_none_or(|id| *id == p.id))
    {
        if !lists.contains_key(&p.id) {
            lists.insert(p.id.clone(), list_tasks(client, &p.id, None, None, true).await?);
        }
        matches.extend(
            lists[&p.id]
                .iter()
                .filter(|task| match &parsed {
                    WorkItemSelector::ShortId(n) | WorkItemSelector::ProductShortId { n, .. } => {
                        task.short_id == Some(*n)
                    }
                    WorkItemSelector::PrimaryId(id) | WorkItemSelector::Other(id) => task.id == *id,
                })
                .cloned(),
        );
    }
    if matches.len() > 1 {
        let candidates: Vec<_> = matches.iter().map(|task| task.id.as_str()).collect();
        return Err(CliError::conflict(format!(
            "--ids: ambiguous selector {selector}: {}",
            candidates.join(", ")
        )));
    }
    Ok(matches
        .first()
        .filter(|task| task.deleted_at.is_some())
        .map(|task| (*task).clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
