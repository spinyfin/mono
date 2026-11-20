//! Exercise clap, RPC, database filtering and both output formats together.
use anyhow::Result;
use boss_client::BossClient;
use boss_protocol::{
    CreateChoreInput, CreateExecutionInput, EffortLevel, ExecutionKind, ExecutionStatus, ReasoningMode,
};
use common::{run_boss, run_boss_expect_failure, run_boss_human};
use harness::{TestEngine, create_chore_with, create_product};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_query_lists_and_groups_across_products() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let db = engine.db()?;
    let mut ids = Vec::new();
    for name in ["Query alpha", "Query beta"] {
        let product = create_product(&mut client, name).await?;
        let chore = create_chore_with(
            &mut client,
            CreateChoreInput::builder()
                .product_id(&product.id)
                .name("Inspect history")
                .autostart(false)
                .reasoning(ReasoningMode::Investigation)
                .build(),
        )
        .await?;
        let execution = db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Completed)
                .build(),
        )?;
        db.record_execution_launch_config(&execution.id, "codex", "model-x", Some(EffortLevel::Medium))?;
        ids.push((execution.id, product.slug, format!("T{}", chore.short_id.unwrap())));
    }
    let rows = run_boss(
        engine.socket_str(),
        &[
            "execution",
            "list",
            "--since",
            "1d",
            "--model",
            "model-x",
            "--driver",
            "codex",
            "--effort",
            "medium",
            "--status",
            "ready,completed",
            "--kind",
            "chore_implementation",
        ],
    )?;
    assert_eq!(rows["rows"].as_array().unwrap().len(), 2, "{rows}");
    for (id, slug, label) in &ids {
        let row = rows["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == *id)
            .unwrap();
        assert_eq!(row["product_slug"], *slug);
        assert_eq!(row["work_item_short_id"], *label);
    }
    let grouped = run_boss(
        engine.socket_str(),
        &[
            "execution",
            "list",
            "--group-by",
            "status,model",
            "--group-by",
            "reasoning",
        ],
    )?;
    assert_eq!(grouped["group_by"], serde_json::json!(["status", "model", "reasoning"]));
    assert_eq!(grouped["groups"][0]["count"], 2);
    assert_eq!(grouped["groups"][0]["keys"]["reasoning"], "investigation");
    let scoped = run_boss(engine.socket_str(), &["execution", "list", "--product", &ids[0].1])?;
    assert_eq!(scoped["rows"].as_array().unwrap().len(), 1);
    let human = run_boss_human(engine.socket_str(), &["execution", "list", "--product", &ids[0].1])?;
    assert!(human.contains(&ids[0].2), "{human}");
    assert!(human.contains(&ids[0].1), "{human}");
    let human = run_boss_human(
        engine.socket_str(),
        &["execution", "list", "--group-by", "driver,reasoning"],
    )?;
    assert!(
        human.contains("codex") && human.contains("investigation") && human.contains("count"),
        "{human}"
    );
    let empty = run_boss(
        engine.socket_str(),
        &["execution", "list", "--error-match", "never recorded"],
    )?;
    assert_eq!(empty["rows"], serde_json::json!([]));
    for args in [
        vec!["execution", "list", "--group-by", "invalid"],
        vec!["execution", "list", "--since", "invalid"],
    ] {
        assert!(!run_boss_expect_failure(engine.socket_str(), &args)?.is_empty());
    }
    Ok(())
}
