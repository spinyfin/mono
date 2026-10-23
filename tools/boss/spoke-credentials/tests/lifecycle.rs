//! The spoke client against the real hub-side authority (real CA and database,
//! time under the test's control). The exchange calls the authority in process;
//! the mutual-TLS transport is covered by the authority's own boundary test.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use boss_host_enrollment::{
    Clock, CredentialError, EnrollInput, HostAuthorizer, HostCredentials, IssuedCertificate, Policy, RenewInput,
};
use boss_protocol_spoke::v1::{Certificate, EnrollRequest, RenewRequest};
use boss_spoke_credentials::{ClientError, EnrollParams, Exchange, SpokeCredentials, renewal_due};
use tonic::{Code, Status};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

struct ManualClock(Mutex<SystemTime>);

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Hub {
    credentials: Arc<HostCredentials>,
    clock: Arc<ManualClock>,
    _dir: tempfile::TempDir,
    /// Process the next call, then lose the reply.
    lose_next_reply: AtomicBool,
    seen: Mutex<Seen>,
}

#[derive(Default)]
struct Seen {
    request_ids: Vec<String>,
    /// The certificate the next `Renew` is made over, as the transport would
    /// have authenticated it.
    caller_leaf: Vec<u8>,
}

impl Hub {
    fn new() -> Arc<Self> {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::set_permissions(dir.path(), std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("chmod");
        let clock = Arc::new(ManualClock(Mutex::new(SystemTime::now())));
        let credentials = Arc::new(
            HostCredentials::open(dir.path(), Policy::default(), Arc::clone(&clock) as Arc<dyn Clock>).expect("open"),
        );
        Arc::new(Self {
            credentials,
            clock,
            _dir: dir,
            lose_next_reply: AtomicBool::new(false),
            seen: Mutex::default(),
        })
    }

    fn grant(&self, host_id: &str, job_id: &str) -> Vec<u8> {
        self.credentials.authorize_host(host_id, "test").expect("authorize");
        self.credentials
            .issue_enrollment_token(host_id, job_id)
            .expect("token")
            .token
            .as_bytes()
            .to_vec()
    }

    fn advance(&self, by: Duration) {
        *self.clock.0.lock().unwrap_or_else(PoisonError::into_inner) += by;
    }

    /// What the spoke sees: the answer, or nothing if the reply was lost.
    fn answer(&self, issued: Result<IssuedCertificate, CredentialError>) -> Result<Certificate, CredentialError> {
        if self.lose_next_reply.swap(false, Ordering::SeqCst) {
            return Err(CredentialError::Internal("reply lost".to_owned()));
        }
        let cert = issued?;
        Ok(Certificate {
            leaf_der: cert.leaf_der,
            chain_der: cert.chain_der,
            expires_unix_ms: cert.expires_unix_ms,
            host_id: cert.host_id,
            serial: cert.serial,
        })
    }

    fn request_ids(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .request_ids
            .clone()
    }

    fn record(&self, request_id: &str) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .request_ids
            .push(request_id.to_owned());
    }
}

fn status_of(error: CredentialError) -> Status {
    match error {
        CredentialError::Denied(_) => Status::permission_denied("not permitted"),
        CredentialError::Invalid(message) => Status::invalid_argument(message),
        other => Status::internal(other.to_string()),
    }
}

#[tonic::async_trait]
impl Exchange for Hub {
    async fn enroll(&self, request: EnrollRequest) -> Result<Certificate, Status> {
        self.record(&request.request_id);
        let issued = self.credentials.enroll(&EnrollInput {
            host_id: &request.host_id,
            job_id: &request.job_id,
            request_id: &request.request_id,
            token: &request.token,
            csr_der: &request.csr_der,
        });
        self.answer(issued).map_err(status_of)
    }

    async fn renew(&self, request: RenewRequest) -> Result<Certificate, Status> {
        self.record(&request.request_id);
        let leaf = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .caller_leaf
            .clone();
        let caller = self
            .credentials
            .authorize(&[rustls::pki_types::CertificateDer::from(leaf)])
            .map_err(|_| Status::unauthenticated("not permitted"))?;
        let issued = self.credentials.renew(
            &caller,
            &RenewInput {
                request_id: &request.request_id,
                csr_der: &request.csr_der,
            },
        );
        self.answer(issued).map_err(status_of)
    }
}

