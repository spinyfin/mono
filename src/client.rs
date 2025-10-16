use std::collections::{HashMap, HashSet};

use db::Database;
use reqwest::{Client, StatusCode};
use serde::de::Error as _;
use serde::{Deserialize, de::DeserializeOwned};
use url::Url;
use uuid::Uuid;

use crate::auth::{
    AUTH_ENDPOINT, AuthChallenge, FinalTokenResponse, IDENTITY_WORKFLOW_ENDPOINT_PREFIX,
    TokenRequest, TokenResponse, WorkflowEntryPointRequest, WorkflowProceedRequest, WorkflowRoute,
    WorkflowRouteResponse,
};
use crate::error::RobinhoodClientError;

#[derive(Clone, Debug)]
pub struct RobinhoodClient {
    http: Client,
    base_url: Url,
    identity_base_url: Url,
}

const DEFAULT_IDENTITY_BASE_URL: &str = "https://identi.robinhood.com/";
const INSTRUMENT_LOOKUP_CHUNK: usize = 50;

impl RobinhoodClient {
    pub fn new(base_url: &str) -> Result<Self, RobinhoodClientError> {
        let http = Client::builder().build()?;
        Self::with_http_client(http, base_url)
    }

    pub fn with_http_client(http: Client, base_url: &str) -> Result<Self, RobinhoodClientError> {
        Self::with_http_client_and_identity_base(http, base_url, DEFAULT_IDENTITY_BASE_URL)
    }

    pub fn with_base_urls(
        base_url: &str,
        identity_base_url: &str,
    ) -> Result<Self, RobinhoodClientError> {
        let http = Client::builder().build()?;
        Self::with_http_client_and_identity_base(http, base_url, identity_base_url)
    }

    pub fn with_http_client_and_identity_base(
        http: Client,
        base_url: &str,
        identity_base_url: &str,
    ) -> Result<Self, RobinhoodClientError> {
        let base_url = Url::parse(base_url)?;
        let identity_base_url = Url::parse(identity_base_url)?;
        Ok(Self {
            http,
            base_url,
            identity_base_url,
        })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub fn http(&self) -> &Client {
        &self.http
    }

    pub async fn initiate_login(
        &self,
        username: &str,
        password: &str,
    ) -> Result<AuthChallenge, RobinhoodClientError> {
        let device_token = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        let device_token_str = device_token.to_string();
        let request_id_str = request_id.to_string();

        let payload = TokenRequest::new(username, password, &device_token_str, &request_id_str);

        let token_response: TokenResponse = self.request_token(&payload, true).await?;

        Ok(AuthChallenge::new(
            device_token,
            request_id,
            token_response.verification_workflow,
        ))
    }

    pub async fn fetch_verification_result(
        &self,
        workflow_id: &str,
    ) -> Result<bool, RobinhoodClientError> {
        let path = format!("verification_workflows/polaris_migrated/{workflow_id}/");
        let url = self
            .identity_base_url
            .join(&path)
            .map_err(RobinhoodClientError::InvalidEndpointUrl)?;

        let response = self.http.get(url).send().await?;

        if response.status() != StatusCode::OK {
            return Err(RobinhoodClientError::UnexpectedStatus(response.status()));
        }

        let body = response.bytes().await?;
        let verification: VerificationResultResponse = serde_json::from_slice(&body)?;
        Ok(verification.result)
    }

    pub async fn advance_workflow_entry_point(
        &self,
        workflow_id: &str,
    ) -> Result<WorkflowRoute, RobinhoodClientError> {
        let path = format!("{IDENTITY_WORKFLOW_ENDPOINT_PREFIX}{workflow_id}/");
        let url = self
            .identity_base_url
            .join(&path)
            .map_err(RobinhoodClientError::InvalidEndpointUrl)?;

        let payload = WorkflowEntryPointRequest::new(workflow_id);

        let response = self.http.patch(url).json(&payload).send().await?;

        if response.status() != StatusCode::OK {
            return Err(RobinhoodClientError::UnexpectedStatus(response.status()));
        }

        let body = response.bytes().await?;
        let route: WorkflowRouteResponse = serde_json::from_slice(&body)?;
        Ok(route.route)
    }

    pub async fn fetch_accounts(
        &self,
        access_token: &str,
    ) -> Result<Vec<RobinhoodAccount>, RobinhoodClientError> {
        let mut url = self
            .base_url
            .join("accounts/")
            .map_err(RobinhoodClientError::InvalidEndpointUrl)?;
        url.set_query(Some(
            "default_to_all_accounts=true&include_managed=true&include_multiple_individual=true&is_default=false",
        ));

        let response = self.http.get(url).bearer_auth(access_token).send().await?;

        if response.status() != StatusCode::OK {
            return Err(RobinhoodClientError::UnexpectedStatus(response.status()));
        }

        let body = response.bytes().await?;
        let payload: RobinhoodAccountsResponse = serde_json::from_slice(&body)?;

        Ok(payload
            .results
            .into_iter()
            .map(RobinhoodAccount::from)
            .collect())
    }

    pub async fn fetch_positions(
        &self,
        access_token: &str,
        account_number: &str,
    ) -> Result<Vec<RobinhoodPosition>, RobinhoodClientError> {
        let mut url = self
            .base_url
            .join("positions/")
            .map_err(RobinhoodClientError::InvalidEndpointUrl)?;
        url.set_query(Some(&format!(
            "account_number={account_number}&nonzero=true"
        )));

        let mut positions = Vec::new();
        let mut next_url: Option<Url> = Some(url);

        while let Some(current_url) = next_url {
            let response = self
                .http
                .get(current_url.clone())
                .bearer_auth(access_token)
                .send()
                .await?;

            if response.status() != StatusCode::OK {
                return Err(RobinhoodClientError::UnexpectedStatus(response.status()));
            }

            let body = response.bytes().await?;
            let page: RobinhoodPositionsResponse = serde_json::from_slice(&body)?;

            for entry in page.results {
                let symbol = entry.symbol.trim().to_uppercase();
                if symbol.is_empty() {
                    continue;
                }

                let quantity_text = entry.quantity.trim();
                if quantity_text.is_empty() {
                    continue;
                }

                let quantity = quantity_text.parse::<f64>().map_err(|error| {
                    RobinhoodClientError::ResponseParse(serde_json::Error::custom(format!(
                        "invalid quantity for symbol {symbol}: {error}"
                    )))
                })?;

                positions.push(RobinhoodPosition { symbol, quantity });
            }

            next_url = match page.next {
                Some(next) => {
                    let trimmed = next.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        match Url::parse(trimmed) {
                            Ok(url) => Some(url),
                            Err(_) => match self.base_url.join(trimmed) {
                                Ok(joined) => Some(joined),
                                Err(err) => {
                                    return Err(RobinhoodClientError::InvalidEndpointUrl(err));
                                }
                            },
                        }
                    }
                }
                None => None,
            };
        }

        Ok(positions)
    }

