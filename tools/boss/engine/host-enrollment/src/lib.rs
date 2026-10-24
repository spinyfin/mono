//! The hub's side of a spoke's identity: who may enroll, the certificates they
//! are issued, and the check that decides whether a presented certificate still
//! speaks for its host.
//!
//! A spoke is a worker host's daemon that dials the hub over mutual TLS. This
//! crate is the trust boundary under that connection (hub-and-spoke design,
//! "Enrollment, service user and credential lifecycle"):
//!
//! - [`CertificateAuthority`] holds the CA key in an owner-only file and signs
//!   one kind of certificate: a client-only certificate whose single SAN is
//!   `urn:boss:host:<id>`.
//! - [`HostCredentials`] runs the lifecycle. A host is authorized, given a
//!   single-use token bound to a provisioning job, enrolls with a CSR, renews
//!   before expiry with an overlap, and can be revoked. Enrollment is
//!   idempotent for a lost reply, never for a replay.
//! - [`HostAuthorizer`] is what the stream manager calls: `authorize` a
//!   handshake's certificate chain, `revalidate` an open stream, and
//!   `subscribe` to the [`CredentialEvent`]s that end one.
//! - [`admit`] and [`StreamGuard`] are the stream manager's use of that: admit a
//!   connection (subscribe, then authorize off the runtime), check its hello,
//!   and learn when it must close.
//! - [`EnrollmentService`] serves the `Enroll` and `Renew` RPCs.
//!
//! Enrollment gives a host an identity, not a place to run work: a host is
//! scheduled only after it connects and passes its preflight, which this crate
//! knows nothing about. The crate depends on the wire crate and nothing in the
//! engine, so the work database never holds the CA key or this state.

mod authority;
mod ca;
mod cert;
mod clock;
mod credentials;
mod csr;
mod enroll;
mod error;
mod limiter;
mod policy;
mod renew;
mod service;
mod session;
mod store;
mod token;

pub use authority::{AuthorizedHost, CredentialEvent, HostAuthorizer};
pub use ca::{CaError, CertificateAuthority};
pub use clock::{Clock, SystemClock};
pub use credentials::{CertificateSummary, CredentialStatus, EnrollmentGrant, HostCredentials, IssuedCertificate};
pub use csr::MAX_CSR_BYTES;
pub use enroll::EnrollInput;
pub use error::{CredentialError, Denial};
pub use limiter::AttemptLimiter;
pub use policy::Policy;
pub use renew::RenewInput;
pub use service::{EnrollmentService, ServiceLimits};
pub use session::{CloseReason, StreamGuard, admit};
pub use store::HostState;
pub use token::{EnrollmentToken, TOKEN_BYTES};
