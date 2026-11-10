//! Provider-boundary regressions: no host git, network or credentials.
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::evidence::{EvidenceProvider, gather_evidence};
use crate::{ProgressionBriefPacket, gather_progression_brief};

struct Probe {
    main: Value,
    conflicts: AtomicUsize,
    logs: AtomicUsize,
    fail: bool,
}

impl EvidenceProvider for Probe {
    async fn pr(&self, _: &str) -> Result<Value> {
        Ok(json!({"baseRefOid":"base", "headRefOid":"head", "statusCheckRollup":[
            {"name":"build", "conclusion":"FAILURE", "detailsUrl":"https://github.com/o/r/actions/runs/1/job/2"}]}))
    }
    async fn main_checks(&self, _: &str) -> Result<Value> {
        Ok(self.main.clone())
    }
    async fn conflict(&self, base: &str, head: &str, _: &str) -> Result<String> {
        self.conflicts.fetch_add(1, Ordering::SeqCst);
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
    Probe {
        main: json!({"data":{"repository":{"defaultBranchRef":{"name":"main","target":{"history":{"nodes":[
            {"oid":"main-sha","committedDate":"2026-11-10T12:00:00Z","statusCheckRollup":{"contexts":{"nodes":[
                {"context":"build","state":outcome,"createdAt":"2026-11-10T12:01:00Z"}]}}}
        ]}}}}}}),
        conflicts: AtomicUsize::new(0),
        logs: AtomicUsize::new(0),
        fail: false,
    }
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
