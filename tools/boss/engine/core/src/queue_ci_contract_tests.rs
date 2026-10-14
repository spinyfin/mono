use super::*;

pub(crate) const CONFIG: &str = r#"{"mono/backend":[{"kind":"build","tool":"project","command":"make build"},{"kind":"test","tool":"project","command":"make test"}]}"#;

pub(crate) struct FakeFetcher(pub Option<&'static str>);

#[async_trait::async_trait]
impl ContractFetcher for FakeFetcher {
    async fn config(&self, _: &str) -> Result<Option<String>> {
        anyhow::ensure!(self.0 != Some("error"), "config unavailable");
        Ok(self.0.map(str::to_owned))
    }

    async fn step_key(&self, _: &str, id: &str) -> Result<String> {
        match id {
            "declared" => Ok("backend".into()),
            "unkeyed" => Ok(String::new()),
            "unknown" => Ok("unknown".into()),
            _ => anyhow::bail!("provider lookup failed"),
        }
    }
}

#[tokio::test]
async fn resolves_siblings_and_leaves_unusable_identities_undeclared() {
    let checks = serde_json::json!([
        {"provider":"buildkite","target_url":"https://buildkite.com/org/mono/builds/1#declared","provider_job_id":"declared"},
        {"provider":"buildkite","target_url":"https://buildkite.com/org/mono/builds/1","provider_job_id":null},
        {"provider":"buildkite","target_url":null,"provider_job_id":"declared"},
        {"provider":"other","provider_job_id":null},
        {"provider":"github_actions","provider_job_id":"123"},
        {"provider":"buildkite","target_url":"https://buildkite.com/org/mono/builds/1","provider_job_id":"unkeyed"},
        {"provider":"buildkite","target_url":"https://buildkite.com/org/mono/builds/1","provider_job_id":"api-error"},
        {"provider":"buildkite","target_url":"invalid","provider_job_id":"declared"},
        {"provider":"buildkite","target_url":"https://buildkite.com/org/mono/builds/1","provider_job_id":"unknown"}
    ]);
    let result = snapshot_with(&FakeFetcher(Some(CONFIG)), "pr", &checks.to_string())
        .await
        .unwrap();
    let result: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
    assert_eq!(result[0]["job_key"], "mono/backend");
    assert_eq!(result[0]["provider_job_id"], "declared");
    assert_eq!(result[0]["verification"][0]["command"], "make build");
    assert_eq!(result[0]["verification"][1]["command"], "make test");
    for check in &result[1..8] {
        assert!(check["job_key"].is_null());
        assert!(check.get("verification").is_none());
    }
    assert_eq!(result[8]["job_key"], "mono/unknown");
    assert!(result[8].get("verification").is_none());
}

#[tokio::test]
async fn absent_or_empty_config_preserves_checks_and_empty_checks_skip_fetch() {
    for config in [None, Some("{}")] {
        assert_eq!(snapshot_with(&FakeFetcher(config), "pr", "[{}]").await.unwrap(), "[{}]");
    }
    assert_eq!(
        snapshot_with(&FakeFetcher(Some("error")), "pr", "[]").await.unwrap(),
        "[]"
    );
}

#[tokio::test]
async fn config_errors_and_invalid_contracts_are_not_heuristic_fallbacks() {
    for config in [
        "error",
        "invalid json",
        r#"{"mono/backend":[]}"#,
        r#"{"mono/backend":[{"kind":"build","tool":"bazel","command":"npm run build"}]}"#,
        r#"{"mono/backend":[{"kind":"test","tool":"project","command":" "}]} "#,
    ] {
        assert!(snapshot_with(&FakeFetcher(Some(config)), "pr", "[{}]").await.is_err());
    }
}
