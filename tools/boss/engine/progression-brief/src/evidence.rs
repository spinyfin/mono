//! Engine-side probes for evidence absent from, or newer than, stored attempts.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::ProgressionBriefPacket;

/// Provider boundary keeps prompt tests independent of host credentials and git.
pub trait EvidenceProvider {
    fn pr(&self, url: &str) -> impl std::future::Future<Output = Result<Value>> + Send;
    /// Live tip of `branch` in the PR's base repository `repo` (`owner/name`).
    fn base_tip(&self, repo: &str, branch: &str) -> impl std::future::Future<Output = Result<String>> + Send;
    fn main_checks(&self, repo: &str) -> impl std::future::Future<Output = Result<Value>> + Send;
    fn conflict(
        &self,
        base: &str,
        head: &str,
        remote: &str,
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

    async fn base_tip(&self, repo: &str, branch: &str) -> Result<String> {
        let output = boss_github::gh_runner::run_gh(
            &["api", &format!("repos/{repo}/git/ref/heads/{branch}")],
            "progression base branch tip",
        )
        .await?;
        let value: Value = serde_json::from_str(&output)?;
        value["object"]["sha"]
            .as_str()
            .filter(|sha| matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_owned)
            .context("GitHub returned no commit SHA for the base branch")
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

    async fn conflict(&self, base: &str, head: &str, remote: &str) -> Result<String> {
        // The probe's object store lives in its own temporary directory beside
        // (not inside) the agent's working directory, and is removed when the
        // probe ends, including when the probe is dropped on timeout.
        let mut builder = tempfile::Builder::new();
        builder.prefix("boss-conflict-evidence-");
        let scratch = match self.workspace.parent() {
            Some(parent) => builder.tempdir_in(parent)?,
            None => builder.tempdir()?,
        };
        Ok(serde_json::to_string(
            &boss_conflict_diagnosis::collect_remote(scratch.path(), remote, base, head).await?,
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
    boss_github::leaf_verdict_token(check).unwrap_or("UNKNOWN")
}

fn failed(check: &Value) -> bool {
    boss_github::leaf_is_failure(check)
}

fn passed(check: &Value) -> bool {
    boss_github::leaf_is_pass(check)
}

/// Upper bounds for one engine probe. The spawn waits on these inline, so each
/// must be finite; a probe that exceeds its bound is rendered as `probe timed
/// out` and the agent treats that evidence as unknown. They are generous
/// ceilings, not measured typical costs (a `gh` call normally returns in
/// seconds), and the conflict fetch is blob-less but still downloads every ancestor
/// commit and tree of the two SHAs, so its cost grows with history and the
/// bound is deliberately the largest.
#[derive(Debug, Clone, Copy)]
pub struct ProbeLimits {
    pub gh: Duration,
    pub conflict: Duration,
}

impl ProbeLimits {
    pub const PUBLISHED: Self = Self {
        gh: Duration::from_secs(120),
        conflict: Duration::from_secs(300),
    };
}

async fn bounded<T>(limit: Duration, probe: impl Future<Output = Result<T>>) -> Result<T> {
    match tokio::time::timeout(limit, probe).await {
        Ok(result) => result,
        Err(_) => bail!("probe timed out after {limit:?}"),
    }
}

/// The product's configured remote, when it has one; otherwise the PR's own
/// repository over HTTPS.
pub(crate) fn conflict_remote(packet: &ProgressionBriefPacket, pr_url: &str) -> Result<String> {
    if let Some(remote) = packet.repo_remote_url.as_deref() {
        return Ok(remote.to_owned());
    }
    let repo_url = pr_url.split_once("/pull/").context("invalid PR URL")?.0;
    Ok(format!("{repo_url}.git"))
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
    gather_evidence_with(packet, provider, ProbeLimits::PUBLISHED).await
}

pub async fn gather_evidence_with(
    packet: &ProgressionBriefPacket,
    provider: &impl EvidenceProvider,
    limits: ProbeLimits,
) -> String {
    let Some(url) = packet.pr_url.as_deref() else {
        return String::new();
    };
    let mut out = String::from("## Engine evidence probes\n\n");
    let pr = match bounded(limits.gh, provider.pr(url)).await {
        Ok(pr) => pr,
        Err(error) => return format!("{out}PR probe failed: {error:#}; current conflict/check evidence unknown.\n"),
    };
    let repo = url
        .strip_prefix("https://github.com/")
        .and_then(|s| s.split_once("/pull/"))
        .map(|(repo, _)| repo);
    let recorded_base = pr["baseRefOid"].as_str();
    let base_ref = pr["baseRefName"].as_str();
    let Some(head) = pr["headRefOid"].as_str() else {
        out.push_str("PR head missing from probe; current evidence unknown.\n");
        return out;
    };
    out.push_str(&format!("Observed PR head={head}.\n"));

    // The PR's recorded base commit is fixed when the PR opens and does not
    // follow the target branch, so it is never the conflict endpoint.
    let live_base = match (repo, base_ref) {
        (Some(repo), Some(branch)) => bounded(limits.gh, provider.base_tip(repo, branch)).await,
        _ => Err(anyhow::anyhow!("PR repository or base branch name missing from probe")),
    };
    match &live_base {
        Ok(tip) => out.push_str(&format!(
            "Live tip of base branch `{}` = {tip} (PR's recorded base commit {} is its fork point, not the current tip).\n\n",
            base_ref.unwrap_or("?"),
            recorded_base.unwrap_or("?")
        )),
        Err(error) => out.push_str(&format!(
            "Base branch tip probe failed: {error:#}; conflict freshness unknown.\n\n"
        )),
    }

    if packet.item.blocked_reason.as_deref() == Some("merge_conflict")
        || packet.pr_mergeable.as_deref() == Some("CONFLICTING")
    {
        if let Ok(base) = &live_base {
            let diagnosis = match current_diagnosis(packet, base, head) {
                Some(stored) => Ok(stored),
                None => match conflict_remote(packet, url) {
                    Ok(remote) => bounded(limits.conflict, provider.conflict(base, head, &remote)).await,
                    Err(error) => Err(error),
                },
            };
            match diagnosis {
                Ok(json) => out.push_str(&format!(
                    "Conflict diagnosis for current base/head:\n```json\n{json}\n```\n\n"
                )),
                Err(error) => out.push_str(&format!("Conflict probe failed: {error:#}\n\n")),
            }
        } else {
            out.push_str("Conflict probe skipped: current base tip unknown.\n\n");
        }
    }
    let checks = pr["statusCheckRollup"].as_array().cloned().unwrap_or_default();
    for check in checks.iter().filter(|c| failed(c)) {
        let name = check_name(check).unwrap_or("(unnamed)");
        let log = match current_log(packet, head, check) {
            Some(stored) => Ok(stored),
            None => bounded(limits.gh, provider.log(check)).await,
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
        match repo {
            Some(repo) => match bounded(limits.gh, provider.main_checks(repo)).await {
                Ok(history) => render_main_history(&mut out, &checks, &history, recorded_base),
                Err(error) => out.push_str(&format!("Main history probe failed: {error:#}; attribution unknown.\n")),
            },
            None => out.push_str("Main history probe failed: unsupported PR URL; attribution unknown.\n"),
        }
    }
    out
}

fn render_main_history(out: &mut String, failing: &[Value], history: &Value, pr_base: Option<&str>) {
    let branch = &history["data"]["repository"]["defaultBranchRef"];
    let name = branch["name"].as_str().unwrap_or("(unknown branch)");
    out.push_str(&format!(
        "### Recent outcomes on main (repository default branch `{name}`, latest 10 commits sampled, newest first)\n\n"
    ));
    for check in failing.iter().filter(|c| failed(c)) {
        // Completed observations, newest first: `true` = failed.
        let mut completed: Vec<bool> = Vec::new();
        let mut base_outcome: Option<String> = None;
        let check_name = check_name(check).unwrap_or("(unnamed)");
        if let Some(commits) = branch["target"]["history"]["nodes"].as_array() {
            for commit in commits {
                let oid = commit["oid"].as_str().unwrap_or("?");
                if let Some(contexts) = commit["statusCheckRollup"]["contexts"]["nodes"].as_array() {
                    for context in contexts.iter().filter(|c| self::check_name(c) == Some(check_name)) {
                        let result = verdict(context);
                        if failed(context) {
                            completed.push(true);
                        } else if passed(context) {
                            completed.push(false);
                        }
                        if pr_base == Some(oid) {
                            base_outcome = Some(result.to_owned());
                        }
                        out.push_str(&format!(
                            "- `{check_name}` branch={name} commit={oid} committed_at={} check_time={} outcome={result}\n",
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
        // Recency decides: the newest completed observation is main's state now.
        let attribution = match completed.first() {
            Some(true) => "main also red: likely unrelated or flaky",
            Some(false) if completed.iter().any(|f| *f) => {
                "main failed earlier in the sample but is green at its tip: fixed on main, so the failure is PR-specific or the PR needs a rebase onto the fix"
            }
            Some(false) => "observed main green: PR-specific failure possible",
            None => "main unobserved: attribution unknown",
        };
        out.push_str(&format!("`{check_name}`: {attribution}.\n"));
        if let Some(outcome) = base_outcome {
            out.push_str(&format!(
                "`{check_name}` at the PR's base commit {}: {outcome}.\n",
                pr_base.unwrap_or("?")
            ));
        }
        out.push('\n');
    }
}
