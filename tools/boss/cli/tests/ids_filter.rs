//! Integration tests for `--ids` on the list verbs (`boss task list`,
//! `boss chore list`, `boss project list`). Drives the `boss` binary
//! against an in-process engine to verify: friendly-id resolution
//! (mixing short and primary id forms in one call), the all-or-nothing
//! unknown-id error, composition with an existing filter
//! (`--priority`), composition with a server-side dependency filter
//! (`--blocked-by-deps`), and the all-empty-selector footgun.

use anyhow::Result;
use boss_client::BossClient;
use boss_protocol::CreateChoreInput;

use common::{run_boss, run_boss_expect_failure};
use harness::{TestEngine, create_chore, create_chore_with, create_product, create_project, create_task};

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

fn chore_ids(value: &serde_json::Value) -> Vec<String> {
    sorted(
        value["chores"]
            .as_array()
            .expect("chores array")
            .iter()
            .map(|c| c["id"].as_str().expect("chore id").to_owned())
            .collect(),
    )
}

fn task_ids(value: &serde_json::Value) -> Vec<String> {
    sorted(
        value["tasks"]
            .as_array()
            .expect("tasks array")
            .iter()
            .map(|t| t["id"].as_str().expect("task id").to_owned())
            .collect(),
    )
}

fn project_ids(value: &serde_json::Value) -> Vec<String> {
    sorted(
        value["projects"]
            .as_array()
            .expect("projects array")
            .iter()
            .map(|p| p["id"].as_str().expect("project id").to_owned())
            .collect(),
    )
}

/// A single `--ids` call mixing a friendly short id (`T<n>`) and a
/// primary id (`task_…`) resolves both the same way `boss task show`
/// would, and returns exactly the requested rows — nothing else from
/// the product's other chores leaks in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_list_ids_accepts_mixed_short_and_primary_ids() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let a = create_chore(&mut client, &product.id, "A").await?;
    let b = create_chore(&mut client, &product.id, "B").await?;
    let _c = create_chore(&mut client, &product.id, "C").await?;
    let a_short = a.short_id.expect("chore has short_id");

    let selector = format!("T{a_short},{}", b.id);
    let value = run_boss(
        engine.socket_str(),
        &["chore", "list", "--product", &product.id, "--ids", &selector, "--json"],
    )?;

    assert_eq!(chore_ids(&value), sorted(vec![a.id, b.id]));
    Ok(())
}

/// An id that doesn't name any row in the product is a hard error —
/// asking for N ids and silently getting fewer back is exactly the
/// footgun `--ids` exists to close.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_list_ids_unknown_id_errors() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let a = create_chore(&mut client, &product.id, "A").await?;

    let bogus_short_id = 999_999;
    let selector = format!("{},T{bogus_short_id}", a.id);
    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &["chore", "list", "--product", &product.id, "--ids", &selector, "--json"],
    )?;
    assert!(
        stderr.contains(&bogus_short_id.to_string()) || stderr.to_lowercase().contains("not found"),
        "unexpected stderr: {stderr}"
    );
    Ok(())
}

/// `--ids` composes with other filters (here `--priority`) as an AND:
/// a requested id that exists but doesn't match the other filter is
/// dropped from the result, not treated as an unknown-id error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_list_ids_composes_with_priority_filter() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let high = create_chore_with(
        &mut client,
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("High")
            .priority("high")
            .created_via("test")
            .build(),
    )
    .await?;
    let medium = create_chore_with(
        &mut client,
        CreateChoreInput::builder()
            .product_id(product.id.clone())
            .name("Medium")
            .priority("medium")
            .created_via("test")
            .build(),
    )
    .await?;

    let selector = format!("{},{}", high.id, medium.id);
    let value = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "list",
            "--product",
            &product.id,
            "--ids",
            &selector,
            "--priority",
            "high",
            "--json",
        ],
    )?;

    // Both requested ids exist — the priority filter narrows the
    // result without raising an unknown-id error.
    assert_eq!(chore_ids(&value), vec![high.id]);
    Ok(())
}

