//! Optional authenticated spoke listener and dispatch provider composition.
use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use async_trait::async_trait;
use boss_execution_contracts::{HostAdapterProvider, RunOutcome, RunWaitState};
use boss_host_enrollment::{HostCredentials, Policy, SystemClock};
use boss_spoke_adapter::{ComposedSpawn, SpawnComposer, SpawnInput, SpokeAdapterConfig, SpokeHostAdapterProvider};
use boss_spoke_hub::HubStream;
use boss_work_db::WorkDb;
use serde::Deserialize;

/// Operator-supplied listener identity and host-side paths. Loading this does
/// not enroll or mark any host ready: both gates still apply independently.
#[derive(Deserialize, bon::Builder)]
#[builder(on(String, into))]
#[serde(deny_unknown_fields)]
struct Configuration {
    listen: SocketAddr,
    hub_id: String,
    authority_generation: String,
    credentials_dir: PathBuf,
    certificate: PathBuf,
    private_key: PathBuf,
    hosts: BTreeMap<String, HostPaths>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostPaths {
    events_socket: PathBuf,
    boss_event: PathBuf,
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
        let composed = boss_execution_runner::compose_worker_spawn(
            &self.db,
            input.worker_id,
            input.execution,
            input.work_item,
            input.workspace,
            input.cube_change_id,
            boss_execution_runner::WorkerSpawnOpts::builder()
                .review_signal_checks(boss_engine_runtime::merge_poller::review_signal_checks_for_owner)
                .bazel_workspace(paths.bazel_workspace)
                .build(),
        )
        .await?;
        // Structured artifacts need host-side custody before these worker
        // kinds can be admitted. Never embed a hub-local output path remotely.
        anyhow::ensure!(
            composed.embedded_output_path.is_none(),
            "spoke worker requires structured-output custody"
        );
        let setup = boss_engine_runtime::worker_setup::WorkerSetupInput::builder()
            .run_id(input.execution.id.clone())
            .lease_id(input.assignment.lease_id.clone().context("missing spoke lease")?)
            .workspace_path(input.workspace.to_path_buf())
            .events_socket_path(paths.events_socket.clone())
            .boss_event_path(paths.boss_event.clone())
            .execution_kind(input.execution.kind.as_str().to_owned())
            .worker_kind(boss_engine_runtime::worker_setup::worker_kind_for_execution(
                &input.execution.kind,
            ))
            .build();
        let driver = boss_engine_driver::DriverRegistry::default().require(&composed.spawn_config.driver)?;
        let settings = boss_engine_runtime::worker_setup::render_remote_settings_json(&setup, driver.as_ref());
        Ok(ComposedSpawn {
            spawn: boss_protocol_spoke::v1::Spawn {
                driver_id: composed.spawn_config.driver.clone(),
                model: composed.spawn_config.model.clone(),
                prompt_utf8: composed.prompt_text.into_bytes(),
                settings_json: settings.into_bytes(),
                ..Default::default()
            },
            outcome: RunOutcome {
                wait_state: RunWaitState::WorkerPaneAlive,
                result_summary: None,
                attention: None,
                slot_id: None,
                spawn_config: Some(composed.spawn_config),
            },
        })
    }
}

pub(super) struct Prepared {
    db: WorkDb,
    hub: HubStream,
    hosts: BTreeMap<String, HostPaths>,
    listener: tokio::net::TcpListener,
    identity: tonic::transport::Identity,
}

/// Validate and bind before publishing server state. Missing configuration must
/// never send a recorded spoke lease or recovery operation through SSH.
pub(super) fn prepare(db: WorkDb) -> Result<Option<Prepared>> {
    prepare_config(db, std::env::var_os("BOSS_SPOKE_HUB_CONFIG"))
}

fn prepare_config(db: WorkDb, path: Option<std::ffi::OsString>) -> Result<Option<Prepared>> {
    let Some(path) = path else {
        for host in db.list_hosts()? {
            anyhow::ensure!(
                !db.host_has_spoke_history(&host.id)?
                    && db.host_transport(&host.id)? != boss_work_db::HostTransport::SpokeReady,
                "BOSS_SPOKE_HUB_CONFIG is required for recorded spoke host {}",
                host.id
            );
        }
        return Ok(None);
    };
    let config: Configuration = serde_json::from_slice(&std::fs::read(path)?)?;
    anyhow::ensure!(
        !config.hub_id.trim().is_empty() && !config.authority_generation.trim().is_empty(),
        "spoke hub identity and authority generation must be nonempty"
    );
    let credentials = Arc::new(HostCredentials::open(
        &config.credentials_dir,
        Policy::default(),
        Arc::new(SystemClock),
    )?);
    let identity =
        tonic::transport::Identity::from_pem(std::fs::read(&config.certificate)?, std::fs::read(&config.private_key)?);
    // Bind before installing dispatch, so an occupied or invalid address never
    // leaves a provider waiting for a listener that did not start.
    let listener = std::net::TcpListener::bind(config.listen)?;
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let hub = HubStream::new(
        boss_spoke_stream::Config::builder()
            .hub_id(config.hub_id)
            .authority_generation(config.authority_generation)
            .build_fingerprint(env!("CARGO_PKG_VERSION"))
            .build(),
        credentials,
        db.clone(),
    );
    tonic::transport::Server::builder().tls_config(boss_spoke_hub::server_tls(identity.clone(), hub.credentials())?)?;
    Ok(Some(Prepared {
        db,
        hub,
        hosts: config.hosts,
        listener,
        identity,
    }))
}

impl Prepared {
    pub(super) fn install(self, inner: Arc<dyn HostAdapterProvider>) -> Arc<dyn HostAdapterProvider> {
        let Self {
            db,
            hub,
            hosts,
            listener,
            identity,
        } = self;
        let provider = Arc::new(SpokeHostAdapterProvider::new(
            inner,
            hub.manager().clone(),
            Arc::new(db.clone()),
            Arc::new(Composer { db, hosts }),
            SpokeAdapterConfig::builder().build(),
        ));
        boss_task_scope::spawn(async move {
            if let Err(error) = hub.serve(listener, identity).await {
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
        assert!(prepare_config(db.clone(), None).unwrap().is_none());
        db.add_host("spoke", "worker@spoke", 1, &[]).unwrap();
        db.set_host_transport("spoke", boss_work_db::HostTransport::SpokeReady)
            .unwrap();
        db.reserve_spoke_assignment("execution", "spoke").unwrap();
        let error = prepare_config(db, None)
            .err()
            .expect("recorded ownership must fail closed");
        assert!(error.to_string().contains("BOSS_SPOKE_HUB_CONFIG is required"));
    }
}
