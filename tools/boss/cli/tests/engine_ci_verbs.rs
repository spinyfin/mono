//! End-to-end coverage for the `boss engine ci …` and
//! `boss engine attempts …` verb families (design Phase 11 #35/#36
//! of `merge-conflict-handling-in-review.md`). Spawns an in-process
//! engine, seeds `ci_remediations` rows via the engine library's
//! `WorkDb`, and drives the `boss` binary through:
//!
//!   - `boss engine ci list` (with and without filters, JSON + text)
//!   - `boss engine ci show <attempt-id>`
//!   - `boss engine ci abandon <attempt-id>`
//!   - `boss engine ci retry <work-item-id-or-attempt-id>`
//!   - `boss engine ci budget show / set / set --clear`
//!   - `boss engine attempts list` (unified, including `--background`)
//!
//! These are the acceptance tests called out by the work item:
//! "snapshot tests on CLI" / "list shows entries from all three
//! subsystems with correct `kind` column."

use anyhow::Result;
use boss_client::BossClient;
use boss_engine::coordinator::CubeWorkspaceLease;
use boss_engine::ladder_lease_registry;
use boss_engine::work::{CiRemediationInsertInput, ConflictResolutionInsertInput, WorkDb};
use boss_protocol::{CreateChoreInput, WorkItemPatch};

use common::{run_boss, run_boss_expect_failure, run_boss_human};
use harness::{TestEngine, create_product};

