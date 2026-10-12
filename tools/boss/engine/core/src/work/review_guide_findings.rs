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
        let label = boss_protocol::short_id_label(short_id).or(id.clone());
        let resolved: bool = match &id {
            Some(id) => conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM pr_review_guide_revision_heads r
                 JOIN pr_review_guide_source_series s ON s.root_task_id = r.root_task_id AND s.canonical_pr_url = r.pr_url
                 JOIN pr_review_guide_versions v ON v.id = s.readable_version_id
                 JOIN pr_review_guide_source_comparisons c ON c.id = v.comparison_id
                 WHERE r.revision_task_id = ?1 AND r.pr_url = ?2
                   AND (r.head_sha = c.head_sha OR r.observation_sequence <= c.observation_sequence))",
                params![id, pr_url],
                |row| row.get(0),
            )?,
            None => false,
        };
        let status = if deleted_at.is_some() {
            None
        } else if resolved {
            Some("resolved")
        } else {
            status.as_deref()
        };
        for (severity, title) in findings {
            text.push(&severity, &title, label.as_deref(), status);
        }
    }
    let update = super::review_guide_jobs::updates::status(conn, root_id, pr_url)?;
    let mut findings = (!text.lines.is_empty()).then(|| text.finish());
    if let Some(update) = update {
        if let Some(findings) = &mut findings {
            findings.status_text = format!("{update} — {}", findings.status_text);
        } else {
            findings = Some(ReviewGuideFindings {
                status_text: update,
                addendum_markdown: String::new(),
            });
        }
    }
    Ok(findings)
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
            Some("in_review" | "done" | "resolved") => {
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

/// Immutable starting points for incremental guide updates and revision attribution.
pub(in crate::work) mod updates {
    use super::*;

    pub(in crate::work) fn migrate(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pr_review_guide_updates (
            attempt_id TEXT PRIMARY KEY REFERENCES pr_review_guide_attempts(id) ON DELETE CASCADE,
            previous_version_id TEXT NOT NULL REFERENCES pr_review_guide_versions(id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS pr_review_guide_revision_heads (
            root_task_id TEXT NOT NULL,
            pr_url TEXT NOT NULL,
            head_sha TEXT NOT NULL,
            revision_task_id TEXT NOT NULL,
            observation_sequence INTEGER NOT NULL,
            PRIMARY KEY(root_task_id, pr_url, head_sha)
         );",
        )?;
        Ok(())
    }

    /// Snapshot the readable version in the admission transaction. Superseding a
    /// queued update still starts from the published version, never unpublished prose.
    pub(in crate::work) fn snapshot(conn: &Connection, attempt: &str, series: &str, comparison: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO pr_review_guide_updates (attempt_id, previous_version_id)
         SELECT ?1, v.id FROM pr_review_guide_source_series s
         JOIN pr_review_guide_versions v ON v.id = s.readable_version_id
         JOIN pr_review_guide_source_comparisons old ON old.id = v.comparison_id
         JOIN pr_review_guide_source_comparisons new ON new.id = ?3
         WHERE s.id = ?2 AND old.head_sha != new.head_sha",
            params![attempt, series, comparison],
        )?;
        conn.execute(
            "UPDATE pr_review_guide_attempts SET prompt_version = ?2
         WHERE id = ?1 AND EXISTS (SELECT 1 FROM pr_review_guide_updates WHERE attempt_id = ?1)",
            params![attempt, boss_review_guide::UPDATE_PROMPT_VERSION],
        )?;
        Ok(())
    }

    impl WorkDb {
        pub(crate) fn review_guide_comparison_has_attempt(&self, comparison: &str) -> Result<bool> {
            Ok(self.connect()?.query_row(
                "SELECT EXISTS(SELECT 1 FROM pr_review_guide_attempts WHERE comparison_id = ?1)",
                [comparison],
                |row| row.get(0),
            )?)
        }

        /// The old head and original Markdown are immutable and bound to this attempt.
        pub(crate) fn review_guide_update_context(&self, execution: &str) -> Result<Option<(String, String)>> {
            let conn = self.connect()?;
            let context = conn
                .query_row(
                    "SELECT c.head_sha, v.markdown FROM pr_review_guide_updates u
             JOIN pr_review_guide_attempts a ON a.id = u.attempt_id
             JOIN pr_review_guide_versions v ON v.id = u.previous_version_id
             JOIN pr_review_guide_source_comparisons c ON c.id = v.comparison_id
             WHERE a.execution_id = ?1",
                    [execution],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let requires_update: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM pr_review_guide_attempts WHERE execution_id = ?1 AND prompt_version = ?2)",
                params![execution, boss_review_guide::UPDATE_PROMPT_VERSION],
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                !requires_update || context.is_some(),
                "incremental guide starting version is missing"
            );
            Ok(context)
        }

        pub(crate) fn record_review_guide_revision_head(
            &self,
            root: &str,
            pr: &str,
            head: &str,
            revision: &str,
            sequence: i64,
        ) -> Result<()> {
            self.connect()?.execute(
                "INSERT INTO pr_review_guide_revision_heads
             (root_task_id, pr_url, head_sha, revision_task_id, observation_sequence)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_task_id, pr_url, head_sha) DO UPDATE SET
               revision_task_id = excluded.revision_task_id,
               observation_sequence = excluded.observation_sequence
             WHERE excluded.observation_sequence > observation_sequence",
                params![root, pr, head, revision, sequence],
            )?;
            Ok(())
        }
    }

    /// Shared by board reads and the series-scoped viewer poll. A revision label
    /// belongs to an exact head, never to a replacement PR.
    pub(in crate::work) fn status(conn: &Connection, root: &str, pr: &str) -> Result<Option<String>> {
        let row = conn
            .query_row(
                "SELECT s.guide_lifecycle, c.head_sha, t.id, t.short_id
         FROM pr_review_guide_source_series s
         JOIN pr_review_guide_source_comparisons c ON c.id = s.selected_comparison_id
         JOIN pr_review_guide_revision_heads r ON r.root_task_id = s.root_task_id
           AND r.pr_url = s.canonical_pr_url
           AND ((s.last_capture_error IS NULL AND r.head_sha = c.head_sha)
             OR (s.last_capture_error IS NOT NULL AND r.observation_sequence = s.latest_observation_sequence))
         JOIN tasks t ON t.id = r.revision_task_id
         WHERE s.root_task_id = ?1 AND s.canonical_pr_url = ?2
           AND s.readable_version_id IS NOT NULL
           AND (s.last_capture_error IS NOT NULL OR EXISTS (SELECT 1 FROM pr_review_guide_attempts a
             JOIN pr_review_guide_updates u ON u.attempt_id = a.id
             WHERE a.series_id = s.id AND a.request_epoch = s.request_epoch
               AND a.comparison_id = s.selected_comparison_id))
         ORDER BY r.observation_sequence DESC LIMIT 1",
                params![root, pr],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.map(|(lifecycle, head, id, short)| {
            let label = boss_protocol::short_id_label(short).unwrap_or(id);
            match lifecycle.as_str() {
                "queued" | "generating" => format!("Updating after revision {label}..."),
                "ready" => format!("Updated for revision {label} at {head}"),
                _ => format!("Update after revision {label} failed; previous guide retained"),
            }
        }))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::test_support::{
            create_active_chore, create_product, open_db, review_guide_source_packet, seed_review_guide_series,
        };

        #[test]
        fn retention_preserves_a_live_updates_starting_version_then_reclaims_terminal_history() {
            let (_dir, db) = open_db();
            let product = create_product(&db);
            let root = create_active_chore(&db, &product, "guide retention");
            let (series, comparison) = seed_review_guide_series(&db, &root);
            let first = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
            let PublishReviewGuideOutcome::Published(old) = db
                .publish_pr_review_guide_version(&first.id, "# Original", "original")
                .unwrap()
            else {
                panic!("publish");
            };
            let PrSourceCapturePersistOutcome::Stored(new) = db
                .persist_pr_review_guide_source_capture(
                    &root,
                    2,
                    PrSourceCaptureTrigger::Completion,
                    &review_guide_source_packet("base", "new-head"),
                )
                .unwrap()
            else {
                panic!("capture");
            };
            let update = db
                .create_pr_review_guide_attempt(&series, &new.comparison_id, "test")
                .unwrap();
            let conn = db.connect().unwrap();
            conn.execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&root])
                .unwrap();
            conn.execute("UPDATE pr_review_guide_versions SET generated_at = '1'", [])
                .unwrap();
            conn.execute("UPDATE pr_review_guide_source_comparisons SET captured_at = '1'", [])
                .unwrap();
            conn.execute("UPDATE pr_review_guide_attempts SET created_at = '1'", [])
                .unwrap();
            drop(conn);
            db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
            assert!(db.get_pr_review_guide_version(&old.id).unwrap().is_some());
            assert_eq!(
                db.get_pr_review_guide_summary_for_root(&root)
                    .unwrap()
                    .unwrap()
                    .readable_version_id,
                Some(old.id.clone())
            );
            db.fail_pr_review_guide_attempt(&update.id, "failed").unwrap();
            db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
            assert!(db.get_pr_review_guide_version(&old.id).unwrap().is_none());
        }

        #[test]
        fn failed_source_capture_keeps_published_guide_and_fences_old_jobs() {
            let (_dir, db) = open_db();
            let product = create_product(&db);
            let root = create_active_chore(&db, &product, "capture failure");
            let (series, comparison) = seed_review_guide_series(&db, &root);
            let first = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
            let PublishReviewGuideOutcome::Published(old) = db
                .publish_pr_review_guide_version(&first.id, "# Original", "original")
                .unwrap()
            else {
                panic!("publish");
            };
            let stale = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
            let url = db
                .get_pr_review_guide_summary_for_root(&root)
                .unwrap()
                .unwrap()
                .canonical_pr_url;
            db.record_pr_review_guide_source_capture_failure(&root, &url, 10, "pinned delta unavailable")
                .unwrap();
            assert_eq!(
                db.publish_pr_review_guide_version(&stale.id, "# Late", "late").unwrap(),
                PublishReviewGuideOutcome::Superseded
            );
            let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
            assert_eq!(summary.lifecycle, "failed");
            assert_eq!(summary.readable_version_id, Some(old.id));
            assert!(
                db.retry_pr_review_guide(&root, None, "test")
                    .unwrap_err()
                    .to_string()
                    .contains("recapture")
            );
        }
    }
}
