use std::collections::HashMap;

use super::*;
use boss_protocol::{AiReviewBadge, ReviewStage};

/// What an id list handed to the review queries names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReviewKey {
    /// Board cards: the query walks each card's revision chain up to its PR
    /// owner (the first non-revision ancestor) and keys the result by card.
    Card,
    /// PR owners already resolved by the caller (non-revision rows): no
    /// ancestor walk, result keyed by owner. A revision id yields nothing.
    Owner,
}

/// The current-head verdict of one PR owner, before the per-card self-link
/// filter: a revision never links to itself as its own findings revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CurrentHeadReview {
    pub state: &'static str,
    pub findings_revision_id: Option<String>,
}

/// The verdict-derived review projection of one PR owner, shared by every
/// card of its revision family. `current_head` is `None` for an owner with
/// no bound PR; `history` is `None` when the caller did not need badges.
#[derive(Clone, Debug)]
pub(crate) struct OwnerReview {
    pub current_head: Option<CurrentHeadReview>,
    pub history: Option<ReviewBadgeHistory>,
}

/// Resolve against the observed PR head, across legacy revision-owned and
/// modern cycle-root verdicts. Missing heads and late old-head results never
/// establish a clean review. This is a single batched query for the tree.
///
/// A findings verdict whose findings revision has delivered without moving the
/// head the verdict reviewed (it edited the PR title/body, or justified no
/// change) no longer counts as unresolved: nothing will ever re-review that
/// unchanged head, so the badge would otherwise stay orange forever. It
/// resolves to `not_reviewed`, the same state a commit-based fix shows until
/// its new head is reviewed. A newer verdict for the same head still wins
/// because it is selected before this check.
///
/// Only owners with a bound PR produce an entry.
pub(super) fn fetch_current_heads(
    conn: &Connection,
    ids: &[String],
    key: ReviewKey,
) -> Result<HashMap<String, CurrentHeadReview>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = (1..=ids.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
    let sql = current_heads_sql(&placeholders, key);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, bool>(5)?,
            row.get::<_, bool>(6)?,
        ))
    })?;
    let mut result = HashMap::new();
    for row in rows {
        let (id, outcome, revision, ci, mergeable, pending_work, revision_delivered_unmoved) = row?;
        let (state, findings_revision_id) = match outcome.as_deref() {
            Some(REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS) if revision_delivered_unmoved => {
                (AI_REVIEW_STATE_NOT_REVIEWED, None)
            }
            Some(REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS | REVIEW_GATE_OUTCOME_REVISION_CREATION_FAILED) => {
                (AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS, revision)
            }
            Some(REVIEW_GATE_OUTCOME_COMPLETED_CLEAN) => {
                let state =
                    if pending_work || ci.as_deref() != Some("success") || mergeable.as_deref() != Some("mergeable") {
                        AI_REVIEW_STATE_REVIEWED_CLEAN_PENDING
                    } else {
                        AI_REVIEW_STATE_REVIEWED_ALL_CLEAR
                    };
                (state, None)
            }
            _ => (AI_REVIEW_STATE_NOT_REVIEWED, None),
        };
        result.insert(
            id,
            CurrentHeadReview {
                state,
                findings_revision_id,
            },
        );
    }
    Ok(result)
}

