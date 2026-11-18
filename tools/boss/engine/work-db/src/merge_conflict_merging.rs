//! A merge request remains live while the engine resolves its conflict.
use super::*;
use boss_protocol::OpenMergeRevision;

pub const RESOLVING_MERGE_CONFLICT: &str = "resolving_conflicts";

/// Abandon reason for an attempt replaced by a fresh one; the merge request survives it.
pub(crate) const SUPERSEDED_STALE_HEAD: &str = "superseded_stale_head";

/// Short human wording for the machine reason codes callers pass to [`stop`].
/// Text that is already prose is shown as written.
fn human_reason(reason: &str) -> String {
    let code = reason.split(':').next().unwrap_or(reason).trim();
    let is_code = !code.is_empty()
        && code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !is_code {
        return reason.to_owned();
    }
    match code {
        "revision_create_failed" => "couldn't start a revision to fix the conflict",
        "manual_abandon" => "conflict resolution was stopped manually",
        SUPERSEDED_STALE_HEAD => "the branch changed during conflict resolution",
        "lockfile_regeneration_failed" => "couldn't regenerate the lockfile",
        _ => "conflict resolution didn't finish",
    }
    .to_owned()
}

pub(crate) fn start(conn: &Connection, id: &str, pr_url: &str, base: &str) -> Result<bool> {
    let prior: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT merge_queue_state, pr_head_sha FROM tasks WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (prior_state, prior_head) = prior.unwrap_or((None, None));
    let requested_at: String = conn
        .query_row(
            "SELECT created_at FROM github_merge_intents WHERE work_item_id = ?1 AND status = 'active'",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(now_string);
    let mut detail = serde_json::json!({
        "source": "conflict_resolution",
        "requested_at": requested_at,
        "status_label": format!("Resolving conflicts with {base}…"),
        "section_order": boss_engine_storage_values::merge_queue_section::QUEUED_NO_POSITION_SECTION_ORDER,
    });
    if let Some(ids) = confirmed_revision_ids(conn, id)? {
        detail["confirmed_revision_ids"] = serde_json::json!(ids);
    }
    let changed = conn.execute(
        "UPDATE tasks SET merge_queue_state = 'resolving_conflicts', merge_queue_detail = ?3
         WHERE id = ?1 AND pr_url = ?2 AND deleted_at IS NULL
           AND (status = 'in_review' OR (status = 'blocked' AND blocked_reason = 'merge_conflict'))",
        params![id, pr_url, detail.to_string()],
    )? > 0;
    if changed {
        persist_observed_github_arm(conn, id, pr_url, prior_state.as_deref(), prior_head.as_deref())?;
    }
    Ok(changed)
}

fn persist_observed_github_arm(
    conn: &Connection,
    id: &str,
    pr_url: &str,
    prior_state: Option<&str>,
    prior_head: Option<&str>,
) -> Result<()> {
    if !matches!(prior_state, Some("auto_merge_enabled" | "queued")) {
        return Ok(());
    }
    let already: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM github_merge_intents WHERE work_item_id = ?1 AND status = 'active')",
        [id],
        |row| row.get(0),
    )?;
    if already {
        return Ok(());
    }
    let head = prior_head.filter(|s| !s.is_empty()).unwrap_or("observed");
    conn.execute(
        "INSERT INTO github_merge_intents (id, work_item_id, pr_url, head_sha, status, created_at)
         VALUES (?1, ?2, ?3, ?4, 'active', ?5)",
        params![next_id("gmi"), id, pr_url, head, now_string()],
    )?;
    Ok(())
}

fn remember_confirmed_merge_revisions_in_tx(
    conn: &Connection,
    id: &str,
    pr_url: &str,
    revisions: &[OpenMergeRevision],
) -> Result<()> {
    conn.execute(
        "DELETE FROM github_merge_intents WHERE work_item_id = ?1 AND status = ?2",
        params![id, CONFIRMED_REVISIONS],
    )?;
    let ids: Vec<&str> = revisions.iter().map(|revision| revision.id.as_str()).collect();
    conn.execute(
        "INSERT INTO github_merge_intents (id, work_item_id, pr_url, head_sha, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            next_id("gmi"),
            id,
            pr_url,
            serde_json::to_string(&ids)?,
            CONFIRMED_REVISIONS,
            now_string()
        ],
    )?;
    Ok(())
}

