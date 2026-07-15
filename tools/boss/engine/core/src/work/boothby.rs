//! Pre/post-image capture for Boothby, Boss's autonomous groundskeeper.
//!
//! Boothby edits the taxonomy without a human in the loop, so every row it
//! touches has to be reconstructable afterwards — that is what the UI's
//! undo affordance is built on. This module is the journal's write half for
//! WorkDb effects: when a mutation runs with `actor = "boothby"`, it diffs
//! the row before and after and appends a `boothby_actions` row holding the
//! touched columns' old and new values.
//!
//! Design: `tools/boss/docs/designs/boothby.md` §"Audit & undo data model".
//!
//! ## How the executor's intent reaches the journal
//!
//! A `boothby_actions` row needs a `verb`, a `rationale` and a
//! `reversibility` — all `NOT NULL`, and none of them knowable from a
//! column delta. They come from the executor's verb catalogue (task 2 of
//! the design's breakdown). Rather than thread them through `update_task`,
//! `delete_work_item` and every future mutation, the executor *arms* a
//! [`BoothbyActionContext`] on the `WorkDb` ([`WorkDb::arm_boothby_action`])
//! and then calls the ordinary actor-attributed path. The mutation layer
//! picks the context up on its way past.
//!
//! ## Three properties this module exists to guarantee
//!
//! 1. **Inert for every other actor.** [`capture_task_update`] and friends
//!    take the actor and return `Ok(())` immediately unless it is Boothby.
//!    A `human` / `boss` / `engine` write does one string compare more than
//!    it used to and is otherwise untouched.
//!
//! 2. **Same transaction as the write.** Every helper takes the caller's
//!    `&Connection` (their open `tx`), so an action row can never commit
//!    without its mutation, nor a mutation without its action row. This
//!    mirrors [`super::audit_misc::record_design_doc_audit`].
//!
//! 3. **No journal, no mutation.** A Boothby write with no armed context or
//!    no open pass is *refused*, rolling back the transaction. An
//!    unexplained autonomous change is precisely what the journal exists to
//!    prevent, so failing closed is the only safe answer — a mutation that
//!    silently escaped the audit trail would be worse than no mutation.

use std::collections::BTreeMap;

use super::*;

/// A row's values keyed by column name, for the subset of columns an
/// audited mutation could touch.
///
/// `BTreeMap` rather than `HashMap` for deterministic key order: these
/// serialise straight into `pre_image` / `post_image` JSON, and stable
/// ordering keeps those blobs diffable and testable.
///
/// Values are `Option<String>` — the SQL rendering of the column, with
/// `None` for SQL `NULL` — so a column going `NULL` -> `'x'` is a real
/// change rather than an indistinguishable empty string.
pub(crate) type ColumnImage = BTreeMap<&'static str, Option<String>>;

/// What the executor is doing, supplied before it calls the mutation layer.
///
/// Carries exactly the `boothby_actions` columns that a column delta cannot
/// supply. `pass_id` and `seq` are resolved at write time instead — the
/// open pass is a database fact, not caller intent.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct BoothbyActionContext {
    /// Catalogue slug, e.g. `close_stale_task`.
    pub verb: String,
    /// Agent-supplied one-liner explaining the call.
    pub rationale: String,
    /// [`BOOTHBY_REVERSIBILITY_REVERSIBLE`] | `..._SEMI` | `..._IRREVERSIBLE`.
    pub reversibility: String,
    /// JSON: the verb's inputs.
    pub params: Option<String>,
}

/// Disarms the action context on drop, so a panicking or early-returning
/// executor cannot leave a stale verb armed for the next mutation to
/// mislabel itself with.
pub struct BoothbyActionGuard<'a> {
    db: &'a WorkDb,
}

impl Drop for BoothbyActionGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.db.boothby_action.lock() {
            *slot = None;
        }
    }
}

impl WorkDb {
    /// Arm the journal for the actor-boothby mutations made while the
    /// returned guard is alive. Disarms on drop.
    ///
    /// Intended for the executor (task 2). Arming does not itself mutate
    /// anything; a mutation with a different actor ignores the context
    /// entirely.
    pub fn arm_boothby_action(&self, context: BoothbyActionContext) -> Result<BoothbyActionGuard<'_>> {
        let mut slot = self
            .boothby_action
            .lock()
            .map_err(|_| anyhow::anyhow!("boothby action context lock poisoned"))?;
        *slot = Some(context);
        drop(slot);
        Ok(BoothbyActionGuard { db: self })
    }

    fn armed_boothby_action(&self) -> Result<Option<BoothbyActionContext>> {
        Ok(self
            .boothby_action
            .lock()
            .map_err(|_| anyhow::anyhow!("boothby action context lock poisoned"))?
            .clone())
    }
}

