//! Controlled first-prompt measurements through the production spawn composer.

use super::*;
use crate::work::Task;
use crate::worker_setup::{WorkerSetupInput, render_claude_md, worker_kind_for_execution};
use boss_protocol::{ExecutionStatus, TaskStatus};

#[tokio::test]
async fn first_prompt_sizes_by_driver_and_kind() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("MODULE.bazel"), "").unwrap();
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    for driver_name in ["claude", "codex"] {
        for kind in [
            ExecutionKind::ChoreImplementation,
            ExecutionKind::RevisionImplementation,
            ExecutionKind::PrReview,
        ] {
            let execution = WorkExecution::builder()
                .id("exec_measure_01")
                .work_item_id("task-measure")
                .kind(kind.clone())
                .status(ExecutionStatus::Running)
                .repo_remote_url("git@github.com:org/repo.git")
                .workspace_path(workspace.path().to_string_lossy())
                .created_at("2026-05-15T00:00:00Z")
                .build();
            let task = Task::builder()
                .id("task-measure")
                .product_id("prod-measure")
                .kind(TaskKind::Chore)
                .name("Widget import")
                .description("Implement widget import. Preserve existing imports and test malformed input.")
                .status(TaskStatus::Todo)
                .created_at("2026-05-15T00:00:00Z")
                .updated_at("2026-05-15T00:00:00Z")
                .driver(driver_name)
                .effort_level(EffortLevel::Large)
                .maybe_pr_url(
                    (kind != ExecutionKind::ChoreImplementation)
                        .then(|| "https://github.com/org/repo/pull/42".to_owned()),
                )
                .build();
            let composed = compose_worker_spawn(
                &db,
                "worker-measure",
                &execution,
                &WorkItem::Chore(task),
                workspace.path(),
                None,
                WorkerSpawnOpts::builder().run_done_proposals_seam_enabled(true).build(),
            )
            .await
            .unwrap();
            assert_eq!(composed.spawn_config.driver, driver_name);
            let registry = crate::driver::DriverRegistry::default();
            let driver = registry.require(driver_name).unwrap();
            let input = WorkerSetupInput::builder()
                .run_id(execution.id.clone())
                .lease_id("lease-measure")
                .workspace_path(std::path::PathBuf::from("/tmp/prompt-measure"))
                .events_socket_path(std::path::PathBuf::from("/tmp/events.sock"))
                .boss_event_path(std::path::PathBuf::from("/tmp/boss-event"))
                .execution_kind(kind.as_str())
                .worker_kind(worker_kind_for_execution(&kind))
                .run_done_proposals_seam_enabled(true)
                .build();
            let rules = render_claude_md(&input, driver.agent_rules_preamble(), driver.descriptor().config_dir);
            let prompt = composed
                .prompt_text
                .replace(workspace.path().to_str().unwrap(), "/tmp/prompt-measure");
            let prompt = match composed.embedded_output_path {
                Some(path) => prompt.replace(&path, "/tmp/prompt-measure/output.json"),
                None => prompt,
            };
            assert!(prompt.contains("Execution recovery bookmark"));
            if kind == ExecutionKind::PrReview {
                assert!(prompt.starts_with("# PR review"));
            }
            println!(
                "PROMPT_BYTES {} {driver_name} {} RULES_BYTES {}",
                kind.as_str(),
                prompt.len(),
                rules.len()
            );
        }
    }
}
