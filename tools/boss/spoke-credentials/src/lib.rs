//! A spoke's side of its credential lifecycle: getting a certificate from the
//! hub, keeping it fresh, and holding the key that proves it is yours.
//!
//! The hub half is `boss-host-enrollment`. The rules this crate keeps (hub-and-
//! spoke design, "Enrollment, service user and credential lifecycle"):
//!
//! - The private key is generated here and never leaves the host. The hub sees
//!   a CSR.
//! - A request's key and id are written to disk **before** it is sent. If the
//!   reply is lost, or the spoke dies mid-call, the retry repeats the same
//!   request and the hub answers with the certificate it already issued.
//! - A hub refusal discards the request, so a refused id is never reused; a
//!   transport failure keeps it.
//! - A certificate and its key are installed with one rename. A reader sees the
//!   old identity or the new one, never a certificate with the wrong key.
//! - Renewal starts [`RENEW_BEFORE_EXPIRY`] before expiry plus a per-certificate
//!   jitter, so a fleet enrolled together does not renew together.
//!
//! Installing an identity bumps a counter on [`SpokeCredentials::subscribe`];
//! the stream connection watches it and reconnects with the new identity.

mod error;
mod files;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use boss_protocol_spoke::v1::{Certificate, EnrollRequest, RenewRequest};
use rand::Rng;
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use tokio::sync::watch;
use tonic::{Status, async_trait};

pub use error::ClientError;
pub use files::Identity;
use files::{PENDING_ENROLL, PENDING_RENEW, Pending};

/// Renewal is due when this much of the certificate's life is left (or more,
/// by the jitter).
pub const RENEW_BEFORE_EXPIRY: Duration = Duration::from_secs(10 * 24 * 60 * 60);

/// The most the per-certificate jitter moves renewal earlier.
pub const MAX_RENEW_JITTER: Duration = Duration::from_secs(24 * 60 * 60);

type Client = boss_protocol_spoke::v1::enrollment_client::EnrollmentClient<tonic::transport::Channel>;

/// The two RPCs the lifecycle makes. Implemented for the generated tonic client
/// below; `Renew` must be sent over a connection authenticated with the
/// *current* identity, which is the caller's to arrange.
#[async_trait]
pub trait Exchange: Send + Sync {
    async fn enroll(&self, request: EnrollRequest) -> Result<Certificate, Status>;
    async fn renew(&self, request: RenewRequest) -> Result<Certificate, Status>;
}

#[async_trait]
impl Exchange for Client {
    async fn enroll(&self, request: EnrollRequest) -> Result<Certificate, Status> {
        Ok(Client::enroll(&mut self.clone(), request).await?.into_inner())
    }

    async fn renew(&self, request: RenewRequest) -> Result<Certificate, Status> {
        Ok(Client::renew(&mut self.clone(), request).await?.into_inner())
    }
}

/// What provisioning handed the spoke to enroll with.
#[derive(Debug, Clone, Copy)]
pub struct EnrollParams<'a> {
    pub host_id: &'a str,
    pub job_id: &'a str,
    pub token: &'a [u8],
}

/// The spoke's credential directory (owner-only) and the lifecycle over it.
pub struct SpokeCredentials {
    dir: PathBuf,
    installed: watch::Sender<u64>,
}

impl SpokeCredentials {
    /// Opens `dir`, creating it owner-only if it does not exist.
    pub fn open(dir: &Path) -> Result<Self, ClientError> {
        files::create_private_dir(dir)?;
        Ok(Self {
            dir: dir.to_owned(),
            installed: watch::channel(0).0,
        })
    }

    /// The installed identity, if the host has enrolled.
    pub fn identity(&self) -> Result<Option<Identity>, ClientError> {
        files::read_identity(&self.dir)
    }