/// `true` when this mutation is Boothby's. The single gate that keeps the
/// journal inert for every other actor.
pub(crate) fn is_boothby_actor(actor: &str) -> bool {
    actor == LAST_STATUS_ACTOR_BOOTHBY
}

/// The columns of `tasks` that a journalled Boothby mutation can move.
///
/// Deliberately omits `updated_at`: it changes on literally every write, so
/// including it would make every image non-empty and defeat the "touched
/// columns only" rule that suppresses no-op actions. Undo does not want to
/// restore it either — reverting is itself a write and earns a fresh
/// `updated_at`.
///
/// `last_status_actor` IS included, and that matters for undo: reverting a
/// Boothby close must hand the row back to whoever owned it before,
/// otherwise the row would stay stamped `boothby` forever and the
/// dep-unblock rule ([`StatusActor::is_engine_cascade`]) would keep reading
/// it as a deliberate Boothby decision that no longer exists.
pub(crate) fn task_image(task: &Task) -> ColumnImage {
    ColumnImage::from([
        ("archived_reason", task.archived_reason.clone()),
        ("autostart", Some(task.autostart.to_string())),
        ("blocked_attempt_id", task.blocked_attempt_id.clone()),
        ("blocked_reason", task.blocked_reason.clone()),
        ("completed_at", task.completed_at.clone()),
        ("deleted_at", task.deleted_at.clone()),
        ("description", Some(task.description.clone())),
        ("driver", task.driver.clone()),
        ("effort_level", task.effort_level.map(|e| e.as_str().to_owned())),
        ("last_status_actor", Some(task.last_status_actor.clone())),
        ("model_override", task.model_override.clone()),
        ("name", Some(task.name.clone())),
        ("ordinal", task.ordinal.map(|o| o.to_string())),
        ("pr_url", task.pr_url.clone()),
        ("priority", Some(task.priority.clone())),
        ("repo_remote_url", task.repo_remote_url.clone()),
        ("status", Some(task.status.as_str().to_owned())),
    ])
}

/// The columns of `projects` that a journalled Boothby mutation can move.
/// Same `updated_at` exclusion and `last_status_actor` inclusion rules as
/// [`task_image`].
pub(crate) fn project_image(project: &Project) -> ColumnImage {
    ColumnImage::from([
        ("description", Some(project.description.clone())),
        ("goal", Some(project.goal.clone())),
        ("last_status_actor", Some(project.last_status_actor.clone())),
        ("name", Some(project.name.clone())),
        ("priority", Some(project.priority.clone())),
        ("slug", Some(project.slug.clone())),
        ("status", Some(project.status.as_str().to_owned())),
    ])
}

/// The columns of `attention_groups` that a journalled Boothby mutation can
/// move. Boothby merges and dismisses attention groups; both land here.
pub(crate) fn attention_group_image(group: &AttentionGroup) -> ColumnImage {
    ColumnImage::from([
        ("actioned_at", group.actioned_at.clone()),
        ("dismissed_at", group.dismissed_at.clone()),
        ("state", Some(group.state.clone())),
    ])
}

/// Reduce a before/after pair to just the columns that changed.
///
/// Returns `None` when nothing moved — the caller then appends no action
/// row at all.
///
/// Both images come from the same `*_image` builder for a given target
/// kind, so their key sets are identical by construction; a key present in
/// one and not the other would be a bug in that builder, and is treated
/// here as a change to `NULL` rather than being silently dropped.
fn changed_columns(before: &ColumnImage, after: &ColumnImage) -> Option<(String, String)> {
    let mut pre = serde_json::Map::new();
    let mut post = serde_json::Map::new();

    for (column, old) in before {
        let new = after.get(column).unwrap_or(&None);
        if old == new {
            continue;
        }
        pre.insert((*column).to_owned(), json_column(old));
        post.insert((*column).to_owned(), json_column(new));
    }

    if pre.is_empty() {
        return None;
    }
    Some((
        serde_json::Value::Object(pre).to_string(),
        serde_json::Value::Object(post).to_string(),
    ))
}