    pub async fn fetch_orders_page(
        &self,
        access_token: &str,
        account_number: &str,
        page_size: usize,
        cursor: Option<&str>,
    ) -> Result<RobinhoodOrdersPage, RobinhoodClientError> {
        let mut url = self
            .base_url
            .join("orders/")
            .map_err(RobinhoodClientError::InvalidEndpointUrl)?;

        let mut params = vec![
            ("account_numbers", account_number.to_owned()),
            ("include_managed", "true".to_owned()),
            ("is_closed", "true".to_owned()),
            ("page_size", page_size.to_string()),
        ];

        if let Some(cursor_value) = cursor.filter(|value| !value.is_empty()) {
            params.push(("cursor", cursor_value.to_owned()));
        }

        let query = serde_urlencoded::to_string(&params).map_err(|error| {
            RobinhoodClientError::ResponseParse(serde_json::Error::custom(error.to_string()))
        })?;
        url.set_query(Some(&query));

        let response = self.http.get(url).bearer_auth(access_token).send().await?;

        if response.status() != StatusCode::OK {
            return Err(RobinhoodClientError::UnexpectedStatus(response.status()));
        }

        let body = response.bytes().await?;
        let page = serde_json::from_slice(&body)?;
        Ok(page)
    }

    pub async fn get_symbols(
        &self,
        access_token: &str,
        instrument_ids: &[String],
        db: &Database,
    ) -> Result<HashMap<String, String>, RobinhoodClientError> {
        let mut symbols = HashMap::new();
        if instrument_ids.is_empty() {
            return Ok(symbols);
        }

        let mut seen = HashSet::new();
        let mut missing = Vec::new();

        for instrument_id in instrument_ids {
            let trimmed_id = instrument_id.trim();
            if trimmed_id.is_empty() || !seen.insert(trimmed_id.to_owned()) {
                continue;
            }

            match db.get_robinhood_instrument_symbol(trimmed_id)? {
                Some(cached) => {
                    let normalized = cached.trim().to_uppercase();
                    if normalized.is_empty() {
                        missing.push(trimmed_id.to_owned());
                    } else {
                        symbols.insert(trimmed_id.to_owned(), normalized);
                    }
                }
                None => missing.push(trimmed_id.to_owned()),
            }
        }

        let mut fetched_entries = HashMap::new();
        for chunk in missing.chunks(INSTRUMENT_LOOKUP_CHUNK) {
            let entries = self.fetch_instrument_chunk(access_token, chunk).await?;

            for entry in entries {
                let entry_id = entry.id.trim();
                let symbol_text = entry.symbol.trim();
                if entry_id.is_empty() || symbol_text.is_empty() {
                    continue;
                }

                let normalized_symbol = symbol_text.to_uppercase();
                symbols.insert(entry_id.to_owned(), normalized_symbol.clone());
                fetched_entries.insert(entry_id.to_owned(), normalized_symbol);
            }
        }

        if !fetched_entries.is_empty() {
            db.set_robinhood_instrument_symbols(&fetched_entries)?;
        }

        Ok(symbols)
    }