    /// A counter that changes each time an identity is installed. The stream
    /// connection reconnects when it does.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.installed.subscribe()
    }

    /// Enrolls with the single-use token from provisioning, installing the
    /// issued identity. Safe to repeat after a [`ClientError::Unavailable`]:
    /// the same key and request id are sent, which the hub recognizes.
    pub async fn enroll(&self, exchange: &dyn Exchange, params: &EnrollParams<'_>) -> Result<Identity, ClientError> {
        let pending = self.pending(PENDING_ENROLL)?;
        let csr_der = csr_for(&pending.key_pkcs8_der)?;
        let reply = exchange
            .enroll(EnrollRequest {
                host_id: params.host_id.to_owned(),
                job_id: params.job_id.to_owned(),
                request_id: pending.request_id.clone(),
                token: params.token.to_vec(),
                csr_der,
            })
            .await;
        self.finish(PENDING_ENROLL, pending, reply, Some(params.host_id))
    }

    /// Renews the installed identity now, over a connection `exchange`
    /// authenticated with it.
    pub async fn renew(&self, exchange: &dyn Exchange) -> Result<Identity, ClientError> {
        let current = self.identity()?.ok_or(ClientError::NotEnrolled)?;
        let pending = self.pending(PENDING_RENEW)?;
        if pending.key_pkcs8_der == current.key_pkcs8_der {
            // The previous renewal was installed and the process died before
            // the request was cleared. The key is the live one: start over.
            files::remove(&self.dir, PENDING_RENEW)?;
            return Box::pin(self.renew(exchange)).await;
        }
        let csr_der = csr_for(&pending.key_pkcs8_der)?;
        let reply = exchange
            .renew(RenewRequest {
                request_id: pending.request_id.clone(),
                csr_der,
            })
            .await;
        self.finish(PENDING_RENEW, pending, reply, Some(&current.host_id))
    }

    /// Renews if the installed identity is within the renewal window.
    /// `jitter` is drawn once per certificate by [`renewal_jitter`] and kept by
    /// the caller. Returns the new identity, or `None` when nothing was due.
    pub async fn renew_if_due(
        &self,
        exchange: &dyn Exchange,
        now: SystemTime,
        jitter: Duration,
    ) -> Result<Option<Identity>, ClientError> {
        let current = self.identity()?.ok_or(ClientError::NotEnrolled)?;
        if !renewal_due(current.expires_at, now, jitter) {
            return Ok(None);
        }
        self.renew(exchange).await.map(Some)
    }

    /// The request in flight for `name`, or a new one persisted before use.
    fn pending(&self, name: &str) -> Result<Pending, ClientError> {
        if let Some(pending) = files::read_pending(&self.dir, name)? {
            return Ok(pending);
        }
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|e| ClientError::Generate(e.to_string()))?;
        let pending = Pending {
            request_id: request_id(),
            key_pkcs8_der: key.serialize_der(),
        };
        files::write_pending(&self.dir, name, &pending)?;
        Ok(pending)
    }

    fn finish(
        &self,
        name: &str,
        pending: Pending,
        reply: Result<Certificate, Status>,
        expect_host: Option<&str>,
    ) -> Result<Identity, ClientError> {
        let cert = match reply {
            Ok(cert) => cert,
            Err(status) if error::keeps_request(status.code()) => {
                return Err(ClientError::Unavailable {
                    code: status.code(),
                    message: status.message().to_owned(),
                });
            }
            Err(status) => {
                files::remove(&self.dir, name)?;
                return Err(ClientError::Refused {
                    code: status.code(),
                    message: status.message().to_owned(),
                });
            }
        };
        if cert.leaf_der.is_empty() || cert.serial.is_empty() {
            return Err(ClientError::BadCertificate("empty certificate or serial"));
        }
        if expect_host.is_some_and(|host| host != cert.host_id) {
            return Err(ClientError::BadCertificate("certificate is for another host"));
        }
        let identity = Identity::builder()
            .host_id(cert.host_id)
            .serial(cert.serial)
            .expires_at(SystemTime::UNIX_EPOCH + Duration::from_millis(cert.expires_unix_ms.max(0) as u64))
            .leaf_der(cert.leaf_der)
            .chain_der(cert.chain_der)
            .key_pkcs8_der(pending.key_pkcs8_der)
            .build();
        files::write_identity(&self.dir, &identity)?;
        files::remove(&self.dir, name)?;
        self.installed.send_modify(|generation| *generation += 1);
        Ok(identity)
    }
}

/// Whether a certificate expiring at `expires_at` should be renewed at `now`.
pub fn renewal_due(expires_at: SystemTime, now: SystemTime, jitter: Duration) -> bool {
    expires_at
        .duration_since(now)
        .is_ok_and(|left| left <= RENEW_BEFORE_EXPIRY + jitter.min(MAX_RENEW_JITTER))
        || expires_at <= now
}

/// A random head start, up to [`MAX_RENEW_JITTER`], for one certificate.
pub fn renewal_jitter() -> Duration {
    Duration::from_secs(rand::thread_rng().gen_range(0..=MAX_RENEW_JITTER.as_secs()))
}

fn csr_for(key_pkcs8_der: &[u8]) -> Result<Vec<u8>, ClientError> {
    let key = KeyPair::try_from(key_pkcs8_der).map_err(|e| ClientError::Generate(e.to_string()))?;
    let csr = CertificateParams::default()
        .serialize_request(&key)
        .map_err(|e| ClientError::Generate(e.to_string()))?;
    Ok(csr.der().to_vec())
}

fn request_id() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    #[test]
    fn renewal_is_due_ten_days_out_plus_jitter() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        assert!(!renewal_due(now + 11 * DAY, now, Duration::ZERO));
        assert!(renewal_due(now + 10 * DAY, now, Duration::ZERO));
        assert!(renewal_due(now + 11 * DAY - Duration::from_secs(1), now, DAY));
        assert!(!renewal_due(now + 12 * DAY, now, DAY));
        assert!(
            renewal_due(now - DAY, now, Duration::ZERO),
            "an expired certificate is overdue"
        );
    }

    #[test]
    fn jitter_is_bounded() {
        for _ in 0..100 {
            assert!(renewal_jitter() <= MAX_RENEW_JITTER);
        }
    }

    #[test]
    fn identity_files_round_trip_and_are_owner_only() {
        let dir = tempfile::tempdir().expect("dir");
        let credentials = SpokeCredentials::open(dir.path()).expect("open");
        assert_eq!(credentials.identity().expect("read"), None);
        let identity = Identity::builder()
            .host_id("edge-1")
            .serial("ab")
            .expires_at(SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123))
            .leaf_der(vec![1, 2, 3])
            .chain_der(vec![vec![4], vec![5, 6]])
            .key_pkcs8_der(vec![7, 8])
            .build();
        files::write_identity(dir.path(), &identity).expect("write");
        assert_eq!(credentials.identity().expect("read"), Some(identity));
        let mode = std::fs::metadata(dir.path().join("identity")).expect("meta");
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
            0o600
        );
    }
}
