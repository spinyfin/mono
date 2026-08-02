//! The remote half of registering a host: make a freshly-added host
//! actually able to do work, or refuse to leave it enabled.
//!
//! Registration has two callers — `bossctl hosts add` and the engine's
//! `AddHost` RPC — and both must run the identical sequence, because the
//! whole point of it is a policy statement: *a host we could not fully
//! provision must not be persisted as enabled.* They previously each
//! carried their own copy of that sequence, which is how they drifted; this
//! module is the one implementation, and each caller only maps the outcome
//! into its own reporting shape.
//!
//! The sequence, in order, each step gating the next:
//!
//! 1. Open the ssh control master — proves the host is reachable and that
//!    non-interactive (`BatchMode=yes`) auth works.
//! 2. Push and version-verify the `boss-remote-run` wrapper.
//! 3. Probe `cube --help`. The wrapper is scp'd byte-for-byte, so step 2
//!    says nothing about whether the separate `cube` binary every dispatch
//!    shells out to is on the remote's non-interactive `PATH`.
//! 4. Discover the host's capabilities.
//!
//! Step 4 is the reason this module exists rather than the callers just
//! sharing steps 1–3: without it a registered remote host reported
//! `caps=0` forever, because capability discovery only ever ran against
//! the local machine. See [`crate::host_capability_probe`].

use crate::host_capability_probe::discover_remote_capabilities;
use crate::ssh_transport::{SshTransport, default_control_socket_dir};
use crate::wrapper_distribution::{
    CubeProbeOutcome, WrapperPushOutcome, push_wrapper, subclass_label, verify_cube_invocable,
};

/// What [`provision_remote_host`] managed to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteProvisionOutcome {
    /// Host reachable, wrapper current, `cube` invocable, capabilities
    /// discovered. `capabilities` is what the host reported — never a
    /// default or a fallback set, so an operator reading `caps=` is reading
    /// the machine's own answer.
    Ok { capabilities: Vec<String> },
    /// Provisioning was not attempted at all. The host's enabled state and
    /// error text should be left exactly as registered.
    Skipped { reason: String },
    /// The host could not be reached or provisioned. `kind` is the Q6
    /// sub-classification; `detail` is the operator-facing reason to store
    /// on `last_error_text`.
    Failed { kind: &'static str, detail: String },
}

/// Contact `ssh_target` and run the full provisioning sequence. See the
/// module docs for the steps and why each one gates the next.
///
/// Never panics and never leaves the decision implicit: every path returns
/// one of the three outcomes above, and the caller is responsible for the
/// DB writes they imply.
pub async fn provision_remote_host(host_id: &str, ssh_target: &str) -> RemoteProvisionOutcome {
    let Some(socket_dir) = default_control_socket_dir() else {
        return RemoteProvisionOutcome::Skipped {
            reason: "HOME unset; cannot determine control-socket dir".to_owned(),
        };
    };
    let transport = SshTransport::new(host_id, ssh_target, &socket_dir);

    if let Err(err) = transport.open_control_master().await {
        return RemoteProvisionOutcome::Failed {
            kind: "connection_lost",
            detail: format!("opening ssh control master: {err:#}"),
        };
    }

    match push_wrapper(&transport).await {
        Ok(WrapperPushOutcome::Ok) => {}
        Ok(WrapperPushOutcome::Failed(kind, detail)) => {
            return wrapper_push_failed(kind, detail);
        }
        Err(err) => {
            return RemoteProvisionOutcome::Failed {
                kind: "unclassified",
                detail: format!("wrapper push errored: {err:#}"),
            };
        }
    }

    match verify_cube_invocable(&transport).await {
        Ok(CubeProbeOutcome::Ok) => {}
        Ok(CubeProbeOutcome::Failed(detail)) => {
            return RemoteProvisionOutcome::Failed {
                kind: "unclassified",
                detail: format!("cube not invocable via non-interactive ssh: {detail}"),
            };
        }
        Err(err) => {
            return RemoteProvisionOutcome::Failed {
                kind: "unclassified",
                detail: format!("probing cube invocability errored: {err:#}"),
            };
        }
    }

    discovered_capabilities_outcome(discover_remote_capabilities(&transport).await)
}

fn wrapper_push_failed(kind: crate::ssh_transport::SshFailureKind, detail: String) -> RemoteProvisionOutcome {
    let kind = subclass_label(&kind);
    RemoteProvisionOutcome::Failed {
        kind,
        detail: format!("wrapper push failed ({kind}): {detail}"),
    }
}

fn discovered_capabilities_outcome(result: anyhow::Result<Vec<String>>) -> RemoteProvisionOutcome {
    match result {
        // Today the `gh-authed=` probe makes this unreachable. Keep this
        // defensive invariant so a future probe-set change cannot create an
        // enabled host with zero discovered capabilities silently.
        Ok(capabilities) if capabilities.is_empty() => RemoteProvisionOutcome::Failed {
            kind: "unclassified",
            detail: "capability discovery returned nothing; host answered `cube --help` \
                     but no probe produced a capability"
                .to_owned(),
        },
        Ok(capabilities) => RemoteProvisionOutcome::Ok { capabilities },
        Err(err) => RemoteProvisionOutcome::Failed {
            kind: "unclassified",
            detail: format!("capability discovery errored: {err:#}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_discovery_result_is_not_accepted_as_a_healthy_host() {
        assert_eq!(
            discovered_capabilities_outcome(Ok(vec![])),
            RemoteProvisionOutcome::Failed {
                kind: "unclassified",
                detail: "capability discovery returned nothing; host answered `cube --help` but no probe produced a capability".to_owned(),
            }
        );
    }

    #[test]
    fn wrapper_push_failure_uses_the_operator_facing_subclass_label() {
        assert_eq!(
            wrapper_push_failed(
                crate::ssh_transport::SshFailureKind::PermissionDenied,
                "remote directory is read-only".to_owned(),
            ),
            RemoteProvisionOutcome::Failed {
                kind: "permission_denied",
                detail: "wrapper push failed (permission_denied): remote directory is read-only".to_owned(),
            }
        );
    }
}
