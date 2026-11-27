//! Snapshot query behind `boss cost review-yield`.
//!
//! Two set-based reads over the review batches created in a window, handed to
//! the pure builder in `boss_pr_review` as [`YieldBatch`]es:
//!
//! 1. the batches, each with its applied verdict row and the remediation task
//!    that row points at (its kind, status, whether it changed code, whether
//!    the PR it belongs to is recorded as merged);
//! 2. the batches' members, each with the experiment arm set recorded on its
//!    execution and the findings in its accepted report or verdict.
//!
//! Every join is on a primary key or an indexed column, and no statement walks
//! a table that is not already limited to the window. Stored payloads are
//! reduced to their findings as they are read, so a window's reports are never
//! held in memory at once. Findings are the only part of a payload read.

use std::collections::HashMap;

use super::*;
use boss_pr_review::{
    RemediationKind, YieldBatch, YieldMember, YieldRemediation, YieldVerdict, findings_from_report_payload,
    findings_from_verdict_payload,
};
use boss_protocol::{
    ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase, ReviewClassification, ReviewYieldReport,
    ReviewYieldRequest,
};

/// The window predicate. `created_at` is stored as epoch-second text.
const IN_WINDOW: &str = "CAST(b.created_at AS INTEGER) >= ?1 AND CAST(b.created_at AS INTEGER) < ?2";

/// The batches in a window, in creation order.
///
/// `prior_verdicts` is how many earlier pre-merge verdicts, other than
/// stale-head ones, the same PR has: the number of review rounds already
/// decided, which is what the review-cycle cap counts. It looks back past the
/// window, so a batch's round does not depend on where the window starts.
///
/// `CROSS JOIN` fixes the loop order: the PR's own batches by `cycle_root_id`,
/// then their verdicts by `batch_id`. Left to choose, the planner walks every
/// verdict once per batch.
///
/// A remediation is a revision when its task is still a `revision`, otherwise
/// a follow-up. A revision converted to a follow-up when its PR merged first
/// is read as a follow-up. A revision changed code when one of its executions
/// recorded a different PR head after than before; a follow-up when it opened
/// a PR. A revision is merged when the PR it was pushed to is; a follow-up
/// when its own PR is. Merge evidence is the confirmed-merge journal, so a
/// merge from before that journal existed reads as not merged.
fn batches_sql() -> String {
    let pre_merge = ReviewBatchPhase::PreMerge.as_str();
    let stale_head = super::review_verdicts::REVIEW_GATE_OUTCOME_STALE_HEAD;
    let revision = TaskKind::Revision.as_str();
    format!(
        "SELECT b.id, b.phase, b.classification_json,
                (SELECT COUNT(*)
                   FROM pr_review_batches earlier
                   CROSS JOIN pr_review_verdicts earlier_verdict ON earlier_verdict.batch_id = earlier.id
                  WHERE earlier.cycle_root_id = b.cycle_root_id
                    AND earlier.phase = '{pre_merge}'
                    AND earlier_verdict.gate_outcome <> '{stale_head}'
                    AND (CAST(earlier.created_at AS INTEGER) < CAST(b.created_at AS INTEGER)
                         OR (CAST(earlier.created_at AS INTEGER) = CAST(b.created_at AS INTEGER)
                             AND earlier.id < b.id))),
                v.gate_outcome, v.revision_warranted, v.revision_task_id,
                t.kind, t.status, t.pr_url,
                EXISTS (SELECT 1 FROM pr_merge_evidence e
                         WHERE e.root_task_id = CASE WHEN t.kind = '{revision}' THEN b.cycle_root_id ELSE t.id END),
                EXISTS (SELECT 1 FROM work_executions x
                         WHERE x.work_item_id = t.id
                           AND x.pr_head_before IS NOT NULL AND x.pr_head_after IS NOT NULL
                           AND x.pr_head_before <> x.pr_head_after)
           FROM pr_review_batches b
           LEFT JOIN pr_review_verdicts v ON v.batch_id = b.id
           LEFT JOIN tasks t ON t.id = v.revision_task_id
          WHERE {IN_WINDOW}
          ORDER BY CAST(b.created_at AS INTEGER), b.id"
    )
}