fn confirmed_revision_ids(conn: &Connection, id: &str) -> Result<Option<Vec<String>>> {
    let from_detail: Option<String> = conn
        .query_row(
            "SELECT CASE WHEN json_valid(merge_queue_detail)
                    THEN json_extract(merge_queue_detail, '$.confirmed_revision_ids') END
             FROM tasks WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    if let Some(text) = from_detail.filter(|text| !text.is_empty() && text != "null")
        && let Ok(ids) = serde_json::from_str::<Vec<String>>(&text)
    {
        return Ok(Some(ids));
    }
    let stored: Option<String> = conn
        .query_row(
            "SELECT head_sha FROM github_merge_intents
             WHERE work_item_id = ?1 AND status = ?2
             ORDER BY created_at DESC LIMIT 1",
            params![id, CONFIRMED_REVISIONS],
            |row| row.get(0),
        )
        .optional()?;
    match stored {
        Some(text) => Ok(Some(serde_json::from_str(&text).unwrap_or_default())),
        None => Ok(None),
    }
}

fn restore_paused_merge_label(conn: &Connection, id: &str) -> Result<bool> {
    let current: Option<String> = conn
        .query_row(
            "SELECT merge_queue_detail FROM tasks
             WHERE id = ?1 AND merge_queue_state IS NULL
               AND CASE WHEN json_valid(merge_queue_detail)
                   THEN json_extract(merge_queue_detail, '$.source') = 'conflict_resolution'
                    AND json_extract(merge_queue_detail, '$.disarm_error') IS NOT NULL
                   ELSE 0 END",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let Some(text) = current else {
        return Ok(false);
    };
    let mut detail: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({}));
    let paused = detail
        .get("paused_label")
        .and_then(|v| v.as_str())
        .unwrap_or("Merge paused")
        .to_owned();
    detail["status_label"] = serde_json::Value::String(paused);
    if let Some(obj) = detail.as_object_mut() {
        obj.remove("disarm_error");
    }
    Ok(conn.execute(
        "UPDATE tasks SET merge_queue_detail = ?2 WHERE id = ?1",
        params![id, detail.to_string()],
    )? > 0)
}

fn truncate_disarm_error(error: &str) -> String {
    let one_line = error.lines().next().unwrap_or(error).trim();
    let mut truncated = String::new();
    for (i, ch) in one_line.chars().enumerate() {
        if i >= DISARM_ERROR_LABEL_MAX {
            truncated.push('…');
            break;
        }
        truncated.push(ch);
    }
    truncated
}

/// Intent status for a request stopped while GitHub auto-merge was armed: the
/// engine still owes GitHub a `--disable-auto` before the stop is real.
pub const DISARM_PENDING: &str = "disarm_pending";

/// Snapshot of the open revisions the person confirmed when they asked to
/// merge, stored as a `github_merge_intents` row whose `head_sha` is a JSON
/// array of revision ids. Written before the GitHub call so a revision that
/// opens while that call is in flight is not treated as confirmed.
pub const CONFIRMED_REVISIONS: &str = "confirmed_revisions";

const DISARM_ERROR_LABEL_MAX: usize = 120;

/// End the standing merge request. If GitHub had accepted the request (an
/// active intent exists) its auto-merge is still armed, so the intent becomes
/// [`DISARM_PENDING`] and the merge poller turns auto-merge off. The readable
/// explanation stays in `merge_queue_detail`; `blocked_detail` is only filled
/// for a blocked row that has none.
pub(crate) fn stop(conn: &Connection, id: &str, reason: &str) -> Result<bool> {
    let human = human_reason(reason);
    let refusal: Option<String> = conn
        .query_row(
            "SELECT CASE WHEN json_valid(merge_queue_detail)
                    THEN json_extract(merge_queue_detail, '$.refusal') END
             FROM tasks WHERE id = ?1 AND merge_queue_state = 'resolving_conflicts'",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let paused = format!("Merge paused: {human}");
    let mut detail = serde_json::json!({
        "source": "conflict_resolution",
        "status_label": paused,
        "paused_label": paused,
    });
    if let Some(refusal) = refusal {
        detail["refusal"] = serde_json::Value::String(refusal);
    }
    let detail = detail.to_string();
    let changed = conn.execute(
        "UPDATE tasks SET merge_queue_state = NULL, merge_queue_detail = ?3,
             blocked_detail = CASE WHEN status = 'blocked' THEN COALESCE(blocked_detail, ?2) ELSE blocked_detail END
         WHERE id = ?1 AND merge_queue_state = 'resolving_conflicts'",
        params![id, human, detail],
    )? > 0;
    if changed {
        conn.execute(
            "UPDATE github_merge_intents SET status = ?2 WHERE work_item_id = ?1 AND status = 'active'",
            params![id, DISARM_PENDING],
        )?;
    }
    Ok(changed)
}

/// Drop a stopped-merge explanation once the thing it explained has ended
/// (the conflict lifted, a sign-off was given, the card moved on). Never
/// touches a live request or GitHub's own queue state.
pub(crate) fn clear_stopped(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE tasks SET merge_queue_detail = NULL
         WHERE id = ?1 AND merge_queue_state IS NULL
           AND CASE WHEN json_valid(merge_queue_detail)
               THEN json_extract(merge_queue_detail, '$.source') = 'conflict_resolution' ELSE 0 END",
        [id],
    )? > 0)
}

fn is_conflict_stop_detail(detail: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(detail)
        .ok()
        .is_some_and(|v| v.get("source").and_then(|s| s.as_str()) == Some("conflict_resolution"))
}

/// A different PR binding or an explicit move out of review cancels the request.
pub(crate) fn cancel(conn: &Connection, id: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE tasks SET merge_queue_state = NULL, merge_queue_detail = NULL
         WHERE id = ?1 AND CASE WHEN json_valid(merge_queue_detail)
             THEN json_extract(merge_queue_detail, '$.source') = 'conflict_resolution' ELSE 0 END",
        [id],
    )?;
    if changed > 0 {
        conn.execute(
            "UPDATE github_merge_intents SET status = 'cancelled' WHERE work_item_id = ?1 AND status = 'active'",
            [id],
        )?;
    }
    Ok(())
}

