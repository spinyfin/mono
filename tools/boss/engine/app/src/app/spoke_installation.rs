//! Install dispatch on the shared authenticated hub listener.
//!
//! The published `HubStream` handle is the stream manager, not the accept
//! socket. It outlives a listener failure so in-flight spoke sessions keep
//! their command path; a new listener is bound on the next engine start.
use anyhow::{Context, Result};
use async_trait::async_trait;
use boss_engine_runtime::hub::{BoundHub, HubConfig, HubStream};
use boss_execution_contracts::{HostAdapterProvider, RunOutcome, RunWaitState};
use boss_spoke_adapter::{ComposedSpawn, SpawnComposer, SpawnInput, SpokeAdapterConfig, SpokeHostAdapterProvider};
use boss_work_db::WorkDb;
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

#[derive(Default, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    hosts: BTreeMap<String, HostPaths>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostPaths {
    events_socket: PathBuf,
    bazel_workspace: bool,
}

struct Composer {
    db: WorkDb,
    hosts: BTreeMap<String, HostPaths>,
}

#[async_trait]
impl SpawnComposer for Composer {
    async fn compose(&self, input: SpawnInput<'_>) -> Result<ComposedSpawn> {
        let paths = self
            .hosts
            .get(&input.assignment.host_id)
            .with_context(|| format!("host {} spawn paths are not configured", input.assignment.host_id))?;
        let spawn = boss_engine_runtime::hub_spawn::compose_remote_spawn(
            boss_engine_runtime::hub_spawn::ComposeRemoteSpawn::builder()
                .db(&self.db)
                .worker_id(input.worker_id)
                .host_id(&input.assignment.host_id)
                .execution(input.execution)
                .item(input.work_item)
                .workspace_path(input.workspace)
                .events_socket_path(&paths.events_socket)
                .maybe_cube_change_id(input.cube_change_id)
                .lease_id(input.assignment.lease_id.as_deref().context("missing spoke lease")?)
                .workspace_id(
                    input
                        .assignment
                        .workspace_id
                        .as_deref()
                        .context("missing spoke workspace")?,
                )
                .opts(
                    boss_execution_runner::WorkerSpawnOpts::builder()
                        .review_signal_checks(boss_engine_runtime::merge_poller::review_signal_checks_for_owner)
                        .bazel_workspace(paths.bazel_workspace)
                        .build(),
                )
                .build(),
        )
        .await?;
        Ok(ComposedSpawn {
            spawn,
            outcome: RunOutcome {
                wait_state: RunWaitState::WorkerPaneAlive,
                result_summary: None,
                attention: None,
                slot_id: None,
                spawn_config: None,
            },
        })
    }
}

pub(super) struct Prepared {
    db: WorkDb,
    bound: BoundHub,
    hosts: BTreeMap<String, HostPaths>,
}

pub(super) fn prepare(db: WorkDb, config: &HubConfig) -> Result<Option<Prepared>> {
    let Some(bound) = boss_engine_runtime::hub::bind_hub_listener_sync(config, db.clone())? else {
        for host in db.list_hosts()? {
            anyhow::ensure!(
                !db.host_has_spoke_history(&host.id)?
                    && db.host_transport(&host.id)? != boss_work_db::HostTransport::SpokeReady,
                "hub listener configuration is required for recorded spoke host {}",
                host.id
            );
        }
        return Ok(None);
    };
    let hosts = load_spoke_host_paths(config)?;
    ensure_spoke_ready_hosts_have_paths(&db, &hosts, config.spoke_hub_config.as_deref())?;
    Ok(Some(Prepared { db, bound, hosts }))
}

fn load_spoke_host_paths(config: &HubConfig) -> Result<BTreeMap<String, HostPaths>> {
    let Some(path) = config.spoke_hub_config.as_ref() else {
        return Ok(BTreeMap::new());
    };
    let bytes =
        std::fs::read(path).with_context(|| format!("reading spoke host launch paths from {}", path.display()))?;
    let parsed: Configuration = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing spoke host launch paths from {}", path.display()))?;
    Ok(parsed.hosts)
}

fn ensure_spoke_ready_hosts_have_paths(
    db: &WorkDb,
    hosts: &BTreeMap<String, HostPaths>,
    source: Option<&std::path::Path>,
) -> Result<()> {
    let source = source.map_or(boss_engine_runtime::hub::SPOKE_HUB_CONFIG_ENV.to_owned(), |path| {
        path.display().to_string()
    });
    for host in db.list_hosts()? {
        if db.host_transport(&host.id)? == boss_work_db::HostTransport::SpokeReady {
            anyhow::ensure!(
                hosts.contains_key(&host.id),
                "spoke-ready host {} has no launch paths in {source}",
                host.id
            );
        }
    }
    Ok(())
}

impl Prepared {
    pub(super) fn stream(&self) -> HubStream {
        self.bound.stream.clone()
    }

