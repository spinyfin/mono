//! Dependency identities and project graph coverage through the public CLI.
use anyhow::Result;
use boss_client::BossClient;
use boss_protocol::{AddDependencyInput, WorkItemPatch};
use common::{run_boss, run_boss_human};
use harness::{TestEngine, create_chore, create_product, create_project, create_task};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_graph_includes_external_edges_and_unblocked_frontier() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Graph").await?;
    let project = create_project(&mut client, &product.id, "Delivery").await?;
    let first = create_task(&mut client, &product.id, &project.id, "First").await?;
    let second = create_task(&mut client, &product.id, &project.id, "Second").await?;
    let independent = create_task(&mut client, &product.id, &project.id, "Independent").await?;
    let external = create_chore(&mut client, &product.id, "External gate").await?;
    let db = engine.db()?;
    for (dependent, prerequisite) in [(&second.id, &first.id), (&first.id, &external.id)] {
        db.add_dependency(AddDependencyInput {
            dependent: dependent.clone(),
            prerequisite: prerequisite.clone(),
            relation: None,
        })?;
    }
    let shown = run_boss(engine.socket_str(), &["task", "show", &first.id])?;
    assert_eq!(
        shown["dependencies"]["prerequisites"][0]["short_id"],
        external.short_id.unwrap()
    );
    assert_eq!(shown["dependencies"]["prerequisites"][0]["product_slug"], product.slug);
    assert_eq!(
        shown["dependencies"]["dependents"][0]["short_id"],
        second.short_id.unwrap()
    );
    assert_eq!(shown["dependencies"]["dependents"][0]["product_slug"], product.slug);
    let human = run_boss_human(engine.socket_str(), &["task", "show", &first.id])?;
    assert!(human.contains(&format!("T{}", external.short_id.unwrap())), "{human}");

    let args = ["project", "deps", project.id.as_str()];
    let graph = run_boss(engine.socket_str(), &args)?;
    let edges = graph["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2, "edges must be deduplicated: {graph}");
    assert!(
        edges
            .iter()
            .any(|edge| edge["prerequisite"]["product_slug"] == product.slug)
    );
    let frontier = graph["frontier"].as_array().unwrap();
    assert!(frontier.iter().any(|item| item["id"] == independent.id));
    assert!(
        !frontier
            .iter()
            .any(|item| item["id"] == first.id || item["id"] == second.id)
    );

    db.update_work_item(
        &external.id,
        WorkItemPatch {
            status: Some("done".into()),
            ..Default::default()
        },
    )?;
    let graph = run_boss(engine.socket_str(), &args)?;
    assert!(
        graph["frontier"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == first.id)
    );
    db.update_work_item(
        &first.id,
        WorkItemPatch {
            status: Some("done".into()),
            ..Default::default()
        },
    )?;
    let graph = run_boss(engine.socket_str(), &args)?;
    let frontier = graph["frontier"].as_array().unwrap();
    assert!(!frontier.iter().any(|item| item["id"] == first.id));
    assert!(frontier.iter().any(|item| item["id"] == second.id));
    let human = run_boss_human(engine.socket_str(), &args)?;
    assert!(
        human.contains(&format!(
            "T{} <- needs T{} (done)",
            second.short_id.unwrap(),
            first.short_id.unwrap()
        )),
        "{human}"
    );
    assert!(human.contains("(done)"), "{human}");
    assert!(human.contains("Unblocked frontier:"), "{human}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frontier_requires_every_prerequisite_and_includes_isolated_tasks() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Frontier").await?;
    let project = create_project(&mut client, &product.id, "Release").await?;
    let task = create_task(&mut client, &product.id, &project.id, "Ship").await?;
    let args = ["project", "deps", project.id.as_str()];
    let graph = run_boss(engine.socket_str(), &args)?;
    assert_eq!(graph["edges"].as_array().unwrap().len(), 0);
    assert!(
        graph["frontier"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == task.id)
    );
    let gates = [
        create_chore(&mut client, &product.id, "First gate").await?,
        create_chore(&mut client, &product.id, "Second gate").await?,
    ];
    let db = engine.db()?;
    for gate in &gates {
        db.add_dependency(AddDependencyInput {
            dependent: task.id.clone(),
            prerequisite: gate.id.clone(),
            relation: None,
        })?;
    }
    db.update_work_item(
        &gates[0].id,
        WorkItemPatch {
            status: Some("done".into()),
            ..Default::default()
        },
    )?;
    let graph = run_boss(engine.socket_str(), &args)?;
    assert!(
        !graph["frontier"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == task.id)
    );
    db.update_work_item(
        &gates[1].id,
        WorkItemPatch {
            status: Some("archived".into()),
            ..Default::default()
        },
    )?;
    let graph = run_boss(engine.socket_str(), &args)?;
    assert!(
        graph["frontier"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == task.id)
    );
    Ok(())
}
