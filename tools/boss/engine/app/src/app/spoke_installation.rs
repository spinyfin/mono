//! Install dispatch on the shared authenticated hub listener.
use anyhow::{Context, Result};
use async_trait::async_trait;
use boss_engine_runtime::hub::{BoundHub, HubConfig, HubStream};
use boss_execution_contracts::{HostAdapterProvider, RunOutcome, RunWaitState};
use boss_spoke_adapter::{ComposedSpawn, SpawnComposer, SpawnInput, SpokeAdapterConfig, SpokeHostAdapterProvider};
use boss_work_db::WorkDb;
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    hosts: BTreeMap<String, HostPaths>,
}

#[derive(Deserialize)]
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
            .context("host spawn paths are not configured")?;
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
    let config: Configuration = match std::env::var_os("BOSS_SPOKE_HUB_CONFIG") {
        Some(path) => serde_json::from_slice(&std::fs::read(path)?)?,
        None => Configuration::default(),
    };
    Ok(Some(Prepared {
        db,
        bound,
        hosts: config.hosts,
    }))
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
        let provider = Arc::new(SpokeHostAdapterProvider::new(
            inner,
            bound.stream.manager().clone(),
            Arc::new(db.clone()),
            Arc::new(Composer { db, hosts }),
            SpokeAdapterConfig::builder().build(),
            signals,
        ));
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
}
