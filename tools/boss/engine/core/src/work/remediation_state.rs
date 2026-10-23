use super::*;

impl WorkDb {
    /// Add current card state to a compact row; never attach diagnostic blobs.
    pub fn remediation_entry(&self, mut row: EngineAttemptListEntry) -> EngineAttemptListEntry {
        if matches!(row.kind.as_str(), "ci" | "ci_state")
            && let Some(id) = row.work_item_id.as_deref()
        {
            match self.ci_budget_snapshot(id) {
                Ok(Some(budget)) => {
                    row.extra.insert("attempts_used".into(), budget.used.to_string());
                    row.extra.insert("budget".into(), budget.effective.to_string());
                    if row.kind == "ci" {
                        // The parent's blocked state is the authoritative
                        // card state, so a terminal attempt pushed without a
                        // preceding start still sets or clears the chip.
                        let card_state = match budget.blocked_reason.as_deref() {
                            Some("ci_failure_exhausted") => "exhausted",
                            Some("ci_failure") => "in_flight",
                            _ => "clear",
                        };
                        row.extra.insert("card_state".into(), card_state.into());
                    }
                }
                Ok(None) => {}
                Err(err) => tracing::warn!(%err, "could not project remediation budget"),
            }
        }
        row
    }

    /// Latest compact attempt per kind for each current, nonterminal PR.
    /// Unlike history, this has no arbitrary row cap: every open card must
    /// recover after a missed push. No diagnosis/log columns are selected.
    pub fn open_remediation_state(&self) -> Result<Vec<EngineAttemptListEntry>> {
        let conn = self.connect()?;
        let mut out = Vec::new();
        for (table, kind, extra_column) in [
            ("conflict_resolutions", "conflict", "NULL"),
            ("ci_remediations", "ci", "a.attempt_kind"),
        ] {
            let sql = format!(
                "SELECT a.id, a.product_id, a.work_item_id, a.pr_url, a.status,
                        a.failure_reason, a.created_at, a.started_at, a.finished_at,
                        a.revision_task_id, {extra_column}
                 FROM tasks t JOIN {table} a ON a.id = (
                     SELECT latest.id FROM {table} latest
                     WHERE latest.work_item_id = t.id AND latest.pr_url = t.pr_url
                     ORDER BY latest.created_at DESC, latest.id DESC LIMIT 1)
                 WHERE t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived')
                   AND t.pr_url = a.pr_url

                 ORDER BY a.created_at DESC, a.id DESC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], |r| {
                let mut extra = std::collections::BTreeMap::new();
                if let Some(id) = r.get::<_, Option<String>>(9)? {
                    extra.insert("revision_task_id".into(), id);
                }
                if let Some(attempt_kind) = r.get::<_, Option<String>>(10)? {
                    extra.insert("attempt_kind".into(), attempt_kind);
                }
                Ok(EngineAttemptListEntry {
                    id: r.get(0)?,
                    product_id: r.get(1)?,
                    work_item_id: r.get(2)?,
                    pr_url: r.get(3)?,
                    status: r.get(4)?,
                    failure_reason: r.get(5)?,
                    created_at: r.get(6)?,
                    started_at: r.get(7)?,
                    finished_at: r.get(8)?,
                    kind: kind.into(),
                    extra,
                })
            })?;
            for row in rows {
                out.push(row?);
            }
        }
        // Failure chips are task signals, including exhausted PRs that have
        // no active attempt. Include an authoritative state row for each PR.
        let mut stmt = conn.prepare(
            "SELECT t.id, t.product_id, t.pr_url, t.created_at,
                    EXISTS(SELECT 1 FROM task_blocked_signals s WHERE s.work_item_id = t.id
                           AND s.reason = 'ci_failure_exhausted' AND s.cleared_at IS NULL),
                    EXISTS(SELECT 1 FROM task_blocked_signals s WHERE s.work_item_id = t.id
                           AND s.reason = 'ci_failure' AND s.cleared_at IS NULL)
             FROM tasks t
             WHERE t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived') AND t.kind != 'revision' AND t.pr_url IS NOT NULL"
        )?;
        let rows = stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            let exhausted: bool = r.get(4)?;
            let failing: bool = r.get(5)?;
            Ok(EngineAttemptListEntry {
                id: format!("ci_state:{id}"),
                work_item_id: Some(id),
                product_id: r.get(1)?,
                pr_url: r.get(2)?,
                created_at: r.get(3)?,
                kind: "ci_state".into(),
                status: if exhausted {
                    "exhausted"
                } else if failing {
                    "in_flight"
                } else {
                    "clear"
                }
                .into(),
                failure_reason: None,
                started_at: None,
                finished_at: None,
                // attempts_used/budget are filled by `remediation_entry`.
                extra: Default::default(),
            })
        })?;
        for row in rows {
            out.push(row?);
        }
        // WorkDb shares one mutex-protected connection. Release the snapshot
        // statements and connection before the budget projection borrows it.
        drop(stmt);
        drop(conn);
        Ok(out.into_iter().map(|row| self.remediation_entry(row)).collect())
    }
}