/// SQL `NULL` renders as JSON `null`, everything else as a JSON string.
/// Keeping `NULL` distinct from `""` is what lets undo restore a
/// genuinely-null column instead of writing an empty string over it.
fn json_column(value: &Option<String>) -> serde_json::Value {
    match value {
        Some(text) => serde_json::Value::String(text.clone()),
        None => serde_json::Value::Null,
    }
}

/// The pass an action belongs to: the one still open (`finished_at IS
/// NULL`). Well-defined only because `boothby_passes_single_open_idx` makes
/// a second concurrent open pass impossible.
fn open_pass_id(conn: &Connection) -> Result<Option<String>> {
    conn.query_row("SELECT id FROM boothby_passes WHERE finished_at IS NULL", [], |row| {
        row.get::<_, String>(0)
    })
    .optional()
    .map_err(Into::into)
}

/// Next `seq` within `pass_id`. Read inside the caller's transaction, and
/// `boothby_actions_by_pass` is UNIQUE on `(pass_id, seq)`, so a racing
/// writer collides loudly rather than silently reusing an ordinal.
fn next_seq(conn: &Connection, pass_id: &str) -> Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM boothby_actions WHERE pass_id = ?1",
        [pass_id],
        |row| row.get::<_, i64>(0),
    )
    .map_err(Into::into)
}

/// Append one `boothby_actions` row for a mutation, inside the caller's
/// transaction. No-op (returning `Ok(None)`) when no column changed.
///
/// Errors — rolling the caller's mutation back — when there is no armed
/// context or no open pass. See this module's header: no journal, no
/// mutation.
fn record_action_in_tx(
    conn: &Connection,
    db: &WorkDb,
    target_kind: &str,
    target_id: &str,
    before: &ColumnImage,
    after: &ColumnImage,
    now: &str,
) -> Result<Option<String>> {
    let Some((pre_image, post_image)) = changed_columns(before, after) else {
        return Ok(None);
    };

    let context = db.armed_boothby_action()?.with_context(|| {
        format!(
            "refusing an unjournalled Boothby mutation of {target_kind} {target_id}: \
             no action context is armed. The executor must call \
             `arm_boothby_action` (verb + rationale + reversibility) before \
             mutating as actor `{LAST_STATUS_ACTOR_BOOTHBY}`."
        )
    })?;
    let pass_id = open_pass_id(conn)?.with_context(|| {
        format!(
            "refusing an unjournalled Boothby mutation of {target_kind} {target_id}: \
             no Boothby pass is open, and every action belongs to a pass."
        )
    })?;
    let seq = next_seq(conn, &pass_id)?;

    // An irreversible verb journals `params` + evidence instead of a
    // restorable pre-image, per the design.
    let pre_image = (context.reversibility != BOOTHBY_REVERSIBILITY_IRREVERSIBLE).then_some(pre_image);

    let id = next_id("ba");
    conn.execute(
        "INSERT INTO boothby_actions
            (id, pass_id, seq, verb, target_kind, target_id, params, rationale,
             pre_image, post_image, reversibility, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            id,
            pass_id,
            seq,
            context.verb,
            target_kind,
            target_id,
            context.params,
            context.rationale,
            pre_image,
            post_image,
            context.reversibility,
            now,
        ],
    )?;
    conn.execute(
        "UPDATE boothby_passes SET actions_count = actions_count + 1 WHERE id = ?1",
        params![pass_id],
    )?;

    tracing::debug!(
        action_id = %id,
        verb = %context.verb,
        target_id,
        pass_id = %pass_id,
        seq,
        "boothby: journalled action pre/post image",
    );
    Ok(Some(id))
}

/// Journal a `tasks` mutation iff `actor` is Boothby. Inert otherwise.
pub(crate) fn capture_task_update(
    conn: &Connection,
    db: &WorkDb,
    actor: &str,
    before: &Task,
    after: &Task,
    now: &str,
) -> Result<()> {
    if !is_boothby_actor(actor) {
        return Ok(());
    }
    record_action_in_tx(
        conn,
        db,
        BOOTHBY_TARGET_TASK,
        &after.id,
        &task_image(before),
        &task_image(after),
        now,
    )?;
    Ok(())
}

/// Journal a `projects` mutation iff `actor` is Boothby. Inert otherwise.
pub(crate) fn capture_project_update(
    conn: &Connection,
    db: &WorkDb,
    actor: &str,
    before: &Project,
    after: &Project,
    now: &str,
) -> Result<()> {
    if !is_boothby_actor(actor) {
        return Ok(());
    }
    record_action_in_tx(
        conn,
        db,
        BOOTHBY_TARGET_PROJECT,
        &after.id,
        &project_image(before),
        &project_image(after),
        now,
    )?;
    Ok(())
}

