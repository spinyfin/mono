//! The authority behind a real mutual-TLS listener.
//!
//! A tonic server serves the `Enrollment` service and a stand-in for the
//! stream manager's `Connect` handler that does what the design requires of
//! it: authorize the handshake's certificate chain with the authority, and
//! close the stream when the authority says the certificate stopped counting.
//! Spokes are real tonic clients holding keys they generated themselves.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use boss_host_enrollment::{EnrollmentService, HostAuthorizer, ServiceLimits, admit};
use boss_protocol_spoke::v1::enrollment_client::EnrollmentClient;
use boss_protocol_spoke::v1::spoke_hub_server::{SpokeHub, SpokeHubServer};
use boss_protocol_spoke::v1::{self, EnrollRequest, RenewRequest};
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig};
use tonic::{Code, Request, Response, Status, Streaming};

use crate::support::{Fixture, Spoke};

const STEP: Duration = Duration::from_secs(20);

// ------------------------------------------------------------ the server

/// The slice of the stream manager's `Connect` handler that this crate's
/// contract governs, built on the library's `admit` and `StreamGuard`.
struct Hub {
    authorizer: Arc<dyn HostAuthorizer>,
}

#[tonic::async_trait]
impl SpokeHub for Hub {
    type ConnectStream = ReceiverStream<Result<v1::HubFrame, Status>>;