    async fn fetch_instrument_chunk(
        &self,
        access_token: &str,
        instrument_ids: &[String],
    ) -> Result<Vec<RobinhoodInstrumentEntry>, RobinhoodClientError> {
        if instrument_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut url = self
            .base_url
            .join("instruments/")
            .map_err(RobinhoodClientError::InvalidEndpointUrl)?;
        url.set_query(Some(&format!(
            "ids={}&active_instruments_only=false",
            instrument_ids.join(",")
        )));

        let response = self.http.get(url).bearer_auth(access_token).send().await?;

        if response.status() != StatusCode::OK {
            return Err(RobinhoodClientError::UnexpectedStatus(response.status()));
        }

        let body = response.bytes().await?;
        let payload: RobinhoodInstrumentsResponse = serde_json::from_slice(&body)?;
        Ok(payload.results)
    }

    pub async fn fetch_push_prompt_status(
        &self,
        challenge_id: &str,
    ) -> Result<String, RobinhoodClientError> {
        let path = format!("push/{challenge_id}/get_prompts_status/");
        let url = self
            .base_url
            .join(&path)
            .map_err(RobinhoodClientError::InvalidEndpointUrl)?;

        let response = self.http.get(url).send().await?;

        if response.status() != StatusCode::OK {
            return Err(RobinhoodClientError::UnexpectedStatus(response.status()));
        }

        let body = response.bytes().await?;
        let status: PushPromptStatusResponse = serde_json::from_slice(&body)?;
        Ok(status.challenge_status)
    }

    pub async fn finalize_login(
        &self,
        username: &str,
        password: &str,
        device_token: &Uuid,
        request_id: &Uuid,
    ) -> Result<FinalTokenResponse, RobinhoodClientError> {
        let device_token_str = device_token.to_string();
        let request_id_str = request_id.to_string();

        let payload = TokenRequest::new(username, password, &device_token_str, &request_id_str);

        self.request_token(&payload, false).await
    }

    pub async fn complete_device_approval(
        &self,
        workflow_id: &str,
    ) -> Result<WorkflowRoute, RobinhoodClientError> {
        let path = format!("{IDENTITY_WORKFLOW_ENDPOINT_PREFIX}{workflow_id}/");
        let url = self
            .identity_base_url
            .join(&path)
            .map_err(RobinhoodClientError::InvalidEndpointUrl)?;

        let payload = WorkflowProceedRequest::new(workflow_id);

        let response = self.http.patch(url).json(&payload).send().await?;

        if response.status() != StatusCode::OK {
            return Err(RobinhoodClientError::UnexpectedStatus(response.status()));
        }

        let body = response.bytes().await?;
        let route: WorkflowRouteResponse = serde_json::from_slice(&body)?;
        Ok(route.route)
    }

