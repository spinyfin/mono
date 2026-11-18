//! Why an operation was refused, and the errors the lifecycle returns.

use thiserror::Error;

/// Why a credential was refused. The code is for logs and the audit trail
/// only: a peer that is told which check failed learns what to probe, so every
/// transport maps all of these to one generic denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    // Enrollment.
    UnknownToken,
    HostMismatch,
    JobMismatch,
    TokenExpired,
    TokenReplayed,
    TokenReplaced,
    JobCancelled,
    KeyChanged,
    RetryWindowElapsed,
    // Renewal.
    RequestReused,
    KeyReused,
    NotCurrentCertificate,
    // Authorization of a presented certificate.
    NoCertificate,
    Untrusted,
    CertificateExpired,
    CertificateNotYetValid,
    WrongIdentity,
    UnknownHost,
    NotEnrolled,
    Revoked,
    Superseded,
    OverlapElapsed,
    /// The credential store could not be read. Denied, because an
    /// authorization that cannot be checked must not be assumed.
    StoreUnavailable,
}

impl Denial {
    /// A short stable name for logs and metrics.
    pub fn code(self) -> &'static str {
        match self {
            Self::UnknownToken => "unknown_token",
            Self::HostMismatch => "host_mismatch",
            Self::JobMismatch => "job_mismatch",
            Self::TokenExpired => "token_expired",
            Self::TokenReplayed => "token_replayed",
            Self::TokenReplaced => "token_replaced",
            Self::JobCancelled => "job_cancelled",
            Self::KeyChanged => "key_changed",
            Self::RetryWindowElapsed => "retry_window_elapsed",
            Self::RequestReused => "request_reused",
            Self::KeyReused => "key_reused",
            Self::NotCurrentCertificate => "not_current_certificate",
            Self::NoCertificate => "no_certificate",
            Self::Untrusted => "untrusted",
            Self::CertificateExpired => "certificate_expired",
            Self::CertificateNotYetValid => "certificate_not_yet_valid",
            Self::WrongIdentity => "wrong_identity",
            Self::UnknownHost => "unknown_host",
            Self::NotEnrolled => "not_enrolled",
            Self::Revoked => "revoked",
            Self::Superseded => "superseded",
            Self::OverlapElapsed => "overlap_elapsed",
            Self::StoreUnavailable => "store_unavailable",
        }
    }
}

impl std::fmt::Display for Denial {
    /// Deliberately uninformative; use [`Denial::code`] for diagnostics.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not permitted")
    }
}

impl std::error::Error for Denial {}

/// An error from a lifecycle operation.
#[derive(Debug, Error)]
pub enum CredentialError {
    /// The request is malformed: a bad id, token length or CSR.
    #[error("invalid request: {0}")]
    Invalid(String),
    /// The request was well formed and refused.
    #[error(transparent)]
    Denied(#[from] Denial),
    /// An administrative call named a host that was never authorized.
    #[error("host is not authorized")]
    UnknownHost,
    /// Storage, key material or the CA failed. Detail is for the log; it is
    /// never sent to a peer.
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<rusqlite::Error> for CredentialError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Internal(format!("credential store: {error}"))
    }
}

impl From<crate::ca::CaError> for CredentialError {
    fn from(error: crate::ca::CaError) -> Self {
        Self::Internal(error.to_string())
    }
}
