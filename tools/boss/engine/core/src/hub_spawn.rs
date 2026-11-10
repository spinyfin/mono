//! Compose a typed spoke `Spawn` payload for a remote worker.
//!
//! Uses the same [`crate::runner::compose_worker_spawn`] policy as local and
//! SSH dispatch, then maps the result onto the protocol-spoke message the
//! spoke supervisor launches from. The composer does not probe the hub's
//! local filesystem for a remote workspace: Bazel membership is an explicit
//! override, PR templates and manifest hints are explicitly empty, and
//! structured-output paths are `$BOSS_STRUCTURED_OUTPUT`. The
//! settings use an absolute spoke events socket supplied by the caller.
//! The supervisor honours `effort` and allocates private host output paths
//! from `structured_output_kind` and `pr_url_output` before launching.
//! `lease_id` and `workspace_id`
//! are supplied by the caller (the adapter that already leased the remote
//! workspace); they are not invented from hub-local cube state.

use std::path::Path;

use anyhow::{Context, Result, bail};
use boss_protocol::ExecutionKind;
use boss_protocol_spoke::v1::{Env, Spawn};
use boss_protocol_spoke::{Validate, ValidationError};

use boss_execution_runner::WorkerSpawnOpts;

use crate::driver::{DriverRegistry, EnvDirective};
use crate::runner::compose_worker_spawn;
use crate::work::{WorkDb, WorkExecution, WorkItem};
use crate::worker_setup::{WorkerSetupInput, render_remote_settings_json, worker_kind_for_execution};

/// Shim binary name resolved on the spoke PATH, matching the SSH remote
/// spawn's `boss-event` contract.
const SPOKE_BOSS_EVENT: &str = "boss-event";

use boss_host_execution::worker::HOST_ENV_DENY;

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
/// the remote path used in prompt text and is never probed on the hub filesystem. PR templates and manifest
/// hints are explicitly empty; the worker discovers repository guidance on its host.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct ComposeRemoteSpawn<'a> {
    pub db: &'a WorkDb,
    pub worker_id: &'a str,
    pub host_id: &'a str,
    pub execution: &'a WorkExecution,
    pub item: &'a WorkItem,
    pub workspace_path: &'a Path,
    /// Absolute socket path supplied by the spoke host configuration.
    pub events_socket_path: &'a Path,
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
        host_id,
        execution,
        item,
        workspace_path,
        events_socket_path,
        cube_change_id,
        lease_id,
        workspace_id,
        mut opts,
    } = request;
    if !events_socket_path.is_absolute() {
        bail!("spoke events socket must be absolute");
    }
    if opts.bazel_workspace.is_none() {
        bail!(
            "remote spawn requires an explicit bazel_workspace override; \
             the hub must not probe a remote workspace on the local filesystem"
        );
    }

    opts.remote_workspace = true;
    let composed =
        compose_worker_spawn(db, worker_id, execution, item, workspace_path, cube_change_id, opts.clone()).await?;
    let RemoteLaunch {
        prompt_text,
        driver,
        settings_json,
        ..
    } = prepare_remote_launch(
        db,
        execution,
        item,
        (&composed.spawn_config.driver, lease_id, host_id),
        (workspace_path, events_socket_path, Path::new(SPOKE_BOSS_EVENT)),
        opts,
        (composed.prompt_text, composed.embedded_output_path.as_deref()),
    )?;

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
        effort: composed.spawn_config.effort_value.map(str::to_owned),
        structured_output_kind: composed
            .embedded_output_path
            .as_ref()
            .and_then(|_| crate::runner::designated_output_kind(execution, item))
            .map(|kind| kind.slug().to_owned()),
        pr_url_output: matches!(
            execution.kind,
            ExecutionKind::TaskImplementation
                | ExecutionKind::ChoreImplementation
                | ExecutionKind::ProjectDesign
                | ExecutionKind::InvestigationImplementation
        ),
        ..Default::default()
    };
    spawn
        .validate()
        .map_err(|err: ValidationError| anyhow::anyhow!("spoke spawn: {err}"))?;
    Ok(spawn)
}

pub(crate) struct RemoteLaunch {
    pub prompt_text: String,
    pub settings_input: WorkerSetupInput,
    pub driver: std::sync::Arc<dyn crate::driver::AgentDriver>,
    pub settings_json: String,
}