    pub(super) fn install(
        self,
        inner: Arc<dyn HostAdapterProvider>,
        signals: Arc<dyn boss_execution_contracts::DriverSignals>,
    ) -> Arc<dyn HostAdapterProvider> {
        let Self { db, bound, hosts } = self;
        let custody_db = db.clone();
        let provider = Arc::new(SpokeHostAdapterProvider::new(
            inner,
            bound.stream.manager().clone(),
            Arc::new(db.clone()),
            Arc::new(Composer { db, hosts }),
            SpokeAdapterConfig::builder().build(),
            signals,
        ));
        // The same custody directory the projector writes, so transcript,
        // worker-log and structured-output reads of a spoke run find its bytes.
        if let Ok(root) = boss_engine_driver::transcript_store::transcript_store_root() {
            provider.install_custody(Arc::new(boss_engine_runtime::hub::projection::Custody::new(
                custody_db, root,
            )));
        }
        boss_task_scope::spawn(async move {
            if let Err(error) = bound.serve().await {
                tracing::error!(error = %error, "spoke listener stopped");
            }
        });
        provider
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_configuration_is_inert_only_without_spoke_ownership_history() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        assert!(prepare(db.clone(), &HubConfig::default()).unwrap().is_none());
        db.add_host("spoke", "worker@spoke", 1, &[]).unwrap();
        db.set_host_transport("spoke", boss_work_db::HostTransport::SpokeReady)
            .unwrap();
        db.reserve_spoke_assignment("execution", "spoke").unwrap();
        let error = prepare(db, &HubConfig::default())
            .err()
            .expect("ownership must fail closed");
        assert!(error.to_string().contains("hub listener configuration is required"));
    }

    #[test]
    fn missing_launch_path_file_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("hosts.json");
        let err = load_spoke_host_paths(&HubConfig {
            spoke_hub_config: Some(missing.clone()),
            ..HubConfig::default()
        })
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains(&missing.display().to_string()), "{text}");
    }

    #[test]
    fn spoke_ready_host_without_paths_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        db.add_host("spoke", "worker@spoke", 1, &[]).unwrap();
        db.set_host_transport("spoke", boss_work_db::HostTransport::SpokeReady)
            .unwrap();
        let err = ensure_spoke_ready_hosts_have_paths(&db, &BTreeMap::new(), None).unwrap_err();
        assert!(err.to_string().contains("spoke-ready host spoke"), "{err:#}");
    }

    #[tokio::test]
    async fn production_composer_composes_a_spawn_when_host_paths_are_set() {
        let workspace = tempfile::tempdir().unwrap();
        let db = WorkDb::open(workspace.path().join("work.db")).unwrap();
        let execution = boss_protocol::WorkExecution::builder()
            .id("exec_compose")
            .work_item_id("task-compose")
            .kind(boss_protocol::ExecutionKind::ChoreImplementation)
            .status(boss_protocol::ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path(workspace.path().to_string_lossy())
            .created_at("2026-05-15T00:00:00Z")
            .build();
        let item = boss_protocol::WorkItem::Chore(
            boss_protocol::Task::builder()
                .id("task-compose")
                .product_id("prod")
                .kind(boss_protocol::TaskKind::Chore)
                .name("Compose")
                .description("Compose a remote spawn.")
                .status(boss_protocol::TaskStatus::Todo)
                .created_at("2026-05-15T00:00:00Z")
                .updated_at("2026-05-15T00:00:00Z")
                .driver("claude")
                .effort_level(boss_protocol::EffortLevel::Small)
                .build(),
        );
        let assignment = boss_work_db::SpokeAssignment {
            execution_id: execution.id.clone(),
            assignment_generation: 1,
            run_id: "exec_compose-g1".into(),
            spawn_token: "token".into(),
            host_id: "spoke".into(),
            repo_id: Some("repo".into()),
            workspace_id: Some("ws".into()),
            lease_id: Some("lease".into()),
            display_path: Some("/remote/ws".into()),
            pane_pid: None,
            resolved: false,
        };
        let missing = Composer {
            db: db.clone(),
            hosts: BTreeMap::new(),
        }
        .compose(
            SpawnInput::builder()
                .worker_id("worker")
                .execution(&execution)
                .work_item(&item)
                .assignment(&assignment)
                .workspace(workspace.path())
                .build(),
        )
        .await
        .unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("host spoke spawn paths are not configured"),
            "{missing:#}"
        );
        let mut hosts = BTreeMap::new();
        hosts.insert(
            "spoke".into(),
            HostPaths {
                events_socket: std::path::PathBuf::from("/spoke/events.sock"),
                bazel_workspace: true,
            },
        );
        let composed = Composer { db, hosts }
            .compose(
                SpawnInput::builder()
                    .worker_id("worker")
                    .execution(&execution)
                    .work_item(&item)
                    .assignment(&assignment)
                    .workspace(workspace.path())
                    .build(),
            )
            .await
            .expect("production composer should compose when host paths are present");
        assert!(!composed.spawn.prompt_utf8.is_empty());
        assert_eq!(composed.outcome.wait_state, RunWaitState::WorkerPaneAlive);
    }
}
