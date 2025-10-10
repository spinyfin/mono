use reqwest::Client;
use url::Url;

use crate::error::RobinhoodClientError;

#[derive(Clone, Debug)]
pub struct RobinhoodClient {
    http: Client,
    base_url: Url,
}

impl RobinhoodClient {
    pub fn new(base_url: &str) -> Result<Self, RobinhoodClientError> {
        let http = Client::builder().build()?;
        Self::with_http_client(http, base_url)
    }

    pub fn with_http_client(http: Client, base_url: &str) -> Result<Self, RobinhoodClientError> {
        let base_url = Url::parse(base_url)?;
        Ok(Self { http, base_url })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub fn http(&self) -> &Client {
        &self.http
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_initializes_with_default_http_client() {
        let client = RobinhoodClient::new("https://api.robinhood.com")
            .expect("expected client to be constructed");

        assert_eq!(client.base_url().as_str(), "https://api.robinhood.com/");
    }

    #[test]
    fn new_with_http_client_rejects_invalid_url() {
        let http = Client::new();

        let err =
            RobinhoodClient::with_http_client(http, "not a url").expect_err("expected invalid url");

        match err {
            RobinhoodClientError::InvalidBaseUrl(_) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
