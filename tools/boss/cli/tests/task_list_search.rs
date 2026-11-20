//! Cross-product search and batch status through the real CLI and engine.
use anyhow::Result;
use boss_client::BossClient;
use common::{run_boss, run_boss_expect_failure, run_boss_human};
use harness::{TestEngine, create_chore, create_product, create_project, create_task};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn searches_products_and_composes_filters() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let alpha = create_product(&mut client, "Alpha").await?;
    let beta = create_product(&mut client, "Beta").await?;
    let project = create_project(&mut client, &alpha.id, "Project").await?;
    let task = create_task(&mut client, &alpha.id, &project.id, "Shared fix").await?;
    let chore = create_chore(&mut client, &beta.id, "Shared cleanup").await?;
    create_chore(&mut client, &beta.id, "Unrelated").await?;

    let found = run_boss(engine.socket_str(), &["task", "list", "--match", "SHARED"])?;
    let rows = found["tasks"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|r| r["id"] == task.id && r["product_slug"] == alpha.slug)
    );
    assert!(
        rows.iter()
            .any(|r| r["id"] == chore.id && r["product_slug"] == beta.slug)
    );

    let filtered = run_boss(
        engine.socket_str(),
        &[
            "task",
            "list",
            "--all-products",
            "--match",
            "shared",
            "--kind",
            "chore",
            "--created-since",
            "24h",
            "--status",
            "todo",
            "--limit",
            "1",
        ],
    )?;
    assert_eq!(filtered["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["tasks"][0]["id"], chore.id);
    let future = run_boss(
        engine.socket_str(),
        &[
            "task",
            "list",
            "--all-products",
            "--created-since",
            "2100-01-01T00:00:00Z",
        ],
    )?;
    assert!(future["tasks"].as_array().unwrap().is_empty());
    let limited = run_boss(engine.socket_str(), &["task", "list", "--all-products", "--limit", "1"])?;
    assert_eq!(limited["tasks"].as_array().unwrap().len(), 1);
    let human = run_boss_human(
        engine.socket_str(),
        &["task", "list", "--all-products", "--match", "shared"],
    )?;
    assert!(human.contains("PRODUCT") && human.contains(&alpha.slug) && human.contains(&beta.slug));
    let scoped = run_boss(engine.socket_str(), &["task", "list", "--product", &beta.slug])?;
    assert!(
        scoped["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["product_slug"] == beta.slug)
    );
    for flags in [
        vec!["--all-products", "--product", &alpha.slug],
        vec!["--all-products", "--kind", "bogus"],
        vec!["--all-products", "--created-since", "yesterday-ish"],
    ] {
        let mut args = vec!["task", "list"];
        args.extend(flags);
        run_boss_expect_failure(engine.socket_str(), &args)?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_ids_span_products_validate_existence_and_deduplicate() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let alpha = create_product(&mut client, "Alpha").await?;
    let beta = create_product(&mut client, "Beta").await?;
    let a = create_chore(&mut client, &alpha.id, "First").await?;
    let b = create_chore(&mut client, &beta.id, "Second").await?;
    let selectors = format!(
        "{}/{},{}/{},{}",
        alpha.slug,
        a.short_id.unwrap(),
        beta.slug,
        b.short_id.unwrap(),
        a.id
    );
    let result = run_boss(engine.socket_str(), &["task", "list", "--ids", &selectors])?;
    assert_eq!(result["tasks"].as_array().unwrap().len(), 2);
    let empty = run_boss(
        engine.socket_str(),
        &["task", "list", "--ids", &selectors, "--kind", "revision"],
    )?;
    assert!(empty["tasks"].as_array().unwrap().is_empty());
    let missing = format!("{selectors},alpha/999999,beta/999998");
    let error = run_boss_expect_failure(engine.socket_str(), &["task", "list", "--ids", &missing])?;
    assert!(error.contains("alpha/999999") && error.contains("beta/999998"));
    run_boss_expect_failure(
        engine.socket_str(),
        &["task", "list", "--ids", &selectors, "--product", &alpha.id],
    )?;
    // A bare number shared by two products must never silently pick one.
    let error = run_boss_expect_failure(
        engine.socket_str(),
        &[
            "task",
            "list",
            "--all-products",
            "--ids",
            &a.short_id.unwrap().to_string(),
        ],
    )?;
    assert!(error.contains("ambig"), "{error}");
    run_boss(engine.socket_str(), &["task", "delete", &b.id])?;
    let live = run_boss(engine.socket_str(), &["task", "list", "--ids", &selectors])?;
    assert_eq!(live["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(live["tasks"][0]["id"], a.id);
    let deleted = run_boss(engine.socket_str(), &["task", "list", "--ids", &selectors, "--deleted"])?;
    assert_eq!(deleted["tasks"].as_array().unwrap().len(), 2);
    assert!(
        deleted["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == b.id && !r["deleted_at"].is_null())
    );
    Ok(())
}
