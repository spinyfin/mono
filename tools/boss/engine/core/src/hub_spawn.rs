//! Compose a typed spoke `Spawn` payload for a remote worker.
//!
//! Uses the same [`crate::runner::compose_worker_spawn`] policy as local and
//! SSH dispatch, then maps the result onto the protocol-spoke message the
//! spoke supervisor launches from. The composer does not probe the hub's
//! local filesystem for a remote workspace: Bazel membership is an explicit
//! override, structured-output paths are `$BOSS_STRUCTURED_OUTPUT`, and
//! settings never embed the hub events socket. `lease_id` and `workspace_id`
//! are supplied by the caller (the adapter that already leased the remote
//! workspace); they are not invented from hub-local cube state.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use boss_protocol::ExecutionKind;
use boss_protocol_spoke::v1::{Env, Spawn};
use boss_protocol_spoke::{Validate, ValidationError};

use boss_execution_runner::WorkerSpawnOpts;

use crate::driver::{DriverRegistry, EnvDirective};
use crate::runner::compose_worker_spawn;
use crate::work::{WorkDb, WorkExecution, WorkItem};
use crate::worker_setup::{WorkerSetupInput, render_remote_settings_json, worker_kind_for_execution};

/// Relative events-socket name the spoke supervisor owns. Never an
/// engine-local absolute path: the hub cannot assume a remote workspace
/// layout, and the spoke writes `settings_json` as-is.
const SPOKE_EVENTS_SOCKET: &str = "spoke-events.sock";
/// Shim binary name resolved on the spoke PATH, matching the SSH remote
/// spawn's `boss-event` contract.
const SPOKE_BOSS_EVENT: &str = "boss-event";

/// Environment names the spoke supervisor strips (and which must never
/// carry hub identity onto a remote worker). Keep in lockstep with
/// `engine/host-execution` `HOST_ENV_DENY`.
const HOST_ENV_DENY: &[&str] = &[
    "BOSS_CODEX_HOMES_DIR",
    "BOSS_GROK_HOMES_DIR",
    "HOME",
    "TMUX",
    "TMUX_TMPDIR",
    "BOSS_CONTROL_SOCKET",
    "BOSS_EVENTS_SOCKET",
];

/// Provider and hub credential names that must never appear in `Spawn.env`.
const CREDENTIAL_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "XAI_API_KEY",
    "GH_TOKEN",
    "GITHUB_TOKEN",
];

/// Inputs for [`compose_remote_spawn`]. `lease_id` and `workspace_id` are
/// the cube identities the adapter already allocated; `workspace_path` is
/// the remote path used in prompt text and is never probed for Bazel
/// markers.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct ComposeRemoteSpawn<'a> {
    pub db: &'a WorkDb,
    pub worker_id: &'a str,
    pub execution: &'a WorkExecution,
    pub item: &'a WorkItem,
    pub workspace_path: &'a Path,
    pub cube_change_id: Option<&'a str>,
    pub lease_id: &'a str,
    pub workspace_id: &'a str,
    pub opts: WorkerSpawnOpts,
}