/// Keep resolver bookkeeping blocked internally while reporting the ongoing
/// merge to board clients. Only this engine-owned request can override a
/// conflict block; human moves and other blocking reasons remain authoritative.
pub(crate) fn project(tasks: &mut [Task], chores: &mut [Task]) {
    for task in tasks.iter_mut().chain(chores.iter_mut()) {
        // A stopped-merge explanation only describes a card under review. A
        // card that has moved on (for example back to work) must not show it.
        if task.merge_queue_state.is_none()
            && !matches!(task.status, TaskStatus::InReview | TaskStatus::Blocked)
            && task.merge_queue_detail.as_deref().is_some_and(is_conflict_stop_detail)
        {
            task.merge_queue_detail = None;
        }
        if task.merge_queue_state.as_deref() == Some(RESOLVING_MERGE_CONFLICT)
            && task.status == TaskStatus::Blocked
            && task.blocked_reason.as_deref() == Some("merge_conflict")
        {
            task.status = TaskStatus::InReview;
            task.blocked_reason = None;
            task.blocked_detail = None;
        }
    }
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Record the user's merge request before releasing a deferred conflict.
    pub fn start_merge_conflict_resolution(&self, id: &str, pr_url: &str, base: &str) -> Result<bool> {
        let conn = self.connect()?;
        start(&conn, id, pr_url, base)
    }

    /// Keep GitHub's first refusal of the merge request with the standing
    /// request, so a later stop can be diagnosed.
    pub fn note_merge_refusal(&self, id: &str, message: &str) -> Result<bool> {
        let conn = self.connect()?;
        Ok(conn.execute(
            "UPDATE tasks SET merge_queue_detail = json_set(merge_queue_detail, '$.refusal', ?2)
             WHERE id = ?1 AND merge_queue_state = 'resolving_conflicts' AND json_valid(merge_queue_detail)",
            params![id, message],
        )? > 0)
    }

    pub fn stop_merge_conflict_resolution(&self, id: &str, reason: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = stop(&tx, id, reason)?;
        tx.commit()?;
        Ok(changed)
    }

    pub fn merge_conflict_resolution_requested(&self, id: &str, pr_url: &str) -> Result<bool> {
        let conn = self.connect()?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND pr_url = ?2
             AND deleted_at IS NULL AND merge_queue_state = 'resolving_conflicts'
             AND (status = 'in_review' OR (status = 'blocked' AND blocked_reason = 'merge_conflict')))",
            params![id, pr_url],
            |row| row.get(0),
        )?)
    }

    /// Whether the request is ready to continue: the card is back in review
    /// (the resolver has released the conflict block), not merely requested.
    pub fn merge_conflict_resolution_ready(&self, id: &str, pr_url: &str) -> Result<bool> {
        let conn = self.connect()?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND pr_url = ?2 AND status = 'in_review'
             AND deleted_at IS NULL AND merge_queue_state = 'resolving_conflicts')",
            params![id, pr_url],
            |row| row.get(0),
        )?)
    }

    /// Persist the open revisions the person confirmed, before GitHub is
    /// called, so the continuation can compare against that snapshot.
    pub fn remember_confirmed_merge_revisions(
        &self,
        id: &str,
        pr_url: &str,
        revisions: &[OpenMergeRevision],
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        remember_confirmed_merge_revisions_in_tx(&tx, id, pr_url, revisions)?;
        tx.commit()?;
        Ok(())
    }

    /// Open revisions that were not in the confirmation snapshot. A missing
    /// snapshot treats every currently open revision as unconfirmed.
    pub fn unconfirmed_merge_revisions(&self, id: &str) -> Result<Vec<OpenMergeRevision>> {
        let conn = self.connect()?;
        let snapshot = confirmed_revision_ids(&conn, id)?;
        drop(conn);
        let open = self.open_merge_revisions(id)?;
        let Some(confirmed) = snapshot else {
            return Ok(open);
        };
        Ok(open
            .into_iter()
            .filter(|revision| !confirmed.iter().any(|id| id == &revision.id))
            .collect())
    }

    /// Cards that still owe GitHub a `--disable-auto`, keyed by the intent's
    /// stored PR URL so a rebound or moved card is still disarmed.
    pub fn list_pending_merge_disarms(&self) -> Result<Vec<PendingMergeCheck>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT t.id, t.product_id, i.pr_url
             FROM github_merge_intents i
             INNER JOIN tasks t ON t.id = i.work_item_id
             WHERE i.status = ?1 AND t.deleted_at IS NULL
             ORDER BY i.created_at ASC",
        )?;
        let rows = stmt.query_map([DISARM_PENDING], |row| {
            Ok(PendingMergeCheck {
                work_item_id: row.get(0)?,
                product_id: row.get(1)?,
                pr_url: row.get(2)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Whether this request was stopped while GitHub auto-merge was armed and
    /// still needs turning off. A newer recorded request for the same PR
    /// supersedes it: that merge is wanted, so the stale marker is dropped.
    /// A pending disarm for a different PR is left alone.
    pub fn merge_disarm_pending(&self, id: &str, pr_url: &str) -> Result<bool> {
        let conn = self.connect()?;
        let pending: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM github_merge_intents
             WHERE work_item_id = ?1 AND pr_url = ?2 AND status = ?3)",
            params![id, pr_url, DISARM_PENDING],
            |row| row.get(0),
        )?;
        if !pending {
            return Ok(false);
        }
        drop(conn);
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM github_merge_intents
             WHERE work_item_id = ?1 AND pr_url = ?2 AND status = ?3)",
            params![id, pr_url, DISARM_PENDING],
            |row| row.get(0),
        )?;
        if !pending {
            tx.commit()?;
            return Ok(false);
        }
        let live: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM github_merge_intents
             WHERE work_item_id = ?1 AND pr_url = ?2 AND status = 'active')",
            params![id, pr_url],
            |row| row.get(0),
        )?;
        if live {
            tx.execute(
                "UPDATE github_merge_intents SET status = 'cancelled'
                 WHERE work_item_id = ?1 AND pr_url = ?2 AND status = ?3",
                params![id, pr_url, DISARM_PENDING],
            )?;
        }
        tx.commit()?;
        Ok(!live)
    }

    /// GitHub auto-merge was turned off; restore the paused explanation if a
    /// transient disarm error had replaced it. Returns whether the card changed.
    pub fn complete_merge_disarm(&self, id: &str, pr_url: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE github_merge_intents SET status = 'conflict_resolution_stopped'
             WHERE work_item_id = ?1 AND pr_url = ?2 AND status = ?3",
            params![id, pr_url, DISARM_PENDING],
        )?;
        let changed = restore_paused_merge_label(&tx, id)?;
        tx.commit()?;
        Ok(changed)
    }

    /// Say on the card that the pause is not real yet: GitHub still has
    /// auto-merge armed because turning it off failed. The original paused
    /// explanation is kept so a later success can restore it. `error` is the
    /// full text (logged by the caller); the card label is truncated.
    pub fn record_merge_disarm_failure(&self, id: &str, error: &str) -> Result<bool> {
        let conn = self.connect()?;
        let current: Option<String> = conn
            .query_row("SELECT merge_queue_detail FROM tasks WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?
            .flatten();
        let mut detail = current
            .as_deref()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
            .filter(|value| value.get("source").and_then(|s| s.as_str()) == Some("conflict_resolution"))
            .unwrap_or_else(|| serde_json::json!({"source": "conflict_resolution"}));
        let paused = detail
            .get("paused_label")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .or_else(|| {
                let label = detail.get("status_label").and_then(|v| v.as_str())?;
                if label.contains("couldn't turn off auto-merge") {
                    None
                } else {
                    Some(label.to_owned())
                }
            });
        if let Some(paused) = paused {
            detail["paused_label"] = serde_json::Value::String(paused);
        }
        detail["status_label"] = serde_json::Value::String(format!(
            "Merge not paused: couldn't turn off auto-merge on GitHub ({})",
            truncate_disarm_error(error)
        ));
        detail["disarm_error"] = serde_json::Value::String(error.to_owned());
        let detail = detail.to_string();
        Ok(conn.execute(
            "UPDATE tasks SET merge_queue_detail = ?2
             WHERE id = ?1 AND merge_queue_state IS NULL
               AND COALESCE(merge_queue_detail, '') != ?2
               AND CASE WHEN json_valid(merge_queue_detail)
                   THEN json_extract(merge_queue_detail, '$.source') = 'conflict_resolution' ELSE 0 END",
            params![id, detail],
        )? > 0)
    }

    /// A GitHub merge succeeded but could not be recorded: keep a durable
    /// disarm obligation for this PR until auto-merge is turned off.
    pub fn record_unrecorded_merge_disarm(&self, id: &str, pr_url: &str, head_sha: &str) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE github_merge_intents SET status = ?3
             WHERE work_item_id = ?1 AND pr_url = ?2 AND status = 'active'",
            params![id, pr_url, DISARM_PENDING],
        )?;
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM github_merge_intents
             WHERE work_item_id = ?1 AND pr_url = ?2 AND status = ?3)",
            params![id, pr_url, DISARM_PENDING],
            |row| row.get(0),
        )?;
        if !pending {
            tx.execute(
                "INSERT INTO github_merge_intents (id, work_item_id, pr_url, head_sha, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![next_id("gmi"), id, pr_url, head_sha, DISARM_PENDING, now_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Replace the old, pre-rebase head with the head GitHub accepted. The
    /// transaction never exposes a gap in the card's Merging state.
    pub fn finish_merge_conflict_resolution(&self, id: &str, pr_url: &str, head: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND pr_url = ?2 AND status = 'in_review'
             AND deleted_at IS NULL AND merge_queue_state = 'resolving_conflicts')",
            params![id, pr_url],
            |row| row.get(0),
        )?;
        if !current {
            return Ok(false);
        }
        tx.execute(
            "UPDATE github_merge_intents SET status = 'head_advanced' WHERE work_item_id = ?1 AND status = 'active'",
            [id],
        )?;
        crate::github_merge_intents::record_github_merge_intent_in_tx(
            &tx,
            &GithubMergeIntentInsertInput::builder()
                .work_item_id(id)
                .pr_url(pr_url)
                .head_sha(head)
                .build(),
            &boss_engine_storage_values::merge_queue_section::queued_no_position_detail(),
        )?;
        tx.execute(
            "UPDATE tasks SET blocked_detail = NULL WHERE id = ?1 AND pr_url = ?2",
            params![id, pr_url],
        )?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_engine_board_gesture::BoardRow;
    use boss_protocol::{BoardGroup, ReviewSection};

    const PR: &str = "https://github.com/example/repo/pull/42";

    fn fixture(requested: bool) -> (WorkDb, String, String, String) {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = crate::test_support::create_test_product_named(&db, "Merging");
        let task = crate::test_support::create_test_chore_manual(&db, product.id.clone(), "Resolve and merge");
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(PR.into()),
                ..Default::default()
            },
        )
        .unwrap();
        if requested {
            db.record_github_merge_intent(
                GithubMergeIntentInsertInput::builder()
                    .work_item_id(&task.id)
                    .pr_url(PR)
                    .head_sha("old-head")
                    .build(),
                "{}",
            )
            .unwrap();
        }
        db.mark_chore_blocked_merge_conflict(&task.id, PR).unwrap();
        let attempt = db
            .insert_conflict_resolution(
                ConflictResolutionInsertInput::builder()
                    .product_id(&product.id)
                    .work_item_id(&task.id)
                    .pr_url(PR)
                    .pr_number(42)
                    .head_branch("feature")
                    .base_branch("main")
                    .base_sha_at_trigger("base")
                    .head_sha_before("old-head")
                    .build(),
            )
            .unwrap()
            .unwrap();
        (db, product.id, task.id, attempt.id)
    }

    fn card(db: &WorkDb, product: &str, id: &str) -> Task {
        db.get_work_tree(product)
            .unwrap()
            .chores
            .into_iter()
            .find(|task| task.id == id)
            .unwrap()
    }

    #[test]
    fn requested_merge_stays_visible_through_resolution_and_polling() {
        let (db, product, id, attempt) = fixture(true);
        for state in [None, Some("auto_merge_enabled")] {
            db.update_task_pr_poll_state(
                &id,
                PrPollStateInput {
                    caller: "merge_conflict_test",
                    ci_required_state: "success",
                    review_required_state: "approved",
                    ci_required_detail: None,
                    review_required_detail: None,
                    merge_queue_state: state,
                    merge_queue_detail: None,
                    preserve_merge_queue_state: false,
                    pr_mergeable_state: "conflicting",
                    pr_merge_state_status: None,
                    pr_head_sha: Some("old-head"),
                    pr_updated_at: None,
                },
            )
            .unwrap();
            let task = card(&db, &product, &id);
            assert_eq!(task.status, TaskStatus::InReview);
            assert_eq!(task.review_section, None);
            let WorkItem::Chore(single) = db.get_work_item(&id).unwrap() else {
                panic!("expected chore")
            };
            assert_eq!(single.status, task.status);
            assert_eq!(single.blocked_reason, task.blocked_reason);
            assert!(
                task.merge_queue_detail
                    .as_deref()
                    .unwrap()
                    .contains("Resolving conflicts with main")
            );
            assert_eq!(
                BoardRow {
                    status: task.status,
                    autostart: task.autostart,
                    blocked_reason: task.blocked_reason,
                    merge_queue_state: task.merge_queue_state
                }
                .position()
                .group,
                Some(BoardGroup::Merging)
            );
            // Resolver bookkeeping remains truthful; only board presentation is projected.
            assert_eq!(db.task_blocked_reason(&id).unwrap().as_deref(), Some("merge_conflict"));
        }
        db.clear_chore_blocked_merge_conflict(&id, PR).unwrap();
        db.mark_conflict_resolution_succeeded_at_rung(&attempt, Some("new-head"), 1)
            .unwrap();
        assert_eq!(
            card(&db, &product, &id).merge_queue_state.as_deref(),
            Some(RESOLVING_MERGE_CONFLICT)
        );
        db.finish_merge_conflict_resolution(&id, PR, "new-head").unwrap();
        assert_eq!(card(&db, &product, &id).merge_queue_state.as_deref(), Some("queued"));
        assert_eq!(
            db.get_active_github_merge_intent(&id, PR).unwrap().unwrap().head_sha,
            "new-head"
        );
    }

    #[test]
    fn failures_and_revision_handoffs_leave_merging_with_a_reason() {
        for outcome in ["failed", "abandoned", "revision"] {
            let (db, product, id, attempt) = fixture(true);
            match outcome {
                "failed" => {
                    db.mark_conflict_resolution_failed(&attempt, "Rebase failed").unwrap();
                }
                "abandoned" => {
                    db.mark_conflict_resolution_abandoned(&attempt, "Resolution stopped")
                        .unwrap();
                }
                _ => {
                    db.set_conflict_resolution_revision_task_id(&attempt, "revision")
                        .unwrap();
                }
            }
            let task = card(&db, &product, &id);
            assert_eq!(task.merge_queue_state, None, "{outcome}");
            assert_eq!(task.review_section, Some(ReviewSection::Blocked));
            assert!(task.blocked_detail.is_some());
            assert!(db.get_active_github_merge_intent(&id, PR).unwrap().is_none());
            db.update_task_pr_poll_state(
                &id,
                PrPollStateInput {
                    caller: "merge_conflict_test",
                    merge_queue_state: Some("auto_merge_enabled"),
                    ci_required_state: "success",
                    pr_mergeable_state: "mergeable",
                    ..Default::default()
                },
            )
            .unwrap();
            let polled = card(&db, &product, &id);
            assert_eq!(polled.merge_queue_state, None);
            assert_eq!(
                polled.merge_queue_detail, task.merge_queue_detail,
                "the failure explanation must survive polling"
            );
        }
    }

    #[test]
    fn background_resolution_does_not_become_a_merge_request() {
        let (db, product, id, _) = fixture(false);
        let task = card(&db, &product, &id);
        assert_eq!(task.status, TaskStatus::Blocked);
        assert_eq!(task.merge_queue_state, None);
        assert!(!db.merge_conflict_resolution_requested(&id, PR).unwrap());
    }

    #[test]
    fn human_moves_and_pr_rebinding_override_the_merge_request() {
        let (db, product, id, _) = fixture(true);
        db.update_work_item(
            &id,
            WorkItemPatch {
                status: Some("active".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(card(&db, &product, &id).status, TaskStatus::Active);
        assert!(!db.merge_conflict_resolution_requested(&id, PR).unwrap());
        db.update_work_item(
            &id,
            WorkItemPatch {
                status: Some("in_review".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            !db.merge_conflict_resolution_requested(&id, PR).unwrap(),
            "returning to review cannot resurrect a cancelled merge"
        );
        db.start_merge_conflict_resolution(&id, PR, "main").unwrap();
        let replacement = "https://github.com/example/repo/pull/43";
        db.update_work_item(
            &id,
            WorkItemPatch {
                pr_url: Some(replacement.into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!db.merge_conflict_resolution_requested(&id, replacement).unwrap());
        assert_eq!(card(&db, &product, &id).merge_queue_state, None);
    }

    #[test]
    fn changing_the_blocked_reason_cancels_the_merge_continuation() {
        let (db, product, id, _) = fixture(true);
        db.update_work_item(
            &id,
            WorkItemPatch {
                blocked_reason: Some("dependency".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(card(&db, &product, &id).merge_queue_state, None);
        assert!(!db.merge_conflict_resolution_requested(&id, PR).unwrap());
    }

    #[test]
    fn a_clean_probe_waits_for_an_in_flight_mechanical_rung() {
        let (db, product, id, attempt) = fixture(true);
        db.stamp_conflict_resolution_mechanical_rung(&attempt, 1, "lease", "ws")
            .unwrap();
        db.clear_resolved_pr_signal(&id, PR, false).unwrap();
        let live = db.get_conflict_resolution(&attempt).unwrap().unwrap();
        assert_eq!(live.status, "pending", "the resolver owns completion");
        assert_eq!(db.task_blocked_reason(&id).unwrap().as_deref(), Some("merge_conflict"));
        assert!(db.active_conflict_resolution_for_work_item(&id).unwrap().is_some());
        assert_eq!(
            card(&db, &product, &id).merge_queue_state.as_deref(),
            Some(RESOLVING_MERGE_CONFLICT)
        );
        assert!(!db.merge_conflict_resolution_ready(&id, PR).unwrap());
        // Once the rung concludes, a clean probe may retire it as before.
        db.clear_conflict_resolution_mechanical_rung(&attempt).unwrap();
        db.clear_resolved_pr_signal(&id, PR, false).unwrap();
        assert!(db.active_conflict_resolution_for_work_item(&id).unwrap().is_none());
        assert!(db.merge_conflict_resolution_ready(&id, PR).unwrap());
    }

    #[test]
    fn a_stale_head_supersede_keeps_the_merge_request_but_other_abandons_stop_it() {
        let (db, product, id, attempt) = fixture(true);
        db.mark_conflict_resolution_abandoned(&attempt, "superseded_stale_head")
            .unwrap();
        assert_eq!(
            card(&db, &product, &id).merge_queue_state.as_deref(),
            Some(RESOLVING_MERGE_CONFLICT)
        );
        assert!(db.get_active_github_merge_intent(&id, PR).unwrap().is_some());
        let fresh = db
            .insert_conflict_resolution(
                ConflictResolutionInsertInput::builder()
                    .product_id(&product)
                    .work_item_id(&id)
                    .pr_url(PR)
                    .pr_number(42)
                    .head_branch("feature")
                    .base_branch("main")
                    .base_sha_at_trigger("base-2")
                    .head_sha_before("old-head")
                    .build(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            card(&db, &product, &id).merge_queue_state.as_deref(),
            Some(RESOLVING_MERGE_CONFLICT)
        );
        db.mark_conflict_resolution_abandoned(&fresh.id, "manual_abandon")
            .unwrap();
        let task = card(&db, &product, &id);
        assert_eq!(task.merge_queue_state, None);
        let label = task.merge_queue_detail.unwrap();
        assert!(label.contains("stopped manually"), "{label}");
        assert!(!label.contains("manual_abandon"), "{label}");
    }

    fn add_open_revision(db: &WorkDb, product: &str, parent: &str, created_at: &str) -> String {
        let revision = crate::test_support::create_test_chore_manual(db, product.to_owned(), "Revision");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET kind = 'revision', parent_task_id = ?2, status = 'active', created_at = ?3 WHERE id = ?1",
                params![revision.id, parent, created_at],
            )
            .unwrap();
        revision.id
    }

    fn poll_state(db: &WorkDb, id: &str, state: Option<&str>) {
        db.update_task_pr_poll_state(
            id,
            PrPollStateInput {
                caller: "merge_conflict_test",
                merge_queue_state: state,
                ci_required_state: "success",
                pr_mergeable_state: "mergeable",
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn only_revisions_outside_the_confirmation_snapshot_stop_the_continuation() {
        let (db, product, id, _) = fixture(true);
        let confirmed = add_open_revision(&db, &product, &id, "1");
        db.remember_confirmed_merge_revisions(
            &id,
            PR,
            &[OpenMergeRevision {
                id: confirmed.clone(),
                label: "r1".into(),
                status: "active".into(),
            }],
        )
        .unwrap();
        db.start_merge_conflict_resolution(&id, PR, "main").unwrap();
        assert!(db.unconfirmed_merge_revisions(&id).unwrap().is_empty());
        let newer = add_open_revision(&db, &product, &id, "99999999999");
        let open = db.unconfirmed_merge_revisions(&id).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, newer);
        assert_ne!(open[0].id, confirmed);
    }

    #[test]
    fn a_reopened_revision_is_unconfirmed_even_if_it_was_created_before_the_request() {
        let (db, product, id, _) = fixture(true);
        let closed = add_open_revision(&db, &product, &id, "1");
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&closed])
            .unwrap();
        db.remember_confirmed_merge_revisions(&id, PR, &[]).unwrap();
        db.start_merge_conflict_resolution(&id, PR, "main").unwrap();
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&closed])
            .unwrap();
        let open = db.unconfirmed_merge_revisions(&id).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, closed);
    }

    #[test]
    fn a_revision_opened_while_the_github_request_is_in_flight_is_unconfirmed() {
        let (db, product, id, _) = fixture(true);
        db.remember_confirmed_merge_revisions(&id, PR, &[]).unwrap();
        let in_flight = add_open_revision(&db, &product, &id, "1");
        db.start_merge_conflict_resolution(&id, PR, "main").unwrap();
        let open = db.unconfirmed_merge_revisions(&id).unwrap();
        assert_eq!(
            open.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec![in_flight.as_str()]
        );
    }

    #[test]
    fn a_stopped_merge_is_turned_off_on_github_before_the_card_stays_paused() {
        let (db, product, id, attempt) = fixture(true);
        db.mark_conflict_resolution_failed(&attempt, "Rebase failed").unwrap();
        assert!(db.get_active_github_merge_intent(&id, PR).unwrap().is_none());
        assert!(db.merge_disarm_pending(&id, PR).unwrap(), "auto-merge is still armed");
        // While the disarm is outstanding, GitHub's armed report cannot hide the pause.
        poll_state(&db, &id, Some("auto_merge_enabled"));
        assert_eq!(card(&db, &product, &id).merge_queue_state, None);
        db.record_merge_disarm_failure(&id, "boom\nmore gh stderr").unwrap();
        let failed = card(&db, &product, &id).merge_queue_detail.unwrap();
        let failed_json: serde_json::Value = serde_json::from_str(&failed).unwrap();
        let label = failed_json["status_label"].as_str().unwrap();
        assert!(label.contains("couldn't turn off auto-merge"), "{label}");
        assert!(label.contains("boom"), "{label}");
        assert!(
            !label.contains("more gh stderr"),
            "the card label truncates extra gh stderr lines: {label}"
        );
        assert_eq!(
            failed_json["disarm_error"].as_str(),
            Some("boom\nmore gh stderr"),
            "the full error stays in the detail JSON for the log"
        );
        db.complete_merge_disarm(&id, PR).unwrap();
        assert!(!db.merge_disarm_pending(&id, PR).unwrap());
        let restored = card(&db, &product, &id).merge_queue_detail.unwrap();
        assert!(restored.contains("Merge paused"), "{restored}");
        assert!(!restored.contains("couldn't turn off auto-merge"), "{restored}");
        poll_state(&db, &id, None);
        let after_empty = card(&db, &product, &id).merge_queue_detail.unwrap();
        assert!(after_empty.contains("Merge paused"), "{after_empty}");
        assert!(!after_empty.contains("couldn't turn off auto-merge"), "{after_empty}");
        // Once disarmed, a live GitHub report is shown as it is.
        poll_state(&db, &id, Some("auto_merge_enabled"));
        assert_eq!(
            card(&db, &product, &id).merge_queue_state.as_deref(),
            Some("auto_merge_enabled")
        );
    }

    #[test]
    fn a_newer_merge_request_supersedes_a_pending_disarm() {
        let (db, _, id, attempt) = fixture(true);
        db.mark_conflict_resolution_failed(&attempt, "Rebase failed").unwrap();
        db.record_github_merge_intent(
            GithubMergeIntentInsertInput::builder()
                .work_item_id(&id)
                .pr_url(PR)
                .head_sha("new-head")
                .build(),
            "{}",
        )
        .unwrap();
        assert!(
            !db.merge_disarm_pending(&id, PR).unwrap(),
            "the new request must stay armed"
        );
        assert!(db.get_active_github_merge_intent(&id, PR).unwrap().is_some());
    }

    #[test]
    fn a_pending_disarm_for_one_pr_survives_a_new_request_for_another() {
        let (db, _, id, attempt) = fixture(true);
        db.mark_conflict_resolution_failed(&attempt, "Rebase failed").unwrap();
        assert!(db.merge_disarm_pending(&id, PR).unwrap());
        let other = "https://github.com/example/repo/pull/43";
        db.record_github_merge_intent(
            GithubMergeIntentInsertInput::builder()
                .work_item_id(&id)
                .pr_url(other)
                .head_sha("other-head")
                .build(),
            "{}",
        )
        .unwrap();
        assert!(
            db.merge_disarm_pending(&id, PR).unwrap(),
            "PR A's disarm must not be cancelled by a request for PR B"
        );
        assert!(db.get_active_github_merge_intent(&id, other).unwrap().is_some());
    }

    #[test]
    fn observed_github_auto_merge_is_disarmed_when_resolution_stops() {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = crate::test_support::create_test_product_named(&db, "Observed arm");
        let task = crate::test_support::create_test_chore_manual(&db, product.id.clone(), "Merge");
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(PR.into()),
                ..Default::default()
            },
        )
        .unwrap();
        db.update_task_pr_poll_state(
            &task.id,
            PrPollStateInput {
                caller: "merge_conflict_test",
                merge_queue_state: Some("auto_merge_enabled"),
                ci_required_state: "success",
                pr_mergeable_state: "mergeable",
                pr_head_sha: Some("polled-head"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(db.get_active_github_merge_intent(&task.id, PR).unwrap().is_none());
        db.mark_chore_blocked_merge_conflict(&task.id, PR).unwrap();
        assert_eq!(
            db.get_active_github_merge_intent(&task.id, PR)
                .unwrap()
                .unwrap()
                .head_sha,
            "polled-head"
        );
        let attempt = db
            .insert_conflict_resolution(
                ConflictResolutionInsertInput::builder()
                    .product_id(&product.id)
                    .work_item_id(&task.id)
                    .pr_url(PR)
                    .pr_number(42)
                    .head_branch("feature")
                    .base_branch("main")
                    .base_sha_at_trigger("base")
                    .head_sha_before("polled-head")
                    .build(),
            )
            .unwrap()
            .unwrap();
        db.mark_conflict_resolution_failed(&attempt.id, "Rebase failed")
            .unwrap();
        assert!(db.merge_disarm_pending(&task.id, PR).unwrap());
    }

    #[test]
    fn the_stopped_explanation_ends_with_its_cause() {
        // A revision lands and the conflict lifts.
        let (db, product, id, attempt) = fixture(true);
        db.set_conflict_resolution_revision_task_id(&attempt, "revision")
            .unwrap();
        assert!(card(&db, &product, &id).merge_queue_detail.is_some());
        db.clear_chore_blocked_merge_conflict(&id, PR).unwrap();
        assert_eq!(card(&db, &product, &id).merge_queue_detail, None);

        // A person approves the deletion sign-off by moving the card back to review.
        let (db, product, id, _) = fixture(true);
        db.mark_chore_blocked_deletion_signoff(&id, PR).unwrap();
        let halted = card(&db, &product, &id);
        assert!(
            halted
                .merge_queue_detail
                .unwrap()
                .contains("needs approval for deleted files")
        );
        db.update_work_item(
            &id,
            WorkItemPatch {
                status: Some("in_review".into()),
                blocked_reason: Some(String::new()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(card(&db, &product, &id).merge_queue_detail, None);
    }

    #[test]
    fn a_stopped_explanation_never_shows_on_a_card_that_moved_to_work() {
        let (db, product, id, attempt) = fixture(true);
        db.mark_conflict_resolution_failed(&attempt, "Rebase failed").unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'active', blocked_reason = NULL WHERE id = ?1",
                [&id],
            )
            .unwrap();
        assert_eq!(card(&db, &product, &id).merge_queue_detail, None);
    }

    #[test]
    fn stopping_keeps_the_readable_blocked_detail_and_leaves_review_rows_alone() {
        let (db, product, id, attempt) = fixture(true);
        db.mark_conflict_resolution_abandoned(&attempt, "manual_abandon")
            .unwrap();
        let detail = card(&db, &product, &id).blocked_detail;
        assert_ne!(detail.as_deref(), Some("manual_abandon"), "{detail:?}");

        let (db, product, id, _) = fixture(true);
        db.clear_chore_blocked_merge_conflict(&id, PR).unwrap();
        assert!(
            db.stop_merge_conflict_resolution(&id, "Merge failed after conflict resolution: x")
                .unwrap()
        );
        assert_eq!(card(&db, &product, &id).blocked_detail, None);
    }

    #[test]
    fn reason_codes_become_short_prose() {
        assert_eq!(
            human_reason("revision_create_failed"),
            "couldn't start a revision to fix the conflict"
        );
        assert_eq!(
            human_reason("lockfile_regeneration_failed: cargo exploded"),
            "couldn't regenerate the lockfile"
        );
        assert_eq!(human_reason("some_new_code"), "conflict resolution didn't finish");
        assert_eq!(human_reason("Rebase failed"), "Rebase failed");
    }
}