/// `task list` accepts the same mixed short-id/primary-id `--ids` form
/// as `chore list` — the four call sites hand-copy the same wiring, so
/// this pins down that `task list`'s copy is wired correctly too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_list_ids_accepts_mixed_short_and_primary_ids() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let project = create_project(&mut client, &product.id, "Project").await?;
    let a = create_task(&mut client, &product.id, &project.id, "A").await?;
    let b = create_task(&mut client, &product.id, &project.id, "B").await?;
    let _c = create_task(&mut client, &product.id, &project.id, "C").await?;
    let a_short = a.short_id.expect("task has short_id");

    let selector = format!("T{a_short},{}", b.id);
    let value = run_boss(
        engine.socket_str(),
        &["task", "list", "--product", &product.id, "--ids", &selector, "--json"],
    )?;

    assert_eq!(task_ids(&value), sorted(vec![a.id, b.id]));
    Ok(())
}

/// `--ids` composes with a server-side dependency filter (`--dep`) as
/// an AND, exactly like it does with `--priority`: a requested id that
/// exists but gets excluded by `--blocked-by-deps` (because it has no
/// incomplete prerequisite) is dropped from the result, not reported
/// as an unknown id. Before the fix, `known_ids` was built from the
/// already dep-filtered listing, so this case spuriously errored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_list_ids_composes_with_dep_filter_without_erroring() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let project = create_project(&mut client, &product.id, "Project").await?;
    // No dependencies, so `--blocked-by-deps` excludes it: it isn't
    // gated by any incomplete prerequisite.
    let a = create_task(&mut client, &product.id, &project.id, "A").await?;

    let value = run_boss(
        engine.socket_str(),
        &[
            "task",
            "list",
            "--product",
            &product.id,
            "--ids",
            &a.id,
            "--blocked-by-deps",
            "--json",
        ],
    )?;

    // `a` exists (so `--ids` doesn't error) but is excluded by
    // `--blocked-by-deps` (so the result is empty, not an error).
    assert_eq!(task_ids(&value), Vec::<String>::new());
    Ok(())
}

/// `project list` accepts the `P<n>` friendly short-id form via
/// `--ids`, same as `task`/`chore` accept `T<n>`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_list_ids_accepts_short_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let a = create_project(&mut client, &product.id, "A").await?;
    let _b = create_project(&mut client, &product.id, "B").await?;
    let a_short = a.short_id.expect("project has short_id");

    let selector = format!("P{a_short}");
    let value = run_boss(
        engine.socket_str(),
        &[
            "project",
            "list",
            "--product",
            &product.id,
            "--ids",
            &selector,
            "--json",
        ],
    )?;

    assert_eq!(project_ids(&value), vec![a.id]);
    Ok(())
}

/// A syntactically valid but non-existent primary id (`task_deadbeef`)
/// takes the typed/`Other`-selector path rather than the short-id
/// path, and must still fail the `known_ids` membership check with the
/// `--ids:`-prefixed message, not a generic engine error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_list_ids_unknown_primary_id_errors_with_ids_message() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let project = create_project(&mut client, &product.id, "Project").await?;
    let _a = create_task(&mut client, &product.id, &project.id, "A").await?;

    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &[
            "task",
            "list",
            "--product",
            &product.id,
            "--ids",
            "task_deadbeef",
            "--json",
        ],
    )?;
    assert!(
        stderr.contains("--ids: no matching row for") && stderr.contains("task_deadbeef"),
        "unexpected stderr: {stderr}"
    );
    Ok(())
}

/// An all-empty/whitespace `--ids` value (the common `--ids "$IDS"`
/// footgun when the shell variable is unset) must error rather than
/// silently degrading to "no id filter" and returning every row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_list_ids_all_empty_errors() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let _a = create_chore(&mut client, &product.id, "A").await?;

    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &["chore", "list", "--product", &product.id, "--ids", "", "--json"],
    )?;
    assert!(stderr.contains("--ids: no ids given"), "unexpected stderr: {stderr}");
    Ok(())
}

/// The old `--id` spelling (pre-`--ids`) must keep working as an
/// alias, unchanged in meaning — deleting it outright would silently
/// break any existing script or agent invocation using it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_list_id_alias_still_works() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let a = create_chore(&mut client, &product.id, "A").await?;
    let _b = create_chore(&mut client, &product.id, "B").await?;

    let value = run_boss(
        engine.socket_str(),
        &["chore", "list", "--product", &product.id, "--id", &a.id, "--json"],
    )?;

    assert_eq!(chore_ids(&value), vec![a.id]);
    Ok(())
}