/// Keep historical evidence separate from current-head state: delivering a
/// revision does not prove its fix has been reviewed. Fetch the whole id
/// list in one query, including legacy revision-owned verdicts.
pub(super) fn fetch_badge_histories(
    conn: &Connection,
    ids: &[String],
    key: ReviewKey,
) -> Result<HashMap<String, ReviewBadgeHistory>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = (1..=ids.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
    let sql = badge_histories_sql(&placeholders, key);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids), |row| {
        Ok((
            row.get::<_, String>(0)?,
            ReviewBadgeHistory {
                findings_revision_id: row.get(9)?,
                verdict_payload: row.get(10)?,
                revision_description: row.get(11)?,
                head: row.get(1)?,
                reviewed_sha: row.get(2)?,
                verdict: row
                    .get::<_, Option<String>>(4)?
                    .map(|outcome| Ok::<_, rusqlite::Error>((outcome, row.get::<_, String>(3)?, row.get::<_, i64>(5)?)))
                    .transpose()?,
                revision: row
                    .get::<_, Option<String>>(6)?
                    .map(|name| Ok::<_, rusqlite::Error>((name, row.get::<_, String>(7)?, row.get::<_, bool>(8)?)))
                    .transpose()?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
}

/// Cards that carry a review badge: in Review with a resolved review state.
pub(super) fn wants_review_badge(row: &Task) -> bool {
    row.status == TaskStatus::InReview && row.ai_review_state.is_some()
}

/// Set `ai_review_badge` on every row from its resolved `ai_review_state`
/// and the history `history_for` supplies for it (none for rows whose owner
/// could not be resolved). Must run after `ai_review_state` is attached.
pub(super) fn apply_review_badges<'h>(
    tasks: &mut [Task],
    chores: &mut [Task],
    history_for: impl Fn(&Task) -> Option<&'h ReviewBadgeHistory>,
) -> Result<()> {
    for row in tasks.iter_mut().chain(chores.iter_mut()) {
        row.ai_review_badge = None;
        if !wants_review_badge(row) {
            continue;
        }
        let history = history_for(row);
        let state = row.ai_review_state.as_deref().expect("checked by wants_review_badge");
        let mut badge = badge_presentation(state, row.ai_review_stage.as_deref(), history, &row.id);
        if state == AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS {
            if let Some(history) = history {
                badge.findings_markdown = findings_markdown(history)?;
            }
            badge.tooltip.push_str(if row.ai_review_findings_revision_id.is_some() {
                "\nClick to read the findings in the follow-up revision."
            } else {
                "\nNo separate findings revision is available from this card."
            });
        }
        row.ai_review_badge = Some(badge);
    }
    Ok(())
}

fn findings_markdown(history: &ReviewBadgeHistory) -> Result<Option<String>> {
    if let Some(payload) = &history.verdict_payload {
        let payload: boss_protocol::ReviewVerdictProposalPayload = serde_json::from_str(payload)?;
        let verdict: crate::pr_review::SupervisorVerdict = serde_json::from_value(payload.verdict)?;
        let origin = crate::pr_review::ReviewOrigin {
            pr_number: verdict.pr_url.rsplit('/').next().and_then(|value| value.parse().ok()),
        };
        return Ok(Some(crate::pr_review::render_revision_instructions(
            &verdict.to_review_result(),
            origin,
        )));
    }
    // Legacy verdicts predate proposals; their existing brief remains readable
    // even on the findings revision itself, where the self-link is suppressed.
    Ok(history.revision_description.clone())
}

/// One owner's badge evidence: the ranked verdict, the head it reviewed, and
/// the findings revision it names. Identical for every card of the family;
/// only the self-link (`findings_revision_id == card`) differs per card.
#[derive(bon::Builder, Clone, Debug)]
#[builder(on(String, into))]
pub(crate) struct ReviewBadgeHistory {
    verdict_payload: Option<String>,
    revision_description: Option<String>,
    /// `tasks.id` of the findings revision the ranked verdict names, when
    /// that row still exists.
    findings_revision_id: Option<String>,
    head: Option<String>,
    reviewed_sha: Option<String>,
    verdict: Option<(String, String, i64)>,
    revision: Option<(String, String, bool)>,
}

impl ReviewBadgeHistory {
    /// Drop the findings text an owner in `state` will never render, so a
    /// cached entry holds the tooltip evidence and not a verdict payload.
    pub(super) fn trimmed_for(mut self, state: Option<&str>) -> Self {
        if state != Some(AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS) {
            self.verdict_payload = None;
            self.revision_description = None;
        }
        self
    }
}

fn badge_presentation(
    state: &str,
    stage: Option<&str>,
    history: Option<&ReviewBadgeHistory>,
    card_id: &str,
) -> AiReviewBadge {
    let findings = history.and_then(|h| h.verdict.as_ref()).map(|v| v.2);
    let consolidating = stage.and_then(|stage| stage.parse().ok()) == Some(ReviewStage::Consolidation);
    let (label, icon, explanation) = match state {
        AI_REVIEW_STATE_REVIEWING if consolidating => (
            "Consolidating review findings".into(),
            "arrow.triangle.merge",
            "The reviewers have finished on this head; one agent is consolidating their findings into a single review.",
        ),
        AI_REVIEW_STATE_REVIEWING => ("AI reviewing…".into(), "brain", "An AI review pass is running."),
        AI_REVIEW_STATE_REVIEW_QUEUED => ("AI review queued".into(), "clock", "Waiting for a review-pool slot."),
        AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS => (
            findings.map_or_else(|| "AI review: findings".into(), |n| format!("AI review: {n} findings")),
            "exclamationmark.circle.fill",
            "AI review found issues on the current PR head.",
        ),
        AI_REVIEW_STATE_REVIEWED_ALL_CLEAR => (
            "AI review: clean".into(),
            "checkmark.seal.fill",
            "AI review passed for the current PR head. Required CI checks passed and no revisions are pending.",
        ),
        AI_REVIEW_STATE_REVIEWED_CLEAN_PENDING => (
            "AI review: clean".into(),
            "checkmark.seal.fill",
            "AI review passed for the current PR head. CI, mergeability, or work in progress still prevent readiness.",
        ),
        AI_REVIEW_STATE_REVIEW_NOT_REQUIRED => (
            "AI review not required".into(),
            "minus.circle",
            "This kind of work item does not require AI review.",
        ),
        _ => (
            "Not reviewed: latest commit".into(),
            "questionmark.circle",
            "The current PR head has no completed AI review, or its head is not yet known.",
        ),
    };
    let mut badge = AiReviewBadge {
        label,
        system_image: icon.into(),
        tooltip: explanation.into(),
        findings_markdown: None,
    };
    let Some(history) = history else { return badge };
    let short = |sha: &str| sha.chars().take(7).collect::<String>();
    if let Some(sha) = &history.reviewed_sha {
        badge.tooltip.push_str(&format!("\nLast reviewed {}", short(sha)));
        if let Some((outcome, date, count)) = &history.verdict {
            let date = date
                .parse::<i64>()
                .ok()
                .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
                .map(|date| date.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| date.clone());
            let verdict = if outcome == REVIEW_GATE_OUTCOME_COMPLETED_CLEAN {
                "clean".into()
            } else {
                format!("{count} findings")
            };
            badge.tooltip.push_str(&format!(" on {date}: {verdict}."));
        } else {
            badge.tooltip.push_str("; review time and verdict unavailable.");
        }
        match &history.head {
            Some(head) if head != sha => badge.tooltip.push_str(&format!(
                "\nCurrent head {} differs from the reviewed commit.",
                short(head)
            )),
            None => badge.tooltip.push_str("\nCurrent PR head is not yet known."),
            _ => {}
        }
    } else {
        badge.tooltip.push_str("\nNo reviewed commit is recorded.");
    }
    if let Some((name, status, delivered)) = &history.revision {
        let status_label = match status.as_str() {
            "todo" => "Backlog",
            "active" => "Doing",
            "in_review" => "Review",
            "done" => "Done",
            "blocked" => "Blocked",
            _ => status,
        };
        if history.findings_revision_id.as_deref() == Some(card_id) {
            badge
                .tooltip
                .push_str(&format!("\nThis revision addresses the findings ({status_label})."));
        } else {
            badge
                .tooltip
                .push_str(&format!("\nFindings revision: {name} ({status_label})."));
        }
        if *delivered && state == AI_REVIEW_STATE_NOT_REVIEWED {
            badge
                .tooltip
                .push_str(" Findings addressed; the fix has not been AI-reviewed.");
            if history.head.is_some() && history.head == history.reviewed_sha {
                badge.tooltip = badge.tooltip.replacen(
                    explanation,
                    "The findings revision delivered without a new commit. No subsequent AI review is recorded.",
                    1,
                );
                badge.label = "Findings addressed".into();
                badge.system_image = "checkmark.circle".into();
            }
        }
    }
    badge
}

/// The `family(root, id, depth)` CTE shared by the review projection and
/// the board-review cache key read, expanded to a literal so both can be
/// built with `concat!`. It seeds from an `owners(card, id)` CTE and walks
/// revision children through the covering `idx_tasks_parent_kind_id`; the
/// key read must see exactly the rows the projection reads, so there is one
/// definition of the walk (kind filter, depth bound, seeding).
macro_rules! family_recursive_sql {
    () => {
        "family(root, id, depth) AS (
            SELECT DISTINCT id, id, 0 FROM owners
            UNION ALL
            SELECT f.root, t.id, f.depth + 1 FROM family f
            JOIN tasks t INDEXED BY idx_tasks_parent_kind_id ON t.parent_task_id = f.id
            WHERE t.kind = 'revision' AND f.depth < 64
         )"
    };
}
pub(super) use family_recursive_sql;

fn informative_gate_outcomes_sql() -> String {
    super::review_verdicts::INFORMATIVE_GATE_OUTCOMES
        .iter()
        .map(|outcome| format!("'{outcome}'"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Production SQL for [`fetch_current_heads`]. Exposed so the query-plan test
/// can `EXPLAIN` the same statement the board runs.
pub(crate) fn current_heads_sql(placeholders: &str, key: ReviewKey) -> String {
    let outcomes = informative_gate_outcomes_sql();
    let family = review_family_cte(placeholders, key);
    format!(
        "{family}
         SELECT o.card, v.gate_outcome, v.revision_task_id,
                root.ci_required_state, root.pr_mergeable_state,
                root.status = 'active' OR EXISTS(SELECT 1 FROM family f JOIN tasks t ON t.id = f.id
                       WHERE f.root = root.id AND t.kind = 'revision'
                         AND t.deleted_at IS NULL AND t.status IN ('todo', 'active', 'blocked')),
                EXISTS(SELECT 1 FROM tasks r
                       WHERE r.id = v.revision_task_id AND r.kind = 'revision'
                         AND r.deleted_at IS NULL AND r.status IN ('in_review', 'done')
                         AND EXISTS(SELECT 1 FROM work_executions we
                                    WHERE we.id = (SELECT e.id FROM work_executions e
                                                   WHERE e.work_item_id = r.id AND e.status = 'completed'
                                                     AND e.kind = 'revision_implementation'
                                                   ORDER BY e.finished_at DESC, e.id DESC LIMIT 1)
                                      AND (we.pr_head_after IS NULL OR we.pr_head_after = v.head_sha)))
         FROM owners o JOIN tasks root ON root.id = o.id
         LEFT JOIN pr_review_verdicts v ON v.id = (
             SELECT rv.id FROM pr_review_verdicts rv
             JOIN family f ON f.id = rv.work_item_id AND f.root = root.id
             WHERE rv.head_sha = NULLIF(root.pr_head_sha, '')
               AND rv.gate_outcome IN ({outcomes})
             ORDER BY rv.created_at DESC, rv.id DESC LIMIT 1
         )
         WHERE NULLIF(root.pr_url, '') IS NOT NULL"
    )
}

/// Production SQL for [`fetch_badge_histories`]. Exposed so the query-plan test
/// can `EXPLAIN` the same statement the board runs.
pub(crate) fn badge_histories_sql(placeholders: &str, key: ReviewKey) -> String {
    let outcomes = informative_gate_outcomes_sql();
    let family = review_family_cte(placeholders, key);
    format!(
        "{family}, ranked AS (
            SELECT o.card, v.*, ROW_NUMBER() OVER (
                PARTITION BY o.card ORDER BY
                    COALESCE(v.head_sha = NULLIF(root.pr_head_sha, ''), 0) DESC,
                    v.created_at DESC, v.id DESC) AS rank
            FROM owners o JOIN tasks root ON root.id = o.id
            JOIN family f ON f.root = root.id
            JOIN pr_review_verdicts v ON v.work_item_id = f.id
            WHERE v.gate_outcome IN ({outcomes}) AND NULLIF(v.head_sha, '') IS NOT NULL
         )
         SELECT o.card, NULLIF(root.pr_head_sha, ''),
                COALESCE(v.head_sha, NULLIF(root.last_reviewed_sha, '')),
                v.created_at, v.gate_outcome, v.findings_count, r.name, r.status,
                r.status IN ('in_review', 'done') AND r.deleted_at IS NULL AND EXISTS(
                    SELECT 1 FROM work_executions e WHERE e.work_item_id = r.id
                    AND e.kind = 'revision_implementation' AND e.status = 'completed'),
                r.id, p.payload_json, r.description
         FROM owners o JOIN tasks root ON root.id = o.id
         LEFT JOIN ranked v ON v.card = o.card AND v.rank = 1
         LEFT JOIN tasks r ON r.id = v.revision_task_id
         LEFT JOIN worker_proposals p ON p.id = v.proposal_id
             AND p.kind = 'review_verdict' AND p.state = 'applied'"
    )
}

/// The `owners(card, id)` and `family(root, id, depth)` CTEs both review
/// queries select from. `Card` resolves each id's owner by walking up its
/// revision chain; `Owner` takes the ids as the owners themselves.
pub(super) fn review_family_cte(placeholders: &str, key: ReviewKey) -> String {
    let owners = match key {
        ReviewKey::Card => format!(
            "ancestors(card, id, depth) AS (
            SELECT id, id, 0 FROM tasks WHERE id IN ({placeholders})
            UNION ALL
            SELECT a.card, t.parent_task_id, a.depth + 1
            FROM ancestors a JOIN tasks t ON t.id = a.id
            WHERE t.kind = 'revision' AND t.parent_task_id IS NOT NULL AND a.depth < 64
         ), owners AS (
            SELECT a.card, t.id FROM ancestors a JOIN tasks t ON t.id = a.id
            WHERE t.kind != 'revision'
         )"
        ),
        ReviewKey::Owner => format!(
            "owners(card, id) AS (
            SELECT id, id FROM tasks WHERE id IN ({placeholders}) AND kind != 'revision'
         )"
        ),
    };
    format!("WITH RECURSIVE {owners}, {}", family_recursive_sql!())
}

#[cfg(test)]
#[path = "review_badge_tests.rs"]
mod tests;
