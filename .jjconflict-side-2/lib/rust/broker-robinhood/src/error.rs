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
    ResponseBodyParse(#[from] serde_json::Error),

    #[error("failed to serialize request query parameters: {0}")]
    QuerySerialize(#[from] serde_urlencoded::ser::Error),

    #[error("invalid position quantity for symbol `{symbol}`: {source}")]
    InvalidPositionQuantity {
        symbol: String,
        source: std::num::ParseFloatError,
    },

    #[error("at least one account number is required")]
    MissingAccountNumbers,
}
