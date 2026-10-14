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

pub(crate) async fn snapshot(pr_url: &str, encoded_checks: &str) -> Result<String> {
    if encoded_checks == "[]" {
        return Ok(encoded_checks.to_owned());
    }
    let (owner, repo, _) = boss_github::pr_url::parse_pr_url_parts(pr_url).context("invalid PR URL")?;
    let Some(config) = boss_github::contents::fetch_repo_file(owner, repo, "BOSS_CI_VERIFICATION.json", "main").await?
    else {
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
        let url = check["target_url"].as_str().context("missing build URL")?;
        let id = check["provider_job_id"]
            .as_str()
            .context("missing provider job identity")?;
        let key = if check["provider"] == "buildkite" {
            let pipeline = boss_ci_log_reader::parse_buildkite_pipeline_slug(url).context("missing pipeline")?;
            let step = boss_ci_log_reader::fetch_buildkite_step_key("bk", url, id).await?;
            format!("{pipeline}/{step}")
        } else {
            id.to_owned()
        };
        attach(check, &key, &contracts)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_mapping_uses_stable_key_and_retains_provider_identity() {
        let contracts: BTreeMap<String, Vec<RequiredRun>> =
            serde_json::from_str(r#"{"backend":[{"kind":"test","tool":"project","command":"make test"}]}"#).unwrap();
        let mut check = serde_json::json!({"name":"Renamed display label","provider_job_id":"uuid"});
        attach(&mut check, "backend", &contracts).unwrap();
        assert_eq!(check["job_key"], "backend");
        assert_eq!(check["provider_job_id"], "uuid");
        assert_eq!(check["verification"][0]["command"], "make test");
        let mut unknown = serde_json::json!({"name":"backend"});
        attach(&mut unknown, "other-key", &contracts).unwrap();
        assert!(unknown.get("verification").is_none());
    }

    #[test]
    fn invalid_declared_contract_is_not_a_heuristic_fallback() {
        assert!(validate_contract(&[]).is_err());
        let runs: Vec<RequiredRun> =
            serde_json::from_str(r#"[{"kind":"build","tool":"bazel","command":"npm run build"}]"#).unwrap();
        assert!(validate_contract(&runs).is_err());
    }
}
