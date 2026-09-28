//! Live review findings, kept separate from immutable guide versions.

use super::*;
use boss_protocol::{OpenMergeRevision, ReviewGuideFindings};

impl WorkDb {
    pub fn review_guide_findings(&self, root_id: &str, pr_url: &str) -> Result<Option<ReviewGuideFindings>> {
        let conn = self.connect()?;
        query_findings(&conn, root_id, pr_url)
    }

    /// Read at merge-click time, including revisions below legacy nested or
    /// deleted parents. Counts only revisions that can still add commits
    /// (`todo` / `active` / `blocked`). The client never infers this gate
    /// from cached cards.
    pub fn open_merge_revisions(&self, task_id: &str) -> Result<Vec<OpenMergeRevision>> {
        let conn = self.connect()?;
        let root = chain_root(&conn, task_id)?;
        let mut revisions = Vec::new();
        for id in chain_helpers::collect_chain_revision_ids_including_deleted(&conn, &root)? {
            if let Some(task) = query_task(&conn, &id)?
                && task.deleted_at.is_none()
                && task.status.can_still_change_pr()
            {
                revisions.push(OpenMergeRevision {
                    label: boss_protocol::short_id_label(task.short_id).unwrap_or_else(|| id.clone()),
                    id,
                    status: task.status.to_string(),
                });
            }
        }
        revisions.sort_by(|a, b| a.id.cmp(&b.id));
        revisions.dedup_by(|a, b| a.id == b.id);
        Ok(revisions)
    }
}

/// Bind the supplement to its PR, so replacing a root's PR cannot show
/// findings belonging to its previous PR. The durable verdict link includes
/// both revisions and post-merge follow-up items.
fn query_findings(conn: &Connection, root_id: &str, pr_url: &str) -> Result<Option<ReviewGuideFindings>> {
    let mut reviewed_ids = chain_helpers::collect_chain_revision_ids_including_deleted(conn, root_id)?;
    reviewed_ids.push(root_id.to_owned());
    let placeholders = vec!["?"; reviewed_ids.len()].join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT p.payload_json, t.id, t.short_id, t.status, t.deleted_at, t.description
         FROM pr_review_verdicts v
         LEFT JOIN worker_proposals p ON p.id = v.proposal_id
         LEFT JOIN pr_review_batches b ON b.id = v.batch_id
         LEFT JOIN work_executions e ON e.id = v.execution_id
         JOIN tasks t ON t.id = v.revision_task_id
         WHERE COALESCE(b.pr_url, e.pr_url) = ? AND v.work_item_id IN ({placeholders})
         ORDER BY v.created_at, v.id"
    ))?;
    let params = std::iter::once(pr_url).chain(reviewed_ids.iter().map(String::as_str));
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut text = FindingsText::default();
    for row in rows {
        let (payload, id, short_id, status, deleted_at, description) = row?;
        let findings = if let Some(payload) = payload {
            let payload: boss_protocol::ReviewVerdictProposalPayload = serde_json::from_str(&payload)?;
            let verdict: boss_pr_review::SupervisorVerdict = serde_json::from_value(payload.verdict)?;
            verdict
                .findings
                .into_iter()
                .map(|finding| (finding.severity.as_str().to_owned(), finding.title))
                .collect()
        } else {
            legacy_finding_titles(&description)
        };
        if findings.is_empty() {
            continue;
        }
        let label = boss_protocol::short_id_label(short_id).unwrap_or(id);
        let status = if deleted_at.is_some() { "deleted" } else { &status };
        text.trackers.insert(format!("ID {label} ({status})"));
        text.all_done &= status == "done";
        for (severity, title) in findings {
            text.lines.push(format!(
                "- [{}] {} — ID {} ({})",
                severity,
                escape_markdown_line(&title),
                escape_markdown_line(&label),
                status,
            ));
        }
    }
    Ok((!text.lines.is_empty()).then(|| text.finish()))
}

struct FindingsText {
    lines: Vec<String>,
    trackers: std::collections::BTreeSet<String>,
    all_done: bool,
}

impl Default for FindingsText {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            trackers: Default::default(),
            all_done: true,
        }
    }
}

impl FindingsText {
    fn finish(self) -> ReviewGuideFindings {
        let tracking = if self.all_done {
            "fixes complete".to_owned()
        } else {
            format!(
                "fix tracking: {}",
                self.trackers.into_iter().collect::<Vec<_>>().join(", ")
            )
        };
        ReviewGuideFindings {
            status_text: format!(
                "AI review found {} issue{}; {tracking}",
                self.lines.len(),
                if self.lines.len() == 1 { "" } else { "s" }
            ),
            addendum_markdown: format!("## AI review findings addendum\n\n{}", self.lines.join("\n")),
        }
    }
}

/// Finding titles are untrusted prose, not Markdown instructions or links.
fn escape_markdown_line(value: &str) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut escaped = String::new();
    for ch in value.chars() {
        if ch.is_ascii_punctuation() {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Before consolidated proposals, the durable finding titles lived in the
/// engine-rendered revision instructions (pr-review::render_revision_instructions).
/// Only read that renderer's finding headings, never the free-form detail.
fn legacy_finding_titles(description: &str) -> Vec<(String, String)> {
    description
        .lines()
        .filter_map(|line| {
            let (severity, title) = line.strip_prefix("### [")?.split_once("] ")?;
            matches!(severity, "critical" | "high" | "medium" | "low").then(|| (severity.to_owned(), title.to_owned()))
        })
        .collect()
}
