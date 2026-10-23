//! The authorization boundary the hub's stream manager calls.
//!
//! TLS proves a peer holds a key the CA signed. That is necessary and not
//! sufficient: a certificate that was revoked, replaced by a renewal whose
//! overlap has ended, or issued to a host that has since been removed still
//! passes the handshake. [`HostAuthorizer::authorize`] is the check that
//! closes the gap, and [`CredentialEvent`] is how a stream already open is told
//! that its certificate stopped counting.

use std::time::{Duration, SystemTime};

use rusqlite::Connection;
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::{CertificateError, Error as TlsError};
use tokio::sync::broadcast;

use crate::cert;
use crate::clock::{from_unix_secs, unix_secs};
use crate::credentials::HostCredentials;
use crate::error::{CredentialError, Denial};
use crate::store::{self, HostRow, HostState};

/// A host whose certificate was accepted, and until when.
///
/// `valid_until` is the certificate's own limit at the time of the check. It
/// can shorten afterwards (a renewal starts the overlap clock, a revocation
/// ends it at once), which is what [`CredentialEvent`] and
/// [`HostAuthorizer::revalidate`] are for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedHost {
    pub host_id: String,
    pub serial: String,
    pub valid_until: SystemTime,
}

/// A committed change that can end an already-authorized stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialEvent {
    /// The host's identity was revoked: close every stream of the host.
    Revoked { host_id: String },
    /// A certificate stopped being the host's current one. Streams that
    /// authenticated with `serial` must close by `valid_until`, which is the
    /// end of the renewal overlap (or now, for a certificate that was dropped
    /// outright).
    Superseded {
        host_id: String,
        serial: String,
        valid_until: SystemTime,
    },
}

/// What the stream manager asks of the credential authority.
///
/// `subscribe` is lossy by construction: a receiver that falls behind gets
/// `Lagged` and must re-run [`revalidate`](Self::revalidate) for every open
/// stream rather than assume nothing was missed.
pub trait HostAuthorizer: Send + Sync {
    /// Authorizes the peer certificate chain a TLS handshake produced. The
    /// caller still compares the returned `host_id` with `Hello.host_id`.
    fn authorize(&self, peer_chain: &[CertificateDer<'_>]) -> Result<AuthorizedHost, Denial>;

    /// Re-checks a host authorized earlier against the current records.
    fn revalidate(&self, host: &AuthorizedHost) -> Result<(), Denial>;

    /// Changes that can end an open stream.
    fn subscribe(&self) -> broadcast::Receiver<CredentialEvent>;
}

impl HostAuthorizer for HostCredentials {
    fn authorize(&self, peer_chain: &[CertificateDer<'_>]) -> Result<AuthorizedHost, Denial> {
        let now = self.clock.now();
        let leaf = peer_chain.first().ok_or(Denial::NoCertificate)?;
        let since_epoch = now.duration_since(SystemTime::UNIX_EPOCH).unwrap_or(Duration::ZERO);
        // The chain the peer sent is not used to build a path: the leaf must
        // be signed directly by the CA, which is the only shape the hub issues.
        self.trust
            .verifier
            .verify_client_cert(leaf, &[], UnixTime::since_unix_epoch(since_epoch))
            .map_err(tls_denial)?;
        let info = cert::parse_leaf(leaf.as_ref()).ok_or(Denial::Untrusted)?;
        let host_id = cert::named_host(&info.san_uris).ok_or(Denial::WrongIdentity)?;
        let valid_until = self
            .store
            .read(|conn| {
                let host = store::host(conn, host_id)?.ok_or(Denial::UnknownHost)?;
                check_serial(conn, &host, &info.serial, unix_secs(now))
            })
            .map_err(denial_of)?;
        Ok(AuthorizedHost {
            host_id: host_id.to_owned(),
            serial: info.serial,
            valid_until: from_unix_secs(valid_until),
        })
    }

    fn revalidate(&self, host: &AuthorizedHost) -> Result<(), Denial> {
        let now = unix_secs(self.clock.now());
        self.store
            .read(|conn| {
                let row = store::host(conn, &host.host_id)?.ok_or(Denial::UnknownHost)?;
                check_serial(conn, &row, &host.serial, now).map(|_| ())
            })
            .map_err(denial_of)
    }

    fn subscribe(&self) -> broadcast::Receiver<CredentialEvent> {
        self.events.subscribe()
    }
}

/// Whether `serial` may authenticate `host` at `now`, and until when.
///
/// The current certificate is good until it expires. The previous one, kept
/// across a renewal so a spoke that has not installed the new one yet can
/// still reconnect, is good until the overlap ends or it expires, whichever is
/// first. Any other serial was superseded.
pub(crate) fn check_serial(conn: &Connection, host: &HostRow, serial: &str, now: i64) -> Result<i64, CredentialError> {
    match host.state {
        HostState::Revoked => return Err(Denial::Revoked.into()),
        HostState::Pending => return Err(Denial::NotEnrolled.into()),
        HostState::Enrolled => {}
    }
    let not_after = store::certificate(conn, serial)?.ok_or(Denial::Superseded)?.not_after;
    let is_current = host.current_serial.as_deref() == Some(serial);
    let (until, expired) = if is_current {
        (not_after, Denial::CertificateExpired)
    } else if host.previous_serial.as_deref() == Some(serial) {
        let overlap_end = host.previous_valid_until.unwrap_or(i64::MIN);
        (overlap_end.min(not_after), Denial::OverlapElapsed)
    } else {
        return Err(Denial::Superseded.into());
    };
    if now >= until {
        return Err(expired.into());
    }
    Ok(until)
}

fn tls_denial(error: TlsError) -> Denial {
    match error {
        TlsError::InvalidCertificate(CertificateError::Expired | CertificateError::ExpiredContext { .. }) => {
            Denial::CertificateExpired
        }
        TlsError::InvalidCertificate(CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. }) => {
            Denial::CertificateNotYetValid
        }
        _ => Denial::Untrusted,
    }
}

/// Folds a lifecycle error into a denial. A store failure denies rather than
/// admits, and is logged because the peer is told only "not permitted".
fn denial_of(error: CredentialError) -> Denial {
    match error {
        CredentialError::Denied(denial) => denial,
        CredentialError::UnknownHost => Denial::UnknownHost,
        other => {
            tracing::error!(error = %other, "host credential check failed; denying");
            Denial::StoreUnavailable
        }
    }
}
