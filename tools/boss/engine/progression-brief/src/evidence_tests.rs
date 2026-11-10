//! Provider-boundary regressions: no host git, network or credentials.
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::evidence::{EvidenceProvider, gather_evidence};
use crate::{ProgressionBriefPacket, gather_progression_brief};

#[derive(bon::Builder)]
struct Probe {
    main: Value,
    #[builder(default)]
    conflicts: AtomicUsize,
    #[builder(default)]
    logs: AtomicUsize,
    #[builder(default)]
    fail: bool,
    /// Live tip of the target branch (independent of the PR's recorded base).
    #[builder(default = "base".to_owned())]
    tip: String,
    #[builder(default)]
    tip_fail: bool,
    #[builder(default = "base".to_owned())]
    recorded_base: String,
    #[builder(default = "FAILURE".to_owned())]
    conclusion: String,
    #[builder(default)]
    hang: bool,
    #[builder(default)]
    hang_conflict: bool,
    #[builder(default)]
    conflict_bases: std::sync::Mutex<Vec<String>>,
    #[builder(default)]
    conflict_remotes: std::sync::Mutex<Vec<String>>,
}

impl EvidenceProvider for Probe {
    async fn pr(&self, _: &str) -> Result<Value> {
        if self.hang {
            std::future::pending::<()>().await;
        }
        Ok(
            json!({"baseRefName":"main", "baseRefOid":self.recorded_base, "headRefOid":"head", "statusCheckRollup":[
            {"name":"build", "conclusion":self.conclusion, "detailsUrl":"https://github.com/o/r/actions/runs/1/job/2"}]}),
        )
    }
    async fn base_tip(&self, repo: &str, branch: &str) -> Result<String> {
        assert_eq!((repo, branch), ("o/r", "main"));
        if self.tip_fail {
            bail!("ref lookup unavailable");
        }
        Ok(self.tip.clone())
    }
    async fn main_checks(&self, _: &str) -> Result<Value> {
        Ok(self.main.clone())
    }
    async fn conflict(&self, base: &str, head: &str, remote: &str) -> Result<String> {
        self.conflicts.fetch_add(1, Ordering::SeqCst);
        self.conflict_remotes.lock().unwrap().push(remote.to_owned());
        if self.hang_conflict {
            std::future::pending::<()>().await;
        }
        self.conflict_bases.lock().unwrap().push(base.to_owned());
        if self.fail {
            bail!("conflict service unavailable");
        }
        Ok(json!({"base_sha":base, "head_sha":head, "files":[{"path":"fresh.rs"}]}).to_string())
    }
    async fn log(&self, _: &Value) -> Result<String> {
        self.logs.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            bail!("log service unavailable");
        }
        Ok("fresh failing log".into())
    }
}

fn packet() -> (tempfile::TempDir, boss_work_db::WorkDb, ProgressionBriefPacket) {
    let (dir, db) = boss_work_db::test_support::open_db();
    let product = boss_work_db::test_support::create_test_product_named(&db, "Evidence");
    let item = boss_work_db::test_support::create_test_chore_manual(&db, &product.id, "Conflicted");
    db.connect().unwrap().execute("UPDATE tasks SET status='blocked', blocked_reason='merge_conflict', pr_url='https://github.com/o/r/pull/1' WHERE id=?1", [&item.id]).unwrap();
    let packet = gather_progression_brief(&db, &item.id).unwrap();
    (dir, db, packet)
}

fn probe(outcome: &str) -> Probe {
    Probe::builder()
        .main(
            json!({"data":{"repository":{"defaultBranchRef":{"name":"main","target":{"history":{"nodes":[
                {"oid":"main-sha","committedDate":"2026-11-10T12:00:00Z","statusCheckRollup":{"contexts":{"nodes":[
                    {"context":"build","state":outcome,"createdAt":"2026-11-10T12:01:00Z"}]}}}
            ]}}}}}}),
        )
        .build()
}

#[tokio::test]
async fn missing_attempts_probe_both_providers_and_keep_main_provenance() {
    let (_dir, _db, packet) = packet();
    let provider = probe("FAILURE");
    let text = gather_evidence(&packet, &provider).await;
    assert_eq!(provider.conflicts.load(Ordering::SeqCst), 1);
    assert_eq!(provider.logs.load(Ordering::SeqCst), 1);
    for word in [
        "fresh.rs",
        "fresh failing log",
        "branch=main commit=main-sha",
        "check_time=2026-11-10T12:01:00Z",
        "main also red: likely unrelated or flaky",
    ] {
        assert!(text.contains(word), "{text}");
    }
}

