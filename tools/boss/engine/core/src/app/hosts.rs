//! Host-registry RPC handlers (AddHost, GetHost, ListHosts, SetHostEnabled,
//! RemoveHost, AddHostTag, RemoveHostTag). Thin wrappers over `WorkDb`'s
//! host CRUD methods; the add path eagerly pushes the remote wrapper
//! (same as `bossctl hosts add`) so the engine is the single owner of
//! the host lifecycle. Dispatched from `app.rs`.

use super::*;
use crate::host_registry::{Host, HostCapability};
use crate::protocol::{HostCapabilitySnapshot, HostSnapshot};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn to_host_snapshot(host: Host, caps: Vec<HostCapability>) -> HostSnapshot {
    HostSnapshot {
        id: host.id,
        ssh_target: host.ssh_target,
        pool_size: host.pool_size,
        enabled: host.enabled,
        last_seen_at: host.last_seen_at,
        last_error_text: host.last_error_text,
        consecutive_failures: host.consecutive_failures,
        created_at: host.created_at,
        capabilities: caps
            .into_iter()
            .map(|c| HostCapabilitySnapshot {
                capability: c.capability,
                source: c.source,
            })
            .collect(),
    }
}

fn fetch_snapshot(work_db: &crate::work::WorkDb, id: &str) -> anyhow::Result<HostSnapshot> {
    let host = work_db
        .get_host(id)?
        .ok_or_else(|| anyhow::anyhow!("host '{}' not found", id))?;
    let caps = work_db.list_host_capabilities(id)?;
    Ok(to_host_snapshot(host, caps))
}