/// Compose a validated spoke [`Spawn`] from the shared worker-spawn policy.
pub async fn compose_remote_spawn(request: ComposeRemoteSpawn<'_>) -> Result<Spawn> {
    let ComposeRemoteSpawn {
        db,
        worker_id,
        execution,
        item,
        workspace_path,
        cube_change_id,
        lease_id,
        workspace_id,
        opts,
    } = request;
    if opts.bazel_workspace.is_none() {
        bail!(
            "remote spawn requires an explicit bazel_workspace override; \
             the hub must not probe a remote workspace on the local filesystem"
        );
    }

    let composed = compose_worker_spawn(db, worker_id, execution, item, workspace_path, cube_change_id, opts).await?;
    let mut prompt_text = composed.prompt_text;
    if let Some(local_path) = composed.embedded_output_path.as_deref() {
        if !prompt_text.contains(local_path) {
            bail!(
                "remote spawn prompt omitted structured-output path {local_path:?} for execution {}",
                execution.id
            );
        }
        prompt_text = prompt_text.replace(local_path, "$BOSS_STRUCTURED_OUTPUT");
    }

    let driver = DriverRegistry::default()
        .require(&composed.spawn_config.driver)
        .map_err(|err| anyhow::anyhow!("remote spawn: {err}"))?;
    if !driver.remote_spawn_host_independent() {
        bail!(
            "driver `{}` has a host-local spawn plan and is not remote-eligible",
            composed.spawn_config.driver
        );
    }

    let review_batch_member_role = if execution.kind == ExecutionKind::PrReview {
        db.review_batch_member_for_execution(&execution.id)
            .with_context(|| format!("review-batch role for execution {}", execution.id))?
            .map(|member| member.role)
    } else {
        None
    };
    let settings_input = WorkerSetupInput::builder()
        .run_id(execution.id.clone())
        .lease_id(lease_id)
        .workspace_path(workspace_path.to_path_buf())
        .events_socket_path(PathBuf::from(SPOKE_EVENTS_SOCKET))
        .boss_event_path(PathBuf::from(SPOKE_BOSS_EVENT))
        .execution_kind(execution.kind.as_str().to_owned())
        .maybe_task_kind(crate::runner::work_item_task_kind(item).map(str::to_owned))
        .worker_kind(worker_kind_for_execution(&execution.kind))
        .run_done_proposals_seam_enabled(opts.run_done_proposals_seam_enabled)
        .worker_signal_proposals_seam_enabled(opts.worker_signal_proposals_seam_enabled)
        .is_review_supervisor(review_batch_member_role == Some(boss_protocol::ReviewBatchMemberRole::Supervisor))
        .is_post_merge_reviewer(
            review_batch_member_role == Some(boss_protocol::ReviewBatchMemberRole::PostMergeReviewer),
        )
        .build();
    if !crate::worker_setup::driver_reports_progress_via_worker_settings(driver.as_ref(), &settings_input) {
        bail!(
            "driver `{}` reports progress as a byte stream rather than hooks in the worker settings file",
            composed.spawn_config.driver
        );
    }
    let settings_json = render_remote_settings_json(&settings_input, driver.as_ref());

    let plan = driver.spawn_invocation(crate::driver::SpawnRequest {
        model: &composed.spawn_config.model,
        effort: composed.spawn_config.effort_value,
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: None,
        run_id: Some(&execution.id),
    });
    let mut env = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for directive in &plan.env {
        if let EnvDirective::Set(name, value) = directive
            && env_allowed(name)
            && seen.insert(name.clone())
        {
            env.push(Env {
                name: name.clone(),
                value: value.clone(),
            });
        }
    }
    if seen.insert("BOSS_RUN_ID".to_owned()) {
        env.push(Env {
            name: "BOSS_RUN_ID".into(),
            value: execution.id.clone(),
        });
    }

    let spawn = Spawn {
        lease_id: lease_id.to_owned(),
        workspace_id: workspace_id.to_owned(),
        driver_id: composed.spawn_config.driver.clone(),
        model: composed.spawn_config.model.clone(),
        prompt_utf8: prompt_text.into_bytes(),
        settings_json: settings_json.into_bytes(),
        env,
        ..Default::default()
    };
    spawn
        .validate()
        .map_err(|err: ValidationError| anyhow::anyhow!("spoke spawn: {err}"))?;
    Ok(spawn)
}