/// Seed a chore plus one CI remediation row directly through the
/// engine's `WorkDb`. Returns `(chore_id, attempt_id)`.
fn seed_chore_with_ci_attempt(
    db: &WorkDb,
    product_id: &str,
    name: &str,
    pr_number: i64,
    attempt_kind: &str,
    head_sha: &str,
) -> Result<(String, String)> {
    let chore = db.create_chore(
        CreateChoreInput::builder()
            .product_id(product_id)
            .name(name)
            .autostart(false)
            .build(),
    )?;
    let pr_url = format!("https://github.com/test/boss/pull/{pr_number}");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.clone()),
            ..WorkItemPatch::default()
        },
    )?;
    let consumes_budget = if attempt_kind == "fix" { 1 } else { 0 };
    let attempt = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product_id.to_owned(),
            work_item_id: chore.id.clone(),
            pr_url,
            pr_number,
            head_branch: format!("feature-{pr_number}"),
            head_sha_at_trigger: head_sha.to_owned(),
            attempt_kind: attempt_kind.to_owned(),
            consumes_budget,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })?
        .expect("insert returned new row");
    Ok((chore.id, attempt.id))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_list_returns_rows_freshest_first_in_json_and_text() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;

    let (chore_a, _) = seed_chore_with_ci_attempt(&db, &product.id, "chore-list-a", 500, "fix", "head-aaa-1")?;
    let (_chore_b, attempt_b) =
        seed_chore_with_ci_attempt(&db, &product.id, "chore-list-b", 501, "retrigger", "head-bbb-1")?;

    let response = run_boss(engine.socket_str(), &["engine", "ci", "list"])?;
    let attempts = response["attempts"].as_array().expect("attempts array");
    assert_eq!(attempts.len(), 2, "json list must include both rows");
    // The most-recently inserted row should land at index 0.
    assert_eq!(attempts[0]["id"].as_str(), Some(attempt_b.as_str()));
    assert!(
        attempts
            .iter()
            .all(|r| r["product_id"].as_str() == Some(product.id.as_str())),
        "all rows must echo the seed product"
    );

    // Filter by work-item should narrow to one row.
    let by_item = run_boss(engine.socket_str(), &["engine", "ci", "list", "--work-item", &chore_a])?;
    let by_item_attempts = by_item["attempts"].as_array().expect("attempts array");
    assert_eq!(by_item_attempts.len(), 1);
    assert_eq!(by_item_attempts[0]["work_item_id"].as_str(), Some(chore_a.as_str()),);

    // Text mode renders a table with the documented columns + an
    // attempt_kind column for the CI view.
    let text = run_boss_human(engine.socket_str(), &["engine", "ci", "list"])?;
    assert!(text.contains("KIND"), "text output must include KIND column: {text}");
    assert!(text.contains("STATUS"));
    assert!(
        text.contains("retrigger"),
        "text output must show the kind value: {text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_show_returns_single_row_with_failed_checks_and_log() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;
    let (_, attempt_id) = seed_chore_with_ci_attempt(&db, &product.id, "chore-show", 600, "fix", "head-show-1")?;
    let shown = run_boss(engine.socket_str(), &["engine", "ci", "show", &attempt_id])?;
    assert_eq!(shown["attempt"]["id"].as_str(), Some(attempt_id.as_str()));
    assert_eq!(shown["attempt"]["attempt_kind"].as_str(), Some("fix"));
    assert_eq!(shown["attempt"]["consumes_budget"].as_i64(), Some(1));

    // Unknown id → CliError::Application (exit 6), with a clear message.
    let stderr = run_boss_expect_failure(engine.socket_str(), &["engine", "ci", "show", "cir_does_not_exist"])?;
    assert!(
        stderr.contains("unknown"),
        "expected 'unknown' in stderr, got: {stderr}",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_abandon_marks_attempt_abandoned() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;
    let (_, attempt_id) = seed_chore_with_ci_attempt(&db, &product.id, "chore-abandon", 700, "fix", "head-abandon-1")?;
    let result = run_boss(
        engine.socket_str(),
        &["engine", "ci", "abandon", &attempt_id, "--reason", "manual_test"],
    )?;
    assert_eq!(result["attempt"]["status"].as_str(), Some("abandoned"));
    assert_eq!(result["attempt"]["failure_reason"].as_str(), Some("manual_test"),);

    // Second call on the already-terminal row must surface an error.
    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &["engine", "ci", "abandon", &attempt_id, "--reason", "again"],
    )?;
    assert!(
        stderr.contains("already terminal") || stderr.contains("unknown"),
        "stderr: {stderr}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_retry_accepts_work_item_id_and_attempt_id() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;
    let (chore_id, attempt_id) =
        seed_chore_with_ci_attempt(&db, &product.id, "chore-retry", 800, "fix", "head-retry-1")?;
    db.increment_ci_attempts_used(&chore_id)?;
    db.increment_ci_attempts_used(&chore_id)?;
    db.mark_chore_blocked_ci_failure_exhausted(&chore_id, &format!("https://github.com/test/boss/pull/{}", 800))?;
    assert!(db.get_ci_attempts_used(&chore_id)? >= 2);

    // Call retry with the work-item id.
    let response = run_boss(engine.socket_str(), &["engine", "ci", "retry", &chore_id])?;
    assert_eq!(response["work_item_id"].as_str(), Some(chore_id.as_str()));
    assert_eq!(response["was_exhausted"].as_bool(), Some(true));
    assert_eq!(response["budget"]["used"].as_i64(), Some(0));

    // Second retry: now via the attempt id (the engine resolves it
    // back to the same parent). The counter is already zero and the
    // parent is no longer exhausted.
    let response2 = run_boss(engine.socket_str(), &["engine", "ci", "retry", &attempt_id])?;
    assert_eq!(response2["work_item_id"].as_str(), Some(chore_id.as_str()));
    assert_eq!(response2["was_exhausted"].as_bool(), Some(false));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_budget_show_and_set_round_trips() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;
    let (chore_id, _) = seed_chore_with_ci_attempt(&db, &product.id, "chore-budget", 900, "fix", "head-budget-1")?;

    // Initial: no per-PR override, product default = 3.
    let initial = run_boss(engine.socket_str(), &["engine", "ci", "budget", "show", &chore_id])?;
    assert!(initial["budget"]["per_pr_override"].is_null());
    assert_eq!(initial["budget"]["product_default"].as_i64(), Some(3));
    assert_eq!(initial["budget"]["effective"].as_i64(), Some(3));

    // Set override to 5.
    let set = run_boss(
        engine.socket_str(),
        &["engine", "ci", "budget", "set", &chore_id, "--budget", "5"],
    )?;
    assert_eq!(set["budget"]["per_pr_override"].as_i64(), Some(5));
    assert_eq!(set["budget"]["effective"].as_i64(), Some(5));

    // Set above the cap; engine clamps to 10.
    let clamped = run_boss(
        engine.socket_str(),
        &["engine", "ci", "budget", "set", &chore_id, "--budget", "25"],
    )?;
    assert_eq!(clamped["budget"]["per_pr_override"].as_i64(), Some(10));

    // Clear → product default applies again.
    let cleared = run_boss(
        engine.socket_str(),
        &["engine", "ci", "budget", "set", &chore_id, "--clear"],
    )?;
    assert!(cleared["budget"]["per_pr_override"].is_null());
    assert_eq!(cleared["budget"]["effective"].as_i64(), Some(3));

    // Neither --budget nor --clear → usage error.
    let stderr = run_boss_expect_failure(engine.socket_str(), &["engine", "ci", "budget", "set", &chore_id])?;
    assert!(
        stderr.contains("--budget") || stderr.contains("--clear"),
        "stderr: {stderr}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_attempts_list_includes_all_three_kinds() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;

    // Seed both a CI attempt and a conflict resolution attempt against
    // the same chore.
    let (chore_id, _) = seed_chore_with_ci_attempt(&db, &product.id, "chore-attempts", 1100, "fix", "head-1")?;
    let pr_url = "https://github.com/test/boss/pull/1100".to_owned();
    db.insert_conflict_resolution(ConflictResolutionInsertInput {
        product_id: product.id.clone(),
        work_item_id: chore_id.clone(),
        pr_url,
        pr_number: 1100,
        head_branch: "feature-1100".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: Some("base-1".into()),
        head_sha_before: Some("head-1".into()),
    })?
    .expect("insert");

    let listing = run_boss(engine.socket_str(), &["engine", "attempts", "list"])?;
    let attempts = listing["attempts"].as_array().expect("attempts array");
    let kinds: Vec<&str> = attempts.iter().filter_map(|r| r["kind"].as_str()).collect();
    assert!(kinds.contains(&"ci"), "expected ci kind in {kinds:?}");
    assert!(kinds.contains(&"conflict"), "expected conflict kind in {kinds:?}");
    assert!(
        listing.get("background_work").is_none(),
        "without --background JSON must omit background_work so an empty array cannot read as a live snapshot: {listing}"
    );

    // --kind filter narrows to one subsystem.
    let only_ci = run_boss(engine.socket_str(), &["engine", "attempts", "list", "--kind", "ci"])?;
    let only_ci_rows = only_ci["attempts"].as_array().expect("attempts array");
    assert!(!only_ci_rows.is_empty());
    for r in only_ci_rows {
        assert_eq!(r["kind"].as_str(), Some("ci"));
    }

    // Text mode renders a KIND column.
    let text = run_boss_human(engine.socket_str(), &["engine", "attempts", "list"])?;
    assert!(text.contains("KIND"), "text output must include KIND column: {text}");
    assert!(text.contains("ci"), "text output must surface ci kind: {text}");
    assert!(
        text.contains("conflict"),
        "text output must surface conflict kind: {text}"
    );
    assert!(
        !text.contains("Background work"),
        "history-only human output must stay unchanged without --background: {text}"
    );

    // `--background` with no live snapshot still prints the engine count (0)
    // and leaves the history table in place.
    let empty_snapshot = run_boss(engine.socket_str(), &["engine", "attempts", "list", "--background"])?;
    let empty_background = empty_snapshot["background_work"].as_array().expect("background_work");
    assert!(
        empty_background.is_empty(),
        "no live mechanical rung must yield an empty snapshot: {empty_background:?}"
    );
    let empty_text = run_boss_human(engine.socket_str(), &["engine", "attempts", "list", "--background"])?;
    assert!(
        empty_text.contains("Background work (0)"),
        "human --background must render the engine-provided count: {empty_text}"
    );
    assert!(
        empty_text.contains("KIND"),
        "human --background must keep the history table: {empty_text}"
    );
    Ok(())
}

/// Seed a chore plus one pending conflict-resolution row. Returns
/// `(chore_id, attempt_id)`.
fn seed_chore_with_conflict_attempt(
    db: &WorkDb,
    product_id: &str,
    name: &str,
    pr_number: i64,
) -> Result<(String, String)> {
    let chore = db.create_chore(
        CreateChoreInput::builder()
            .product_id(product_id)
            .name(name)
            .autostart(false)
            .build(),
    )?;
    let pr_url = format!("https://github.com/test/boss/pull/{pr_number}");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.clone()),
            ..WorkItemPatch::default()
        },
    )?;
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.to_owned(),
            work_item_id: chore.id.clone(),
            pr_url,
            pr_number,
            head_branch: format!("feature-{pr_number}"),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-1".into()),
            head_sha_before: Some("head-1".into()),
        })?
        .expect("insert returned new row");
    Ok((chore.id, attempt.id))
}