fn send_error_msg(sink: &super::SessionSink, request_id: &str, msg: impl Into<String>) {
    send_response(sink, request_id, FrontendEvent::Error { message: msg.into() });
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub(super) async fn handle_list_hosts(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListHosts = req else {
        unreachable!()
    };
    let result = (|| -> anyhow::Result<Vec<HostSnapshot>> {
        let hosts = work_db.list_hosts()?;
        hosts
            .into_iter()
            .map(|h| {
                let caps = work_db.list_host_capabilities(&h.id)?;
                Ok(to_host_snapshot(h, caps))
            })
            .collect()
    })();
    match result {
        Ok(hosts) => {
            send_response(&sink, &request_id, FrontendEvent::HostsList { hosts });
        }
        Err(err) => {
            tracing::warn!(?err, "list_hosts failed");
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

pub(super) async fn handle_get_host(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetHost { id } = req else {
        unreachable!()
    };
    match fetch_snapshot(&work_db, &id) {
        Ok(host) => {
            send_response(&sink, &request_id, FrontendEvent::HostResult { host });
        }
        Err(err) => {
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

pub(super) async fn handle_add_host(ctx: Dispatch, req: FrontendRequest) {
    handle_add_host_with(ctx, req, &SshHostProvisioner).await
}

/// [`handle_add_host`] with the remote-provisioning step injected.
/// Production passes [`SshHostProvisioner`]; tests pass a double so the
/// registration path can be driven without an actual remote host.
pub(super) async fn handle_add_host_with(ctx: Dispatch, req: FrontendRequest, provisioner: &dyn HostProvisioner) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::AddHost {
        id,
        ssh_target,
        pool_size,
        tags,
    } = req
    else {
        unreachable!()
    };

    // Insert the host row.
    if let Err(err) = work_db.add_host(&id, &ssh_target, pool_size, &tags) {
        send_error_msg(&sink, &request_id, err.to_string());
        return;
    }

    // Eagerly provision the remote wrapper (same path as `bossctl hosts add`).
    let outcome = provisioner.provision(&id, &ssh_target).await;
    apply_provision_outcome(&work_db, &id, outcome);

    match fetch_snapshot(&work_db, &id) {
        Ok(host) => {
            send_response(&sink, &request_id, FrontendEvent::HostResult { host });
        }
        Err(err) => {
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

pub(super) async fn handle_set_host_enabled(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetHostEnabled { id, enabled } = req else {
        unreachable!()
    };
    if let Err(err) = work_db.set_host_enabled(&id, enabled) {
        send_error_msg(&sink, &request_id, err.to_string());
        return;
    }
    match fetch_snapshot(&work_db, &id) {
        Ok(host) => {
            send_response(&sink, &request_id, FrontendEvent::HostUpdated { host });
        }
        Err(err) => {
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

pub(super) async fn handle_remove_host(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RemoveHost { id } = req else {
        unreachable!()
    };
    match work_db.remove_host(&id) {
        Ok(()) => {
            send_response(&sink, &request_id, FrontendEvent::HostRemoved { id });
        }
        Err(err) => {
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

pub(super) async fn handle_add_host_tag(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::AddHostTag { host_id, tag } = req else {
        unreachable!()
    };
    if let Err(err) = work_db.add_user_host_capability(&host_id, &tag) {
        send_error_msg(&sink, &request_id, err.to_string());
        return;
    }
    match fetch_snapshot(&work_db, &host_id) {
        Ok(host) => {
            send_response(&sink, &request_id, FrontendEvent::HostUpdated { host });
        }
        Err(err) => {
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

pub(super) async fn handle_remove_host_tag(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RemoveHostTag { host_id, tag } = req else {
        unreachable!()
    };
    if let Err(err) = work_db.remove_user_host_capability(&host_id, &tag) {
        send_error_msg(&sink, &request_id, err.to_string());
        return;
    }
    match fetch_snapshot(&work_db, &host_id) {
        Ok(host) => {
            send_response(&sink, &request_id, FrontendEvent::HostUpdated { host });
        }
        Err(err) => {
            send_error_msg(&sink, &request_id, err.to_string());
        }
    }
}

// ── Eager wrapper push (mirrors bossctl hosts add path) ──────────────────────

/// What contacting a freshly-registered host to install the
/// `boss-remote-run` wrapper produced. Separating the outcome from the
/// DB writes it drives ([`apply_provision_outcome`]) keeps the
/// talk-to-a-real-machine half behind [`HostProvisioner`], so the
/// registration policy — "a host we could not provision must not be left
/// enabled" — is exercisable without an actual remote.
#[derive(Clone)]
pub(super) enum ProvisionOutcome {
    /// Wrapper installed and `cube` confirmed invocable on the remote.
    Ok,
    /// Provisioning was not attempted at all, so the host's enabled state
    /// and error text are left exactly as registered.
    Skipped,
    /// The host could not be reached or provisioned. The string is the
    /// operator-facing reason stored on `last_error_text`.
    Failed(String),
}

/// The remote half of host registration. `handle_add_host` inserts the
/// row, asks this to make the host actually usable, then records the
/// outcome.
#[async_trait::async_trait]
pub(super) trait HostProvisioner: Send + Sync {
    async fn provision(&self, host_id: &str, ssh_target: &str) -> ProvisionOutcome;
}

/// Production [`HostProvisioner`]: opens an ssh control master, pushes
/// the wrapper, and probes `cube` over the same connection.
pub(super) struct SshHostProvisioner;

#[async_trait::async_trait]
impl HostProvisioner for SshHostProvisioner {
    async fn provision(&self, host_id: &str, ssh_target: &str) -> ProvisionOutcome {
        use crate::ssh_transport::{SshTransport, default_control_socket_dir};
        use crate::wrapper_distribution::{
            CubeProbeOutcome, WrapperPushOutcome, push_wrapper, subclass_label, verify_cube_invocable,
        };

        let Some(socket_dir) = default_control_socket_dir() else {
            tracing::warn!(host_id, "eager_push_wrapper: HOME unset; skipping");
            return ProvisionOutcome::Skipped;
        };
        let transport = SshTransport::new(host_id, ssh_target, &socket_dir);

        if let Err(err) = transport.open_control_master().await {
            return ProvisionOutcome::Failed(format!("opening ssh control master: {err:#}"));
        }

        match push_wrapper(&transport).await {
            Ok(WrapperPushOutcome::Ok) => {
                // Wrapper script verified — but that's a separate artifact
                // from the `cube` binary every dispatch-time call shells out
                // to. Probe it now so a host missing `cube` on its
                // non-interactive PATH (the anaplian incident) is caught and
                // disabled at registration time instead of silently failing
                // every future dispatch.
                match verify_cube_invocable(&transport).await {
                    Ok(CubeProbeOutcome::Ok) => ProvisionOutcome::Ok,
                    Ok(CubeProbeOutcome::Failed(detail)) => {
                        ProvisionOutcome::Failed(format!("cube not invocable via non-interactive ssh: {detail}"))
                    }
                    Err(err) => ProvisionOutcome::Failed(format!("probing cube invocability errored: {err:#}")),
                }
            }
            Ok(WrapperPushOutcome::Failed(kind, detail)) => ProvisionOutcome::Failed(format!(
                "wrapper push failed ({label}): {detail}",
                label = subclass_label(&kind)
            )),
            Err(err) => ProvisionOutcome::Failed(format!("wrapper push errored: {err:#}")),
        }
    }
}

/// Record a [`ProvisionOutcome`] on the host row. A host we could not
/// provision is disabled with `last_error_text` set; the caller reads the
/// updated snapshot back, so the UI sees disabled + error text = add
/// failed.
///
/// The writes are best-effort, carried over verbatim from the
/// `eager_push_wrapper_rpc` this was extracted from. Note that dropping
/// the disable's error is not harmless: the caller re-reads the row to
/// build its reply, so a failed `set_host_enabled` makes AddHost report
/// an unprovisionable host as enabled and healthy. Left as-is here to
/// keep the extraction behaviour-preserving.
fn apply_provision_outcome(work_db: &crate::work::WorkDb, host_id: &str, outcome: ProvisionOutcome) {
    match outcome {
        ProvisionOutcome::Skipped => {}
        ProvisionOutcome::Ok => {
            let _ = work_db.set_host_last_error(host_id, None);
        }
        ProvisionOutcome::Failed(detail) => {
            tracing::warn!(host_id, %detail, "eager_push_wrapper: provisioning failed; disabling host");
            let _ = work_db.set_host_enabled(host_id, false);
            let _ = work_db.set_host_last_error(host_id, Some(&detail));
        }
    }
}