/// Journal an `attention_groups` mutation iff `actor` is Boothby. Inert
/// otherwise.
pub(crate) fn capture_attention_group_update(
    conn: &Connection,
    db: &WorkDb,
    actor: &str,
    before: &AttentionGroup,
    after: &AttentionGroup,
    now: &str,
) -> Result<()> {
    if !is_boothby_actor(actor) {
        return Ok(());
    }
    record_action_in_tx(
        conn,
        db,
        BOOTHBY_TARGET_ATTENTION,
        &after.id,
        &attention_group_image(before),
        &attention_group_image(after),
        now,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(pairs: &[(&'static str, Option<&str>)]) -> ColumnImage {
        pairs.iter().map(|(col, val)| (*col, val.map(str::to_owned))).collect()
    }

    /// A minimal `todo` task owned by `human`. Only the columns each test
    /// moves are interesting; the rest just have to be present.
    fn sample_task() -> Task {
        Task::builder()
            .id("task_1")
            .product_id("prod_1")
            .kind(TaskKind::Chore)
            .name("a chore")
            .description("")
            .status(TaskStatus::Todo)
            .created_at("1700000000")
            .updated_at("1700000000")
            .build()
    }

    #[test]
    fn is_boothby_actor_matches_only_the_boothby_literal() {
        assert!(is_boothby_actor(LAST_STATUS_ACTOR_BOOTHBY));
        for other in ["human", "boss", "engine", "Boothby", "boothby ", ""] {
            assert!(!is_boothby_actor(other), "{other:?} must not read as boothby");
        }
    }

    #[test]
    fn changed_columns_is_none_when_nothing_moved() {
        let before = image(&[("status", Some("todo")), ("name", Some("a"))]);
        assert_eq!(changed_columns(&before.clone(), &before), None);
    }

    #[test]
    fn changed_columns_keeps_only_the_touched_columns() {
        let before = image(&[("status", Some("todo")), ("name", Some("a"))]);
        let after = image(&[("status", Some("archived")), ("name", Some("a"))]);

        let (pre, post) = changed_columns(&before, &after).expect("status moved");
        // `name` is unchanged and must not appear in either image, or an
        // undo would rewrite a column Boothby never touched.
        assert_eq!(pre, r#"{"status":"todo"}"#);
        assert_eq!(post, r#"{"status":"archived"}"#);
    }

    #[test]
    fn changed_columns_distinguishes_null_from_empty_string() {
        let before = image(&[("archived_reason", None)]);
        let after = image(&[("archived_reason", Some(""))]);

        let (pre, post) = changed_columns(&before, &after).expect("NULL -> '' is a change");
        assert_eq!(pre, r#"{"archived_reason":null}"#);
        assert_eq!(post, r#"{"archived_reason":""}"#);
    }

    #[test]
    fn changed_columns_emits_sorted_keys_so_images_are_stable() {
        let before = image(&[("status", Some("todo")), ("name", Some("a")), ("driver", None)]);
        let after = image(&[
            ("status", Some("done")),
            ("name", Some("b")),
            ("driver", Some("claude")),
        ]);

        let (pre, _) = changed_columns(&before, &after).expect("all three moved");
        assert_eq!(pre, r#"{"driver":null,"name":"a","status":"todo"}"#);
    }

    #[test]
    fn task_image_excludes_updated_at_so_a_bare_touch_is_not_an_action() {
        let task = sample_task();
        let mut touched = task.clone();
        touched.updated_at = "9999999999".to_owned();

        assert_eq!(
            changed_columns(&task_image(&task), &task_image(&touched)),
            None,
            "moving only updated_at must not register as a touched column",
        );
    }

    #[test]
    fn task_image_tracks_last_status_actor_so_undo_can_restore_ownership() {
        let task = sample_task();
        let mut taken = task.clone();
        taken.last_status_actor = LAST_STATUS_ACTOR_BOOTHBY.to_owned();

        let (pre, post) = changed_columns(&task_image(&task), &task_image(&taken)).expect("last_status_actor moved");
        assert_eq!(pre, r#"{"last_status_actor":"human"}"#);
        assert_eq!(post, r#"{"last_status_actor":"boothby"}"#);
    }
}