    async fn connect(
        &self,
        request: Request<Streaming<v1::SpokeFrame>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let chain = request
            .peer_certs()
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let chain: Vec<_> = chain.iter().map(|cert| cert.clone().into_owned()).collect();
        let mut guard = admit(Arc::clone(&self.authorizer), chain)
            .await
            .map_err(|_| Status::unauthenticated("not permitted"))?;
        // The first frame must be a Hello naming the host the certificate proved.
        let mut inbound = request.into_inner();
        let hello = match inbound.message().await?.and_then(|frame| frame.body) {
            Some(v1::spoke_frame::Body::Hello(hello)) => hello,
            _ => return Err(Status::invalid_argument("the first frame must be a Hello")),
        };
        guard
            .check_hello(&hello.host_id)
            .map_err(|_| Status::permission_denied("not permitted"))?;
        let (tx, rx) = mpsc::channel(4);
        tx.send(Ok(v1::HubFrame::default())).await.expect("first frame");
        tokio::spawn(async move {
            let _ = guard.closed().await;
            let _ = tx
                .send(Err(Status::unauthenticated("credential no longer valid")))
                .await;
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// A throwaway CA for the hub's *server* certificate, which is separate trust
/// from the host CA under test.
struct ServerPki {
    issuer: CertifiedIssuer<'static, KeyPair>,
}

impl ServerPki {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.distinguished_name.push(DnType::CommonName, "test server CA");
        Self {
            issuer: CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("ca"),
        }
    }

    fn trusted(&self) -> Certificate {
        Certificate::from_pem(self.issuer.pem())
    }

    fn identity(&self) -> Identity {
        let key = KeyPair::generate().expect("key");
        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![
            SanType::DnsName(Ia5String::try_from("localhost").expect("dns")),
            SanType::IpAddress("127.0.0.1".parse().expect("ip")),
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let cert = params.signed_by(&key, &self.issuer).expect("server cert");
        Identity::from_pem(cert.pem(), key.serialize_pem())
    }
}

struct Running {
    addr: SocketAddr,
    pki: ServerPki,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    /// Stops the listener without waiting for open `Connect` streams to end;
    /// any connection still open is dropped when the test's runtime shuts
    /// down. This aborts rather than shutting down gracefully: a graceful
    /// shutdown waits for those streams, and a test may finish with one still
    /// open, which would stall here until a timeout instead of returning at
    /// once.
    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

async fn start(fx: &Fixture, limits: ServiceLimits) -> Running {
    let pki = ServerPki::new();
    // Client certificates are optional so `Enroll` can be answered before the
    // caller has one; `Renew` and `Connect` then insist on seeing one.
    let tls = ServerTlsConfig::new()
        .identity(pki.identity())
        .client_ca_root(Certificate::from_pem(fx.credentials.trust_bundle_pem()))
        .client_auth_optional(true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let authorizer: Arc<dyn HostAuthorizer> = fx.credentials.clone();
    let server = Server::builder()
        .tls_config(tls)
        .expect("server tls")
        .add_service(EnrollmentService::new(fx.credentials.clone(), limits).into_server())
        .add_service(SpokeHubServer::new(Hub { authorizer }))
        .serve_with_incoming(TcpListenerStream::new(listener));
    let task = tokio::spawn(async move {
        let _ = server.await;
    });
    Running { addr, pki, task }
}

// ------------------------------------------------------------ the spokes

async fn dial(hub: &Running, identity: Option<Identity>) -> Result<Channel, tonic::transport::Error> {
    let mut tls = ClientTlsConfig::new()
        .ca_certificate(hub.pki.trusted())
        .domain_name("localhost");
    if let Some(identity) = identity {
        tls = tls.identity(identity);
    }
    Endpoint::from_shared(format!("https://127.0.0.1:{}", hub.addr.port()))?
        .tls_config(tls)?
        .connect()
        .await
}

fn identity_of(spoke: &Spoke, cert: &v1::Certificate) -> Identity {
    Identity::from_pem(
        boss_host_enrollment::pem_encode("CERTIFICATE", &cert.leaf_der),
        spoke.key.serialize_pem(),
    )
}

fn enroll_request(spoke: &Spoke, ids: (&str, &str, &str), token: &[u8]) -> EnrollRequest {
    let (host_id, job_id, request_id) = ids;
    EnrollRequest {
        host_id: host_id.to_owned(),
        job_id: job_id.to_owned(),
        request_id: request_id.to_owned(),
        token: token.to_vec(),
        csr_der: spoke.csr(),
    }
}

/// An open `Connect` stream, with the frames the hub has sent so far.
struct Session {
    frames: Streaming<v1::HubFrame>,
    _requests: mpsc::Sender<v1::SpokeFrame>,
}

/// Opens a `Connect` stream as `identity`. A refusal is reported as the status
/// code the hub answered with (`Unavailable` if the handshake itself failed).
async fn connect(hub: &Running, identity: Identity) -> Result<Session, Code> {
    connect_claiming(hub, identity, "edge-1").await
}

/// Like [`connect`], with the host id the spoke claims in its Hello.
async fn connect_claiming(hub: &Running, identity: Identity, claimed_host_id: &str) -> Result<Session, Code> {
    let channel = dial(hub, Some(identity)).await.map_err(|_| Code::Unavailable)?;
    let (requests, rx) = mpsc::channel(2);
    requests
        .send(v1::SpokeFrame {
            body: Some(v1::spoke_frame::Body::Hello(v1::Hello {
                host_id: claimed_host_id.to_owned(),
                ..v1::Hello::default()
            })),
        })
        .await
        .expect("hello");
    let mut client = v1::spoke_hub_client::SpokeHubClient::new(channel);
    let mut frames = tokio::time::timeout(STEP, client.connect(ReceiverStream::new(rx)))
        .await
        .expect("connect timed out")
        .map_err(|status| status.code())?
        .into_inner();
    // The hub's first frame is the signal that it accepted the stream.
    tokio::time::timeout(STEP, frames.message())
        .await
        .expect("first frame timed out")
        .map_err(|status| status.code())?;
    Ok(Session {
        frames,
        _requests: requests,
    })
}

async fn closed_with(session: &mut Session) -> Code {
    match tokio::time::timeout(STEP, session.frames.message())
        .await
        .expect("stream was not closed")
    {
        Err(status) => status.code(),
        Ok(other) => panic!("expected the hub to close the stream, got {other:?}"),
    }
}

// ----------------------------------------------------------------- tests

#[tokio::test(flavor = "multi_thread")]
async fn a_spoke_enrolls_renews_and_is_cut_off_on_revocation_over_real_mtls() {
    let fx = Fixture::new();
    let hub = start(&fx, ServiceLimits::default()).await;
    let grant = fx.grant("edge-1", "job-1");

    // Enroll: server-authenticated TLS only, the spoke has no certificate yet.
    let spoke_a = Spoke::new();
    let mut anonymous = EnrollmentClient::new(dial(&hub, None).await.expect("dial"));
    let a = anonymous
        .enroll(enroll_request(
            &spoke_a,
            ("edge-1", "job-1", "req-1"),
            grant.token.as_bytes(),
        ))
        .await
        .expect("enroll")
        .into_inner();
    assert_eq!(a.host_id, "edge-1");

    // Renew needs a certificate; without one it is refused.
    let refused = anonymous
        .renew(RenewRequest {
            request_id: "renew-1".to_owned(),
            csr_der: Spoke::new().csr(),
        })
        .await
        .expect_err("no client certificate");
    assert_eq!(refused.code(), Code::Unauthenticated);

    let mut stream_a = connect(&hub, identity_of(&spoke_a, &a))
        .await
        .expect("the new certificate connects");

    // Renew over mTLS with the current certificate.
    let spoke_b = Spoke::new();
    let mut authenticated = EnrollmentClient::new(dial(&hub, Some(identity_of(&spoke_a, &a))).await.expect("dial"));
    let b = authenticated
        .renew(RenewRequest {
            request_id: "renew-1".to_owned(),
            csr_der: spoke_b.csr(),
        })
        .await
        .expect("renew")
        .into_inner();
    assert_ne!(a.serial, b.serial);

    // Both certificates connect during the overlap, and the stream that was
    // already open on the old one is not cut by the renewal.
    let mut stream_b = connect(&hub, identity_of(&spoke_b, &b))
        .await
        .expect("the replacement connects");
    let mut stream_a2 = connect(&hub, identity_of(&spoke_a, &a))
        .await
        .expect("the old certificate still connects");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), stream_a.frames.message())
            .await
            .is_err(),
        "renewal alone must not close the stream"
    );

    // Revocation closes every open stream and every way back in.
    fx.credentials.revoke("edge-1", "operator", "retired").expect("revoke");
    for stream in [&mut stream_a, &mut stream_b, &mut stream_a2] {
        assert_eq!(closed_with(stream).await, Code::Unauthenticated);
    }
    for (spoke, cert) in [(&spoke_a, &a), (&spoke_b, &b)] {
        let refused = connect(&hub, identity_of(spoke, cert))
            .await
            .err()
            .expect("revoked reconnect");
        assert_eq!(refused, Code::Unauthenticated);
    }
    let mut revoked = EnrollmentClient::new(dial(&hub, Some(identity_of(&spoke_b, &b))).await.expect("dial"));
    let error = revoked
        .renew(RenewRequest {
            request_id: "renew-2".to_owned(),
            csr_der: Spoke::new().csr(),
        })
        .await
        .expect_err("revoked renewal");
    assert_eq!(error.code(), Code::Unauthenticated);

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hello_naming_another_host_than_the_certificate_proves_is_refused() {
    let fx = Fixture::new();
    let hub = start(&fx, ServiceLimits::default()).await;
    let (spoke, cert) = fx.enrolled("edge-1");
    fx.enrolled("edge-2");
    let wire = v1::Certificate {
        leaf_der: cert.leaf_der.clone(),
        ..v1::Certificate::default()
    };
    let refused = connect_claiming(&hub, identity_of(&spoke, &wire), "edge-2")
        .await
        .err()
        .expect("a certificate for edge-1 cannot speak as edge-2");
    assert_eq!(refused, Code::PermissionDenied);
    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_enrollment_reply_is_recovered_on_the_wire_and_a_changed_key_is_not() {
    let fx = Fixture::new();
    let hub = start(&fx, ServiceLimits::default()).await;
    let grant = fx.grant("edge-1", "job-1");
    let spoke = Spoke::new();
    let mut client = EnrollmentClient::new(dial(&hub, None).await.expect("dial"));

    let first = client
        .enroll(enroll_request(
            &spoke,
            ("edge-1", "job-1", "req-1"),
            grant.token.as_bytes(),
        ))
        .await
        .expect("enroll")
        .into_inner();
    let retry = client
        .enroll(enroll_request(
            &spoke,
            ("edge-1", "job-1", "req-1"),
            grant.token.as_bytes(),
        ))
        .await
        .expect("identical retry")
        .into_inner();
    assert_eq!(retry, first);

    let stolen = client
        .enroll(enroll_request(
            &Spoke::new(),
            ("edge-1", "job-1", "req-1"),
            grant.token.as_bytes(),
        ))
        .await
        .expect_err("different key");
    assert_eq!(stolen.code(), Code::PermissionDenied);
    assert_eq!(
        stolen.message(),
        "not permitted",
        "the refusal says nothing about which check fired"
    );

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn enrollment_is_throttled_separately_from_connect() {
    let fx = Fixture::new();
    let limits = ServiceLimits {
        enroll_per_source: 2,
        ..ServiceLimits::default()
    };
    let hub = start(&fx, limits).await;
    let (spoke, cert) = fx.enrolled("edge-1");
    let wire_cert = v1::Certificate {
        leaf_der: cert.leaf_der.clone(),
        ..v1::Certificate::default()
    };
    let mut client = EnrollmentClient::new(dial(&hub, None).await.expect("dial"));

    let mut codes = Vec::new();
    for attempt in 0..3 {
        let guess = enroll_request(&Spoke::new(), ("edge-1", "job-1", &format!("req-{attempt}")), &[7; 32]);
        codes.push(client.enroll(guess).await.expect_err("a guessed token").code());
    }
    assert_eq!(
        codes,
        [Code::PermissionDenied, Code::PermissionDenied, Code::ResourceExhausted]
    );

    // The flood does not touch authenticated connections.
    connect(&hub, identity_of(&spoke, &wire_cert))
        .await
        .expect("connect is unaffected");
    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_certificate_from_another_ca_never_gets_past_the_handshake() {
    let hub_fx = Fixture::new();
    let hub = start(&hub_fx, ServiceLimits::default()).await;
    let elsewhere = Fixture::new();
    let (spoke, foreign) = elsewhere.enrolled("edge-1");
    hub_fx.grant("edge-1", "job-1");

    let wire_cert = v1::Certificate {
        leaf_der: foreign.leaf_der.clone(),
        ..v1::Certificate::default()
    };
    let outcome = connect(&hub, identity_of(&spoke, &wire_cert)).await;
    assert!(
        outcome.is_err(),
        "a certificate this hub's CA did not sign must be refused"
    );
    hub.stop().await;
}

/// Stands in for time passing: the hub's clock moves, and the open stream must
/// close without anyone revoking anything.
#[tokio::test(flavor = "multi_thread")]
async fn an_idle_stream_closes_when_its_certificate_expires() {
    let fx = Fixture::new();
    let hub = start(&fx, ServiceLimits::default()).await;
    let (spoke, cert) = fx.enrolled("edge-1");
    let wire = v1::Certificate {
        leaf_der: cert.leaf_der.clone(),
        ..v1::Certificate::default()
    };
    let mut stream = connect(&hub, identity_of(&spoke, &wire)).await.expect("connects");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), stream.frames.message())
            .await
            .is_err(),
        "a healthy idle stream stays open"
    );

    fx.clock.advance(Duration::from_secs(31 * 24 * 3600));
    assert_eq!(closed_with(&mut stream).await, Code::Unauthenticated);
    hub.stop().await;
}

/// Events about other hosts arriving faster than the poll interval must not
/// postpone the deadline of a stream they do not concern.
#[tokio::test(flavor = "multi_thread")]
async fn unrelated_credential_traffic_cannot_postpone_an_expired_streams_closure() {
    let fx = Fixture::new();
    let hub = start(&fx, ServiceLimits::default()).await;
    let (spoke, cert) = fx.enrolled("edge-1");
    let wire = v1::Certificate {
        leaf_der: cert.leaf_der.clone(),
        ..v1::Certificate::default()
    };
    let mut stream = connect(&hub, identity_of(&spoke, &wire)).await.expect("connects");
    let others: Vec<String> = (0..60).map(|i| format!("other-{i}")).collect();
    for id in &others {
        fx.enrolled(id);
    }

    let credentials = Arc::clone(&fx.credentials);
    let storm = tokio::spawn(async move {
        for id in others {
            credentials.revoke(&id, "test", "traffic").expect("revoke");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    fx.clock.advance(Duration::from_secs(31 * 24 * 3600));
    assert_eq!(closed_with(&mut stream).await, Code::Unauthenticated);
    assert!(
        !storm.is_finished(),
        "the stream closed while unrelated events were still arriving"
    );
    storm.abort();
    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_stream_on_the_old_certificate_closes_when_the_renewal_overlap_ends() {
    let fx = Fixture::new();
    let hub = start(&fx, ServiceLimits::default()).await;
    let (spoke_a, a) = fx.enrolled("edge-1");
    let wire_a = v1::Certificate {
        leaf_der: a.leaf_der.clone(),
        ..v1::Certificate::default()
    };
    let mut old_stream = connect(&hub, identity_of(&spoke_a, &wire_a)).await.expect("connects");

    let caller = fx
        .credentials
        .authorize(&crate::support::presented(&a))
        .expect("authorized");
    let spoke_b = Spoke::new();
    let b = fx
        .credentials
        .renew(
            &caller,
            &boss_host_enrollment::RenewInput {
                request_id: "renew-1",
                csr_der: &spoke_b.csr(),
            },
        )
        .expect("renew");
    let wire_b = v1::Certificate {
        leaf_der: b.leaf_der.clone(),
        ..v1::Certificate::default()
    };
    let mut new_stream = connect(&hub, identity_of(&spoke_b, &wire_b)).await.expect("connects");

    // Inside the overlap neither stream is cut.
    fx.clock.advance(Duration::from_secs(23 * 3600));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), old_stream.frames.message())
            .await
            .is_err()
    );
    // Past the overlap the old one closes on its own; the replacement stays.
    fx.clock.advance(Duration::from_secs(2 * 3600));
    assert_eq!(closed_with(&mut old_stream).await, Code::Unauthenticated);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), new_stream.frames.message())
            .await
            .is_err(),
        "the replacement certificate is unaffected"
    );
    hub.stop().await;
}