fn members_sql() -> String {
    format!(
        "SELECT m.batch_id, m.role, m.status, we.experiment_arms, p.payload_json
           FROM pr_review_batches b
           CROSS JOIN pr_review_batch_members m ON m.batch_id = b.id
           LEFT JOIN work_executions we ON we.id = m.execution_id
           LEFT JOIN worker_proposals p ON p.id = m.report_proposal_id
          WHERE {IN_WINDOW}
          ORDER BY m.batch_id, m.role, m.attempt"
    )
}

fn parse_stored<T: std::str::FromStr<Err = String>>(text: &str) -> Result<T> {
    text.parse().map_err(anyhow::Error::msg)
}

pub(crate) fn review_yield_batches_in(
    conn: &Connection,
    since_epoch_s: i64,
    until_epoch_s: i64,
) -> Result<Vec<YieldBatch>> {
    let mut batches: Vec<YieldBatch> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    let mut statement = conn.prepare(&batches_sql())?;
    let mut rows = statement.query(params![since_epoch_s, until_epoch_s])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let phase: ReviewBatchPhase = parse_stored(&row.get::<_, String>(1)?)?;
        let classification: Option<ReviewClassification> = serde_json::from_str(&row.get::<_, String>(2)?).ok();
        let prior_verdicts: i64 = row.get(3)?;
        let gate_outcome: Option<String> = row.get(4)?;
        let warranted: Option<i64> = row.get(5)?;
        let remediation_task: Option<String> = row.get(6)?;
        let task_kind: Option<String> = row.get(7)?;
        let task_status: Option<String> = row.get(8)?;
        let task_pr_url: Option<String> = row.get(9)?;
        let merged: bool = row.get(10)?;
        let heads_changed: bool = row.get(11)?;

        let remediation = remediation_task.as_ref().and(task_kind.as_deref()).map(|kind| {
            let revision = kind == TaskKind::Revision.as_str();
            YieldRemediation {
                kind: if revision {
                    RemediationKind::Revision
                } else {
                    RemediationKind::FollowUp
                },
                open: task_status
                    .as_deref()
                    .and_then(|status| status.parse::<TaskStatus>().ok())
                    .is_none_or(|status| !status.is_terminal()),
                changed_code: if revision {
                    heads_changed
                } else {
                    task_pr_url.as_deref().is_some_and(|url| !url.is_empty())
                },
                merged,
            }
        });
        let verdict = gate_outcome.map(|gate_outcome| YieldVerdict {
            gate_outcome,
            engine_revision_warranted: warranted.unwrap_or(0) != 0,
            remediation,
        });
        positions.insert(id, batches.len());
        batches.push(
            YieldBatch::builder()
                .phase(phase)
                .round(u32::try_from(prior_verdicts).unwrap_or(0).saturating_add(1))
                .maybe_classification(classification)
                .maybe_verdict(verdict)
                .build(),
        );
    }
    drop(rows);
    drop(statement);

    let mut statement = conn.prepare(&members_sql())?;
    let mut rows = statement.query(params![since_epoch_s, until_epoch_s])?;
    while let Some(row) = rows.next()? {
        let batch_id: String = row.get(0)?;
        let Some(&position) = positions.get(&batch_id) else {
            continue;
        };
        let role: ReviewBatchMemberRole = parse_stored(&row.get::<_, String>(1)?)?;
        let status: ReviewBatchMemberStatus = parse_stored(&row.get::<_, String>(2)?)?;
        let experiment_arms: Option<String> = row.get(3)?;
        let payload: Option<String> = row.get(4)?;
        // A deciding member's accepted submission is a verdict; a leaf's is a
        // report. Either is unreadable when its payload is missing or does not
        // parse, which the builder counts rather than reading as no findings.
        let findings = match (status, payload.as_deref()) {
            (ReviewBatchMemberStatus::Reported, Some(payload)) => match role {
                ReviewBatchMemberRole::Supervisor | ReviewBatchMemberRole::PostMergeReviewer => {
                    findings_from_verdict_payload(payload)
                }
                _ => findings_from_report_payload(payload),
            },
            _ => None,
        };
        batches[position].members.push(YieldMember {
            role,
            status,
            experiment_arms,
            findings,
        });
    }
    Ok(batches)
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// The review batches created in `[since_epoch_s, until_epoch_s)` with
    /// what the yield report reads from them. See the module docs for what is
    /// read and how.
    pub fn review_yield_batches(&self, since_epoch_s: i64, until_epoch_s: i64) -> Result<Vec<YieldBatch>> {
        let conn = self.connect()?;
        review_yield_batches_in(&conn, since_epoch_s, until_epoch_s)
    }

    /// The yield report for `request`: the window's batches read once, then
    /// handed to the pure builder.
    pub fn review_yield_report(
        &self,
        request: &ReviewYieldRequest,
        generated_at_epoch_s: i64,
    ) -> Result<ReviewYieldReport> {
        let batches = self.review_yield_batches(request.since_epoch_s, request.until_epoch_s)?;
        Ok(boss_pr_review::build_review_yield_report(
            request,
            generated_at_epoch_s,
            &batches,
        ))
    }
}

