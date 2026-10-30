//! Snapshot product-declared verification requirements before minting an attempt.
//! BOSS_CI_VERIFICATION.json on main maps pipeline/step keys to required runs.
//! Each run has a kind (lint/build/test), tool (bazel/project), and exact command.
//! Every declared run is required; command strings are data, never executed here.

use anyhow::{Context, Result};
use std::collections::BTreeMap;

pub(crate) use boss_engine_storage_values::ci_contract::{Kind, RequiredRun, Tool, validate_contract};

// Check the legacy field types while storage consumes the policy crate directly.
const _: fn(&RequiredRun) -> (&Kind, &Tool) = |run| (&run.kind, &run.tool);

#[async_trait::async_trait]
pub trait ContractFetcher: Send + Sync {
    async fn config(&self, pr_url: &str) -> Result<Option<String>>;
    async fn step_keys(&self, url: &str) -> Result<BTreeMap<String, String>>;
}

pub struct ProviderFetcher;

#[async_trait::async_trait]
impl ContractFetcher for ProviderFetcher {
    async fn config(&self, pr_url: &str) -> Result<Option<String>> {
        let (owner, repo, _) = boss_github::pr_url::parse_pr_url_parts(pr_url).context("invalid PR URL")?;
        Ok(tokio::time::timeout(
            std::time::Duration::from_secs(10),
            boss_github::contents::fetch_repo_file(owner, repo, "BOSS_CI_VERIFICATION.json", "main"),
        )
        .await
        .context("contract fetch timed out")??)
    }

    async fn step_keys(&self, url: &str) -> Result<BTreeMap<String, String>> {
        boss_ci_log_reader::fetch_buildkite_step_keys("bk", url).await
    }
}

pub(crate) async fn snapshot_with(fetcher: &dyn ContractFetcher, pr_url: &str, encoded_checks: &str) -> Result<String> {
    if encoded_checks == "[]" {
        return Ok(encoded_checks.to_owned());
    }
    let Some(config) = fetcher.config(pr_url).await? else {
        return Ok(encoded_checks.to_owned());
    };
    let contracts: BTreeMap<String, Vec<RequiredRun>> = serde_json::from_str(&config)?;
    if contracts.is_empty() {
        return Ok(encoded_checks.to_owned());
    }
    for runs in contracts.values() {
        validate_contract(runs)?;
    }
    let mut checks: Vec<serde_json::Value> = serde_json::from_str(encoded_checks)?;
    let mut builds = BTreeMap::new();
    for check in &mut checks {
        check
            .as_object_mut()
            .context("invalid failed check")?
            .remove("verification");

        // Only Buildkite has a stable declaration identity. Actions job IDs
        // identify one run, not a job a product can declare ahead of time.
        if check["provider"] != "buildkite" {
            continue;
        }
        if let Some(key) = check["job_key"].as_str().map(str::to_owned) {
            attach(check, &key, &contracts)?;
            continue;
        }
        let (Some(url), Some(id)) = (check["target_url"].as_str(), check["provider_job_id"].as_str()) else {
            continue;
        };
        let Some(pipeline) = boss_ci_log_reader::parse_buildkite_pipeline_slug(url) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let build_url = url.split('#').next().unwrap_or(url);
        if !builds.contains_key(build_url) {
            // Cache errors too: a degraded build is fetched only once and
            // leaves its jobs unresolved without dropping resolved siblings.
            let result = tokio::time::timeout(std::time::Duration::from_secs(10), fetcher.step_keys(build_url)).await;
            let keys = match result {
                Ok(Ok(keys)) => keys,
                result => {
                    tracing::warn!(?result, "queue CI identity unresolved; using heuristic verification");
                    BTreeMap::new()
                }
            };
            builds.insert(build_url.to_owned(), keys);
        }
        let Some(step) = builds[build_url].get(id) else {
            continue;
        };
        attach(check, &format!("{pipeline}/{step}"), &contracts)?;
    }
    Ok(serde_json::to_string(&checks)?)
}

fn attach(check: &mut serde_json::Value, key: &str, contracts: &BTreeMap<String, Vec<RequiredRun>>) -> Result<()> {
    check["job_key"] = key.into();
    if let Some(runs) = contracts.get(key) {
        check["verification"] = serde_json::to_value(runs)?;
    }
    Ok(())
}

pub(crate) const SNAPSHOT_ERROR_ATTENTION_KIND: &str = "queue_ci_contract_error";

pub(crate) async fn surface_error(
    db: &crate::work::WorkDb,
    publisher: &dyn crate::coordinator::ExecutionPublisher,
    candidate: &crate::work::PendingMergeCheck,
    error: &anyhow::Error,
) {
    // Polling may retry after configuration or credentials are repaired; one
    // open attention is enough to keep the failed capture visible meanwhile.
    let result = (|| -> Result<Option<boss_protocol::WorkAttentionItem>> {
        if db
            .list_attention_items_for_work_item(&candidate.work_item_id)?
            .iter()
            .any(|item| item.kind == SNAPSHOT_ERROR_ATTENTION_KIND && item.status == "open")
        {
            return Ok(None);
        }
        Ok(Some(db.create_attention_item(boss_protocol::CreateAttentionItemInput {
            execution_id: None,
            work_item_id: Some(candidate.work_item_id.clone()),
            kind: SNAPSHOT_ERROR_ATTENTION_KIND.to_owned(),
            status: None,
            title: "Queue CI verification contract could not be captured".to_owned(),
            body_markdown: format!("Capture for {} failed: {error:#}. Check BOSS_CI_VERIFICATION.json on main and provider access, then re-check the dequeued PR. No remediation attempt was spent.", candidate.pr_url),
            resolved_at: None,
        })?))
    })();
    match result {
        Ok(Some(item)) => {
            publisher
                .publish_frontend_event_on_product(
                    &candidate.product_id,
                    boss_protocol::FrontendEvent::AttentionItemCreated { item },
                )
                .await
        }
        Ok(None) => {}
        Err(err) => tracing::error!(?err, "could not surface queue CI contract failure"),
    }
}

#[cfg(test)]
#[path = "queue_ci_contract_tests.rs"]
pub(crate) mod tests;

/// External dependencies for queue failure detection, separate from events.
#[derive(Clone, Copy)]
pub struct DetectionContext<'a> {
    pub publisher: &'a dyn crate::coordinator::ExecutionPublisher,
    pub fetcher: &'a dyn ContractFetcher,
}

#[cfg(test)]
pub(crate) fn test_context(publisher: &dyn crate::coordinator::ExecutionPublisher) -> DetectionContext<'_> {
    DetectionContext {
        publisher,
        fetcher: &tests::FakeFetcher(None),
    }
}