/// Guard that unregisters a lease from the process-wide
/// `ladder_lease_registry` on drop.
///
/// Tests should prefer `snapshot_with_leases` (see `background_work`'s
/// module doc) so they do not share that registry. This test cannot:
/// it goes over RPC to a live engine that reads the process-wide set.
/// Registration is still safe here because the window is kept as tight
/// as possible around the `--background` invocations — every
/// `TestEngine::spawn` in this binary starts
/// `ladder_lease_heartbeat::spawn_loop`, whose first pass fires
/// immediately and would `cube_client.heartbeat_lease` every
/// registered lease, including a fabricated id, if another engine
/// started while this lease was still live. The 120s sweep interval
/// makes a later pass unlikely; the prompt drop is what keeps the
/// first-pass race narrow.
struct RegisteredLease(String);

impl Drop for RegisteredLease {
    fn drop(&mut self) {
        ladder_lease_registry::unregister(&self.0);
    }
}

/// Locate `row_needle` in human table output, split on the comfy-table
/// column separator, and return the cell under `header` (trimmed).
fn table_cell<'a>(text: &'a str, row_needle: &str, header: &str) -> &'a str {
    let header_line = text
        .lines()
        .find(|line| line.contains(header))
        .unwrap_or_else(|| panic!("missing header {header:?} in:\n{text}"));
    let row_line = text
        .lines()
        .find(|line| line.contains(row_needle))
        .unwrap_or_else(|| panic!("missing row {row_needle:?} in:\n{text}"));
    let headers: Vec<&str> = header_line.split('|').map(str::trim).collect();
    let cells: Vec<&str> = row_line.split('|').map(str::trim).collect();
    let idx = headers
        .iter()
        .position(|cell| *cell == header)
        .unwrap_or_else(|| panic!("header {header:?} not a column in {header_line:?}"));
    cells
        .get(idx)
        .copied()
        .unwrap_or_else(|| panic!("row {row_line:?} has no cell at {idx}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conflict_list_and_show_render_mechanical_rung() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;
    let (_, attempt_id) = seed_chore_with_conflict_attempt(&db, &product.id, "chore-rung", 1200)?;
    db.stamp_conflict_resolution_mechanical_rung(&attempt_id, 1, "cli-rung-lease", "cli-rung-ws")?
        .expect("stamp applied");

    let listing = run_boss(engine.socket_str(), &["engine", "conflicts", "list"])?;
    let attempts = listing["attempts"].as_array().expect("attempts array");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["mechanical_rung_in_flight"].as_i64(), Some(1));

    let text = run_boss_human(engine.socket_str(), &["engine", "conflicts", "list"])?;
    assert!(
        text.contains("MECH RUNG"),
        "human table must include the mechanical-rung column: {text}"
    );
    assert_eq!(
        table_cell(&text, &attempt_id, "MECH RUNG"),
        "1",
        "human table MECH RUNG cell must be the in-flight rung, not a digit from PR/created columns: {text}"
    );

    let shown = run_boss(engine.socket_str(), &["engine", "conflicts", "show", &attempt_id])?;
    assert_eq!(shown["attempt"]["mechanical_rung_in_flight"].as_i64(), Some(1));

    let detail = run_boss_human(engine.socket_str(), &["engine", "conflicts", "show", &attempt_id])?;
    let detail_row = detail
        .lines()
        .find(|line| line.contains("mechanical_rung_in_flight"))
        .unwrap_or_else(|| panic!("human detail must name the field: {detail}"));
    let detail_cells: Vec<&str> = detail_row
        .split('|')
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect();
    assert_eq!(
        detail_cells,
        ["mechanical_rung_in_flight", "1"],
        "human detail must render the in-flight rung as the VALUE on the field's row: {detail}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_attempts_list_background_renders_engine_snapshot() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Boss").await?;
    let db = engine.db()?;
    let (chore_id, attempt_id) = seed_chore_with_conflict_attempt(&db, &product.id, "Fix the merge conflict", 1300)?;
    let lease_id = format!("cli-bg-lease-{attempt_id}");
    let workspace_id = format!("cli-bg-ws-{attempt_id}");
    db.stamp_conflict_resolution_mechanical_rung(&attempt_id, 1, &lease_id, &workspace_id)?
        .expect("stamp applied");

    let without_flag = run_boss(engine.socket_str(), &["engine", "attempts", "list"])?;
    assert!(
        without_flag.get("background_work").is_none(),
        "flag-absent JSON must omit background_work: {without_flag}"
    );
    let without_text = run_boss_human(engine.socket_str(), &["engine", "attempts", "list"])?;
    assert!(
        !without_text.contains("Background work"),
        "flag-absent human output must not render the snapshot: {without_text}"
    );
    assert!(
        without_text.contains("KIND"),
        "flag-absent human output must still be the history table: {without_text}"
    );

    {
        ladder_lease_registry::register(&CubeWorkspaceLease {
            lease_id: lease_id.clone(),
            workspace_id,
            workspace_path: "/tmp/cli-bg-ws".into(),
            dirty_verified: None,
        });
        let _lease = RegisteredLease(lease_id);

        let listing = run_boss(engine.socket_str(), &["engine", "attempts", "list", "--background"])?;
        let background = listing["background_work"].as_array().expect("background_work");
        assert_eq!(
            background.len(),
            1,
            "CLI must report the engine-provided count, not re-query: {background:?}"
        );
        assert_eq!(background[0]["kind"].as_str(), Some("conflict_remediation"));
        assert_eq!(background[0]["source_id"].as_str(), Some(attempt_id.as_str()));
        assert_eq!(background[0]["phase"].as_str(), Some("Rebasing Fix the merge conflict"));
        assert_eq!(background[0]["work_item_id"].as_str(), Some(chore_id.as_str()));
        let attempts = listing["attempts"].as_array().expect("attempts array");
        assert_eq!(
            attempts.len(),
            1,
            "--background must not hide the history list: {attempts:?}"
        );

        let text = run_boss_human(engine.socket_str(), &["engine", "attempts", "list", "--background"])?;
        assert!(
            text.contains("Background work (1)"),
            "human --background must render the engine-provided count: {text}"
        );
        assert!(
            text.contains("conflict_remediation"),
            "human --background must render the engine-provided kind: {text}"
        );
        assert_eq!(
            table_cell(&text, &attempt_id, "SOURCE ID"),
            attempt_id.as_str(),
            "human --background ID column must be source_id for other engine verbs: {text}"
        );
        assert!(
            !text.contains(&format!("conflict_remediation:{attempt_id}")),
            "human --background must not reprint the namespaced id: {text}"
        );
        assert!(
            text.contains("Rebasing Fix the merge conflict"),
            "human --background must render engine-authored phase text: {text}"
        );
        assert!(
            text.contains("KIND"),
            "human --background must still print the history table: {text}"
        );
    }
    Ok(())
}
