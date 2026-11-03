use super::*;

pub(crate) const CONFIG: &str = r#"{"mono/backend":[{"kind":"build","tool":"project","command":"make build"},{"kind":"test","tool":"project","command":"make test"}]}"#;

pub(crate) struct FakeFetcher(pub Option<&'static str>);

#[async_trait::async_trait]
impl ContractFetcher for FakeFetcher {
    async fn config(&self, _: &str) -> Result<Option<String>> {
        anyhow::ensure!(self.0 != Some("error"), "config unavailable");
        Ok(self.0.map(str::to_owned))
    }

    async fn step_keys(&self, _: &str) -> Result<BTreeMap<String, String>> {
        Ok(BTreeMap::from([
            ("declared".into(), "backend".into()),
            ("unknown".into(), "unknown".into()),
        ]))
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

#[tokio::test]
async fn captured_trunk_key_needs_no_provider_lookup() {
    struct Captured;
    #[async_trait::async_trait]
    impl ContractFetcher for Captured {
        async fn config(&self, _: &str) -> Result<Option<String>> {
            Ok(Some(CONFIG.into()))
        }
        async fn step_keys(&self, _: &str) -> Result<BTreeMap<String, String>> {
            panic!("captured keys must not be refetched")
        }
    }
    let build = boss_ci_log_reader::TrunkEvictionBuild {
        pipeline_slug: "mono".into(),
        build_number: "1".into(),
        web_url: "https://buildkite.com/org/mono/builds/1".into(),
        failed_job_ids: vec!["declared".into()],
        failed_job_details: vec![boss_ci_log_reader::TrunkJobDetails {
            label: "backend".into(),
            step_key: Some("backend".into()),
        }],
    };
    let failures = crate::ci_watch::trunk_eviction_failures_from_build(&build);
    let checks = crate::ci_watch::encode_failed_checks(&failures);
    let result = snapshot_with(&Captured, "pr", &checks).await.unwrap();
    let result: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result[0]["job_key"], "mono/backend");
    assert_eq!(result[0]["verification"][1]["command"], "make test");
}

pub(crate) struct SlowFetcher(pub std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl ContractFetcher for SlowFetcher {
    async fn config(&self, _: &str) -> Result<Option<String>> {
        Ok(Some(CONFIG.into()))
    }
    async fn step_keys(&self, _: &str) -> Result<BTreeMap<String, String>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn timed_out_builds_are_cached_without_discarding_resolved_siblings() {
    let fetcher = SlowFetcher(std::sync::atomic::AtomicUsize::new(0));
    let mut checks = Vec::new();
    for build in 1..=4 {
        for id in ["one", "two"] {
            checks.push(serde_json::json!({"provider":"buildkite", "target_url":format!("https://buildkite.com/org/mono/builds/{build}#{id}"), "provider_job_id":id}));
        }
    }
    checks.push(serde_json::json!({"provider":"buildkite", "job_key":"mono/backend"}));
    let result = snapshot_with(&fetcher, "pr", &serde_json::to_string(&checks).unwrap())
        .await
        .unwrap();
    let result: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
    assert_eq!(fetcher.0.load(std::sync::atomic::Ordering::SeqCst), 4);
    assert!(result[..8].iter().all(|check| check.get("verification").is_none()));
    assert_eq!(result[8]["verification"][1]["command"], "make test");
}
