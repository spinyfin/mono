//! End-to-end coverage for the effort-level / model-override CLI
//! surface added by PR #370's schema task. Spawns an in-process
//! engine on a temp socket, drives the `boss` binary through the
//! create / edit / show paths, and checks the new fields land in
//! both the JSON response shape and the round-tripped DB row.
//!
//! Acceptance criteria from the work item:
//! - `boss chore create --effort large --model claude-opus-4-7 …`
//!   succeeds and `boss chore show <id> --json` returns both fields.
//! - `boss chore create --effort galaxybrain` fails fast.
//! - `boss product set-default-model boss --model sonnet` succeeds
//!   and `boss product show boss --json` includes
//!   `default_model: "sonnet"`.

use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_protocol::CreateProductInput;

use common::{run_boss, run_boss_expect_failure};
use harness::{TestEngine, create_product_with};

// Multi-thread runtime: the test launches the `boss` binary as a
// blocking subprocess via `Command::output()`. The in-process
// engine's accept loop needs a separate worker thread to handle
// connects while the test thread is parked on the subprocess.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_with_effort_and_model_round_trips_through_show() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            // Test product has a repo so creation-time repo
            // resolution (design §Q4) doesn't refuse in --no-input;
            // this test is about effort / model fields, not repo
            // inference.
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )
    .await?;

    let created = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "Investigate the slow path",
            "--description",
            "ought to take all day",
            "--effort",
            "large",
            "--model",
            "claude-opus-4-7",
        ],
    )?;
    let chore_id = created["chore"]["id"]
        .as_str()
        .ok_or_else(|| anyhow!("chore create did not return an id: {created}"))?
        .to_owned();
    assert_eq!(created["chore"]["effort_level"].as_str(), Some("large"));
    assert_eq!(created["chore"]["model_override"].as_str(), Some("claude-opus-4-7"),);

    let shown = run_boss(engine.socket_str(), &["chore", "show", &chore_id])?;
    assert_eq!(shown["effort_level"].as_str(), Some("large"));
    assert_eq!(shown["model_override"].as_str(), Some("claude-opus-4-7"),);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_rejects_invalid_effort_level() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            // Test product has a repo so creation-time repo
            // resolution (design §Q4) doesn't refuse in --no-input;
            // this test is about effort / model fields, not repo
            // inference.
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )
    .await?;

    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "fix it",
            "--description",
            "",
            "--effort",
            "galaxybrain",
        ],
    )?;
    // clap surfaces the allowed-values set in its error message; the
    // five values from the design's Q1 enum are the contract we ship.
    assert!(stderr.contains("galaxybrain"), "stderr: {stderr}");
    assert!(stderr.contains("trivial"), "stderr: {stderr}");
    assert!(stderr.contains("max"), "stderr: {stderr}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_update_sets_clears_effort_and_model_round_trip() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            // Test product has a repo so creation-time repo
            // resolution (design §Q4) doesn't refuse in --no-input;
            // this test is about effort / model fields, not repo
            // inference.
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )
    .await?;

    let created = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "Some work",
            "--description",
            "",
        ],
    )?;
    let chore_id = created["chore"]["id"].as_str().expect("chore id").to_owned();
    // Fresh row: NULL on both fields.
    assert!(created["chore"]["effort_level"].is_null());
    assert!(created["chore"]["model_override"].is_null());

    // Set via update.
    let updated = run_boss(
        engine.socket_str(),
        &["chore", "update", &chore_id, "--effort", "medium", "--model", "sonnet"],
    )?;
    assert_eq!(updated["chore"]["effort_level"].as_str(), Some("medium"));
    assert_eq!(updated["chore"]["model_override"].as_str(), Some("sonnet"));

    // Clear via --unset-effort / --unset-model.
    let cleared = run_boss(
        engine.socket_str(),
        &["chore", "update", &chore_id, "--unset-effort", "--unset-model"],
    )?;
    assert!(cleared["chore"]["effort_level"].is_null());
    assert!(cleared["chore"]["model_override"].is_null());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn product_set_default_model_lifecycle_round_trips() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            // Test product has a repo so creation-time repo
            // resolution (design §Q4) doesn't refuse in --no-input;
            // this test is about effort / model fields, not repo
            // inference.
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )
    .await?;

    // Fresh row: no default model.
    let shown = run_boss(engine.socket_str(), &["product", "show", &product.id])?;
    assert!(shown["product"]["default_model"].is_null());

    // Set.
    let set_resp = run_boss(
        engine.socket_str(),
        &["product", "set-default-model", &product.id, "--model", "sonnet"],
    )?;
    assert_eq!(set_resp["product"]["default_model"].as_str(), Some("sonnet"),);

    // Show reflects it.
    let shown = run_boss(engine.socket_str(), &["product", "show", &product.id])?;
    assert_eq!(shown["product"]["default_model"].as_str(), Some("sonnet"));

    // Clear.
    let unset_resp = run_boss(
        engine.socket_str(),
        &["product", "set-default-model", &product.id, "--unset"],
    )?;
    assert!(unset_resp["product"]["default_model"].is_null());
    Ok(())
}

