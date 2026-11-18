//! Optional-client-auth mTLS listener shared by enrollment and Connect.

use std::sync::Arc;

use boss_host_enrollment::{EnrollmentService, HostCredentials, ServiceLimits};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

use crate::HubStream;

/// TLS config for a listener that answers `Enroll` before the spoke has a
/// client certificate, then requires one for `Renew` and `Connect`.
pub fn server_tls(
    identity: Identity,
    credentials: &HostCredentials,
) -> Result<ServerTlsConfig, tonic::transport::Error> {
    Ok(ServerTlsConfig::new()
        .identity(identity)
        .client_ca_root(Certificate::from_pem(credentials.trust_bundle_pem()))
        .client_auth_optional(true))
}

/// The enrollment service and stream manager mounted together.
pub struct HubListener {
    stream: HubStream,
    limits: ServiceLimits,
}

impl HubListener {
    pub fn from_stream(stream: HubStream) -> Self {
        Self {
            stream,
            limits: ServiceLimits::default(),
        }
    }

    pub fn with_limits(mut self, limits: ServiceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Serve until the listener is dropped. The server certificate is not the
    /// host CA; the caller supplies it.
    pub async fn serve(
        self,
        incoming: tokio::net::TcpListener,
        identity: Identity,
    ) -> Result<(), tonic::transport::Error> {
        let tls = server_tls(identity, self.stream.credentials())?;
        let enrollment = EnrollmentService::new(Arc::clone(self.stream.credentials()), self.limits).into_server();
        let connect = self.stream.manager().service();
        Server::builder()
            .tls_config(tls)?
            .add_service(enrollment)
            .add_service(connect)
            .serve_with_incoming(TcpListenerStream::new(incoming))
            .await
    }
}