    async fn request_token<T>(
        &self,
        payload: &TokenRequest<'_>,
        allow_forbidden: bool,
    ) -> Result<T, RobinhoodClientError>
    where
        T: DeserializeOwned,
    {
        let url = self
            .base_url
            .join(AUTH_ENDPOINT)
            .map_err(RobinhoodClientError::InvalidEndpointUrl)?;

        let response = self.http.post(url).json(payload).send().await?;

        let status = response.status();
        if !(status.is_success() || allow_forbidden && status == StatusCode::FORBIDDEN) {
            return Err(RobinhoodClientError::UnexpectedStatus(status));
        }

        let body = response.bytes().await?;
        let token = serde_json::from_slice(body.as_ref())?;
        Ok(token)
    }
}

#[derive(Debug, Deserialize)]
struct RobinhoodAccountsResponse {
    results: Vec<RobinhoodAccountEntry>,
}

#[derive(Debug, Deserialize)]
struct RobinhoodAccountEntry {
    account_number: String,
    brokerage_account_type: Option<String>,
    #[serde(default)]
    is_default: bool,
}

#[derive(Debug, Deserialize)]
struct RobinhoodPositionsResponse {
    next: Option<String>,
    results: Vec<RobinhoodPositionEntry>,
}

#[derive(Debug, Deserialize)]
struct RobinhoodPositionEntry {
    symbol: String,
    quantity: String,
}

