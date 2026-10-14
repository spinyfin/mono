//! Snapshot product-declared verification requirements before minting an attempt.
//! BOSS_CI_VERIFICATION.json on main maps pipeline/step keys to required runs.
//! Each run has a kind (lint/build/test), tool (bazel/project), and exact command.
//! Every declared run is required; command strings are data, never executed here.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequiredRun {
    pub kind: Kind,
    pub tool: Tool,
    pub command: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Lint,
    Build,
    Test,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Tool {
    Bazel,
    Project,
}

pub(crate) fn validate_contract(runs: &[RequiredRun]) -> Result<()> {
    anyhow::ensure!(!runs.is_empty(), "empty verification contract");
    for run in runs {
        anyhow::ensure!(!run.command.trim().is_empty(), "empty verification command");
        if matches!(run.tool, Tool::Bazel) {
            let words: Vec<_> = run.command.split_whitespace().collect();
            let verb = match run.kind {
                Kind::Build => "build",
                Kind::Test => "test",
                Kind::Lint => "run",
            };
            anyhow::ensure!(
                words.first() == Some(&"bazel") && words.get(1) == Some(&verb),
                "Bazel verification kind disagrees with command"
            );
        }
    }
    Ok(())
}

#[async_trait::async_trait]
pub(crate) trait ContractFetcher: Send + Sync {
    async fn config(&self, pr_url: &str) -> Result<Option<String>>;
    async fn step_key(&self, url: &str, id: &str) -> Result<String>;
}

struct ProviderFetcher;

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

    async fn step_key(&self, url: &str, id: &str) -> Result<String> {
        Ok(tokio::time::timeout(
            std::time::Duration::from_secs(10),
            boss_ci_log_reader::fetch_buildkite_step_key("bk", url, id),
        )
        .await
        .context("step lookup timed out")??)
    }
}

pub(crate) async fn snapshot(pr_url: &str, encoded_checks: &str) -> Result<String> {
    snapshot_with(&ProviderFetcher, pr_url, encoded_checks).await
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
    for check in &mut checks {
        check
            .as_object_mut()
            .context("invalid failed check")?
            .remove("verification");
        check["job_key"] = serde_json::Value::Null;
        // Only Buildkite has a stable declaration identity. Actions job IDs
        // identify one run, not a job a product can declare ahead of time.
        if check["provider"] != "buildkite" {
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
        // One bounded lookup per check: provider errors must not suppress
        // this failure or a declared sibling's contract.
        let step = match fetcher.step_key(url, id).await {
            Ok(step) if !step.is_empty() => step,
            result => {
                tracing::warn!(?result, "queue CI identity unresolved; using heuristic verification");
                continue;
            }
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
