use reqwest::StatusCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RobinhoodClientError {
    #[error("invalid base url: {0}")]
    InvalidBaseUrl(#[from] url::ParseError),

    #[error("failed to build HTTP client: {0}")]
    HttpClient(#[from] reqwest::Error),

    #[error("invalid endpoint url: {0}")]
    InvalidEndpointUrl(url::ParseError),

    #[error("unexpected response status: {0}")]
    UnexpectedStatus(StatusCode),

    #[error("failed to parse response body: {0}")]
    ResponseParse(#[from] serde_json::Error),
}
