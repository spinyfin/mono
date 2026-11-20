//! Project dependency inspection, composed from the existing diagnostic reads.
use std::collections::BTreeMap;

use crate::*;

#[derive(serde::Serialize)]
struct ProjectDependencyGraph {
    project_id: String,
    edges: Vec<ProjectDependencyEdge>,
    frontier: Vec<DependencyEdge>,
}

#[derive(serde::Serialize)]
struct ProjectDependencyEdge {
    dependent: DependencyEdge,
    prerequisite: DependencyEdge,
    relation: String,
}

fn endpoint(id: &str, short_id: Option<i64>, name: &str, status: &str, kind: &str, slug: &str) -> DependencyEdge {
    DependencyEdge::builder()
        .id(id)
        .maybe_short_id(short_id)
        .product_slug(slug)
        .name(name)
        .status(status)
        .kind(kind)
        .relation("blocks")
        .build()
}

fn label(item: &DependencyEdge, product_slug: &str) -> String {
    let id = boss_protocol::short_id_label(item.short_id).unwrap_or_else(|| item.id.clone());
    match &item.product_slug {
        Some(slug) if slug != product_slug => format!("{slug}/{id}"),
        _ => id,
    }
}

pub(crate) async fn run(args: ProjectSelectorArgs, client: &mut BossClient, ctx: &RunContext) -> Result<(), CliError> {
    let product = resolve_product_inferable(client, args.product, Some(&args.selector), ctx).await?;
    let project = resolve_project(client, &product.id, Some(args.selector), ctx).await?;
    let tasks = list_tasks(client, &product.id, Some(&project.id), None, false).await?;
    let mut items = vec![endpoint(
        &project.id,
        project.short_id,
        &project.name,
        &project.status.to_string(),
        "project",
        &product.slug,
    )];
    items.extend(tasks.iter().map(|task| {
        endpoint(
            &task.id,
            task.short_id,
            &task.name,
            &task.status.to_string(),
            "task",
            &product.slug,
        )
    }));
    let mut edges = BTreeMap::new();
    let mut frontier = Vec::new();
    for item in items {
        let detail = list_dependencies_detailed(
            client,
            ListDependenciesInput {
                work_item: item.id.clone(),
                direction: Some(DependencyDirection::Both),
            },
        )
        .await?;
        if item.id != project.id
            && !dependency_status_is_satisfied(&item.status)
            && detail
                .prerequisites
                .iter()
                .all(|peer| dependency_status_is_satisfied(&peer.status))
        {
            frontier.push(item.clone());
        }
        for peer in detail.prerequisites {
            let relation = peer.relation.clone();
            edges.insert(
                (item.id.clone(), peer.id.clone(), relation.clone()),
                ProjectDependencyEdge {
                    dependent: item.clone(),
                    prerequisite: peer,
                    relation,
                },
            );
        }
        for peer in detail.dependents {
            let relation = peer.relation.clone();
            edges.insert(
                (peer.id.clone(), item.id.clone(), relation.clone()),
                ProjectDependencyEdge {
                    dependent: peer,
                    prerequisite: item.clone(),
                    relation,
                },
            );
        }
    }
    frontier.sort_by_key(|item| (item.short_id, item.id.clone()));
    let graph = ProjectDependencyGraph {
        project_id: project.id,
        edges: edges.into_values().collect(),
        frontier,
    };
    print_entity(ctx, &graph, || {
        for edge in &graph.edges {
            println!(
                "{} <- needs {} ({})",
                label(&edge.dependent, &product.slug),
                label(&edge.prerequisite, &product.slug),
                edge.prerequisite.status
            );
        }
        if graph.edges.is_empty() {
            println!("No dependency edges.");
        }
        println!("Unblocked frontier:");
        if graph.frontier.is_empty() {
            println!("  (none)");
        }
        for item in &graph.frontier {
            println!("  {} ({}) {}", label(item, &product.slug), item.status, item.name);
        }
    })
}
