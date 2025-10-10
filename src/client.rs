use reqwest::{Client, StatusCode};
use url::Url;
use uuid::Uuid;

use crate::auth::{
    AUTH_ENDPOINT, AuthChallenge, IDENTITY_WORKFLOW_ENDPOINT_PREFIX, TokenRequest, TokenResponse,
    WorkflowEntryPointRequest, WorkflowRoute, WorkflowRouteResponse,
};
use crate::error::RobinhoodClientError;

#[derive(Clone, Debug)]
pub struct RobinhoodClient {
    http: Client,
    base_url: Url,
    identity_base_url: Url,
}

const DEFAULT_IDENTITY_BASE_URL: &str = "https://identi.robinhood.com/";

impl RobinhoodClient {
    pub fn new(base_url: &str) -> Result<Self, RobinhoodClientError> {
        let http = Client::builder().build()?;
        Self::with_http_client(http, base_url)
    }

    pub fn with_http_client(http: Client, base_url: &str) -> Result<Self, RobinhoodClientError> {
        Self::with_http_client_and_identity_base(http, base_url, DEFAULT_IDENTITY_BASE_URL)
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

        let device_token_string = device_token.to_string();
        let request_id_string = request_id.to_string();

        let payload =
            TokenRequest::new(username, password, &device_token_string, &request_id_string);

        let url = self
            .base_url
            .join(AUTH_ENDPOINT)
            .map_err(RobinhoodClientError::InvalidEndpointUrl)?;

        let response = self.http.post(url).json(&payload).send().await?;

        if response.status() != StatusCode::FORBIDDEN {
            return Err(RobinhoodClientError::UnexpectedStatus(response.status()));
        }

        let body = response.bytes().await?;
        let token_response: TokenResponse = serde_json::from_slice(&body)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{
        CLIENT_ID, GRANT_TYPE, IDENTITY_CLIENT_VERSION, READ_ONLY_SECONDARY_TOKEN,
        TOKEN_REQUEST_PATH,
    };
    use serde_json::json;
    use wiremock::matchers::{body_json, body_partial_json, method, path};
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
            .respond_with(ResponseTemplate::new(200))
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
            RobinhoodClientError::UnexpectedStatus(StatusCode::OK) => {}
            other => panic!("unexpected error variant: {other:?}"),
        }
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
}