fn env_allowed(name: &str) -> bool {
    if HOST_ENV_DENY.contains(&name) || CREDENTIAL_ENV.contains(&name) {
        return false;
    }
    if name.starts_with("BOSS_HUB_") {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::Task;
    use boss_protocol::{ExecutionStatus, TaskKind, TaskStatus};
    use std::path::PathBuf;

    fn chore_fixture(workspace: &Path) -> (WorkDb, WorkExecution, WorkItem) {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let execution = WorkExecution::builder()
            .id("exec_remote_spawn_01")
            .work_item_id("task-remote-spawn")
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path(workspace.to_string_lossy())
            .created_at("2026-05-15T00:00:00Z")
            .build();
        let task = Task::builder()
            .id("task-remote-spawn")
            .product_id("prod-remote")
            .kind(TaskKind::Chore)
            .name("Widget import")
            .description("Implement widget import.")
            .status(TaskStatus::Todo)
            .created_at("2026-05-15T00:00:00Z")
            .updated_at("2026-05-15T00:00:00Z")
            .driver("claude")
            .build();
        (db, execution, WorkItem::Chore(task))
    }

    fn opts(bazel_workspace: Option<bool>) -> WorkerSpawnOpts {
        WorkerSpawnOpts::builder()
            .review_signal_checks(|_| &[])
            .maybe_bazel_workspace(bazel_workspace)
            .build()
    }

    #[tokio::test]
    async fn compose_remote_spawn_requires_bazel_override() {
        let workspace = tempfile::tempdir().unwrap();
        let (db, execution, item) = chore_fixture(workspace.path());
        let err = compose_remote_spawn(ComposeRemoteSpawn {
            db: &db,
            worker_id: "worker-1",
            execution: &execution,
            item: &item,
            workspace_path: workspace.path(),
            cube_change_id: None,
            lease_id: "lease-1",
            workspace_id: "ws-1",
            opts: opts(None),
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("bazel_workspace"), "error was: {err:#}");
    }

    #[tokio::test]
    async fn compose_remote_spawn_does_not_probe_local_workspace_markers() {
        let workspace = tempfile::tempdir().unwrap();
        // Existing empty directory would probe as non-Bazel if the override
        // were ignored.
        let (db, execution, item) = chore_fixture(workspace.path());
        let spawn = compose_remote_spawn(ComposeRemoteSpawn {
            db: &db,
            worker_id: "worker-1",
            execution: &execution,
            item: &item,
            workspace_path: workspace.path(),
            cube_change_id: None,
            lease_id: "lease-1",
            workspace_id: "ws-1",
            opts: opts(Some(true)),
        })
        .await
        .unwrap();
        let prompt = String::from_utf8(spawn.prompt_utf8.clone()).unwrap();
        assert!(
            prompt.contains("This repository is a Bazel workspace"),
            "explicit bazel_workspace=true must not be overwritten by a local empty dir"
        );
        assert!(
            prompt.contains("$BOSS_STRUCTURED_OUTPUT"),
            "structured output must be the spoke-side env var, not a hub-local path"
        );

        std::fs::write(workspace.path().join("MODULE.bazel"), "").unwrap();
        let spawn = compose_remote_spawn(ComposeRemoteSpawn {
            db: &db,
            worker_id: "worker-1",
            execution: &execution,
            item: &item,
            workspace_path: workspace.path(),
            cube_change_id: None,
            lease_id: "lease-1",
            workspace_id: "ws-1",
            opts: opts(Some(false)),
        })
        .await
        .unwrap();
        let prompt = String::from_utf8(spawn.prompt_utf8).unwrap();
        assert!(
            prompt.contains("This repository is not a Bazel workspace"),
            "explicit bazel_workspace=false must ignore a local MODULE.bazel"
        );
    }

    #[tokio::test]
    async fn compose_remote_spawn_payload_has_driver_prompt_settings_and_run_identity() {
        let workspace = tempfile::tempdir().unwrap();
        let (db, execution, item) = chore_fixture(workspace.path());
        let spawn = compose_remote_spawn(ComposeRemoteSpawn {
            db: &db,
            worker_id: "worker-1",
            execution: &execution,
            item: &item,
            workspace_path: Path::new("/remote/ws"),
            cube_change_id: None,
            lease_id: "lease-1",
            workspace_id: "ws-1",
            opts: opts(Some(true)),
        })
        .await
        .unwrap();
        spawn.validate().expect("composed spawn must validate");
        assert_eq!(spawn.lease_id, "lease-1");
        assert_eq!(spawn.workspace_id, "ws-1");
        assert_eq!(spawn.driver_id, "claude");
        assert!(!spawn.model.is_empty());
        assert!(!spawn.prompt_utf8.is_empty());
        let settings = String::from_utf8(spawn.settings_json.clone()).unwrap();
        assert!(
            settings.contains(SPOKE_EVENTS_SOCKET),
            "settings should name the spoke-owned socket"
        );
        assert!(
            !settings.contains("boss-path-guard.py") && !settings.contains("BOSS_DATA_DIR="),
            "remote settings must not install the hub data-dir path-guard: {settings}"
        );
        assert!(
            spawn
                .env
                .iter()
                .any(|env| env.name == "BOSS_RUN_ID" && env.value == execution.id),
            "run identity missing from env: {:?}",
            spawn.env
        );
        assert!(
            spawn
                .env
                .iter()
                .all(|env| env.name != "ANTHROPIC_API_KEY" && env.name != "BOSS_EVENTS_SOCKET"),
            "credential or hub events env leaked: {:?}",
            spawn.env
        );
    }
}
