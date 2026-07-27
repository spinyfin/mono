//! End-to-end coverage for `boss decision` create/list/show/revoke/supersede
//! and the create-time overlap warning (stderr-only, non-blocking).

use std::process::Command;

use anyhow::{Result, anyhow};
use boss_client::BossClient;
use boss_protocol::CreateProductInput;

use common::{boss_binary, run_boss, run_boss_human};
use harness::{TestEngine, create_product_with};

// Multi-thread runtime: the test launches the `boss` binary as a
// blocking subprocess via `Command::output()`. With the default
// current_thread runtime, that call parks the executor and the
// in-process engine's accept loop never gets to handle the
// subprocess's connect — the test hangs until the global timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decision_create_list_show_revoke_supersede_round_trip() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@example.com:boss.git")
            .build(),
    )
    .await?;

    // create
    let created = run_boss(
        engine.socket_str(),
        &[
            "decision",
            "create",
            "--product",
            &product.slug,
            "--name",
            "No checkleft all-gating",
            "--description",
            "We considered all-gating and declined for now.",
            "--kind",
            "wontfix",
            "--keywords",
            "checkleft gating",
        ],
    )?;
    let d1 = &created["decision"];
    assert_eq!(d1["title"], "No checkleft all-gating");
    assert_eq!(d1["kind"], "wontfix");
    assert_eq!(d1["status"], "active");
    assert_eq!(d1["short_id"], 1);
    let d1_id = d1["id"].as_str().ok_or_else(|| anyhow!("missing id: {created}"))?;

    // second decision (successor candidate)
    let created2 = run_boss(
        engine.socket_str(),
        &[
            "decision",
            "create",
            "--product",
            &product.slug,
            "--name",
            "Remote is the plan",
            "--description",
            "Local concurrency ceiling stands.",
            "--kind",
            "decided",
        ],
    )?;
    let d2_id = created2["decision"]["id"]
        .as_str()
        .ok_or_else(|| anyhow!("missing id: {created2}"))?
        .to_owned();

    // list envelope
    let listed = run_boss(engine.socket_str(), &["decision", "list", "--product", &product.slug])?;
    let decisions = listed["decisions"]
        .as_array()
        .ok_or_else(|| anyhow!("expected decisions array: {listed}"))?;
    assert_eq!(decisions.len(), 2);
    // newest first
    assert_eq!(decisions[0]["id"].as_str(), Some(d2_id.as_str()));
    assert_eq!(decisions[1]["id"].as_str(), Some(d1_id));

    // show flat (no wrapper) via short id
    let shown = run_boss(
        engine.socket_str(),
        &["decision", "show", "D1", "--product", &product.slug],
    )?;
    assert_eq!(shown["id"].as_str(), Some(d1_id));
    assert_eq!(shown["title"], "No checkleft all-gating");
    assert!(shown.get("decision").is_none(), "show must be flat, got {shown}");

    // human show
    let human = run_boss_human(
        engine.socket_str(),
        &["decision", "show", "D1", "--product", &product.slug],
    )?;
    assert!(human.contains("No checkleft all-gating"), "human output: {human}");
    assert!(human.contains(d1_id), "human output: {human}");

    // supersede d1 by d2
    let superseded = run_boss(engine.socket_str(), &["decision", "supersede", d1_id, "--by", &d2_id])?;
    assert_eq!(superseded["decision"]["status"], "superseded");
    assert_eq!(superseded["decision"]["superseded_by"].as_str(), Some(d2_id.as_str()));

    // list active only → just d2
    let active = run_boss(engine.socket_str(), &["decision", "list", "--product", &product.slug])?;
    assert_eq!(active["decisions"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(active["decisions"][0]["id"].as_str(), Some(d2_id.as_str()));

    // list include-inactive → both
    let all = run_boss(
        engine.socket_str(),
        &["decision", "list", "--product", &product.slug, "--include-inactive"],
    )?;
    assert_eq!(all["decisions"].as_array().map(|a| a.len()), Some(2));

    // revoke d2
    let revoked = run_boss(engine.socket_str(), &["decision", "revoke", &d2_id])?;
    assert_eq!(revoked["decision"]["status"], "revoked");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_create_warns_on_decision_overlap_without_breaking_json() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product_with(
        &mut client,
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url("git@example.com:boss.git")
            .build(),
    )
    .await?;

    // Seed an active decision whose significant tokens will overlap.
    let _ = run_boss(
        engine.socket_str(),
        &[
            "decision",
            "create",
            "--product",
            &product.slug,
            "--name",
            "No checkleft all-gating",
            "--description",
            "Declined for now.",
            "--keywords",
            "checkleft gating",
        ],
    )?;

    // Strong overlap: checkleft + gating in the task name.
    let output = Command::new(boss_binary())
        .args([
            "--json",
            "--no-input",
            "--no-autostart",
            "--socket-path",
            engine.socket_str(),
            "chore",
            "create",
            "--product",
            &product.slug,
            "--name",
            "Add checkleft all-gating enforcement",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "chore create should succeed; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    // stdout must remain valid JSON (warning goes to stderr only).
    let value: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|e| anyhow!("stdout not valid JSON ({e}): {stdout}\nstderr={stderr}"))?;
    assert!(value["chore"]["id"].as_str().is_some(), "expected chore id: {value}");
    assert!(
        !stdout.contains("warning:"),
        "warning must not appear on stdout: {stdout}"
    );
    assert!(
        stderr.contains("warning:") && stderr.contains("checkleft"),
        "expected overlap warning on stderr, got: {stderr}"
    );

    // Near-miss: only one significant shared token → silent.
    let output = Command::new(boss_binary())
        .args([
            "--json",
            "--no-input",
            "--no-autostart",
            "--socket-path",
            engine.socket_str(),
            "chore",
            "create",
            "--product",
            &product.slug,
            "--name",
            "Fix checkleft lint rule",
        ])
        .output()?;
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    let _: serde_json::Value = serde_json::from_str(&stdout)?;
    assert!(
        !stderr.contains("warning:"),
        "near-miss must stay silent; stderr={stderr}"
    );

    Ok(())
}