/// Slugs are stored verbatim — the engine deliberately does not
/// validate against any known set. Confirms a hypothetical future
/// model slug round-trips today without an engine release.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_override_passes_through_unrecognised_slug() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            // Test product has a repo so creation-time repo
            // resolution (design §Q4) doesn't refuse in --no-input;
            // this test is about effort / model fields, not repo
            // inference.
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )
    .await?;

    let created = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "n",
            "--description",
            "",
            "--model",
            "claude-future-model-7-extra-thinking",
        ],
    )?;
    assert_eq!(
        created["chore"]["model_override"].as_str(),
        Some("claude-future-model-7-extra-thinking"),
    );
    Ok(())
}

/// The end-to-end shape of the capability lever: `--reasoning` is settable at
/// create, visible on `show`, updatable and clearable afterwards, and moves
/// entirely independently of `--effort`. The `small` + `investigation`
/// combination in this test is the one that had no expression at all before
/// the column existed — the only way to get that row onto a stronger model was
/// to call it `large`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_reasoning_round_trips_and_is_independent_of_effort() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )
    .await?;

    let created = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "Cards don't surface merge-conflict state",
            "--description",
            "repro supplied; diagnose why the transition never fires",
            "--effort",
            "small",
            "--reasoning",
            "investigation",
        ],
    )?;
    let chore_id = created["chore"]["id"]
        .as_str()
        .ok_or_else(|| anyhow!("chore create did not return an id: {created}"))?
        .to_owned();
    assert_eq!(created["chore"]["effort_level"].as_str(), Some("small"));
    assert_eq!(created["chore"]["reasoning"].as_str(), Some("investigation"));

    let shown = run_boss(engine.socket_str(), &["chore", "show", &chore_id])?;
    assert_eq!(shown["effort_level"].as_str(), Some("small"));
    assert_eq!(shown["reasoning"].as_str(), Some("investigation"));

    // Demote to standard. The effort level must not move with it.
    let updated = run_boss(
        engine.socket_str(),
        &["chore", "update", &chore_id, "--reasoning", "standard"],
    )?;
    assert_eq!(updated["chore"]["reasoning"].as_str(), Some("standard"));
    assert_eq!(
        updated["chore"]["effort_level"].as_str(),
        Some("small"),
        "changing reasoning must not touch the size signal",
    );

    // …and the converse: changing effort must not touch reasoning.
    let re_sized = run_boss(
        engine.socket_str(),
        &["chore", "update", &chore_id, "--effort", "large"],
    )?;
    assert_eq!(re_sized["chore"]["effort_level"].as_str(), Some("large"));
    assert_eq!(
        re_sized["chore"]["reasoning"].as_str(),
        Some("standard"),
        "changing effort must not touch the capability signal",
    );

    // Clearing drops the row back to the dispatcher's legacy resolution.
    let cleared = run_boss(
        engine.socket_str(),
        &["chore", "update", &chore_id, "--unset-reasoning"],
    )?;
    assert!(
        cleared["chore"]["reasoning"].is_null(),
        "--unset-reasoning must clear the column, got: {cleared}",
    );
    Ok(())
}

/// A plain chore created without `--reasoning` is seeded `standard`, so the
/// signal is visible on the row from the moment it exists rather than being an
/// invisible fall-through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_seeds_standard_reasoning_by_default() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )
    .await?;

    let created = run_boss(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "Rename a symbol",
            "--description",
            "",
            "--effort",
            "trivial",
        ],
    )?;
    assert_eq!(created["chore"]["reasoning"].as_str(), Some("standard"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chore_create_rejects_invalid_reasoning_mode() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@github.com:test/boss.git")
            .build(),
    )
    .await?;

    let stderr = run_boss_expect_failure(
        engine.socket_str(),
        &[
            "chore",
            "create",
            "--product",
            &product.id,
            "--name",
            "fix it",
            "--description",
            "",
            "--reasoning",
            "galaxybrain",
        ],
    )?;
    assert!(stderr.contains("galaxybrain"), "stderr: {stderr}");
    assert!(stderr.contains("standard"), "stderr: {stderr}");
    assert!(stderr.contains("investigation"), "stderr: {stderr}");
    Ok(())
}
