//! Engine-side probes for evidence absent from, or newer than, stored attempts.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::ProgressionBriefPacket;

/// Provider boundary keeps prompt tests independent of host credentials and git.
pub trait EvidenceProvider {
    fn pr(&self, url: &str) -> impl std::future::Future<Output = Result<Value>> + Send;
    fn main_checks(&self, repo: &str) -> impl std::future::Future<Output = Result<Value>> + Send;
    fn conflict(
        &self,
        base: &str,
        head: &str,
        pr_url: &str,
    ) -> impl std::future::Future<Output = Result<String>> + Send;
    fn log(&self, check: &Value) -> impl std::future::Future<Output = Result<String>> + Send;
}

/// Production probes reuse the GitHub command runner, conflict collector and CI
/// log readers; progression workers themselves need no additional permissions.
pub struct EngineEvidence<'a> {
    pub workspace: &'a Path,
}

impl EvidenceProvider for EngineEvidence<'_> {
    async fn pr(&self, url: &str) -> Result<Value> {
        boss_github::pr_files::fetch_pr_view_json(url, "baseRefName,baseRefOid,headRefOid,statusCheckRollup").await
    }

    async fn main_checks(&self, repo: &str) -> Result<Value> {
        let (owner, name) = repo.split_once('/').context("invalid repository slug")?;
        // Ten commits is an explicitly sampled recent-history window, not an
        // admission limit. Missing contexts and unfinished runs stay unknown.
        let query = "query($owner:String!,$name:String!){repository(owner:$owner,name:$name){defaultBranchRef{name target{... on Commit{history(first:10){nodes{oid committedDate statusCheckRollup{contexts(first:100){pageInfo{hasNextPage} nodes{... on CheckRun{name conclusion completedAt detailsUrl} ... on StatusContext{context state createdAt targetUrl}}}}}}}}}}}";
        let output = boss_github::gh_runner::run_gh(
            &[
                "api",
                "graphql",
                "-f",
                &format!("query={query}"),
                "-f",
                &format!("owner={owner}"),
                "-f",
                &format!("name={name}"),
            ],
            "progression main check history",
        )
        .await?;
        let value: Value = serde_json::from_str(&output)?;
        if value.get("errors").is_some() {
            bail!("GitHub history query: {}", value["errors"]);
        }
        Ok(value)
    }

    async fn conflict(&self, base: &str, head: &str, pr_url: &str) -> Result<String> {
        let repo_url = pr_url.split_once("/pull/").context("invalid PR URL")?.0;
        let scratch = self.workspace.join("conflict-evidence");
        Ok(serde_json::to_string(
            &boss_conflict_diagnosis::collect_remote(&scratch, &format!("{repo_url}.git"), base, head).await?,
        )?)
    }

    async fn log(&self, check: &Value) -> Result<String> {
        let url = check_url(check).context("check has no provider URL")?;
        let provider = boss_github::check_runs::provider_for_url(url);
        let job = boss_github::check_runs::parse_provider_job_id(provider, url)
            .context("check URL has no supported provider job id")?;
        boss_ci_log_reader::reader_for(provider, url)
            .read_log_tail(&job, 100)
            .await
    }
}

fn check_name(check: &Value) -> Option<&str> {
    check["name"].as_str().or_else(|| check["context"].as_str())
}

fn check_url(check: &Value) -> Option<&str> {
    check["detailsUrl"]
        .as_str()
        .or_else(|| check["targetUrl"].as_str())
        .or_else(|| check["target_url"].as_str())
}

fn verdict(check: &Value) -> &str {
    check["conclusion"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| check["state"].as_str())
        .unwrap_or("UNKNOWN")
}

fn failed(check: &Value) -> bool {
    matches!(
        verdict(check).to_ascii_uppercase().as_str(),
        "FAILURE" | "ERROR" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED"
    )
}

fn current_diagnosis(packet: &ProgressionBriefPacket, base: &str, head: &str) -> Option<String> {
    packet.conflict_attempts.iter().find_map(|attempt| {
        let json = attempt.conflict_diagnosis.as_deref()?;
        let value: Value = serde_json::from_str(json).ok()?;
        (value["base_sha"] == base && value["head_sha"] == head && value["error"].is_null()).then(|| json.to_owned())
    })
}

fn current_log(packet: &ProgressionBriefPacket, head: &str, check: &Value) -> Option<String> {
    packet.ci_attempts.iter().find_map(|attempt| {
        if attempt.head_sha_at_trigger != head {
            return None;
        }
        let checks: Value = serde_json::from_str(&attempt.failed_checks).ok()?;
        // ci_watch stores the first failing job with a provider_job_id, not
        // one excerpt per check; another failure in the same attempt must probe.
        let stored = checks
            .as_array()?
            .iter()
            .find(|stored| stored["provider_job_id"].as_str().is_some())?;
        let matches = check_name(stored) == check_name(check)
            && check_url(stored).is_some()
            && check_url(stored) == check_url(check);
        matches
            .then(|| attempt.log_excerpt.clone())
            .flatten()
            .filter(|s| !s.trim().is_empty())
    })
}

