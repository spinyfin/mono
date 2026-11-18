use super::store::Memory;
use boss_host_enrollment::{AuthorizedHost, CredentialEvent, Denial, HostAuthorizer};
use boss_protocol_spoke::{negotiate::Contracts, v1::*};
use boss_spoke_stream::{Config, StreamManager};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};
use rustls::pki_types::CertificateDer;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{
    Status, Streaming,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig},
};

pub const STEP: Duration = Duration::from_secs(10);
pub struct Authority {
    pub events: broadcast::Sender<CredentialEvent>,
    pub identity: Mutex<AuthorizedHost>,
    pub revoked: Mutex<bool>,
    leaf: Vec<u8>,
}
impl HostAuthorizer for Authority {
    fn authorize(&self, chain: &[CertificateDer<'_>]) -> Result<AuthorizedHost, Denial> {
        if !chain.first().is_some_and(|c| c.as_ref() == self.leaf) {
            return Err(Denial::Untrusted);
        }
        let identity = self.identity.lock().unwrap().clone();
        self.revalidate(&identity)
    }
    fn revalidate(&self, _: &AuthorizedHost) -> Result<AuthorizedHost, Denial> {
        if *self.revoked.lock().unwrap() {
            return Err(Denial::Revoked);
        }
        let identity = self.identity.lock().unwrap().clone();
        if identity.valid_until <= SystemTime::now() {
            return Err(Denial::CertificateExpired);
        }
        Ok(identity)
    }
    fn subscribe(&self) -> broadcast::Receiver<CredentialEvent> {
        self.events.subscribe()
    }
}

#[derive(bon::Builder)]
pub struct Fixture {
    pub manager: StreamManager,
    pub store: Arc<Memory>,
    pub auth: Arc<Authority>,
    pub channel: Channel,
    pub anonymous: Channel,
    /// The authenticated client's endpoint, for dialing the hub from a real spoke session.
    pub dialer: Endpoint,
    server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    pub async fn new() -> Self {
        Self::with_contracts(None).await
    }
    /// A hub offering `contracts` instead of the first-release set.
    pub async fn with_contracts(contracts: impl Into<Option<Contracts>>) -> Self {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_identity = Identity::from_pem(params.signed_by(&key, &ca).unwrap().pem(), key.serialize_pem());
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![SanType::URI("urn:boss:host:host".try_into().unwrap())];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let leaf = params.signed_by(&key, &ca).unwrap();
        let identity = Identity::from_pem(leaf.pem(), key.serialize_pem());
        let auth = Arc::new(Authority {
            events: broadcast::channel(4).0,
            identity: Mutex::new(AuthorizedHost {
                host_id: "host".into(),
                serial: "serial".into(),
                valid_until: SystemTime::now() + STEP * 10,
            }),
            revoked: Mutex::new(false),
            leaf: leaf.der().to_vec(),
        });
        let store = Arc::new(Memory::default());
        let manager = StreamManager::new(
            Config::builder()
                .hub_id("hub")
                .authority_generation("authority")
                .build_fingerprint("test")
                .max_hosts(1)
                .max_pending(1)
                .hello_timeout(Duration::from_millis(300))
                .pulse_interval(Duration::from_secs(2))
                .pulse_timeout(Duration::from_millis(500))
                .maybe_contracts(contracts.into())
                .build(),
            auth.clone(),
            store.clone(),
            store.clone(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tls = ServerTlsConfig::new()
            .identity(server_identity)
            .client_ca_root(Certificate::from_pem(ca.pem()))
            .client_auth_optional(true);
        let service = manager.service();
        let server = tokio::spawn(async move {
            Server::builder()
                .tls_config(tls)
                .unwrap()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let endpoint = Endpoint::from_shared(format!("https://{addr}")).unwrap();
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(ca.pem()))
            .domain_name("localhost");
        let dialer = endpoint.clone().tls_config(tls.clone().identity(identity)).unwrap();
        let channel = dialer.connect().await.unwrap();
        let anonymous = endpoint.tls_config(tls).unwrap().connect().await.unwrap();
        Self::builder()
            .manager(manager)
            .store(store)
            .auth(auth)
            .channel(channel)
            .anonymous(anonymous)
            .dialer(dialer)
            .server(server)
            .build()
    }
    pub async fn connect(&self, hello: Hello) -> Result<Peer, Box<Status>> {
        connect(self.channel.clone(), hello).await
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

pub async fn connect(channel: Channel, hello: Hello) -> Result<Peer, Box<Status>> {
    let (tx, rx) = mpsc::channel(8);
    tx.send(SpokeFrame {
        body: Some(spoke_frame::Body::Hello(hello)),
    })
    .await
    .unwrap();
    let rx = tokio::time::timeout(
        STEP,
        spoke_hub_client::SpokeHubClient::new(channel).connect(ReceiverStream::new(rx)),
    )
    .await
    .unwrap()?
    .into_inner();
    Ok(Peer { tx, rx, pulses: 0 })
}
pub struct Peer {
    pub tx: mpsc::Sender<SpokeFrame>,
    pub rx: Streaming<HubFrame>,
    pub pulses: usize,
}
impl Peer {
    pub async fn send(&self, body: spoke_frame::Body) {
        self.tx.send(SpokeFrame { body: Some(body) }).await.unwrap();
    }
    pub async fn next(&mut self) -> Option<hub_frame::Body> {
        loop {
            let message = tokio::time::timeout(STEP, self.rx.message())
                .await
                .expect("bounded stream read");
            let Ok(Some(message)) = message else {
                return None;
            };
            if let Some(hub_frame::Body::Pulse(pulse)) = message.body {
                self.pulses += 1;
                // The peer may already be fenced while a previously delivered
                // pulse is still buffered in gRPC. Keep reading to observe EOF.
                let _ = self
                    .tx
                    .send(SpokeFrame {
                        body: Some(spoke_frame::Body::PulseReply(pulse)),
                    })
                    .await;
            } else {
                return message.body;
            }
        }
    }
    /// Read until the stream ends; the terminal gRPC status, `None` when it was OK.
    pub async fn closed(&mut self) -> Option<Status> {
        loop {
            let message = tokio::time::timeout(STEP, self.rx.message())
                .await
                .expect("bounded stream read");
            match message {
                Ok(Some(frame)) => {
                    if let Some(hub_frame::Body::Pulse(pulse)) = frame.body {
                        let _ = self
                            .tx
                            .send(SpokeFrame {
                                body: Some(spoke_frame::Body::PulseReply(pulse)),
                            })
                            .await;
                    }
                }
                Ok(None) => return None,
                Err(status) => return Some(status),
            }
        }
    }
    /// Block until the hub's routine pulse has been seen and answered.
    pub async fn settle_pulse(&mut self) {
        while self.pulses == 0 {
            let message = tokio::time::timeout(STEP, self.rx.message())
                .await
                .expect("bounded stream read")
                .expect("stream open")
                .expect("stream open");
            if let Some(hub_frame::Body::Pulse(pulse)) = message.body {
                self.pulses += 1;
                let _ = self
                    .tx
                    .send(SpokeFrame {
                        body: Some(spoke_frame::Body::PulseReply(pulse)),
                    })
                    .await;
            }
        }
    }
    pub async fn welcome(&mut self) -> Welcome {
        match self.next().await {
            Some(hub_frame::Body::Welcome(w)) => w,
            other => panic!("expected welcome: {other:?}"),
        }
    }
}
pub fn hello() -> Hello {
    Hello {
        host_id: "host".into(),
        journal_id: "journal".into(),
        boot_id: "boot".into(),
        min_epoch: 1,
        max_epoch: 1,
        features: Contracts::first_release().features,
        instance_nonce: vec![1; 16],
        first_retained: 1,
        ..Default::default()
    }
}
