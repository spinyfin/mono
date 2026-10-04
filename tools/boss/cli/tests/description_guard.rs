//! Description safety through the real CLI, RPC, and database write paths.

use anyhow::Result;
use boss_client::BossClient;
use boss_protocol::{CreateChoreInput, WorkItem, WorkItemPatch};
use common::{run_boss, run_boss_expect_failure};
use harness::{TestEngine, create_chore_with, create_product, create_project, create_task};

fn description(item: WorkItem) -> String {
    match item {
        WorkItem::Product(product) => product.description,
        WorkItem::Project(project) => project.description,
        WorkItem::Task(task) | WorkItem::Chore(task) => task.description,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn description_updates_reject_data_loss_and_allow_explicit_clearing() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Description guards").await?;
    let project = create_project(&mut client, &product.id, "Guard project").await?;
    let task = create_task(&mut client, &product.id, &project.id, "Guard task").await?;
    let chore = create_chore_with(
        &mut client,
        CreateChoreInput::builder()
            .product_id(&product.id)
            .name("Empty create remains allowed")
            .description("")
            .autostart(false)
            .build(),
    )
    .await?;
    assert!(chore.description.is_empty());
    for (index, value) in ["null", "undefined", "None", " "].iter().enumerate() {
        let created = create_chore_with(
            &mut client,
            CreateChoreInput::builder()
                .product_id(&product.id)
                .name(format!("Legacy create {index}"))
                .description(*value)
                .autostart(false)
                .build(),
        )
        .await?;
        assert_eq!(created.description, *value);
    }
    let db = engine.db()?;
    let original = "é".repeat(250); // 500 bytes, 250 characters.

    for id in [&product.id, &project.id, &task.id, &chore.id] {
        db.update_work_item(id, WorkItemPatch::builder().description(&original).build())?;
        let before = serde_json::to_value(db.get_work_item(id)?)?;
        for invalid in ["", " \t\n", "null", " NuLl ", "UNDEFINED", "None", "short"] {
            let error = db
                .update_work_item(
                    id,
                    WorkItemPatch::builder()
                        .description(invalid)
                        .name("must not persist")
                        .build(),
                )
                .unwrap_err();
            assert!(error.to_string().contains("description"));
            assert_eq!(serde_json::to_value(db.get_work_item(id)?)?, before);
        }
    }

    for (command, id) in [
        ("product", &product.id),
        ("project", &project.id),
        ("task", &task.id),
        ("chore", &chore.id),
    ] {
        for invalid in ["null", " \n", "short"] {
            let stderr =
                run_boss_expect_failure(engine.socket_str(), &[command, "update", id, "--description", invalid])?;
            assert!(stderr.contains("description"), "{stderr}");
            if invalid == "short" {
                assert!(stderr.contains("500 bytes") && stderr.contains("5 bytes"), "{stderr}");
            }
            assert_eq!(description(db.get_work_item(id)?), original);
        }
        let normal = "A revised brief. ".repeat(40);
        run_boss(engine.socket_str(), &[command, "update", id, "--description", &normal])?;
        assert_eq!(description(db.get_work_item(id)?), normal);
        for replacement in ["short", ""] {
            run_boss(
                engine.socket_str(),
                &[command, "update", id, "--description", replacement, "--force-shrink"],
            )?;
            assert_eq!(description(db.get_work_item(id)?), replacement);
        }
        let stderr = run_boss_expect_failure(
            engine.socket_str(),
            &[command, "update", id, "--description", "null", "--force-shrink"],
        )?;
        assert!(stderr.contains("placeholder"), "{stderr}");
    }
    Ok(())
}

#[tokio::test]
async fn reconciler_cannot_bypass_description_guard() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let product = create_product(&mut client, "Sync descriptions").await?;
    let original = "x".repeat(1000);
    let chore = create_chore_with(
        &mut client,
        CreateChoreInput::builder()
            .product_id(product.id)
            .name("Synced chore")
            .description(&original)
            .autostart(false)
            .build(),
    )
    .await?;
    let db = engine.db()?;
    for invalid in ["null", " ", "short"] {
        assert!(
            db.reconciler_update_name_and_description(&chore.id, "new name", invalid, "upstream", invalid)
                .is_err()
        );
        assert_eq!(description(db.get_work_item(&chore.id)?), original);
    }
    assert!(db.reconciler_update_name_and_description(&chore.id, "new name", &original, "upstream", &original)?);
    Ok(())
}