#[derive(Clone, Debug)]
pub struct RobinhoodPosition {
    pub symbol: String,
    pub quantity: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RobinhoodAccount {
    pub account_number: String,
    pub brokerage_account_type: Option<String>,
    pub is_default: bool,
}

impl From<RobinhoodAccountEntry> for RobinhoodAccount {
    fn from(entry: RobinhoodAccountEntry) -> Self {
        Self {
            account_number: entry.account_number,
            brokerage_account_type: entry.brokerage_account_type,
            is_default: entry.is_default,
        }
    }
}

#[derive(serde::Deserialize)]
struct VerificationResultResponse {
    result: bool,
}

#[derive(serde::Deserialize)]
struct PushPromptStatusResponse {
    #[serde(rename = "challenge_status")]
    challenge_status: String,
}

#[derive(Debug, Deserialize)]
struct RobinhoodInstrumentsResponse {
    results: Vec<RobinhoodInstrumentEntry>,
}

#[derive(Debug, Deserialize)]
struct RobinhoodInstrumentEntry {
    id: String,
    symbol: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RobinhoodOrdersPage {
    pub next: Option<String>,
    pub results: Vec<RobinhoodOrderEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RobinhoodOrderEntry {
    pub id: String,
    #[serde(rename = "instrument_id")]
    pub instrument_id: String,
    #[serde(default)]
    pub executions: Vec<RobinhoodOrderExecutionEntry>,
    pub created_at: String,
    pub last_transaction_at: Option<String>,
    pub side: String,
    pub price: Option<String>,
    pub quantity: Option<String>,
    #[serde(rename = "type")]
    pub order_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RobinhoodOrderExecutionEntry {
    pub price: Option<String>,
    pub quantity: Option<String>,
    pub timestamp: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{
        CLIENT_ID, GRANT_TYPE, IDENTITY_CLIENT_VERSION, READ_ONLY_SECONDARY_TOKEN,
        TOKEN_REQUEST_PATH,
    };
    use serde_json::json;
    use tempfile::TempDir;
    use uuid::Uuid;
    use wiremock::matchers::{body_json, body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    #[test]
    fn token_request_includes_expected_defaults() {
        use crate::auth::{EXPIRES_IN_SECONDS, LONG_SESSION, SCOPE, TokenRequest};

        let device_token = "device-token";
        let request_id = "request-id";

        let request = TokenRequest::new("username", "password", device_token, request_id);

        let json = serde_json::to_value(&request).expect("serializes to json");

        assert_eq!(json["client_id"], json!(CLIENT_ID));
        assert_eq!(
            json["create_read_only_secondary_token"],
            json!(READ_ONLY_SECONDARY_TOKEN)
        );
        assert_eq!(json["expires_in"], json!(EXPIRES_IN_SECONDS));
        assert_eq!(json["grant_type"], json!(GRANT_TYPE));
        assert_eq!(json["scope"], json!(SCOPE));
        assert_eq!(json["token_request_path"], json!(TOKEN_REQUEST_PATH));
        assert_eq!(json["username"], json!("username"));
        assert_eq!(json["password"], json!("password"));
        assert_eq!(json["long_session"], json!(LONG_SESSION));
        assert_eq!(json["request_id"], json!(request_id));
        assert_eq!(json["device_token"], json!(device_token));
    }

    #[tokio::test]
    async fn initiate_login_returns_challenge_on_forbidden() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/oauth2/token/"))
            .and(body_partial_json(json!({
                "username": "user",
                "password": "pass",
                "grant_type": GRANT_TYPE,
                "client_id": CLIENT_ID,
            })))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "verification_workflow": {
                    "id": "workflow-id",
                    "workflow_status": "workflow_status_internal_pending"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid base url");

        let challenge = client
            .initiate_login("user", "pass")
            .await
            .expect("challenge expected");

        assert_eq!(challenge.verification_workflow().id, "workflow-id");
        assert_eq!(
            challenge.verification_workflow().workflow_status,
            "workflow_status_internal_pending"
        );
    }

    #[tokio::test]
    async fn initiate_login_errors_when_status_is_unexpected() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/oauth2/token/"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid base url");

        let err = client
            .initiate_login("user", "pass")
            .await
            .expect_err("unexpected status should error");

        match err {
            RobinhoodClientError::UnexpectedStatus(StatusCode::UNAUTHORIZED) => {}
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_positions_returns_positions() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/positions/"))
            .and(query_param("account_number", "5QT29231"))
            .and(query_param("nonzero", "true"))
            .and(header("authorization", "Bearer access-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next": null,
                "results": [
                    { "symbol": "AMZN", "quantity": "1618.57743000" },
                    { "symbol": "v", "quantity": "1500.00000000" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid base url");

        let positions = client
            .fetch_positions("access-token", "5QT29231")
            .await
            .expect("positions should load");

        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].symbol, "AMZN");
        assert!((positions[0].quantity - 1618.57743).abs() < 1e-6);
        assert_eq!(positions[1].symbol, "V");
        assert_eq!(positions[1].quantity, 1500.0);
    }

    #[tokio::test]
    async fn fetch_positions_follows_pagination_links() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/positions/"))
            .and(query_param("account_number", "12345678"))
            .and(query_param("nonzero", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next": "/positions/?cursor=cursor123",
                "results": [
                    { "symbol": "AAPL", "quantity": "42.0" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/positions/"))
            .and(query_param("cursor", "cursor123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next": null,
                "results": [
                    { "symbol": "MSFT", "quantity": "15.500000" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid base url");

        let positions = client
            .fetch_positions("token", "12345678")
            .await
            .expect("positions should load");

        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].symbol, "AAPL");
        assert_eq!(positions[0].quantity, 42.0);
        assert_eq!(positions[1].symbol, "MSFT");
        assert_eq!(positions[1].quantity, 15.5);
    }

    #[tokio::test]
    async fn fetch_verification_result_returns_true_on_success() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path(
                "/verification_workflows/polaris_migrated/workflow-id/",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": true
            })))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid urls");

        let result = client
            .fetch_verification_result("workflow-id")
            .await
            .expect("result expected");

        assert!(result);
    }

    #[tokio::test]
    async fn fetch_verification_result_errors_on_unexpected_status() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path(
                "/verification_workflows/polaris_migrated/workflow-id/",
            ))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid urls");

        let err = client
            .fetch_verification_result("workflow-id")
            .await
            .expect_err("expected error");

        match err {
            RobinhoodClientError::UnexpectedStatus(StatusCode::NOT_FOUND) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn advance_workflow_entry_point_returns_route() {
        let server = MockServer::start().await;

        Mock::given(method("PATCH"))
            .and(path("/idl/v1/workflow/workflow-id/"))
            .and(body_json(json!({
                "clientVersion": IDENTITY_CLIENT_VERSION,
                "id": "workflow-id",
                "entryPointAction": {}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "route": {
                    "replace": {
                        "screen": {
                            "name": "DEVICE_APPROVAL_CHALLENGE",
                            "blockId": "5a11ee22-8b07-434e-9cc2-306fd98ad9aa",
                            "deviceApprovalChallengeScreenParams": {
                                "sheriffChallenge": {
                                    "id": "32c929b0-8186-4025-9d70-e3043c6f1429",
                                    "type": "PROMPT",
                                    "status": "ISSUED",
                                    "remainingRetries": 3,
                                    "remainingAttempts": 1,
                                    "expiresAt": "2025-10-10T20:32:34.517455Z"
                                },
                                "sheriffFlowId": "login_suv",
                                "fallbackCtaText": "Send text instead"
                            }
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid urls");

        let route = client
            .advance_workflow_entry_point("workflow-id")
            .await
            .expect("route expected");

        let screen = &route.replace.expect("replace route").screen;
        assert_eq!(screen.name, "DEVICE_APPROVAL_CHALLENGE");
        let params = screen
            .device_approval_challenge_screen_params
            .as_ref()
            .expect("challenge params");
        assert_eq!(params.sheriff_flow_id.as_deref(), Some("login_suv"));
        let challenge = params
            .sheriff_challenge
            .as_ref()
            .expect("sheriff challenge");
        assert_eq!(
            challenge.id.as_deref(),
            Some("32c929b0-8186-4025-9d70-e3043c6f1429")
        );
        assert_eq!(challenge.challenge_type.as_deref(), Some("PROMPT"));
    }

    #[tokio::test]
    async fn advance_workflow_entry_point_errors_on_unexpected_status() {
        let server = MockServer::start().await;

        Mock::given(method("PATCH"))
            .and(path("/idl/v1/workflow/workflow-id/"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid urls");

        let err = client
            .advance_workflow_entry_point("workflow-id")
            .await
            .expect_err("expected error");

        match err {
            RobinhoodClientError::UnexpectedStatus(StatusCode::INTERNAL_SERVER_ERROR) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_push_prompt_status_returns_status() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/push/challenge-id/get_prompts_status/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "challenge_status": "issued"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid urls");

        let status = client
            .fetch_push_prompt_status("challenge-id")
            .await
            .expect("status expected");

        assert_eq!(status, "issued");
    }

    #[tokio::test]
    async fn fetch_push_prompt_status_errors_on_unexpected_status() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/push/challenge-id/get_prompts_status/"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid urls");

        let err = client
            .fetch_push_prompt_status("challenge-id")
            .await
            .expect_err("expected error");

        match err {
            RobinhoodClientError::UnexpectedStatus(StatusCode::NOT_FOUND) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn finalize_login_returns_token_on_success() {
        let server = MockServer::start().await;

        let device_token = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        Mock::given(method("POST"))
            .and(path("/oauth2/token/"))
            .and(body_partial_json(json!({
                "username": "user",
                "password": "pass",
                "grant_type": GRANT_TYPE,
                "client_id": CLIENT_ID,
                "device_token": device_token.to_string(),
                "request_id": request_id.to_string(),
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "token",
                "token_type": "Bearer"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid urls");

        let token = client
            .finalize_login("user", "pass", &device_token, &request_id)
            .await
            .expect("token response");

        assert_eq!(token["access_token"], "token");
    }

    #[tokio::test]
    async fn finalize_login_errors_on_unexpected_status() {
        let server = MockServer::start().await;

        let device_token = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        Mock::given(method("POST"))
            .and(path("/oauth2/token/"))
            .respond_with(ResponseTemplate::new(400))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid urls");

        let err = client
            .finalize_login("user", "pass", &device_token, &request_id)
            .await
            .expect_err("expected error");

        match err {
            RobinhoodClientError::UnexpectedStatus(StatusCode::BAD_REQUEST) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn complete_device_approval_returns_route_exit() {
        let server = MockServer::start().await;

        Mock::given(method("PATCH"))
            .and(path("/idl/v1/workflow/workflow-id/"))
            .and(body_json(json!({
                "clientVersion": IDENTITY_CLIENT_VERSION,
                "screenName": "DEVICE_APPROVAL_CHALLENGE",
                "id": "workflow-id",
                "deviceApprovalChallengeAction": {
                    "proceed": {}
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "route": {
                    "exit": {
                        "status": "WORKFLOW_STATUS_APPROVED"
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid urls");

        let route = client
            .complete_device_approval("workflow-id")
            .await
            .expect("route expected");

        let exit = route.exit.expect("exit route");
        assert_eq!(exit.status, "WORKFLOW_STATUS_APPROVED");
    }

    #[tokio::test]
    async fn complete_device_approval_errors_on_unexpected_status() {
        let server = MockServer::start().await;

        Mock::given(method("PATCH"))
            .and(path("/idl/v1/workflow/workflow-id/"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("valid urls");

        let err = client
            .complete_device_approval("workflow-id")
            .await
            .expect_err("expected error");

        match err {
            RobinhoodClientError::UnexpectedStatus(StatusCode::INTERNAL_SERVER_ERROR) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_accounts_returns_entries() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/accounts/"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer test-token",
            ))
            .and(wiremock::matchers::query_param(
                "default_to_all_accounts",
                "true",
            ))
            .and(wiremock::matchers::query_param("include_managed", "true"))
            .and(wiremock::matchers::query_param(
                "include_multiple_individual",
                "true",
            ))
            .and(wiremock::matchers::query_param("is_default", "false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    {
                        "account_number": "1234",
                        "brokerage_account_type": "Cash",
                        "is_default": true
                    },
                    {
                        "account_number": "5678",
                        "brokerage_account_type": "Margin",
                        "is_default": false
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("construct client");

        let accounts = client
            .fetch_accounts("test-token")
            .await
            .expect("fetch accounts");

        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].account_number, "1234");
        assert_eq!(accounts[0].brokerage_account_type.as_deref(), Some("Cash"));
        assert!(accounts[0].is_default);
        assert_eq!(accounts[1].account_number, "5678");
        assert_eq!(
            accounts[1].brokerage_account_type.as_deref(),
            Some("Margin")
        );
        assert!(!accounts[1].is_default);
    }

    #[tokio::test]
    async fn get_symbols_returns_cached_entries_without_fetching() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let db = Database::open_at(temp_dir.path()).expect("open database");

        let mut cached = HashMap::new();
        cached.insert("instrument-1".to_string(), "tsla".to_string());
        db.set_robinhood_instrument_symbols(&cached)
            .expect("cache instrument symbol");

        let server = MockServer::start().await;
        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client =
            RobinhoodClient::with_base_urls(&base_url, &identity_url).expect("create client");

        let ids = vec!["instrument-1".to_string()];
        let symbols = client
            .get_symbols("ignored-token", &ids, &db)
            .await
            .expect("fetch symbols");

        assert_eq!(symbols.get("instrument-1"), Some(&"TSLA".to_string()));

        let requests = server
            .received_requests()
            .await
            .expect("inspect received requests");
        assert!(
            requests.is_empty(),
            "expected no network requests when cache is populated"
        );
    }

    #[tokio::test]
    async fn get_symbols_refetches_when_cache_value_is_empty() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let db = Database::open_at(temp_dir.path()).expect("open database");

        let mut cached = HashMap::new();
        cached.insert("instrument-1".to_string(), "   ".to_string());
        db.set_robinhood_instrument_symbols(&cached)
            .expect("cache instrument symbol");

        let server = MockServer::start().await;
        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client =
            RobinhoodClient::with_base_urls(&base_url, &identity_url).expect("create client");

        Mock::given(method("GET"))
            .and(path("/instruments/"))
            .and(query_param("ids", "instrument-1"))
            .and(query_param("active_instruments_only", "false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next": null,
                "previous": null,
                "results": [
                    { "id": "instrument-1", "symbol": "TSLA" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let ids = vec!["instrument-1".to_string()];
        let symbols = client
            .get_symbols("test-token", &ids, &db)
            .await
            .expect("fetch symbols");

        assert_eq!(symbols.get("instrument-1"), Some(&"TSLA".to_string()));
    }

    #[tokio::test]
    async fn get_symbols_fetches_and_caches_missing_entries() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let db = Database::open_at(temp_dir.path()).expect("open database");
        let server = MockServer::start().await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client =
            RobinhoodClient::with_base_urls(&base_url, &identity_url).expect("create client");

        Mock::given(method("GET"))
            .and(path("/instruments/"))
            .and(query_param("ids", "instrument-1,instrument-2"))
            .and(query_param("active_instruments_only", "false"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next": null,
                "previous": null,
                "results": [
                    { "id": "instrument-1", "symbol": "tsla" },
                    { "id": "instrument-2", "symbol": "AAPL" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let ids = vec![
            "instrument-1".to_string(),
            "instrument-2".to_string(),
            "instrument-1".to_string(),
        ];
        let symbols = client
            .get_symbols("test-token", &ids, &db)
            .await
            .expect("fetch symbols");

        assert_eq!(symbols.get("instrument-1"), Some(&"TSLA".to_string()));
        assert_eq!(symbols.get("instrument-2"), Some(&"AAPL".to_string()));

        let cached = db
            .get_robinhood_instrument_symbol("instrument-1")
            .expect("fetch cached symbol")
            .expect("symbol should exist");
        assert_eq!(cached, "TSLA");
    }

    #[tokio::test]
    async fn get_symbols_batches_requests_when_needed() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let db = Database::open_at(temp_dir.path()).expect("open database");
        let server = MockServer::start().await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client =
            RobinhoodClient::with_base_urls(&base_url, &identity_url).expect("create client");

        let total = INSTRUMENT_LOOKUP_CHUNK + 5;
        let ids: Vec<String> = (0..total)
            .map(|index| format!("instrument-{index}"))
            .collect();

        let first_ids = (0..INSTRUMENT_LOOKUP_CHUNK)
            .map(|index| format!("instrument-{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let second_ids = (INSTRUMENT_LOOKUP_CHUNK..total)
            .map(|index| format!("instrument-{index}"))
            .collect::<Vec<_>>()
            .join(",");

        let first_results: Vec<_> = (0..INSTRUMENT_LOOKUP_CHUNK)
            .map(|index| {
                json!({
                    "id": format!("instrument-{index}"),
                    "symbol": format!("SYM{index}"),
                })
            })
            .collect();
        let second_results: Vec<_> = (INSTRUMENT_LOOKUP_CHUNK..total)
            .map(|index| {
                json!({
                    "id": format!("instrument-{index}"),
                    "symbol": format!("SYM{index}"),
                })
            })
            .collect();

        Mock::given(method("GET"))
            .and(path("/instruments/"))
            .and(query_param("ids", &first_ids))
            .and(query_param("active_instruments_only", "false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next": null,
                "previous": null,
                "results": first_results,
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/instruments/"))
            .and(query_param("ids", &second_ids))
            .and(query_param("active_instruments_only", "false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next": null,
                "previous": null,
                "results": second_results,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let symbols = client
            .get_symbols("test-token", &ids, &db)
            .await
            .expect("fetch symbols");

        assert_eq!(symbols.len(), total);
        assert_eq!(symbols.get("instrument-0"), Some(&"SYM0".to_string()));
        assert_eq!(
            symbols.get(&format!("instrument-{}", total - 1)),
            Some(&format!("SYM{}", total - 1)),
        );
    }

    #[tokio::test]
    async fn fetch_orders_page_requests_expected_query() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/orders/"))
            .and(query_param("account_numbers", "5QT29231"))
            .and(query_param("include_managed", "true"))
            .and(query_param("is_closed", "true"))
            .and(query_param("page_size", "200"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next": null,
                "previous": null,
                "results": [
                    {
                        "id": "order-1",
                        "instrument_id": "instrument-1",
                        "executions": [
                            {
                                "price": "10.0",
                                "quantity": "2.0",
                                "timestamp": "2024-01-01T12:00:00Z"
                            }
                        ],
                        "created_at": "2024-01-01T11:59:00Z",
                        "last_transaction_at": "2024-01-01T12:00:00Z",
                        "side": "buy",
                        "price": "10.0",
                        "quantity": "2.0",
                        "type": "market"
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client =
            RobinhoodClient::with_base_urls(&base_url, &identity_url).expect("create client");

        let page = client
            .fetch_orders_page("token", "5QT29231", 200, None)
            .await
            .expect("fetch orders");

        assert!(page.next.is_none());
        assert_eq!(page.results.len(), 1);
        assert_eq!(page.results[0].id, "order-1");
        assert_eq!(page.results[0].executions.len(), 1);
    }

    #[tokio::test]
    async fn fetch_orders_page_includes_cursor_when_present() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/orders/"))
            .and(query_param("cursor", "abc123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next": null,
                "previous": null,
                "results": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client =
            RobinhoodClient::with_base_urls(&base_url, &identity_url).expect("create client");

        let page = client
            .fetch_orders_page("token", "5QT29231", 50, Some("abc123"))
            .await
            .expect("fetch orders");

        assert!(page.results.is_empty());
        assert!(page.next.is_none());
    }

    #[tokio::test]
    async fn fetch_accounts_errors_on_unexpected_status() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/accounts/"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;

        let base_url = format!("{}/", server.uri());
        let identity_url = format!("{}/", server.uri());
        let client = RobinhoodClient::with_http_client_and_identity_base(
            Client::new(),
            &base_url,
            &identity_url,
        )
        .expect("construct client");

        let error = client
            .fetch_accounts("test-token")
            .await
            .expect_err("unexpected status should error");

        match error {
            RobinhoodClientError::UnexpectedStatus(StatusCode::UNAUTHORIZED) => {}
            other => panic!("unexpected error variant: {other:?}"),
        }
    }
}