#[cfg(test)]
mod tests {
    use boss_pr_review::{PrReviewMetadata, classify_pr_review_metadata};

    use super::*;
    use crate::test_support::{create_test_chore_manual, create_test_product, open_db};

    const REPORT: &str = r#"{"batch_id":"b","target_sha":"s","report":{"findings":[
        {"severity":"high","category":"correctness"},{"severity":"low","category":"readability"}]}}"#;
    const VERDICT: &str = r#"{"batch_id":"b","verdict":{"findings":[
        {"severity":"low","category":"regression"}]}}"#;

    fn classification(additions: i64) -> String {
        serde_json::to_string(&classify_pr_review_metadata(&PrReviewMetadata {
            additions: Some(additions),
            changed_files: Some(vec!["src/lib.rs".to_owned()]),
            deletions: Some(0),
        }))
        .unwrap()
    }

    struct Fixture {
        db: WorkDb,
        _dir: tempfile::TempDir,
        root: String,
    }

    impl Fixture {
        fn new() -> Self {
            let (dir, db) = open_db();
            let product = create_test_product(&db);
            let root = create_test_chore_manual(&db, product.id, "reviewed").id;
            Self { db, _dir: dir, root }
        }

        fn sql(&self, sql: &str, params: impl rusqlite::Params) {
            self.db.connect().unwrap().execute(sql, params).unwrap();
        }

        fn execution(&self, id: &str, work_item: &str, arms: Option<&str>) {
            self.sql(
                "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at, experiment_arms)
                 VALUES (?1, ?2, 'pr_review', 'completed', 'u', '1', ?3)",
                params![id, work_item, arms],
            );
        }

        fn proposal(&self, id: &str, execution: &str, kind: &str, payload: &str) {
            self.sql(
                "INSERT INTO worker_proposals (id, execution_id, work_item_id, kind, payload_json,
                                               idempotency_key, state, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?1, 'applied', '1')",
                params![id, execution, self.root, kind, payload],
            );
        }

        fn batch(&self, id: &str, phase: &str, created_at: i64, classification: &str) {
            self.sql(
                "INSERT INTO pr_review_batches (id, cycle_root_id, base_sha, classification_json, created_at, phase,
                                                pr_number, pr_url, status, target_sha, updated_at)
                 VALUES (?1, ?2, 'base', ?3, ?4, ?5, 7, 'https://github.com/o/r/pull/7', 'completed', ?1, ?4)",
                params![id, self.root, classification, created_at.to_string(), phase],
            );
        }

        fn member(
            &self,
            id: &str,
            batch: &str,
            role: &str,
            status: &str,
            execution: Option<&str>,
            proposal: Option<&str>,
        ) {
            self.sql(
                "INSERT INTO pr_review_batch_members (id, batch_id, attempt, created_at, provider_effort,
                        requested_driver, resolved_model, role, status, updated_at, execution_id, report_proposal_id)
                 VALUES (?1, ?2, 1, '1', 'high', 'claude', 'm', ?3, ?4, '1', ?5, ?6)",
                params![id, batch, role, status, execution, proposal],
            );
        }

        fn verdict(
            &self,
            id: &str,
            execution: &str,
            batch: &str,
            outcome: &str,
            warranted: bool,
            remediation: Option<&str>,
        ) {
            self.sql(
                "INSERT INTO pr_review_verdicts (id, execution_id, work_item_id, findings_count, revision_warranted,
                                                 gate_outcome, revision_task_id, created_at, batch_id)
                 VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, '1', ?7)",
                params![
                    id,
                    execution,
                    self.root,
                    i64::from(warranted),
                    outcome,
                    remediation,
                    batch
                ],
            );
        }

        fn task(&self, id: &str, kind: &str, status: &str, pr_url: Option<&str>) {
            self.sql(
                "INSERT INTO tasks (id, product_id, kind, name, status, created_at, updated_at, pr_url, parent_task_id)
                 SELECT ?1, product_id, ?2, ?1, ?3, '1', '1', ?4, CASE WHEN ?2 = 'revision' THEN ?5 END
                   FROM tasks WHERE id = ?5",
                params![id, kind, status, pr_url, self.root],
            );
        }

        fn batches(&self, since: i64, until: i64) -> Vec<YieldBatch> {
            self.db.review_yield_batches(since, until).unwrap()
        }
    }

    #[test]
    fn a_window_reads_its_batches_in_order_with_rounds_counted_past_its_start() {
        let f = Fixture::new();
        f.execution("exec_sup", &f.root, None);
        for (id, at, outcome) in [
            ("b1", 1_000, "completed_with_findings"),
            ("b2", 2_000, "stale_head"),
            ("b3", 3_000, "completed_clean"),
        ] {
            f.batch(id, "pre_merge", at, &classification(10));
            f.verdict(&format!("v_{id}"), "exec_sup", id, outcome, false, None);
        }
        f.batch("b_out", "pre_merge", 9_000, &classification(10));

        let all = f.batches(0, 5_000);
        assert_eq!(all.len(), 3, "b_out is past the window");
        // b2's round counts b1; b3's round counts b1 but not the stale-head b2.
        assert_eq!(all.iter().map(|b| b.round).collect::<Vec<_>>(), [1, 2, 2]);

        // The window starts after b1; rounds still count it.
        let later = f.batches(1_500, 5_000);
        assert_eq!(later.iter().map(|b| b.round).collect::<Vec<_>>(), [2, 2]);
        assert_eq!(
            later[0].verdict.as_ref().map(|v| v.gate_outcome.as_str()),
            Some("stale_head")
        );
        assert!(f.batches(5_000, 6_000).is_empty());
        assert_eq!(f.batches(0, 1_000).len(), 0, "the window is half-open at its end");
        assert_eq!(f.batches(1_000, 1_001).len(), 1, "and closed at its start");
    }

    #[test]
    fn members_carry_their_arms_and_the_findings_of_what_they_submitted() {
        let f = Fixture::new();
        f.execution("exec_claude", &f.root, Some(r#"{"exp":"treatment"}"#));
        f.execution("exec_sup", &f.root, Some(r#"{"exp":"treatment"}"#));
        f.execution("exec_codex", &f.root, None);
        f.proposal("p_claude", "exec_claude", "review_report", REPORT);
        f.proposal("p_sup", "exec_sup", "review_verdict", VERDICT);
        f.batch("b1", "pre_merge", 1_000, &classification(10));
        f.member(
            "m_claude",
            "b1",
            "claude_reviewer",
            "reported",
            Some("exec_claude"),
            Some("p_claude"),
        );
        f.member("m_codex", "b1", "codex_reviewer", "failed", Some("exec_codex"), None);
        f.member("m_sup", "b1", "supervisor", "reported", Some("exec_sup"), Some("p_sup"));
        // Reported, but its proposal row is gone: unreadable, not empty.
        f.member("m_grok", "b1", "grok_reviewer", "reported", None, Some("p_missing"));

        let batches = f.batches(0, 2_000);
        let members = &batches[0].members;
        let by_role = |role| members.iter().find(|m| m.role == role).unwrap();

        let claude = by_role(ReviewBatchMemberRole::ClaudeReviewer);
        assert_eq!(claude.experiment_arms.as_deref(), Some(r#"{"exp":"treatment"}"#));
        let found: Vec<_> = claude
            .findings
            .as_ref()
            .unwrap()
            .iter()
            .map(|x| format!("{:?}", x.severity))
            .collect();
        assert_eq!(found, ["High", "Low"], "a leaf's findings come from its report");

        let codex = by_role(ReviewBatchMemberRole::CodexReviewer);
        assert_eq!(
            (codex.status, codex.experiment_arms.as_deref(), codex.findings.is_none()),
            (ReviewBatchMemberStatus::Failed, None, true)
        );

        let supervisor = by_role(ReviewBatchMemberRole::Supervisor);
        assert_eq!(
            supervisor.findings.as_ref().unwrap().len(),
            1,
            "a supervisor's come from its verdict"
        );

        let grok = by_role(ReviewBatchMemberRole::GrokReviewer);
        assert!(grok.findings.is_none() && grok.experiment_arms.is_none());
    }

    #[test]
    fn a_revision_changed_code_when_its_pr_head_moved_and_merged_when_its_parent_pr_did() {
        let f = Fixture::new();
        f.execution("exec_sup", &f.root, None);
        f.task("rev_pushed", "revision", "done", None);
        f.task("rev_noop", "revision", "done", None);
        f.task("rev_open", "revision", "active", None);
        for (id, before, after) in [("x1", "aaa", "bbb"), ("x2", "aaa", "aaa"), ("x3", "aaa", "")] {
            let work_item = match id {
                "x1" => "rev_pushed",
                "x2" => "rev_noop",
                _ => "rev_open",
            };
            f.sql(
                "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at, pr_head_before, pr_head_after)
                 VALUES (?1, ?2, 'chore_implementation', 'completed', 'u', '1', ?3, NULLIF(?4, ''))",
                params![id, work_item, before, after],
            );
        }
        f.sql(
            "INSERT INTO pr_merge_evidence (root_task_id, canonical_pr_url, observed_at) VALUES (?1, 'pr', '1')",
            [&f.root],
        );
        for (batch, at, revision) in [
            ("b1", 1_000, "rev_pushed"),
            ("b2", 2_000, "rev_noop"),
            ("b3", 3_000, "rev_open"),
        ] {
            f.batch(batch, "pre_merge", at, &classification(10));
            f.verdict(
                &format!("v_{batch}"),
                "exec_sup",
                batch,
                "completed_with_findings",
                true,
                Some(revision),
            );
        }

        let remediations: Vec<_> = f
            .batches(0, 5_000)
            .into_iter()
            .map(|b| b.verdict.unwrap().remediation.unwrap())
            .collect();
        assert!(remediations.iter().all(|r| r.kind == RemediationKind::Revision));
        // Pushed: head moved. No-op: head unchanged. Open: after-head never recorded.
        assert_eq!(
            remediations.iter().map(|r| r.changed_code).collect::<Vec<_>>(),
            [true, false, false]
        );
        assert_eq!(
            remediations.iter().map(|r| r.open).collect::<Vec<_>>(),
            [false, false, true]
        );
        // Every revision belongs to the one PR, which merged.
        assert!(remediations.iter().all(|r| r.merged));
    }

    #[test]
    fn a_follow_up_changed_code_when_it_opened_a_pr_and_merged_when_that_pr_did() {
        let f = Fixture::new();
        f.execution("exec_post", &f.root, None);
        f.task("fu_merged", "followup", "done", Some("https://github.com/o/r/pull/20"));
        f.task(
            "fu_open_pr",
            "followup",
            "in_review",
            Some("https://github.com/o/r/pull/21"),
        );
        f.task("fu_nothing", "chore", "done", None);
        f.sql(
            "INSERT INTO pr_merge_evidence (root_task_id, canonical_pr_url, observed_at) VALUES ('fu_merged', 'pr', '1')",
            [],
        );
        for (batch, at, followup) in [
            ("b1", 1_000, "fu_merged"),
            ("b2", 2_000, "fu_open_pr"),
            ("b3", 3_000, "fu_nothing"),
        ] {
            f.batch(batch, "post_merge", at, &classification(2_000));
            f.verdict(
                &format!("v_{batch}"),
                "exec_post",
                batch,
                "completed_with_findings",
                true,
                Some(followup),
            );
        }
        // A verdict that minted nothing has no remediation.
        f.batch("b4", "post_merge", 4_000, &classification(2_000));
        f.verdict("v_b4", "exec_post", "b4", "completed_clean", false, None);

        let found = f.batches(0, 9_000);
        let rem = |i: usize| found[i].verdict.as_ref().unwrap().remediation;
        let facts = |r: YieldRemediation| (r.kind, r.open, r.changed_code, r.merged);
        assert_eq!(facts(rem(0).unwrap()), (RemediationKind::FollowUp, false, true, true));
        assert_eq!(facts(rem(1).unwrap()), (RemediationKind::FollowUp, true, true, false));
        assert_eq!(facts(rem(2).unwrap()), (RemediationKind::FollowUp, false, false, false));
        assert!(rem(3).is_none());
        assert!(found.iter().all(|b| b.phase == ReviewBatchPhase::PostMerge));
        assert!(found[3].verdict.as_ref().is_some_and(|v| !v.engine_revision_warranted));
    }

    #[test]
    fn an_unreadable_classification_is_kept_as_none_and_a_batch_with_no_verdict_has_none() {
        let f = Fixture::new();
        f.batch("b1", "pre_merge", 1_000, "{ not a classification");
        let batches = f.batches(0, 2_000);
        assert_eq!(batches.len(), 1);
        assert!(batches[0].classification.is_none());
        assert!(batches[0].verdict.is_none());
        assert!(batches[0].members.is_empty());
    }

    #[test]
    fn the_report_is_built_from_what_the_window_stored() {
        let f = Fixture::new();
        f.execution("exec_claude", &f.root, Some(r#"{"exp":"control"}"#));
        f.execution("exec_sup", &f.root, Some(r#"{"exp":"control"}"#));
        f.proposal("p_claude", "exec_claude", "review_report", REPORT);
        f.proposal("p_sup", "exec_sup", "review_verdict", VERDICT);
        f.task("rev", "revision", "done", None);
        f.sql(
            "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at, pr_head_before, pr_head_after)
             VALUES ('x_rev', 'rev', 'chore_implementation', 'completed', 'u', '1', 'aaa', 'bbb')",
            [],
        );
        f.sql(
            "INSERT INTO pr_merge_evidence (root_task_id, canonical_pr_url, observed_at) VALUES (?1, 'pr', '1')",
            [&f.root],
        );
        f.batch("b1", "pre_merge", 1_000, &classification(10));
        f.member(
            "m_claude",
            "b1",
            "claude_reviewer",
            "reported",
            Some("exec_claude"),
            Some("p_claude"),
        );
        f.member("m_sup", "b1", "supervisor", "reported", Some("exec_sup"), Some("p_sup"));
        f.verdict("v1", "exec_sup", "b1", "completed_with_findings", true, Some("rev"));

        let request = ReviewYieldRequest {
            since_epoch_s: 0,
            until_epoch_s: 2_000,
            experiment: Some("exp".to_owned()),
        };
        let report = f.db.review_yield_report(&request, 5_000).unwrap();

        assert_eq!(report.generated_at_epoch_s, 5_000);
        assert_eq!(report.arms.len(), 1);
        let arm = &report.arms[0];
        assert_eq!(arm.arm, Some(boss_protocol::ReviewYieldArm::Control));
        let round1 = &arm.phases[0];
        assert_eq!(round1.batches, 1);
        let claude = &round1.roles[0];
        assert_eq!(
            (claude.findings, claude.by_severity.high, claude.by_severity.low),
            (2, 1, 1)
        );
        // The verdict passed only on a low regression, and its revision merged.
        assert_eq!(round1.decisions.passed_on_always_on_category_only, 1);
        assert_eq!(round1.decisions.revisions.changed_code_and_merged, 1);
        assert_eq!(round1.decisions.from_always_on_category_only.merged, 1);
        assert!(report.notes.is_empty(), "{:?}", report.notes);
    }

    /// The window filter cannot use an index (it is on an expression), so the
    /// batches themselves are scanned; everything joined to them must be found
    /// by key, or a long history would be read once per batch.
    #[test]
    fn the_only_table_either_read_scans_is_the_batches_it_filters() {
        let f = Fixture::new();
        let conn = f.db.connect().unwrap();
        for sql in [batches_sql(), members_sql()] {
            let plan = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(params![0, 1], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let scans: Vec<_> = plan.iter().filter(|line| line.starts_with("SCAN")).collect();
            assert_eq!(scans.len(), 1, "expected one scan, got:\n{}", plan.join("\n"));
            assert!(
                scans[0].starts_with("SCAN b"),
                "the scan is not of the batches:\n{}",
                plan.join("\n")
            );
            assert!(
                !plan.iter().any(|line| line.contains("BLOOM FILTER")),
                "a bloom filter reads a whole table:\n{}",
                plan.join("\n")
            );
        }
    }
}
