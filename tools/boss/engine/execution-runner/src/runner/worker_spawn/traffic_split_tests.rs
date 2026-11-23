use super::*;
use boss_protocol::{CreateChoreInput, CreateExecutionInput, DriverTrafficSplit, ExecutionStatus, ReasoningMode};

#[tokio::test]
async fn launch_rechecks_shares_changed_after_claim_for_implementations_revisions_and_reviews() {
    let workspace = tempfile::tempdir().unwrap();
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    let product = crate::test_support::create_test_product(&db);
    for kind in [
        ExecutionKind::TaskImplementation,
        ExecutionKind::ChoreImplementation,
        ExecutionKind::RevisionImplementation,
        ExecutionKind::PrReview,
    ] {
        for old_split in [
            DriverTrafficSplit::new(100, 0, 0, 0),
            DriverTrafficSplit::new(0, 100, 0, 0),
            DriverTrafficSplit::new(0, 0, 100, 0),
        ] {
            db.set_driver_traffic_split(old_split).unwrap();
            let chore = db
                .create_chore(
                    CreateChoreInput::builder()
                        .product_id(&product.id)
                        .name(format!("queued large worker {kind} {old_split:?}"))
                        .effort_level(EffortLevel::Large)
                        .reasoning(ReasoningMode::Standard)
                        .autostart(false)
                        .build(),
                )
                .unwrap();
            let execution = db
                .create_execution(
                    CreateExecutionInput::builder()
                        .created_via("test")
                        .work_item_id(&chore.id)
                        .kind(kind.clone())
                        .status(ExecutionStatus::Ready)
                        .build(),
                )
                .unwrap();
            db.claim_execution_for_dispatch(&execution.id).unwrap();
            // The selected driver's share becomes zero after the claim but
            // before launch composition, as can happen while cube prepares a
            // workspace.
            let (split, expected) = if old_split.codex == 100 {
                (DriverTrafficSplit::new(0, 100, 0, 0), "claude")
            } else {
                (DriverTrafficSplit::new(0, 0, 100, 0), "codex")
            };
            db.set_driver_traffic_split(split).unwrap();
            let worker = if kind == ExecutionKind::PrReview {
                "review-1"
            } else {
                "worker-1"
            };
            let item = db.get_work_item(&chore.id).unwrap();
            let composed = compose_worker_spawn(
                &db,
                worker,
                &execution,
                &item,
                workspace.path(),
                None,
                WorkerSpawnOpts::builder().review_signal_checks(|_| &[]).build(),
            )
            .await
            .unwrap();
            if kind == ExecutionKind::PrReview {
                assert_eq!(composed.spawn_config.driver, "claude");
                assert_eq!(composed.spawn_config.driver_source.as_str(), "pool_policy");
            } else {
                assert_eq!(composed.spawn_config.driver, expected);
                assert_eq!(composed.spawn_config.driver_source.as_str(), "traffic_allocation");
                assert_eq!(
                    db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
                    Some(expected)
                );
            }
        }
    }
}

#[tokio::test]
async fn launch_surfaces_allocation_errors_instead_of_using_engine_default() {
    let workspace = tempfile::tempdir().unwrap();
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    let product = crate::test_support::create_test_product(&db);
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(&product.id)
                .name("no launchable share")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(&chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100, 0))
        .unwrap();
    db.update_work_item(
        &chore.id,
        crate::work::WorkItemPatch {
            model_override: Some("opus".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let item = db.get_work_item(&chore.id).unwrap();
    let result = compose_worker_spawn(
        &db,
        "worker-1",
        &execution,
        &item,
        workspace.path(),
        None,
        WorkerSpawnOpts::builder().review_signal_checks(|_| &[]).build(),
    )
    .await;
    let err = result
        .err()
        .expect("launch must fail when no eligible driver has a share");
    assert!(err.to_string().contains("0 share"), "{err}");
    assert!(err.to_string().contains("claude excluded: 0% share"), "{err}");
}

/// A product default that lacks the design/investigation tier model yields to
/// the recorded allocation, so the spawned driver, the spawn-config source,
/// the saved decision and the hook lookup all name the same driver, and a
/// driver at 0% share is never launched.
#[tokio::test]
async fn tier_row_with_grok_product_default_spawns_on_the_allocated_driver() {
    for design in [true, false] {
        let workspace = tempfile::tempdir().unwrap();
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = crate::test_support::create_test_product(&db);
        db.update_product(
            &product.id,
            crate::work::WorkItemPatch::builder().default_driver("grok").build(),
            "human",
        )
        .unwrap();
        db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100, 0))
            .unwrap();
        let reasoning = if design {
            ReasoningMode::Standard
        } else {
            ReasoningMode::Investigation
        };
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(&product.id)
                    .name("tier row under a grok product default")
                    .autostart(false)
                    .reasoning(reasoning)
                    .build(),
            )
            .unwrap();
        if design {
            db.connect()
                .unwrap()
                .execute("UPDATE tasks SET kind = 'design' WHERE id = ?1", [&chore.id])
                .unwrap();
        }
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .created_via("test")
                    .work_item_id(&chore.id)
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let item = db.get_work_item(&chore.id).unwrap();
        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &item,
            workspace.path(),
            None,
            WorkerSpawnOpts::builder().review_signal_checks(|_| &[]).build(),
        )
        .await
        .unwrap();
        let saved = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
        assert_eq!(composed.spawn_config.driver, "codex", "design={design}");
        assert_eq!(composed.spawn_config.driver_source.as_str(), "traffic_allocation");
        assert_eq!(saved.driver.as_deref(), Some("codex"));
        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
            Some("codex")
        );
    }
}
