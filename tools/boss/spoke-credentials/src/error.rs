use tonic::Code;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("credential files: {0}")]
    Io(#[from] std::io::Error),
    #[error("a credential file is unreadable; re-provision the host")]
    Corrupt,
    #[error("key or request generation failed: {0}")]
    Generate(String),
    #[error("renewal needs an enrolled identity")]
    NotEnrolled,
    /// The hub answered with something that cannot be installed.
    #[error("the hub's certificate was unusable: {0}")]
    BadCertificate(&'static str),
    /// The hub refused the request. The request and key were discarded, so the
    /// next attempt starts a new request; `Enroll` also needs a new token.
    #[error("the hub refused the request ({code:?}): {message}")]
    Refused { code: Code, message: String },
    /// The request did not get a usable answer. It is kept, and the next
    /// attempt repeats it with the same key and request id.
    #[error("the hub could not be reached ({code:?}): {message}")]
    Unavailable { code: Code, message: String },
}

impl ClientError {
    /// Whether the same call, repeated, can succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

/// Whether a failure leaves the outcome of the request unknown (or the hub too
/// busy to say), so the same request must be repeated rather than replaced.
pub(crate) fn keeps_request(code: Code) -> bool {
    matches!(
        code,
        Code::Unavailable
            | Code::DeadlineExceeded
            | Code::Cancelled
            | Code::Unknown
            | Code::Internal
            | Code::Aborted
            | Code::ResourceExhausted
    )
}
