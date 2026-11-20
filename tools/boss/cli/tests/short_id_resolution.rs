//! End-to-end coverage for the shared work-item id resolver.
//!
//! Pins the behaviours that previous short-id chores left regressed:
//!
//! 1. Globally unique short ids resolve via `GetWorkItem` without a
//!    product scope (the engine shared choke point).
//! 2. A nonexistent short id errors as not-found / could-not-resolve —
//!    never as "product is required" (CLI subprocess).
//! 3. An ambiguous short id (same number in two products) hard-errors
//!    listing every candidate (long id, product, name, status) and does
//!    not silently pick one (CLI subprocess + engine RPC).
//! 4. Product-scoped `GetWorkItemByShortId` disambiguates.
//!
//! Surface enumeration (adversarial id-shaped clap walk + per-module
//! routing) lives in unit tests in `src/tests.rs`. This file is the
//! behavioural half: drive the built binary with nonexistent short ids
//! on representative surfaces and assert exit != 0 with a resolve error
//! (never "product is required", never empty success).

use std::process::Command;

use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_protocol::{CreateProductInput, FrontendEvent, FrontendRequest, WorkItem};

use common::{boss_binary, run_boss, run_boss_expect_failure};
use harness::{TestEngine, create_chore, create_product_with, create_project, create_task};

