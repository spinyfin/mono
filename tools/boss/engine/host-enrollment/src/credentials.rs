//! The host credential lifecycle: which hosts the hub has authorized, the
//! certificates it has issued them, and when those stop working.
//!
//! A host moves `pending` (authorized, no certificate) → `enrolled` →
//! `revoked`. Enrolling and renewing live in [`crate::enroll`] and
//! [`crate::renew`]; checking a presented certificate lives in
//! [`crate::authority`]. Everything here is synchronous and blocking on a
//! local SQLite file; async callers wrap it in `spawn_blocking`.

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use boss_protocol_spoke::identity::is_valid_host_id;
use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::ClientCertVerifier;
use tokio::sync::broadcast;

use crate::authority::CredentialEvent;
use crate::ca::CertificateAuthority;
use crate::clock::{Clock, from_unix_secs, unix_secs};
use crate::error::{CredentialError, Denial};
use crate::policy::Policy;
use crate::store::{self, HostState, Store};
use crate::token::EnrollmentToken;

const MAX_ID_BYTES: usize = 128;
const MAX_NOTE_BYTES: usize = 256;
const EVENT_BACKLOG: usize = 256;

/// The CA and the verifier built from it.
pub(crate) struct Trust {
    pub ca: CertificateAuthority,
    pub verifier: Arc<dyn ClientCertVerifier>,
}

/// The hub's host credential authority.
pub struct HostCredentials {
    pub(crate) trust: Trust,
    pub(crate) store: Store,
    pub(crate) policy: Policy,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) events: broadcast::Sender<CredentialEvent>,
}

/// A certificate as returned to a spoke by `Enroll` or `Renew`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedCertificate {
    pub host_id: String,
    pub serial: String,
    pub leaf_der: Vec<u8>,
    /// The CA trust bundle, the issuing CA first.
    pub chain_der: Vec<Vec<u8>>,
    pub expires_unix_ms: i64,
}

/// A freshly minted enrollment secret and when it stops working.
#[derive(Debug)]
pub struct EnrollmentGrant {
    pub token: EnrollmentToken,
    pub expires_at: SystemTime,
}

/// A certificate in a host's [`CredentialStatus`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateSummary {
    pub serial: String,
    /// When it stops authorizing: its expiry, or for a superseded certificate
    /// the end of the renewal overlap if that is sooner.
    pub valid_until: SystemTime,
}

/// What the hub currently believes about a host's credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialStatus {
    pub state: HostState,
    pub current: Option<CertificateSummary>,
    pub previous: Option<CertificateSummary>,
}

