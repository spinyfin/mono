//! Local setup timings through production functions; never starts a worker or engine.
use std::{fs, path::PathBuf, time::Instant};

use boss_engine::{
    driver::{AgentDriver, ClaudeDriver, CodexDriver, GrokDriver, PermissionInput, SpawnRequest, WorkerKind},
    worker_setup::{WorkerSetupInput, write_workspace_files},
};
use boss_execution_runner::{WorkerSpawnOpts, compose_worker_spawn};
use boss_protocol::{ExecutionKind, ExecutionStatus, Task, TaskKind, TaskStatus, WorkExecution};
use boss_work_db::{WorkDb, WorkItem};

fn record(driver: &str, run: usize, step: &str, began: Instant) {
    println!("{driver}\t{run}\t{step}\t{}", began.elapsed().as_secs_f64());
}

fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::args().nth(1).expect("absolute fresh scratch directory"));
    anyhow::ensure!(
        root.is_absolute() && !root.exists(),
        "scratch must be absolute and fresh"
    );
    fs::create_dir_all(&root)?;
    // Before creating any threads; every writable runtime path is private.
    unsafe {
        for (key, leaf) in [
            ("HOME", "home"),
            ("TMPDIR", "tmp"),
            ("BOSS_GROK_HOMES_DIR", "grok"),
            ("BOSS_CODEX_HOMES_DIR", "codex"),
        ] {
            let path = root.join(leaf);
            fs::create_dir_all(&path)?;
            std::env::set_var(key, path);
        }
        std::env::remove_var("BOSS_EVENTS_SOCKET");
    }
    tokio::runtime::Runtime::new()?.block_on(measure(root))
}

async fn measure(root: PathBuf) -> anyhow::Result<()> {
    let db = WorkDb::open(root.join("probe.db"))?;
    let utility = boss_engine_utility_model::AnthropicUtilityModel::from_lookup(None, |_| None);
    for (name, model, driver) in [
        (
            "claude",
            "claude-opus-4-6",
            Box::new(ClaudeDriver) as Box<dyn AgentDriver>,
        ),
        ("codex", "gpt-6-astra", Box::new(CodexDriver::default())),
        ("grok", "grok-4.6", Box::new(GrokDriver::default())),
    ] {
        for run in 0..30 {
            let run_id = format!("spawn-probe-{name}-{run}");
            let workspace = root.join(&run_id);
            fs::create_dir_all(&workspace)?;
            // Provisioning is measured separately. No credentials or preflight are needed
            // for these local permission/config paths; give each a fresh empty home.
            fs::create_dir_all(boss_engine::driver::codex::codex_home_for_run(&run_id)?)?;
            fs::create_dir_all(boss_engine::driver::grok::grok_home_for_run(&run_id)?)?;
            let task = Task::builder()
                .id(&run_id)
                .product_id("probe-product")
                .kind(TaskKind::Chore)
                .name("Measure local spawn setup")
                .description("Exercise the local worker setup paths. ".repeat(1000))
                .status(TaskStatus::Todo)
                .created_at("2026-11-18T00:00:00Z")
                .updated_at("2026-11-18T00:00:00Z")
                .driver(name)
                .model_override(model)
                .build();
            let item = WorkItem::Chore(task);
            let execution = WorkExecution::builder()
                .id(&run_id)
                .work_item_id(&run_id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .repo_remote_url("https://github.com/spinyfin/mono")
                .workspace_path(workspace.display().to_string())
                .created_at("2026-11-18T00:00:00Z")
                .build();
            let began = Instant::now();
            let composed = compose_worker_spawn(
                &db,
                "probe-worker",
                &execution,
                &item,
                &workspace,
                None,
                WorkerSpawnOpts::builder()
                    .review_signal_checks(|_| &[])
                    .bazel_workspace(true)
                    .build(),
            )
            .await?;
            record(name, run, "composition", began);
            let setup = WorkerSetupInput::builder()
                .run_id(&run_id)
                .lease_id("probe-lease")
                .workspace_path(workspace.clone())
                .events_socket_path(root.join("events.sock"))
                .boss_event_path(root.join("boss-event"))
                .execution_kind("chore_implementation")
                .build();
            let began = Instant::now();
            let files = write_workspace_files(&setup, driver.as_ref())?;
            record(name, run, "workspace settings and hooks", began);
            let settings_dir = files.settings_path.parent().unwrap();
            let input = PermissionInput {
                worker_kind: WorkerKind::Standard,
                workspace_path: workspace.clone(),
                events_socket_path: root.join("events.sock"),
                frontend_socket_path: None,
                boss_event_path: root.join("boss-event"),
                run_id: run_id.clone(),
                lease_id: "probe-lease".into(),
                execution_kind: "chore_implementation".into(),
                task_kind: Some("chore".into()),
                is_remote: false,
                path_guard_script: Some(boss_engine::worker_setup::ensure_path_guard_script_in(settings_dir)?),
                checkleft_guard_script: Some(boss_engine::worker_setup::ensure_checkleft_push_guard_script_in(
                    settings_dir,
                )?),
                git_store_guard_script: Some(boss_engine::worker_setup::ensure_git_store_guard_script_in(
                    settings_dir,
                )?),
                codex_sandbox_enforced: false,
            };
            let began = Instant::now();
            driver.write_permission_config(&input, settings_dir).await?;
            record(name, run, "permission config", began);
            let began = Instant::now();
            let plan = driver.spawn_invocation(SpawnRequest {
                model,
                effort: composed.spawn_config.effort_value,
                settings_path: Some(&files.settings_path),
                non_opus_auto_mode: false,
                permission_mode_override: None,
                run_id: Some(&run_id),
            });
            anyhow::ensure!(!plan.command.is_empty(), "empty spawn command");
            record(name, run, "spawn invocation", began);
            let began = Instant::now();
            let missing =
                boss_pane_summary::title_for_spawn(&db, &utility, &execution.kind, None, &run_id, &item).await;
            anyhow::ensure!(missing.is_none(), "unexpected uncached title");
            record(name, run, "pane title (no credential)", began);
            // Exercise the real cache-hit title path, without an inference request.
            let basis = boss_pane_summary::compute_basis(
                "Measure local spawn setup",
                &"Exercise the local worker setup paths. ".repeat(1000),
            );
            db.set_pane_summary(&run_id, "measuring local spawn setup", &basis)?;
            let began = Instant::now();
            let title = boss_pane_summary::title_for_spawn(&db, &utility, &execution.kind, None, &run_id, &item).await;
            anyhow::ensure!(title.is_some(), "missing cached title");
            record(name, run, "pane title (cached)", began);
        }
    }
    Ok(())
}