async fn select_product(client: &mut BossClient, product_id: &str) -> Result<()> {
    let event = client.send_request(&FrontendRequest::RegisterAppSession).await?;
    assert!(matches!(event, FrontendEvent::AppSessionRegistered), "{event:?}");
    let event = client
        .send_request(&FrontendRequest::ReportSelectedProduct {
            product_id: Some(product_id.to_owned()),
        })
        .await?;
    assert!(
        matches!(event, FrontendEvent::SelectedProductReported { accepted: true }),
        "{event:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_filters_resolve_product_scope_and_reject_unknown_selectors() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let mut products = Vec::new();
    let mut rows = Vec::new();
    for name in ["Boss", "Flunge"] {
        let product = create_product_with(
            &mut client,
            CreateProductInput::builder()
                .name(name)
                .repo_remote_url(format!("git@example.com:{name}.git"))
                .build(),
        )
        .await?;
        let project = create_project(&mut client, &product.id, "Project").await?;
        let task = create_task(&mut client, &product.id, &project.id, "Task").await?;
        let chore = create_chore(&mut client, &product.id, "Chore").await?;
        products.push(product);
        rows.push((project, task, chore));
    }
    let (project, task, chore) = &rows[1];
    assert_eq!(rows[0].1.short_id, task.short_id);
    run_boss(engine.socket_str(), &["task", "depend", "add", &task.id, &chore.id])?;
    run_boss(engine.socket_str(), &["task", "depend", "add", &task.id, &project.id])?;
    run_boss(
        engine.socket_str(),
        &["task", "bind-pr", &task.id, "https://github.com/acme/repo/pull/1"],
    )?;
    let revision = engine.db()?.create_revision(
        boss_protocol::CreateRevisionInput::builder()
            .parent_task_id(&task.id)
            .description("Revision")
            .build(),
        &boss_engine::work::StaticPrStateChecker(boss_engine::work::PrOpenState::Open),
    )?;
    run_boss(engine.socket_str(), &["task", "depend", "add", &revision.id, &chore.id])?;
    select_product(&mut client, &products[0].id).await?;

    for (flag, expected) in [
        ("--parent", vec![revision.id.as_str()]),
        ("--prerequisites-of", vec![chore.id.as_str()]),
        ("--dependents-of", vec![task.id.as_str(), revision.id.as_str()]),
    ] {
        let anchor = if flag == "--dependents-of" { chore } else { task };
        let other = if flag == "--dependents-of" {
            &rows[0].2
        } else {
            &rows[0].1
        };
        let number = anchor.short_id.unwrap().to_string();
        let error = run_boss_expect_failure(engine.socket_str(), &["task", "list", "--all-products", flag, &number])?;
        assert!(
            error.contains("ambiguous") && error.contains(&anchor.id) && error.contains(&other.id),
            "{error}"
        );
        for selector in [format!("{}/{}", products[1].slug, number), anchor.id.clone()] {
            let value = run_boss(
                engine.socket_str(),
                &["task", "list", "--all-products", flag, &selector],
            )?;
            let mut actual: Vec<_> = value["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].as_str().unwrap())
                .collect();
            let mut expected = expected.clone();
            actual.sort();
            expected.sort();
            assert_eq!(actual, expected, "{flag} {selector}: {value}");
        }
        let error = run_boss_expect_failure(engine.socket_str(), &["task", "list", "--all-products", flag, "999999"])?;
        assert!(
            error.contains("no work item") || error.contains("could not resolve"),
            "{error}"
        );
    }

    let cases = [
        (
            "task",
            "list",
            "tasks",
            "--dependents-of",
            chore.short_id.unwrap(),
            &task.id,
        ),
        (
            "chore",
            "list",
            "chores",
            "--prerequisites-of",
            task.short_id.unwrap(),
            &chore.id,
        ),
        (
            "project",
            "list",
            "projects",
            "--prerequisites-of",
            task.short_id.unwrap(),
            &project.id,
        ),
        (
            "task",
            "list-revisions",
            "revisions",
            "--dependents-of",
            chore.short_id.unwrap(),
            &revision.id,
        ),
        (
            "task",
            "list",
            "tasks",
            "--parent",
            task.short_id.unwrap(),
            &revision.id,
        ),
        (
            "task",
            "list-revisions",
            "revisions",
            "--parent",
            task.short_id.unwrap(),
            &revision.id,
        ),
        (
            "task",
            "list",
            "tasks",
            "--project",
            project.short_id.unwrap(),
            &task.id,
        ),
    ];
    for (verb, list, key, flag, n, expected) in cases {
        for selector in [n.to_string(), format!("T{n}"), format!("{}/{n}", products[1].slug)] {
            let value = run_boss(
                engine.socket_str(),
                &[verb, list, "--product", &products[1].slug, flag, &selector],
            )?;
            assert!(
                value[key].as_array().unwrap().iter().any(|row| row["id"] == *expected),
                "{verb} {list} {flag} {selector}: {value}"
            );
            assert!(
                !value[key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["product_id"] == products[0].id),
                "{value}"
            );
        }
    }
    for (flag, id, n) in [
        ("--task", &task.id, task.short_id.unwrap()),
        ("--project", &project.id, project.short_id.unwrap()),
    ] {
        for selector in [n.to_string(), format!("{}/{n}", products[1].slug)] {
            let created = run_boss(
                engine.socket_str(),
                &[
                    "attention",
                    "create",
                    "--product",
                    &products[1].slug,
                    flag,
                    &selector,
                    "--group-key",
                    &format!("scope-{flag}-{selector}"),
                    "--kind",
                    "question",
                    "--question-type",
                    "yes_no",
                    "--prompt",
                    "Proceed?",
                ],
            )?;
            let field = if flag == "--task" {
                "association_task_id"
            } else {
                "association_project_id"
            };
            assert_eq!(created["attention_group"][field], *id, "{created}");
            let listed = run_boss(
                engine.socket_str(),
                &["attention", "list", "--product", &products[1].slug, flag, &selector],
            )?;
            assert!(
                listed["attention_groups"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|group| group["id"] == created["attention_group"]["id"]),
                "{listed}"
            );
        }
    }
    // Explicit prefixes override --product for the anchor, while the listing
    // still limits returned rows to its product. Edges stay within a product,
    // so an anchor in Boss has no prerequisites in the Flunge listing.
    run_boss(
        engine.socket_str(),
        &["task", "depend", "add", &rows[0].1.id, &rows[0].2.id],
    )?;
    let anchor = format!("{}/{}", products[0].slug, rows[0].1.short_id.unwrap());
    let value = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "list",
            "--product",
            &products[1].slug,
            "--prerequisites-of",
            &anchor,
        ],
    )?;
    assert_eq!(value["chores"], serde_json::json!([]));
    let value = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "list",
            "--product",
            &products[0].slug,
            "--prerequisites-of",
            &anchor,
        ],
    )?;
    assert_eq!(value["chores"].as_array().unwrap().len(), 1);
    assert_eq!(value["chores"][0]["id"], rows[0].2.id);

    for (verb, list) in [
        ("task", "list"),
        ("chore", "list"),
        ("project", "list"),
        ("task", "list-revisions"),
    ] {
        for flag in ["--prerequisites-of", "--dependents-of"] {
            for missing in ["999999", "flunge/999999", "task_missing", "not-a-selector"] {
                let err = run_boss_expect_failure(
                    engine.socket_str(),
                    &[verb, list, "--product", &products[1].slug, flag, missing],
                )?;
                assert!(err.contains("could not resolve") || err.contains("unknown"), "{err}");
            }
        }
    }
    for list in ["list", "list-revisions"] {
        for missing in ["999999", "flunge/999999", "task_missing", "not-a-selector"] {
            run_boss_expect_failure(
                engine.socket_str(),
                &["task", list, "--product", &products[1].slug, "--parent", missing],
            )?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_product_diagnostics_and_environment_override() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let mut products = Vec::new();
    let mut chores = Vec::new();
    for name in ["Boss", "Flunge"] {
        let product = create_product_with(
            &mut client,
            CreateProductInput::builder()
                .name(name)
                .repo_remote_url(format!("git@example.com:{name}.git"))
                .build(),
        )
        .await?;
        chores.push(create_chore(&mut client, &product.id, name).await?);
        products.push(product);
    }
    select_product(&mut client, &products[1].id).await?;
    let n = chores[0].short_id.unwrap();
    for selector in [n.to_string(), format!("T{n}")] {
        let err = run_boss_expect_failure(engine.socket_str(), &["task", "show", &selector])?;
        assert!(
            err.contains("ambiguous") && err.contains(&chores[0].id) && err.contains(&chores[1].id),
            "{err}"
        );
    }
    let unique = create_chore(&mut client, &products[0].id, "Unique").await?;
    let selector = format!("T{}", unique.short_id.unwrap());
    let err = run_boss_expect_failure(engine.socket_str(), &["task", "show", &selector])?;
    let qualified = format!("{}/{}", products[0].slug, unique.short_id.unwrap());
    assert!(
        err.contains("Flunge") && err.contains("selected in the app") && err.contains(&qualified),
        "{err}"
    );
    let err = run_boss_expect_failure(
        engine.socket_str(),
        &["task", "comment", &selector, "--body", "Comment"],
    )?;
    assert!(err.contains("selected in the app") && err.contains(&qualified), "{err}");

    let run = |args: &[&str], env: Option<&str>| {
        let mut command = Command::new(boss_binary());
        command.args([
            "--json",
            "--no-input",
            "--no-autostart",
            "--socket-path",
            engine.socket_str(),
        ]);
        command.args(args).env_remove("BOSS_PRODUCT");
        if let Some(product) = env {
            command.env("BOSS_PRODUCT", product);
        }
        command.output()
    };
    select_product(&mut client, &products[0].id).await?;
    let output = run(&["task", "show", &selector], None)?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["id"], unique.id);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Resolved") && stderr.contains("Boss") && stderr.contains("implicit scope"),
        "{stderr}"
    );

    let short = n.to_string();
    let qualified = format!("{}/{n}", products[0].slug);
    for (args, expected) in [
        (vec!["task", "show", &short], &chores[1].id),
        (
            vec!["task", "show", &short, "--product", &products[0].slug],
            &chores[0].id,
        ),
        (vec!["task", "show", &qualified], &chores[0].id),
    ] {
        let output = run(&args, Some(&products[1].slug))?;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(value["id"], *expected);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("implicit scope"));
    }
    let output = run(&["chore", "list", "--ids", &short], Some(&products[1].slug))?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["chores"][0]["id"], chores[1].id);
    let output = run(&["task", "show", &short], Some("unknown-product"))?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown product"));

    engine.db()?.delete_work_item(&unique.id)?;
    let output = run(&["task", "restore", &selector], None)?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["item"]["id"], unique.id);
    assert!(String::from_utf8_lossy(&output.stderr).contains("product Boss (implicit scope)"));

    engine.db()?.delete_work_item(&chores[1].id)?;
    let output = run(&["task", "restore", &short], Some(&products[1].slug))?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["item"]["id"], chores[1].id);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("implicit scope"));

    let implicit_dependent = create_chore(&mut client, &products[0].id, "Implicit dependent").await?;
    for operation in ["add", "rm"] {
        let output = run(&["task", "depend", operation, &implicit_dependent.id, &selector], None)?;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let prerequisite = if operation == "add" {
            &value["edge"]["prerequisite_id"]
        } else {
            &value["prerequisite_id"]
        };
        assert_eq!(prerequisite, &unique.id);
        assert!(String::from_utf8_lossy(&output.stderr).contains("product Boss (implicit scope)"));
    }
    run(&["task", "depend", "add", &implicit_dependent.id, &selector], None)?;
    engine.db()?.delete_work_item(&unique.id)?;
    let output = run(&["task", "depend", "rm", &implicit_dependent.id, &selector], None)?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["prerequisite_id"], unique.id);
    assert_eq!(value["removed"], false);
    assert!(String::from_utf8_lossy(&output.stderr).contains("product Boss (implicit scope)"));

    let dependent = create_chore(&mut client, &products[1].id, "Dependent").await?;
    let output = run(
        &["task", "depend", "add", &dependent.id, &short],
        Some(&products[1].slug),
    )?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["edge"]["prerequisite_id"], chores[1].id);
    let output = run(
        &["task", "depend", "rm", &dependent.id, &short],
        Some(&products[1].slug),
    )?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["prerequisite_id"], chores[1].id);
    assert_eq!(value["removed"], true);
    let output = run(
        &["task", "depend", "add", &dependent.id, &short],
        Some(&products[1].slug),
    )?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    engine.db()?.delete_work_item(&chores[1].id)?;
    // Deletion removes the edge; an idempotent removal still resolves the tombstone.
    let output = run(
        &["task", "depend", "rm", &dependent.id, &short],
        Some(&products[1].slug),
    )?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["prerequisite_id"], chores[1].id);
    assert_eq!(value["removed"], false);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_work_item_resolves_globally_unique_short_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    // Two products so single-product auto-select cannot mask a real
    // global resolve. The short id under test exists in only one of them.
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@example.com:boss.git")
            .build(),
    )
    .await?;
    let _other = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Flunge")
            .repo_remote_url("git@example.com:flunge.git")
            .build(),
    )
    .await?;
    let chore = create_chore(&mut client, &product.id, "unique short id").await?;
    let short_id = chore.short_id.ok_or_else(|| anyhow!("chore has no short_id"))?;
    let selector = boss_protocol::short_id_wire_form(short_id);

    for app_connected in [false, true] {
        if app_connected {
            let event = client.send_request(&FrontendRequest::RegisterAppSession).await?;
            assert!(matches!(event, FrontendEvent::AppSessionRegistered));
        }
        let output = Command::new(boss_binary())
            .args([
                "--json",
                "--no-input",
                "--no-autostart",
                "--socket-path",
                engine.socket_str(),
                "task",
                "show",
                &selector,
            ])
            .env_remove("BOSS_PRODUCT")
            .output()?;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(value["id"], chore.id);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("implicit scope"));
    }

    // Engine shared choke point (what the CLI routes bare short ids through).
    match client
        .send_request(&FrontendRequest::GetWorkItem { id: selector })
        .await?
    {
        FrontendEvent::WorkItemResult {
            item: WorkItem::Chore(t) | WorkItem::Task(t),
        } => {
            assert_eq!(t.id, chore.id);
            assert_eq!(t.short_id, Some(short_id));
        }
        other => return Err(anyhow!("expected WorkItemResult, got {other:?}")),
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_show_nonexistent_short_id_is_not_found_not_product_required() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let _product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@example.com:boss.git")
            .build(),
    )
    .await?;
    let _other = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Flunge")
            .repo_remote_url("git@example.com:flunge.git")
            .build(),
    )
    .await?;
    drop(client);

    let stderr = run_boss_expect_failure(engine.socket_str(), &["task", "show", &format!("T{}", 999_999)])?;
    let lower = stderr.to_ascii_lowercase();
    assert!(
        !lower.contains("product is required"),
        "nonexistent short id must not report a missing --product flag: {stderr}"
    );
    assert!(
        lower.contains("could not resolve")
            || lower.contains("no matching")
            || lower.contains("unknown work item")
            || lower.contains("no work item"),
        "expected a not-found / could-not-resolve error, got: {stderr}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_show_ambiguous_short_id_lists_every_candidate() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    // Distinct repos so chore create is allowed under each product.
    let boss = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@example.com:boss.git")
            .build(),
    )
    .await?;
    let flunge = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Flunge")
            .repo_remote_url("git@example.com:flunge.git")
            .build(),
    )
    .await?;

    // Force the same short_id number in both products by creating one
    // chore in each. Per-product sequences both start at 1, so the first
    // chore in each product shares short_id = 1.
    let boss_chore = create_chore(&mut client, &boss.id, "Boss side").await?;
    let flunge_chore = create_chore(&mut client, &flunge.id, "Flunge side").await?;
    let boss_n = boss_chore.short_id.ok_or_else(|| anyhow!("no short_id"))?;
    let flunge_n = flunge_chore.short_id.ok_or_else(|| anyhow!("no short_id"))?;
    assert_eq!(boss_n, flunge_n, "first chore in each product should share short_id=1");
    let selector = format!("T{boss_n}");
    let boss_id = boss_chore.id.clone();
    let flunge_id = flunge_chore.id.clone();
    let boss_slug = boss.slug.clone();
    let flunge_slug = flunge.slug.clone();
    drop(client);

    let output = Command::new(boss_binary())
        .args([
            "--json",
            "--no-input",
            "--no-autostart",
            "--socket-path",
            engine.socket_str(),
            "task",
            "show",
            &selector,
        ])
        .output()?;
    assert!(
        !output.status.success(),
        "ambiguous short id must exit non-zero; stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ambiguous"), "error must name ambiguity: {stderr}");
    // Every candidate: long id, product, name, status.
    assert!(
        stderr.contains(&boss_id) && stderr.contains(&flunge_id),
        "error must list both primary ids: {stderr}"
    );
    assert!(
        (stderr.contains("Boss") && stderr.contains("Flunge"))
            || (stderr.contains(&boss_slug) && stderr.contains(&flunge_slug)),
        "error must name both products: {stderr}"
    );
    Ok(())
}

