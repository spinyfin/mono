//! Shared fixtures: a controllable clock, a credential authority in a private
//! temp directory, and a stand-in spoke that holds a key and builds CSRs.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use boss_host_enrollment::{
    Clock, CredentialError, Denial, EnrollInput, EnrollmentGrant, EnrollmentToken, HostCredentials, IssuedCertificate,
    Policy,
};
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::pki_types::CertificateDer;
use tempfile::TempDir;

/// A clock that only moves when a test says so. It starts at the real time so
/// certificates it causes to be issued also pass a real TLS handshake.
pub struct ManualClock(Mutex<SystemTime>);

impl ManualClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(SystemTime::now())))
    }

    pub fn advance(&self, by: Duration) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct Fixture {
    pub credentials: Arc<HostCredentials>,
    pub clock: Arc<ManualClock>,
    pub dir: TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        // The hub's state directory is owner-only, and the store insists.
        std::fs::set_permissions(dir.path(), std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("chmod");
        let clock = ManualClock::new();
        let credentials = open(&dir, &clock);
        Self {
            credentials,
            clock,
            dir,
        }
    }

    /// Closes and reopens the credential state in the same directory, as a
    /// restart of the hub would.
    pub fn restart(&mut self) {
        self.credentials = open(&self.dir, &self.clock);
    }

    pub fn now(&self) -> SystemTime {
        self.clock.now()
    }

    /// Authorizes `host_id` and mints the token for one provisioning job.
    pub fn grant(&self, host_id: &str, job_id: &str) -> EnrollmentGrant {
        self.credentials
            .authorize_host(host_id, "test")
            .expect("authorize host");
        self.credentials
            .issue_enrollment_token(host_id, job_id)
            .expect("issue token")
    }

    pub fn enroll(
        &self,
        spoke: &Spoke,
        ids: (&str, &str, &str),
        token: &EnrollmentToken,
    ) -> Result<IssuedCertificate, CredentialError> {
        let (host_id, job_id, request_id) = ids;
        self.credentials.enroll(&EnrollInput {
            host_id,
            job_id,
            request_id,
            token: token.as_bytes(),
            csr_der: &spoke.csr(),
        })
    }

    /// Enrolls a fresh spoke for `host_id` and returns it with its certificate.
    pub fn enrolled(&self, host_id: &str) -> (Spoke, IssuedCertificate) {
        let grant = self.grant(host_id, "job-1");
        let spoke = Spoke::new();
        let cert = self
            .enroll(&spoke, (host_id, "job-1", "req-1"), &grant.token)
            .expect("enroll");
        (spoke, cert)
    }
}

fn open(dir: &TempDir, clock: &Arc<ManualClock>) -> Arc<HostCredentials> {
    let clock: Arc<dyn Clock> = Arc::clone(clock) as Arc<dyn Clock>;
    Arc::new(HostCredentials::open(dir.path(), Policy::default(), clock).expect("open credentials"))
}

/// A spoke's side of enrollment: a private key and the requests built from it.
pub struct Spoke {
    pub key: KeyPair,
}

impl Spoke {
    pub fn new() -> Self {
        Self {
            key: KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key"),
        }
    }

    /// A new, correctly self-signed CSR each call. ECDSA signatures are
    /// randomized, so two calls differ in bytes while naming the same key.
    pub fn csr(&self) -> Vec<u8> {
        CertificateParams::default()
            .serialize_request(&self.key)
            .expect("csr")
            .der()
            .to_vec()
    }
}

pub fn presented(cert: &IssuedCertificate) -> Vec<CertificateDer<'static>> {
    vec![CertificateDer::from(cert.leaf_der.clone())]
}

/// The reason a lifecycle call was refused; panics on any other outcome.
pub fn refusal<T: std::fmt::Debug>(result: Result<T, CredentialError>) -> Denial {
    match result {
        Err(CredentialError::Denied(denial)) => denial,
        other => panic!("expected a refusal, got {other:?}"),
    }
}