#[tokio::test]
async fn observed_main_green_and_missing_main_are_distinct() {
    let (_dir, _db, packet) = packet();
    let mut provider = probe("SUCCESS");
    assert!(
        gather_evidence(&packet, &provider)
            .await
            .contains("observed main green: PR-specific failure possible")
    );
    provider.main = json!({});
    let text = gather_evidence(&packet, &provider).await;
    assert!(text.contains("main unobserved: attribution unknown"));
    assert!(!text.contains("observed main green"));
}

#[tokio::test]
async fn matching_attempts_are_reused_but_missing_or_stale_evidence_is_probed() {
    let (_dir, db, mut packet) = packet();
    let conflict = db
        .insert_conflict_resolution(
            boss_work_db::ConflictResolutionInsertInput::builder()
                .product_id(&packet.item.product_id)
                .work_item_id(&packet.item.id)
                .pr_url("https://github.com/o/r/pull/1")
                .pr_number(1)
                .head_branch("feature")
                .base_branch("main")
                .base_sha_at_trigger("base")
                .head_sha_before("head")
                .build(),
        )
        .unwrap()
        .unwrap();
    let ci = db
        .insert_ci_remediation(boss_work_db::CiRemediationInsertInput {
            product_id: packet.item.product_id.clone(),
            work_item_id: packet.item.id.clone(),
            pr_url: "https://github.com/o/r/pull/1".into(),
            pr_number: 1,
            head_branch: "feature".into(),
            head_sha_at_trigger: "head".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: json!([{"name":"build","provider_job_id":"2","target_url":"https://github.com/o/r/actions/runs/1/job/2"}])
                .to_string(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .unwrap();
    packet.conflict_attempts = db
        .list_conflict_resolutions(None, &[], Some(&packet.item.id), None)
        .unwrap();
    packet.ci_attempts = db.list_ci_remediations(None, &[], Some(&packet.item.id), None).unwrap();
    let provider = probe("SUCCESS");
    gather_evidence(&packet, &provider).await;
    assert_eq!(provider.conflicts.load(Ordering::SeqCst), 1);
    assert_eq!(provider.logs.load(Ordering::SeqCst), 1);
    assert_eq!(packet.conflict_attempts[0].id, conflict.id);
    assert_eq!(packet.ci_attempts[0].id, ci.id);
    packet.conflict_attempts[0].conflict_diagnosis =
        Some(json!({"base_sha":"base","head_sha":"head","files":[]}).to_string());
    packet.ci_attempts[0].log_excerpt = Some("stored exact job".into());
    let reused = gather_evidence(&packet, &provider).await;
    assert!(reused.contains("stored exact job"));
    assert_eq!(provider.conflicts.load(Ordering::SeqCst), 1);
    assert_eq!(provider.logs.load(Ordering::SeqCst), 1);
    let exact_checks = packet.ci_attempts[0].failed_checks.clone();
    packet.ci_attempts[0].failed_checks = json!([
        {"name":"other","provider_job_id":"other","target_url":"https://github.com/o/r/actions/runs/1/job/3"},
        {"name":"build","provider_job_id":"2","target_url":"https://github.com/o/r/actions/runs/1/job/2"}
    ])
    .to_string();
    assert!(gather_evidence(&packet, &provider).await.contains("fresh failing log"));
    packet.ci_attempts[0].failed_checks = exact_checks;
    for (base, head) in [("old-base", "head"), ("base", "old-head")] {
        packet.conflict_attempts[0].conflict_diagnosis =
            Some(json!({"base_sha":base,"head_sha":head,"files":[]}).to_string());
        packet.ci_attempts[0].head_sha_at_trigger = "old-head".into();
        let text = gather_evidence(&packet, &provider).await;
        assert!(text.contains("fresh.rs") && text.contains("fresh failing log"));
    }
    packet.ci_attempts[0].head_sha_at_trigger = "head".into();
    packet.ci_attempts[0].failed_checks = json!([{"name":"different-job"}]).to_string();
    assert!(gather_evidence(&packet, &provider).await.contains("fresh failing log"));
}

#[tokio::test]
async fn probe_errors_are_visible() {
    let (_dir, _db, packet) = packet();
    let mut provider = probe("PENDING");
    provider.fail = true;
    let text = gather_evidence(&packet, &provider).await;
    assert!(text.contains("Conflict probe failed: conflict service unavailable"));
    assert!(text.contains("Log probe failed for `build` at head head: log service unavailable"));
    assert!(text.contains("attribution unknown"));
}

#[tokio::test]
async fn advanced_target_branch_rejects_stored_diagnosis_even_when_recorded_base_is_unchanged() {
    let (_dir, db, mut packet) = packet();
    db.insert_conflict_resolution(
        boss_work_db::ConflictResolutionInsertInput::builder()
            .product_id(&packet.item.product_id)
            .work_item_id(&packet.item.id)
            .pr_url("https://github.com/o/r/pull/1")
            .pr_number(1)
            .head_branch("feature")
            .base_branch("main")
            .base_sha_at_trigger("old")
            .head_sha_before("head")
            .build(),
    )
    .unwrap()
    .unwrap();
    packet.conflict_attempts = db
        .list_conflict_resolutions(None, &[], Some(&packet.item.id), None)
        .unwrap();
    // Stored diagnosis matches the PR's fixed recorded base and unchanged head.
    packet.conflict_attempts[0].conflict_diagnosis =
        Some(json!({"base_sha":"old","head_sha":"head","files":[]}).to_string());
    let mut provider = probe("SUCCESS");
    provider.recorded_base = "old".into();
    provider.tip = "new-tip".into();
    let text = gather_evidence(&packet, &provider).await;
    assert_eq!(*provider.conflict_bases.lock().unwrap(), ["new-tip"]);
    assert!(text.contains("Live tip of base branch `main` = new-tip"), "{text}");
    assert!(text.contains("fresh.rs"), "{text}");
}

#[tokio::test]
async fn unresolvable_base_tip_makes_conflict_evidence_unknown_not_recorded_base() {
    let (_dir, _db, packet) = packet();
    let mut provider = probe("SUCCESS");
    provider.tip_fail = true;
    let text = gather_evidence(&packet, &provider).await;
    assert_eq!(provider.conflicts.load(Ordering::SeqCst), 0);
    assert!(
        text.contains("Base branch tip probe failed: ref lookup unavailable"),
        "{text}"
    );
    assert!(
        text.contains("Conflict probe skipped: current base tip unknown"),
        "{text}"
    );
}

#[tokio::test]
async fn cancelled_and_stale_checks_get_log_and_main_history_probes() {
    for conclusion in ["CANCELLED", "STALE"] {
        let (_dir, _db, packet) = packet();
        let mut provider = probe("FAILURE");
        provider.conclusion = conclusion.into();
        let text = gather_evidence(&packet, &provider).await;
        assert_eq!(provider.logs.load(Ordering::SeqCst), 1, "{conclusion}: {text}");
        assert!(text.contains("fresh failing log"), "{conclusion}: {text}");
        assert!(text.contains("Recent outcomes on main"), "{conclusion}: {text}");

        let mut failing = probe("FAILURE");
        failing.conclusion = conclusion.into();
        failing.fail = true;
        let text = gather_evidence(&packet, &failing).await;
        assert!(text.contains("Log probe failed for `build`"), "{conclusion}: {text}");
    }
}

fn history(outcomes: &[&str]) -> Value {
    let nodes: Vec<Value> = outcomes
        .iter()
        .enumerate()
        .map(|(i, outcome)| {
            json!({"oid":format!("c{i}"),"committedDate":"2026-11-10T12:00:00Z","statusCheckRollup":{"contexts":{"nodes":[
                {"context":"build","state":outcome,"createdAt":"2026-11-10T12:01:00Z"}]}}})
        })
        .collect();
    json!({"data":{"repository":{"defaultBranchRef":{"name":"main","target":{"history":{"nodes":nodes}}}}}})
}

#[tokio::test]
async fn main_attribution_follows_the_newest_completed_observation() {
    let (_dir, _db, packet) = packet();
    let mut provider = probe("SUCCESS");
    // Newest first: green now, red nine commits ago.
    provider.main = history(&["SUCCESS", "SUCCESS", "FAILURE"]);
    let text = gather_evidence(&packet, &provider).await;
    assert!(text.contains("green at its tip: fixed on main"), "{text}");
    assert!(!text.contains("likely unrelated or flaky"), "{text}");

    provider.main = history(&["FAILURE", "SUCCESS"]);
    assert!(
        gather_evidence(&packet, &provider)
            .await
            .contains("main also red: likely unrelated or flaky")
    );

    provider.main = history(&["PENDING", "SUCCESS"]);
    let text = gather_evidence(&packet, &provider).await;
    assert!(text.contains("observed main green"), "{text}");

    provider.main = history(&["PENDING"]);
    assert!(
        gather_evidence(&packet, &provider)
            .await
            .contains("main unobserved: attribution unknown")
    );
}

#[tokio::test]
async fn pr_base_commit_outcome_is_named_when_sampled() {
    let (_dir, _db, packet) = packet();
    let mut provider = probe("SUCCESS");
    provider.recorded_base = "c1".into();
    provider.main = history(&["SUCCESS", "FAILURE"]);
    let text = gather_evidence(&packet, &provider).await;
    assert!(text.contains("at the PR's base commit c1: FAILURE"), "{text}");
}

#[tokio::test]
async fn hung_providers_yield_timeout_lines_instead_of_hanging() {
    use crate::evidence::{ProbeLimits, gather_evidence_with};
    let (_dir, _db, packet) = packet();
    let mut provider = probe("SUCCESS");
    provider.hang = true;
    let limits = ProbeLimits {
        gh: std::time::Duration::from_millis(50),
        conflict: std::time::Duration::from_millis(50),
    };
    let text = gather_evidence_with(&packet, &provider, limits).await;
    assert!(text.contains("PR probe failed: probe timed out"), "{text}");
}

#[tokio::test]
async fn hung_conflict_probe_is_reported_and_later_probes_still_run() {
    use crate::evidence::{ProbeLimits, gather_evidence_with};
    let (_dir, _db, packet) = packet();
    let mut provider = probe("FAILURE");
    provider.hang_conflict = true;
    let limits = ProbeLimits {
        gh: std::time::Duration::from_secs(30),
        conflict: std::time::Duration::from_millis(50),
    };
    let text = gather_evidence_with(&packet, &provider, limits).await;
    assert!(
        text.contains("Conflict probe failed: probe timed out after 50ms"),
        "{text}"
    );
    assert!(text.contains("fresh failing log"), "{text}");
}

#[tokio::test]
async fn conflict_probe_uses_the_recorded_remote_else_the_pr_repository() {
    let (_dir, _db, mut packet) = packet();
    packet.repo_remote_url = None;
    let provider = probe("FAILURE");
    gather_evidence(&packet, &provider).await;
    assert_eq!(
        *provider.conflict_remotes.lock().unwrap(),
        ["https://github.com/o/r.git"]
    );

    packet.repo_remote_url = Some("git@example.com:team/mirror.git".into());
    let provider = probe("FAILURE");
    gather_evidence(&packet, &provider).await;
    assert_eq!(
        *provider.conflict_remotes.lock().unwrap(),
        ["git@example.com:team/mirror.git"]
    );
}

#[test]
fn conflict_remote_rejects_a_pr_url_without_a_pull_segment() {
    let (_dir, _db, mut packet) = packet();
    packet.repo_remote_url = None;
    let err = crate::evidence::conflict_remote(&packet, "https://github.com/o/r/issues/1").unwrap_err();
    assert!(err.to_string().contains("invalid PR URL"), "{err}");
}

#[test]
fn gather_resolves_remote_from_item_then_product_and_drops_blank() {
    let (_dir, db, packet) = packet();
    let id = packet.item.id.clone();
    let product_id = packet.item.product_id.clone();
    let quote = |value: Option<&str>| value.map_or("NULL".to_owned(), |v| format!("'{v}'"));
    let set = |item: Option<&str>, product: Option<&str>| {
        db.connect()
            .unwrap()
            .execute_batch(&format!(
                "UPDATE tasks SET repo_remote_url = {} WHERE id = '{id}';
                 UPDATE products SET repo_remote_url = {} WHERE id = '{product_id}';",
                quote(item),
                quote(product),
            ))
            .unwrap();
        gather_progression_brief(&db, &id).unwrap().repo_remote_url
    };
    assert_eq!(
        set(Some("item-remote"), Some("product-remote")).as_deref(),
        Some("item-remote")
    );
    assert_eq!(set(None, Some("product-remote")).as_deref(), Some("product-remote"));
    assert_eq!(set(None, Some("   ")), None);
    assert_eq!(set(Some("  "), None), None);
    assert_eq!(
        set(Some("  "), Some("product-remote")).as_deref(),
        Some("product-remote")
    );
}