/// Representative id-accepting surfaces: each argv must hard-error on a
/// nonexistent short id (exit != 0) with a resolve / not-found message,
/// never "product is required" and never a successful empty listing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn representative_surfaces_hard_error_on_missing_short_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let _product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@example.com:boss.git")
            .build(),
    )
    .await?;
    let _other = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Flunge")
            .repo_remote_url("git@example.com:flunge.git")
            .build(),
    )
    .await?;
    drop(client);

    let missing = format!("T{}", 999_999);
    // Surfaces that only need the short id (or the short id as a filter)
    // plus fixed flags that do not themselves require a live work item.
    let surfaces: Vec<Vec<&str>> = vec![
        vec!["task", "show", &missing],
        vec!["chore", "show", &missing],
        vec!["task", "executions", &missing],
        vec!["task", "depend", "list", &missing],
        vec!["engine", "ci", "list", "--work-item", &missing],
        vec!["engine", "attempts", "list", "--work-item", &missing],
        vec!["engine", "conflicts", "list", "--work-item", &missing],
        vec!["task", "create-revision", "--parent", &missing, "--description", "x"],
        vec![
            "task",
            "create-revision",
            "--parent",
            "task_deadbeef",
            "--description",
            "x",
            "--depends-on",
            &missing,
        ],
    ];
    for args in &surfaces {
        let output = Command::new(boss_binary())
            .args([
                "--json",
                "--no-input",
                "--no-autostart",
                "--socket-path",
                engine.socket_str(),
            ])
            .args(args)
            .output()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let combined = format!("{stderr}\n{stdout}").to_ascii_lowercase();
        assert!(
            !output.status.success(),
            "surface {args:?} must exit non-zero for missing short id; stdout={stdout} stderr={stderr}"
        );
        assert!(
            !combined.contains("product is required"),
            "surface {args:?} must not misreport missing short id as product-required: {stderr}"
        );
        assert!(
            combined.contains("could not resolve")
                || combined.contains("no matching")
                || combined.contains("unknown work item")
                || combined.contains("no work item"),
            "surface {args:?} expected resolve/not-found error, got: {stderr}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_work_item_by_short_id_disambiguates_with_product() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let boss = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@example.com:boss.git")
            .build(),
    )
    .await?;
    let flunge = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Flunge")
            .repo_remote_url("git@example.com:flunge.git")
            .build(),
    )
    .await?;
    let boss_chore = create_chore(&mut client, &boss.id, "Boss side").await?;
    let _flunge_chore = create_chore(&mut client, &flunge.id, "Flunge side").await?;
    let n = boss_chore.short_id.ok_or_else(|| anyhow!("no short_id"))?;

    match client
        .send_request(&FrontendRequest::GetWorkItemByShortId {
            product_id: boss.id.clone(),
            short_id: n,
        })
        .await?
    {
        FrontendEvent::WorkItemResult {
            item: WorkItem::Chore(t) | WorkItem::Task(t),
        } => {
            assert_eq!(t.id, boss_chore.id);
        }
        other => return Err(anyhow!("expected WorkItemResult for boss product, got {other:?}")),
    }
    Ok(())
}