/// Gather live observations before the first agent turn. Probe failures are
/// part of the packet, never silently converted into an empty successful read.
pub async fn gather_evidence(packet: &ProgressionBriefPacket, provider: &impl EvidenceProvider) -> String {
    let Some(url) = packet.pr_url.as_deref() else {
        return String::new();
    };
    let mut out = String::from("## Engine evidence probes\n\n");
    let pr = match provider.pr(url).await {
        Ok(pr) => pr,
        Err(error) => return format!("{out}PR probe failed: {error:#}; current conflict/check evidence unknown.\n"),
    };
    let base = pr["baseRefOid"].as_str();
    let head = pr["headRefOid"].as_str();
    if let (Some(base), Some(head)) = (base, head) {
        out.push_str(&format!("Observed PR base={base} head={head}.\n\n"));
        if packet.item.blocked_reason.as_deref() == Some("merge_conflict")
            || packet.pr_mergeable.as_deref() == Some("CONFLICTING")
            || !packet.conflict_attempts.is_empty()
        {
            let diagnosis = match current_diagnosis(packet, base, head) {
                Some(stored) => Ok(stored),
                None => provider.conflict(base, head, url).await,
            };
            match diagnosis {
                Ok(json) => out.push_str(&format!(
                    "Conflict diagnosis for current base/head:\n```json\n{json}\n```\n\n"
                )),
                Err(error) => out.push_str(&format!("Conflict probe failed: {error:#}\n\n")),
            }
        }
        let checks = pr["statusCheckRollup"].as_array().cloned().unwrap_or_default();
        for check in checks.iter().filter(|c| failed(c)) {
            let name = check_name(check).unwrap_or("(unnamed)");
            let log = match current_log(packet, head, check) {
                Some(stored) => Ok(stored),
                None => provider.log(check).await,
            };
            match log {
                Ok(log) => out.push_str(&format!(
                    "Failing check `{name}` at head {head}, URL {}:\n```\n{log}\n```\n\n",
                    check_url(check).unwrap_or("(missing)")
                )),
                Err(error) => out.push_str(&format!("Log probe failed for `{name}` at head {head}: {error:#}\n\n")),
            }
        }
        if checks.iter().any(failed) {
            let repo = url
                .strip_prefix("https://github.com/")
                .and_then(|s| s.split_once("/pull/"))
                .map(|(repo, _)| repo);
            match repo {
                Some(repo) => match provider.main_checks(repo).await {
                    Ok(history) => render_main_history(&mut out, &checks, &history),
                    Err(error) => {
                        out.push_str(&format!("Main history probe failed: {error:#}; attribution unknown.\n"))
                    }
                },
                None => out.push_str("Main history probe failed: unsupported PR URL; attribution unknown.\n"),
            }
        }
    } else {
        out.push_str("PR head/base missing from probe; current evidence unknown.\n");
    }
    out
}

fn render_main_history(out: &mut String, failing: &[Value], history: &Value) {
    let branch = &history["data"]["repository"]["defaultBranchRef"];
    let name = branch["name"].as_str().unwrap_or("(unknown branch)");
    out.push_str(&format!(
        "### Recent outcomes on main (repository default branch `{name}`, latest 10 commits sampled)\n\n"
    ));
    for check in failing.iter().filter(|c| failed(c)) {
        let mut red = false;
        let mut green = false;
        let check_name = check_name(check).unwrap_or("(unnamed)");
        if let Some(commits) = branch["target"]["history"]["nodes"].as_array() {
            for commit in commits {
                if let Some(contexts) = commit["statusCheckRollup"]["contexts"]["nodes"].as_array() {
                    for context in contexts.iter().filter(|c| self::check_name(c) == Some(check_name)) {
                        let result = verdict(context);
                        red |= failed(context);
                        green |= matches!(result, "SUCCESS" | "NEUTRAL" | "SKIPPED");
                        out.push_str(&format!(
                            "- `{check_name}` branch={name} commit={} committed_at={} check_time={} outcome={result}\n",
                            commit["oid"].as_str().unwrap_or("?"),
                            commit["committedDate"].as_str().unwrap_or("?"),
                            context["completedAt"]
                                .as_str()
                                .or_else(|| context["createdAt"].as_str())
                                .unwrap_or("?")
                        ));
                    }
                }
                if commit["statusCheckRollup"]["contexts"]["pageInfo"]["hasNextPage"] == true {
                    out.push_str("Main context sample truncated; unobserved contexts remain unknown.\n");
                }
            }
        }
        let attribution = if red {
            "main also red: likely unrelated or flaky"
        } else if green {
            "observed main green: PR-specific failure possible"
        } else {
            "main unobserved: attribution unknown"
        };
        out.push_str(&format!("`{check_name}`: {attribution}.\n\n"));
    }
}