/// Shared prompt relocation, remote eligibility and driver settings policy.
pub(crate) fn prepare_remote_launch(
    db: &WorkDb,
    execution: &WorkExecution,
    item: &WorkItem,
    identity: (&str, &str, &str),
    paths: (&Path, &Path, &Path),
    opts: WorkerSpawnOpts,
    prompt: (String, Option<&str>),
) -> Result<RemoteLaunch> {
    let (driver_slug, lease_id, host) = identity;
    let (workspace_path, events_socket_path, boss_event_path) = paths;
    let (prompt_text, embedded_output_path) = prompt;
    let mut prompt_text = prompt_text;
    if let Some(local_path) = embedded_output_path {
        if !prompt_text.contains(local_path) {
            bail!(
                "remote spawn prompt omitted structured-output path {local_path:?} for execution {}",
                execution.id
            );
        }
        prompt_text = prompt_text.replace(local_path, "$BOSS_STRUCTURED_OUTPUT");
    }

    let pr_url_path = crate::structured_output::default_path_string(
        &execution.id,
        crate::structured_output::StructuredOutputKind::PrUrl,
    );
    prompt_text = prompt_text.replace(&pr_url_path, "$BOSS_PR_URL_OUTPUT");

    let driver = DriverRegistry::default()
        .require(driver_slug)
        .map_err(|err| anyhow::anyhow!("remote spawn: {err}"))?;
    crate::host_adapter::reject_host_local_remote_spawn_plan(driver.as_ref(), driver_slug, host)?;

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
        .events_socket_path(events_socket_path.to_path_buf())
        .boss_event_path(boss_event_path.to_path_buf())
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
    crate::host_adapter::reject_unobservable_remote_driver(driver.as_ref(), driver_slug, &settings_input, host)?;
    let settings_json = render_remote_settings_json(&settings_input, driver.as_ref());

    Ok(RemoteLaunch {
        prompt_text,
        settings_input,
        driver,
        settings_json,
    })
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
            .effort_level(boss_protocol::EffortLevel::Small)
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
            host_id: "spoke-1",
            execution: &execution,
            item: &item,
            workspace_path: workspace.path(),
            events_socket_path: Path::new("/spoke/run/events.sock"),
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
        // Without the explicit override, this directory has no Bazel marker.
        let (db, mut execution, mut item) = chore_fixture(workspace.path());
        let product_id = workspace.path().display().to_string();
        if let WorkItem::Chore(task) = &mut item {
            task.product_id = product_id.clone();
        }
        execution.cube_lease_id = Some("probe-lease".into());
        std::fs::write(workspace.path().join("package.json"), "{}").unwrap();
        std::fs::create_dir_all(workspace.path().join(".github")).unwrap();
        std::fs::write(
            workspace.path().join(".github/PULL_REQUEST_TEMPLATE.md"),
            "hub-local-template-sentinel",
        )
        .unwrap();
        let spawn = compose_remote_spawn(ComposeRemoteSpawn {
            db: &db,
            worker_id: "worker-1",
            host_id: "spoke-1",
            execution: &execution,
            item: &item,
            workspace_path: workspace.path(),
            events_socket_path: Path::new("/spoke/run/events.sock"),
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
            host_id: "spoke-1",
            execution: &execution,
            item: &item,
            workspace_path: workspace.path(),
            events_socket_path: Path::new("/spoke/run/events.sock"),
            cube_change_id: None,
            lease_id: "lease-1",
            workspace_id: "ws-1",
            opts: opts(Some(false)),
        })
        .await
        .unwrap();
        let prompt = String::from_utf8(spawn.prompt_utf8).unwrap();
        assert!(!prompt.contains("hub-local-template-sentinel"));
        assert!(!prompt.contains("npm/Node"));
        assert!(
            prompt.contains("This repository is not a Bazel workspace"),
            "explicit bazel_workspace=false must ignore a local MODULE.bazel"
        );
        let empty = tempfile::tempdir().unwrap();
        assert!(
            crate::pr_template::load(&product_id, "probe-lease", empty.path()).is_empty(),
            "remote composition must not cache hub-local PR templates"
        );
    }

    #[tokio::test]
    async fn review_output_contract_only_exists_when_rendered() {
        let workspace = tempfile::tempdir().unwrap();
        let (db, mut execution, mut item) = chore_fixture(workspace.path());
        execution.kind = ExecutionKind::PrReview;
        for rendered in [false, true] {
            if let WorkItem::Chore(task) = &mut item {
                task.pr_url = rendered.then(|| "fixture-pr".into());
            }
            let spawn = compose_remote_spawn(
                ComposeRemoteSpawn::builder()
                    .db(&db)
                    .worker_id("worker")
                    .host_id("spoke-1")
                    .execution(&execution)
                    .item(&item)
                    .workspace_path(workspace.path())
                    .events_socket_path(Path::new("/spoke/run/events.sock"))
                    .lease_id("lease")
                    .workspace_id("workspace")
                    .opts(opts(Some(true)))
                    .build(),
            )
            .await
            .unwrap();
            assert_eq!(
                spawn.structured_output_kind.as_deref(),
                rendered.then_some("review-result")
            );
            assert!(!spawn.pr_url_output);
            assert_eq!(
                String::from_utf8(spawn.prompt_utf8)
                    .unwrap()
                    .contains("$BOSS_STRUCTURED_OUTPUT"),
                rendered
            );
        }
    }

    #[tokio::test]
    async fn compose_remote_spawn_payload_has_driver_prompt_settings_and_run_identity() {
        let workspace = tempfile::tempdir().unwrap();
        let (db, execution, item) = chore_fixture(workspace.path());
        let spawn = compose_remote_spawn(ComposeRemoteSpawn {
            db: &db,
            worker_id: "worker-1",
            host_id: "spoke-1",
            execution: &execution,
            item: &item,
            workspace_path: Path::new("/remote/ws"),
            events_socket_path: Path::new("/spoke/run/events.sock"),
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
        assert_eq!(spawn.effort.as_deref(), Some("medium"));
        assert_eq!(spawn.structured_output_kind.as_deref(), Some("followups"));
        assert!(spawn.pr_url_output);
        assert!(!spawn.model.is_empty());
        assert!(!spawn.prompt_utf8.is_empty());
        let settings = String::from_utf8(spawn.settings_json.clone()).unwrap();
        assert!(
            settings.contains("/spoke/run/events.sock"),
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