impl HostCredentials {
    /// Opens (creating on first use) the credential state under `dir`: the
    /// CA in `dir/ca` and the database at `dir/credentials.db`.
    ///
    /// `dir` and any existing database or `-wal`/`-shm` file must be
    /// owner-only (no group or other permission bits, e.g. `0o700`); permissive
    /// ones are refused with an internal error rather than tightened. A `dir`
    /// that does not exist is created `0o700`.
    pub fn open(dir: &Path, policy: Policy, clock: Arc<dyn Clock>) -> Result<Self, CredentialError> {
        let ca = CertificateAuthority::open_or_create(&dir.join("ca"), clock.now(), &policy)?;
        let mut roots = RootCertStore::empty();
        for der in ca.trust_bundle_der() {
            roots
                .add(CertificateDer::from(der))
                .map_err(|error| CredentialError::Internal(format!("CA certificate is not a trust anchor: {error}")))?;
        }
        let verifier = WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .map_err(|error| CredentialError::Internal(format!("client certificate verifier: {error}")))?;
        let store = Store::open(&dir.join("credentials.db"))?;
        Ok(Self {
            trust: Trust { ca, verifier },
            store,
            policy,
            clock,
            events: broadcast::channel(EVENT_BACKLOG).0,
        })
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The CA certificates, as PEM, that a listener must accept as client
    /// certificate issuers.
    pub fn trust_bundle_pem(&self) -> String {
        self.trust.ca.trust_bundle_pem()
    }

    /// The same bundle as DER, which is also what `Certificate.chain_der`
    /// carries to a spoke.
    pub fn trust_bundle_der(&self) -> Vec<Vec<u8>> {
        self.trust.ca.trust_bundle_der()
    }

    /// Authorizes `host_id` to enroll. Idempotent for a host already
    /// authorized. For a revoked host this is the explicit new authorization
    /// that reopens it: the old certificates stay dead and the host starts
    /// again from `pending`.
    pub fn authorize_host(&self, host_id: &str, actor: &str) -> Result<(), CredentialError> {
        check_host_id(host_id)?;
        let now = unix_secs(self.clock.now());
        self.store.write(|tx| {
            match store::host(tx, host_id)? {
                None => {
                    store::insert_host(tx, host_id)?;
                    store::audit(tx, now, host_id, "authorized", &note("actor", actor))?;
                }
                Some(row) if row.state == HostState::Revoked => {
                    store::set_host(tx, host_id, HostState::Pending, (None, None))?;
                    store::bump_generation(tx, host_id)?;
                    store::audit(tx, now, host_id, "reauthorized", &note("actor", actor))?;
                }
                Some(_) => {}
            }
            Ok(())
        })
    }

    /// Mints the single-use secret for one provisioning job of an authorized
    /// host. Any earlier unused secret for the host stops working: an expired
    /// or lost credential is replaced through the job, never extended.
    ///
    /// A revoked host gets nothing until [`authorize_host`](Self::authorize_host)
    /// reopens it; an automatic retry must not resurrect it.
    pub fn issue_enrollment_token(&self, host_id: &str, job_id: &str) -> Result<EnrollmentGrant, CredentialError> {
        check_host_id(host_id)?;
        check_opaque_id("job_id", job_id)?;
        let now = self.clock.now();
        let expires_at = now + self.policy.token_lifetime;
        let token = EnrollmentToken::generate();
        self.store.write(|tx| {
            let host = store::host(tx, host_id)?.ok_or(CredentialError::UnknownHost)?;
            if host.state == HostState::Revoked {
                return Err(Denial::Revoked.into());
            }
            store::retire_grants(tx, host_id, None, "replaced")?;
            store::insert_grant(
                tx,
                &token.hash(),
                host_id,
                job_id,
                (host.generation, unix_secs(expires_at)),
            )?;
            store::audit(tx, unix_secs(now), host_id, "token_issued", &note("job", job_id))
        })?;
        Ok(EnrollmentGrant { token, expires_at })
    }

    /// Cancels a provisioning job: its secret stops working and an enrollment
    /// retry for it is refused. A certificate it already produced stays valid;
    /// [`revoke`](Self::revoke) is what ends that.
    pub fn cancel_job(&self, host_id: &str, job_id: &str, actor: &str) -> Result<(), CredentialError> {
        check_host_id(host_id)?;
        check_opaque_id("job_id", job_id)?;
        let now = unix_secs(self.clock.now());
        self.store.write(|tx| {
            store::host(tx, host_id)?.ok_or(CredentialError::UnknownHost)?;
            store::retire_grants(tx, host_id, Some(job_id), "cancelled")?;
            let detail = format!("{} {}", note("actor", actor), note("job", job_id));
            store::audit(tx, now, host_id, "job_cancelled", &detail)
        })
    }

    /// Revokes a host's identity. Its streams are told to close (see
    /// [`CredentialEvent::Revoked`]), and reconnect, renewal and
    /// re-enrollment are refused until [`authorize_host`](Self::authorize_host)
    /// reopens it. This cannot stop a worker already running on an offline
    /// host; it only ends what that host may report.
    pub fn revoke(&self, host_id: &str, actor: &str, reason: &str) -> Result<(), CredentialError> {
        check_host_id(host_id)?;
        let now = unix_secs(self.clock.now());
        self.store.write(|tx| {
            store::host(tx, host_id)?.ok_or(CredentialError::UnknownHost)?;
            store::set_state(tx, host_id, HostState::Revoked)?;
            store::retire_grants(tx, host_id, None, "cancelled")?;
            let detail = format!("{} {}", note("actor", actor), note("reason", reason));
            store::audit(tx, now, host_id, "revoked", &detail)
        })?;
        self.publish([CredentialEvent::Revoked {
            host_id: host_id.to_owned(),
        }]);
        Ok(())
    }

    /// The hub's view of a host's credentials, or `None` for an unknown host.
    pub fn status(&self, host_id: &str) -> Result<Option<CredentialStatus>, CredentialError> {
        check_host_id(host_id)?;
        self.store.read(|conn| {
            let Some(host) = store::host(conn, host_id)? else {
                return Ok(None);
            };
            let summarize =
                |serial: &Option<String>, cap: Option<i64>| -> Result<Option<CertificateSummary>, CredentialError> {
                    let Some(serial) = serial else { return Ok(None) };
                    let Some(cert) = store::certificate(conn, serial)? else {
                        return Ok(None);
                    };
                    let until = cap.map_or(cert.not_after, |cap| cap.min(cert.not_after));
                    Ok(Some(CertificateSummary {
                        serial: cert.serial,
                        valid_until: from_unix_secs(until),
                    }))
                };
            Ok(Some(CredentialStatus {
                state: host.state,
                current: summarize(&host.current_serial, None)?,
                previous: summarize(&host.previous_serial, host.previous_valid_until)?,
            }))
        })
    }

    /// The audit trail for `host_id`, oldest first, as `(event, detail)`.
    pub fn audit_trail(&self, host_id: &str) -> Result<Vec<(String, String)>, CredentialError> {
        check_host_id(host_id)?;
        self.store.read(|conn| store::audit_trail(conn, host_id))
    }

    /// Tells subscribers about committed changes. Nobody listening is not an
    /// error: the stream manager revalidates on connect regardless.
    pub(crate) fn publish(&self, events: impl IntoIterator<Item = CredentialEvent>) {
        for event in events {
            let _ = self.events.send(event);
        }
    }
}

pub(crate) fn check_host_id(host_id: &str) -> Result<(), CredentialError> {
    if is_valid_host_id(host_id) {
        Ok(())
    } else {
        Err(CredentialError::Invalid("host_id is not a valid host id".to_owned()))
    }
}

/// Ids a peer chooses (`job_id`, `request_id`) are opaque but bounded and
/// printable, so they are safe to store, compare and log.
pub(crate) fn check_opaque_id(name: &str, value: &str) -> Result<(), CredentialError> {
    if !value.is_empty() && value.len() <= MAX_ID_BYTES && value.bytes().all(|b| b.is_ascii_graphic()) {
        Ok(())
    } else {
        Err(CredentialError::Invalid(format!(
            "{name} must be 1..={MAX_ID_BYTES} printable ASCII bytes"
        )))
    }
}

/// `key=value` for an audit detail, with the value flattened and bounded.
fn note(key: &str, value: &str) -> String {
    let flat: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_NOTE_BYTES)
        .collect();
    format!("{key}={flat}")
}

pub(crate) fn certificate_response(
    credentials: &HostCredentials,
    host_id: &str,
    cert: &store::CertRow,
) -> IssuedCertificate {
    IssuedCertificate {
        host_id: host_id.to_owned(),
        serial: cert.serial.clone(),
        leaf_der: cert.leaf_der.clone(),
        chain_der: credentials.trust.ca.trust_bundle_der(),
        expires_unix_ms: cert.not_after.saturating_mul(1000),
    }
}
