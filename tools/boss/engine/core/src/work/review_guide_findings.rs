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
         LEFT JOIN tasks t ON t.id = v.revision_task_id
         WHERE COALESCE(b.pr_url, e.pr_url) = ? AND v.work_item_id IN ({placeholders})
         ORDER BY v.created_at, v.id"
    ))?;
    let params = std::iter::once(pr_url).chain(reviewed_ids.iter().map(String::as_str));
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
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
            legacy_finding_titles(description.as_deref().unwrap_or_default())
        };
        if findings.is_empty() {
            continue;
        }
        let label = boss_protocol::short_id_label(short_id).or(id);
        let status = if deleted_at.is_some() { None } else { status.as_deref() };
        for (severity, title) in findings {
            text.push(&severity, &title, label.as_deref(), status);
        }
    }
    Ok((!text.lines.is_empty()).then(|| text.finish()))
}

#[derive(Default)]
struct FindingsText {
    lines: Vec<String>,
    fixed: usize,
    in_progress: usize,
    open: usize,
}

impl FindingsText {
    fn push(&mut self, severity: &str, title: &str, label: Option<&str>, status: Option<&str>) {
        let finding = format!("[{}] {}", severity, escape_markdown_line(title));
        let tracker = label
            .map(|label| format!(" — ID {}", escape_markdown_line(label)))
            .unwrap_or_default();
        let line = match status {
            Some("in_review" | "done") => {
                self.fixed += 1;
                format!("- ✓ ~~{finding}~~{tracker}")
            }
            // Task rows use todo/active; accept the queue/run vocabulary too.
            Some("todo" | "active" | "queued" | "blocked" | "running") => {
                self.in_progress += 1;
                format!("- ◷ {finding}{tracker} — in progress")
            }
            _ => {
                self.open += 1;
                format!("- {finding}{tracker}")
            }
        };
        self.lines.push(line);
    }

    fn finish(self) -> ReviewGuideFindings {
        ReviewGuideFindings {
            status_text: format!(
                "{} finding{}: {} fixed on PR, {} in progress, {} open",
                self.lines.len(),
                if self.lines.len() == 1 { "" } else { "s" },
                self.fixed,
                self.in_progress,
                self.open,
            ),
            // The viewer supplies the title in its disclosure label.
            addendum_markdown: self.lines.join("\n"),
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

#[cfg(test)]
mod tests {
    use super::FindingsText;

    #[test]
    fn status_mapping_covers_missing_and_execution_statuses() {
        for (status, prefix, counts) in [
            (Some("queued"), "- ◷ ", "0 fixed on PR, 1 in progress, 0 open"),
            (Some("running"), "- ◷ ", "0 fixed on PR, 1 in progress, 0 open"),
            (Some("failed"), "- [high]", "0 fixed on PR, 0 in progress, 1 open"),
            (Some("cancelled"), "- [high]", "0 fixed on PR, 0 in progress, 1 open"),
            (None, "- [high]", "0 fixed on PR, 0 in progress, 1 open"),
        ] {
            let mut text = FindingsText::default();
            text.push("high", "Finding", None, status);
            let result = text.finish();
            assert_eq!(result.status_text, format!("1 finding: {counts}"));
            assert!(result.addendum_markdown.lines().last().unwrap().starts_with(prefix));
        }
    }
}