fn params<'a>(token: &'a [u8]) -> EnrollParams<'a> {
    EnrollParams {
        host_id: "edge-1",
        job_id: "job-1",
        token,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn enrolling_installs_an_identity_the_hub_accepts_with_a_key_that_stays_local() {
    let hub = Hub::new();
    let token = hub.grant("edge-1", "job-1");
    let dir = tempfile::tempdir().expect("dir");
    let spoke = SpokeCredentials::open(dir.path()).expect("open");

    let identity = spoke.enroll(&*hub, &params(&token)).await.expect("enroll");

    assert_eq!(identity.host_id, "edge-1");
    assert_eq!(spoke.identity().expect("read"), Some(identity.clone()));
    assert!(identity.key_pem().starts_with("-----BEGIN PRIVATE KEY-----"));
    let authorized = hub
        .credentials
        .authorize(&[rustls::pki_types::CertificateDer::from(identity.leaf_der.clone())])
        .expect("the hub accepts the installed certificate");
    assert_eq!(authorized.serial, identity.serial);
    assert!(
        !dir.path().join("pending-enroll").exists(),
        "the request is cleared once installed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_reply_is_retried_with_the_same_request_even_across_a_restart() {
    let hub = Hub::new();
    let token = hub.grant("edge-1", "job-1");
    let dir = tempfile::tempdir().expect("dir");

    hub.lose_next_reply.store(true, Ordering::SeqCst);
    let first = SpokeCredentials::open(dir.path()).expect("open");
    let error = first.enroll(&*hub, &params(&token)).await.expect_err("reply lost");
    assert!(error.is_retryable(), "{error:?}");
    assert_eq!(first.identity().expect("read"), None);
    drop(first);

    // The process died and came back: the persisted request is repeated, and
    // the token the hub already spent is not needed again.
    let restarted = SpokeCredentials::open(dir.path()).expect("reopen");
    let identity = restarted.enroll(&*hub, &params(&token)).await.expect("retry");
    let ids = hub.request_ids();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], ids[1], "the retry repeats the request id");
    assert_eq!(restarted.identity().expect("read"), Some(identity));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_discards_the_request_so_its_id_is_never_reused() {
    let hub = Hub::new();
    hub.grant("edge-1", "job-1");
    let dir = tempfile::tempdir().expect("dir");
    let spoke = SpokeCredentials::open(dir.path()).expect("open");

    for _ in 0..2 {
        let error = spoke.enroll(&*hub, &params(&[7u8; 32])).await.expect_err("refused");
        assert!(
            matches!(
                error,
                ClientError::Refused {
                    code: Code::PermissionDenied,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(!error.is_retryable());
    }
    let ids = hub.request_ids();
    assert_ne!(ids[0], ids[1], "a refused request is replaced, not repeated");
    assert_eq!(spoke.identity().expect("read"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn renewal_waits_for_the_window_then_swaps_identity_and_signals_a_reconnect() {
    let hub = Hub::new();
    let token = hub.grant("edge-1", "job-1");
    let dir = tempfile::tempdir().expect("dir");
    let spoke = SpokeCredentials::open(dir.path()).expect("open");
    let first = spoke.enroll(&*hub, &params(&token)).await.expect("enroll");
    let mut installs = spoke.subscribe();
    installs.mark_unchanged();
    hub.seen.lock().unwrap_or_else(PoisonError::into_inner).caller_leaf = first.leaf_der.clone();

    assert!(!renewal_due(first.expires_at, hub.clock.now(), Duration::ZERO));
    assert!(
        spoke
            .renew_if_due(&*hub, hub.clock.now(), Duration::ZERO)
            .await
            .expect("check")
            .is_none(),
        "a fresh certificate is not renewed"
    );

    hub.advance(21 * DAY);
    let second = spoke
        .renew_if_due(&*hub, hub.clock.now(), Duration::ZERO)
        .await
        .expect("renew")
        .expect("due");

    assert_ne!(second.serial, first.serial);
    assert_ne!(second.key_pem(), first.key_pem(), "renewal uses a new key");
    assert_eq!(spoke.identity().expect("read"), Some(second.clone()));
    assert!(
        installs.has_changed().expect("sender alive"),
        "the stream is told to reconnect"
    );
    assert!(!dir.path().join("pending-renew").exists());
    // The old certificate still opens a stream during the overlap.
    hub.credentials
        .authorize(&[rustls::pki_types::CertificateDer::from(first.leaf_der)])
        .expect("overlap");
    hub.credentials
        .authorize(&[rustls::pki_types::CertificateDer::from(second.leaf_der)])
        .expect("new certificate");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_renewal_reply_is_repeated_with_the_same_key_and_id() {
    let hub = Hub::new();
    let token = hub.grant("edge-1", "job-1");
    let dir = tempfile::tempdir().expect("dir");
    let spoke = SpokeCredentials::open(dir.path()).expect("open");
    let first = spoke.enroll(&*hub, &params(&token)).await.expect("enroll");
    hub.seen.lock().unwrap_or_else(PoisonError::into_inner).caller_leaf = first.leaf_der.clone();
    hub.advance(21 * DAY);

    hub.lose_next_reply.store(true, Ordering::SeqCst);
    assert!(spoke.renew(&*hub).await.expect_err("lost").is_retryable());
    assert_eq!(
        spoke.identity().expect("read").expect("still enrolled").serial,
        first.serial
    );
    let second = spoke.renew(&*hub).await.expect("retry");

    let ids = hub.request_ids();
    assert_eq!(ids[1], ids[2], "the renewal retry repeats the request id");
    assert_ne!(second.serial, first.serial);
}

#[tokio::test(flavor = "multi_thread")]
async fn renewing_without_an_identity_is_an_error() {
    let hub = Hub::new();
    let dir = tempfile::tempdir().expect("dir");
    let spoke = SpokeCredentials::open(dir.path()).expect("open");
    assert!(matches!(spoke.renew(&*hub).await, Err(ClientError::NotEnrolled)));
}
