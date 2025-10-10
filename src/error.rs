use thiserror::Error;

#[derive(Debug, Error)]
pub enum RobinhoodClientError {
    #[error("invalid base url: {0}")]
    InvalidBaseUrl(#[from] url::ParseError),

    #[error("failed to build HTTP client: {0}")]
    HttpClient(#[from] reqwest::Error),
}
