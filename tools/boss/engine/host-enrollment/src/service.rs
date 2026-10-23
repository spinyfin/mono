//! The gRPC `Enrollment` service: `Enroll` and `Renew` on top of
//! [`HostCredentials`].
//!
//! The service is the restricted surface the design asks for. `Enroll` is
//! answered on a server-authenticated connection without a client certificate,
//! so it is rate limited on its own and every refusal looks the same to the
//! caller. `Renew` additionally requires a client certificate the authority
//! accepts. The listener that serves this must request client certificates
//! optionally (`ServerTlsConfig::client_auth_optional(true)`) and be built
//! with the CA's [`trust_bundle_pem`](HostCredentials::trust_bundle_pem), or
//! `Renew` never sees one.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use boss_protocol_spoke::v1::enrollment_server::{Enrollment, EnrollmentServer};
use boss_protocol_spoke::v1::{self, Certificate, EnrollRequest, RenewRequest};
use tonic::{Request, Response, Status};

use crate::authority::HostAuthorizer;
use crate::credentials::{HostCredentials, IssuedCertificate};
use crate::enroll::EnrollInput;
use crate::error::CredentialError;
use crate::limiter::AttemptLimiter;
use crate::renew::RenewInput;

/// Requests carry a CSR of a few hundred bytes and a 32-byte token.
const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// Attempt limits for the enrollment endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceLimits {
    pub window: Duration,
    /// `Enroll` attempts per source address per window.
    pub enroll_per_source: u32,
    /// `Enroll` attempts from all sources per window.
    pub enroll_overall: u32,
    /// `Renew` attempts per host per window.
    pub renew_per_host: u32,
}

impl Default for ServiceLimits {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(60),
            enroll_per_source: 10,
            enroll_overall: 60,
            renew_per_host: 6,
        }
    }
}

pub struct EnrollmentService {
    credentials: Arc<HostCredentials>,
    enroll_limiter: AttemptLimiter<Option<IpAddr>>,
    renew_limiter: AttemptLimiter<String>,
}

impl EnrollmentService {
    pub fn new(credentials: Arc<HostCredentials>, limits: ServiceLimits) -> Self {
        Self {
            credentials,
            enroll_limiter: AttemptLimiter::new(limits.window, limits.enroll_per_source, limits.enroll_overall),
            renew_limiter: AttemptLimiter::new(limits.window, limits.renew_per_host, u32::MAX),
        }
    }

    /// The tonic server wrapper, with request size bounded.
    pub fn into_server(self) -> EnrollmentServer<Self> {
        EnrollmentServer::new(self).max_decoding_message_size(MAX_REQUEST_BYTES)
    }
}

#[tonic::async_trait]
impl Enrollment for EnrollmentService {
    async fn enroll(&self, request: Request<EnrollRequest>) -> Result<Response<Certificate>, Status> {
        let source = request.remote_addr().map(|addr| addr.ip());
        if !self.enroll_limiter.admit(&source, self.credentials.clock().now()) {
            return Err(Status::resource_exhausted("too many enrollment attempts"));
        }
        let message = request.into_inner();
        let credentials = Arc::clone(&self.credentials);
        let issued = blocking(move || {
            credentials.enroll(&EnrollInput {
                host_id: &message.host_id,
                job_id: &message.job_id,
                request_id: &message.request_id,
                token: &message.token,
                csr_der: &message.csr_der,
            })
        })
        .await
        .map_err(status_of)?;
        Ok(Response::new(to_wire(issued)))
    }

    async fn renew(&self, request: Request<RenewRequest>) -> Result<Response<Certificate>, Status> {
        let chain = request
            .peer_certs()
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let credentials = Arc::clone(&self.credentials);
        let caller = tokio::task::spawn_blocking(move || credentials.authorize(&chain))
            .await
            .map_err(|_| Status::internal("internal error"))?
            .map_err(|denial| {
                tracing::warn!(reason = denial.code(), "renewal peer not authorized");
                Status::unauthenticated("not permitted")
            })?;
        if !self
            .renew_limiter
            .admit(&caller.host_id, self.credentials.clock().now())
        {
            return Err(Status::resource_exhausted("too many renewal attempts"));
        }
        let message = request.into_inner();
        let credentials = Arc::clone(&self.credentials);
        let issued = blocking(move || {
            credentials.renew(
                &caller,
                &RenewInput {
                    request_id: &message.request_id,
                    csr_der: &message.csr_der,
                },
            )
        })
        .await
        .map_err(status_of)?;
        Ok(Response::new(to_wire(issued)))
    }
}

/// Runs a blocking lifecycle call off the async threads. The error stays a
/// [`CredentialError`] so only the trait methods, whose signatures tonic fixes,
/// deal in the (large) `Status`.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, CredentialError> + Send + 'static,
) -> Result<T, CredentialError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| CredentialError::Internal(format!("lifecycle task failed: {error}")))?
}

/// Maps a lifecycle error to a status. A refusal is one generic status
/// whatever check fired, and internal detail stays in the log.
pub(crate) fn status_of(error: CredentialError) -> Status {
    match error {
        CredentialError::Invalid(message) => Status::invalid_argument(message),
        CredentialError::Denied(_) | CredentialError::UnknownHost => Status::permission_denied("not permitted"),
        CredentialError::Internal(detail) => {
            tracing::error!(%detail, "host credential operation failed");
            Status::internal("internal error")
        }
    }
}

fn to_wire(issued: IssuedCertificate) -> v1::Certificate {
    v1::Certificate {
        leaf_der: issued.leaf_der,
        chain_der: issued.chain_der,
        expires_unix_ms: issued.expires_unix_ms,
        host_id: issued.host_id,
        serial: issued.serial,
    }
}
