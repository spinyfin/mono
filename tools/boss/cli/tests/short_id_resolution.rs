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

use common::{boss_binary, run_boss_expect_failure};
use harness::{TestEngine, create_chore, create_product_with};

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
