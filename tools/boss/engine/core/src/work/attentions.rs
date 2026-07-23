//! Attention store — the engine core for the Attentions feature
//! (design: `tools/boss/docs/designs/attentions.md`).
//!
//! An *attention* is an agent-authored, human-actionable notification
//! (a `question` or a `followup`). Attentions never stand alone: each is a
//! member of an [`AttentionGroup`], the unit the human reads and acts on.
//! This module owns creation + reconciliation, the answer/dismiss state
//! transitions, and — via [`WorkDb::action_attention_group`] — producing the
//! single downstream artifact when the human actions a group: a revision (or
//! a fresh design task) for a question group, or a batch task-create for a
//! followup group.
//!
//! Reconciliation is an upsert on the `(grouping_key, generation)` unique
//! index: re-running a source that emits the same questions/followups joins
//! the open group of the current generation rather than spawning a second
//! one. Once a group is `actioned`/`dismissed` (terminal), a re-run bumps
//! `generation` and starts a fresh group — this is what keeps "one group ⇒
//! one revision" true across iteration.

use super::*;
use std::collections::HashSet;

/// Canonical column order for `attention_groups` SELECTs. Must stay in
/// lockstep with [`map_attention_group`].
const GROUP_COLS: &str = "id, product_id, short_id, kind, \
     association_project_id, association_task_id, source_kind, source_task_id, \
     source_run_id, source_doc_path, source_doc_repo_remote_url, source_doc_branch, \
     grouping_key, generation, state, produced_artifact_kind, produced_artifact_ref, \
     created_at, actioned_at, dismissed_at";

/// Canonical column order for `attentions` SELECTs. Must stay in lockstep
/// with [`map_attention`].
const ATTN_COLS: &str = "id, group_id, ordinal, source_anchor, answer_state, \
     created_at, answered_at, question_type, prompt_text, choice_options, answer, \
     proposed_name, proposed_description, proposed_effort, proposed_work_kind, \
     rationale, confidence_source, score, linked_work_item_id, source_proposal_id";

/// A group in `actioned`/`dismissed` is terminal: members can no longer be
/// changed and new attentions for the same key form a fresh generation.
fn group_is_terminal(state: &str) -> bool {
    matches!(state, "actioned" | "dismissed")
}

/// Stable content key used by [`WorkDb::reconcile_attentions`] for
/// member-level dedup. Two members with the same key are "the same
/// question / followup re-emitted" and the second is skipped, so a
/// re-detected PR or a re-emitted `FOLLOWUPS:` block never appends
/// duplicate members within a generation. The unit separator (`\u{1f}`)
/// keeps the joined fields unambiguous.
///
/// - **question** → `question_type` + `prompt_text` + `source_anchor`
///   (a worker may legitimately ask the same prompt about two different
///   doc sections, so the anchor is part of the identity).
/// - **followup** → `proposed_name` (the title is the human-meaningful
///   identity; re-phrased descriptions of the same proposal collapse).
fn content_key(
    kind: &str,
    question_type: Option<&str>,
    prompt_text: Option<&str>,
    source_anchor: Option<&str>,
    proposed_name: Option<&str>,
) -> String {
    match kind {
        "question" => format!(
            "q\u{1f}{}\u{1f}{}\u{1f}{}",
            question_type.unwrap_or_default(),
            prompt_text.unwrap_or_default(),
            source_anchor.unwrap_or_default(),
        ),
        "followup" => format!("f\u{1f}{}", proposed_name.unwrap_or_default()),
        other => format!("{other}\u{1f}{}", prompt_text.unwrap_or_default()),
    }
}

fn query_attention_group(conn: &Connection, id: &str) -> Result<Option<AttentionGroup>> {
    conn.query_row(
        &format!("SELECT {GROUP_COLS} FROM attention_groups WHERE id = ?1"),
        [id],
        map_attention_group,
    )
    .optional()
    .map_err(Into::into)
}

fn query_attention(conn: &Connection, id: &str) -> Result<Option<Attention>> {
    conn.query_row(
        &format!("SELECT {ATTN_COLS} FROM attentions WHERE id = ?1"),
        [id],
        map_attention,
    )
    .optional()
    .map_err(Into::into)
}

/// Resolve a group reference to its row. Accepts the canonical `atg_…` id
/// or an `A<n>` per-product short id. Because the lookup wire request
/// carries no product, an `A<n>` is resolved across all products and is an
/// error when it is ambiguous (the caller should use the `atg_…` id).
fn resolve_group(conn: &Connection, id: &str) -> Result<Option<AttentionGroup>> {
    if let Some(rest) = id.strip_prefix('A')
        && let Ok(short_id) = rest.parse::<i64>()
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT {GROUP_COLS} FROM attention_groups WHERE short_id = ?1"
        ))?;
        let mut groups = collect_rows(stmt.query_map([short_id], map_attention_group)?)?;
        return match groups.len() {
            0 => Ok(None),
            1 => Ok(Some(groups.remove(0))),
            _ => bail!(
                "attention short id A{short_id} is ambiguous across products; \
                     use the atg_… id"
            ),
        };
    }
    query_attention_group(conn, id)
}

/// Derive the stable grouping key per the design's two concrete shapes:
/// `question|{project_id}|doc:{path}` and `followup|{task_id}`. Used only
/// when the caller passes neither an explicit `group_id` nor a `group_key`.
fn derive_grouping_key(input: &CreateAttentionInput) -> Result<String> {
    match input.kind.as_str() {
        "question" => {
            let project_id = input
                .association_project_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .context(
                    "question attention needs association_project_id to derive a grouping key \
                     (or pass group_id / group_key)",
                )?;
            let doc_path = input.source_doc_path.as_deref().filter(|s| !s.is_empty()).context(
                "question attention needs source_doc_path to derive a grouping key \
                     (or pass group_id / group_key)",
            )?;
            Ok(format!("question|{project_id}|doc:{doc_path}"))
        }
        "followup" => {
            let task_id = input
                .source_task_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .or_else(|| input.association_task_id.as_deref().filter(|s| !s.is_empty()))
                .context(
                    "followup attention needs source_task_id or association_task_id to derive \
                     a grouping key (or pass group_id / group_key)",
                )?;
            Ok(format!("followup|{task_id}"))
        }
        other => bail!("unknown attention kind {other:?}; expected \"question\" or \"followup\""),
    }
}

/// Per-kind sanity checks on the member content before it is inserted.
fn validate_member_input(input: &CreateAttentionInput) -> Result<()> {
    match input.kind.as_str() {
        "question" => {
            let question_type = input
                .question_type
                .as_deref()
                .filter(|s| !s.is_empty())
                .context("question attention needs question_type (yes_no|multiple_choice|prompt)")?;
            if !matches!(question_type, "yes_no" | "multiple_choice" | "prompt") {
                bail!(
                    "invalid question_type {question_type:?}; \
                     expected yes_no|multiple_choice|prompt"
                );
            }
            if input.prompt_text.as_deref().filter(|s| !s.is_empty()).is_none() {
                bail!("question attention needs a non-empty prompt_text");
            }
            if question_type == "multiple_choice" && input.choice_options.as_deref().filter(|s| !s.is_empty()).is_none()
            {
                bail!("multiple_choice question needs choice_options (a JSON array of strings)");
            }
        }
        "followup" => {
            if input.proposed_name.as_deref().filter(|s| !s.is_empty()).is_none() {
                bail!("followup attention needs a non-empty proposed_name");
            }
            if let Some(work_kind) = input.proposed_work_kind.as_deref().filter(|s| !s.is_empty())
                && !matches!(work_kind, "task" | "chore" | "project")
            {
                bail!("invalid proposed_work_kind {work_kind:?}; expected task|chore|project");
            }
        }
        other => bail!("unknown attention kind {other:?}; expected \"question\" or \"followup\""),
    }
    Ok(())
}

/// Resolve the group the new member belongs to: an explicit `group_id`
/// wins; otherwise reconcile on the grouping key, joining the latest-
/// generation open group or bumping `generation` past a terminal one.
fn resolve_or_create_group(conn: &Connection, input: &CreateAttentionInput) -> Result<AttentionGroup> {
    if let Some(group_id) = input.group_id.as_deref().filter(|s| !s.is_empty()) {
        return resolve_group(conn, group_id).require("attention group", group_id);
    }

    let grouping_key = match input.group_key.as_deref().filter(|s| !s.is_empty()) {
        Some(key) => key.to_owned(),
        None => derive_grouping_key(input)?,
    };

    let latest = conn
        .query_row(
            &format!(
                "SELECT {GROUP_COLS} FROM attention_groups \
                 WHERE grouping_key = ?1 ORDER BY generation DESC LIMIT 1"
            ),
            [&grouping_key],
            map_attention_group,
        )
        .optional()?;

    match latest {
        // An open / partially-answered group of the current generation is
        // the reconciliation target.
        Some(group) if !group_is_terminal(&group.state) => Ok(group),
        // The prior group is closed — start the next generation so members
        // never reopen a closed group.
        Some(group) => create_group(conn, input, &grouping_key, group.generation + 1),
        None => create_group(conn, input, &grouping_key, 0),
    }
}

/// Insert a fresh `attention_groups` row (product + short id derived from
/// the association) at the requested generation and return it.
fn create_group(
    conn: &Connection,
    input: &CreateAttentionInput,
    grouping_key: &str,
    generation: i64,
) -> Result<AttentionGroup> {
    let assoc_project = input.association_project_id.as_deref().filter(|s| !s.is_empty());
    let assoc_task = input.association_task_id.as_deref().filter(|s| !s.is_empty());

    // The schema's XOR CHECK requires exactly one association; enforce it
    // here with a clear message rather than surfacing a raw SQLite error.
    let product_id = match (assoc_project, assoc_task) {
        (Some(project_id), None) => product_id_for_work_item(conn, project_id)?,
        (None, Some(task_id)) => product_id_for_work_item(conn, task_id)?,
        (Some(_), Some(_)) => bail!(
            "attention association is exclusive: set association_project_id OR \
             association_task_id, not both"
        ),
        (None, None) => bail!("attention needs an association: set association_project_id or association_task_id"),
    };

    let id = next_id("atg");
    let now = now_string();
    let short_id = allocate_attention_group_short_id(conn, &product_id)?;
    let source_kind = input
        .source_kind
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("manual");

    conn.execute(
        "INSERT INTO attention_groups (
             id, product_id, short_id, kind, association_project_id, association_task_id,
             source_kind, source_task_id, source_run_id, source_doc_path,
             source_doc_repo_remote_url, source_doc_branch, grouping_key, generation, state,
             produced_artifact_kind, produced_artifact_ref, created_at, actioned_at, dismissed_at
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
             'open', NULL, NULL, ?15, NULL, NULL
         )",
        params![
            id,
            product_id,
            short_id,
            input.kind,
            assoc_project,
            assoc_task,
            source_kind,
            input.source_task_id,
            input.source_run_id,
            input.source_doc_path,
            input.source_doc_repo_remote_url,
            input.source_doc_branch,
            grouping_key,
            generation,
            now,
        ],
    )?;

    query_attention_group(conn, &id)?.with_context(|| format!("missing attention group after insert: {id}"))
}

/// Next ordinal for a group: one past the current maximum (1-based).
fn next_member_ordinal(conn: &Connection, group_id: &str) -> Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM attentions WHERE group_id = ?1",
        [group_id],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Insert one member row into `group_id` at `ordinal` and return it.
/// Shared by [`WorkDb::create_attention`] (single append) and
/// [`WorkDb::reconcile_attentions`] (idempotent batch upsert). Callers are
/// responsible for validating the member and for confirming the group is
/// non-terminal before calling.
fn insert_member(conn: &Connection, group_id: &str, ordinal: i64, input: &CreateAttentionInput) -> Result<Attention> {
    let id = next_id("atn");
    let now = now_string();
    let confidence_source = input
        .confidence_source
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("structured");

    conn.execute(
        "INSERT INTO attentions (
             id, group_id, ordinal, source_anchor, answer_state, created_at, answered_at,
             question_type, prompt_text, choice_options, answer,
             proposed_name, proposed_description, proposed_effort, proposed_work_kind,
             rationale, confidence_source, source_proposal_id
         ) VALUES (
             ?1, ?2, ?3, ?4, 'open', ?5, NULL,
             ?6, ?7, ?8, NULL,
             ?9, ?10, ?11, ?12,
             ?13, ?14, ?15
         )",
        params![
            id,
            group_id,
            ordinal,
            input.source_anchor,
            now,
            input.question_type,
            input.prompt_text,
            input.choice_options,
            input.proposed_name,
            input.proposed_description,
            input.proposed_effort,
            input.proposed_work_kind,
            input.rationale,
            confidence_source,
            input.source_proposal_id,
        ],
    )?;

    query_attention(conn, &id)?.with_context(|| format!("missing attention after insert: {id}"))
}

/// Recompute and persist a non-terminal group's `state` from its members:
/// `open` while every member is untouched, `partially_answered` once any
/// member has reached a terminal answer-state. Terminal groups
/// (`actioned`/`dismissed`) are left untouched — only an explicit action /
/// dismissal moves a group into or out of those.
fn recompute_group_state(conn: &Connection, group_id: &str) -> Result<()> {
    let state: String = conn.query_row("SELECT state FROM attention_groups WHERE id = ?1", [group_id], |row| {
        row.get(0)
    })?;
    if group_is_terminal(&state) {
        return Ok(());
    }
    let touched: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM attentions WHERE group_id = ?1 AND answer_state <> 'open'
         )",
        [group_id],
        |row| row.get(0),
    )?;
    let new_state = if touched { "partially_answered" } else { "open" };
    conn.execute(
        "UPDATE attention_groups SET state = ?2 WHERE id = ?1",
        params![group_id, new_state],
    )?;
    Ok(())
}

/// Create a new attention member within an already-open transaction,
/// reconciling (or creating) its owning group. Shared by
/// [`WorkDb::create_attention`] and callers that need the attention filed
/// atomically alongside other writes in their own transaction (e.g. the
/// automation pre-file dedup gate, which files a suppression attention item
/// in the same `Immediate` transaction as the create-path gate check it
/// lost to — see `WorkDb::create_automation_task`).
pub(crate) fn create_attention_in_tx(
    conn: &Connection,
    input: CreateAttentionInput,
) -> Result<(Attention, AttentionGroup)> {
    let group = resolve_or_create_group(conn, &input)?;
    if group.kind != input.kind {
        bail!(
            "attention kind {:?} does not match group {} kind {:?}",
            input.kind,
            group.id,
            group.kind
        );
    }
    if group_is_terminal(&group.state) {
        bail!(
            "attention group {} is {} (terminal); new attentions form a new generation, \
             they cannot join a closed group",
            group.id,
            group.state
        );
    }
    validate_member_input(&input)?;

    let ordinal = next_member_ordinal(conn, &group.id)?;
    let attention = insert_member(conn, &group.id, ordinal, &input)?;
    // A brand-new member is always `open`, so it cannot change the
    // group's `open`/`partially_answered` state; re-fetch only to return
    // a canonical group row.
    let group = query_attention_group(conn, &group.id)?
        .with_context(|| format!("missing attention group after insert: {}", group.id))?;
    Ok((attention, group))
}

impl WorkDb {
    /// Create a new attention member, reconciling (or creating) its owning
    /// group. Returns the member plus its group so the caller can push an
    /// [`boss_protocol::FrontendEvent::AttentionCreated`].
    ///
    /// Each call appends exactly one member — a bare create is a one-shot,
    /// not content-idempotent. The `(grouping_key, generation)` unique index
    /// makes the *group* idempotent; the structured manifest/sentinel
    /// reconcilers (task 3) layer content-dedup on top of this.
    pub fn create_attention(&self, input: CreateAttentionInput) -> Result<(Attention, AttentionGroup)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let result = create_attention_in_tx(&tx, input)?;
        tx.commit()?;
        Ok(result)
    }

    /// Reconcile a batch of structured attentions — a design-doc question
    /// manifest or a transcript `FOLLOWUPS:` block — into a single group.
    ///
    /// This is the content-idempotent counterpart to [`Self::create_attention`]
    /// that the creation-pipeline detectors (design `<slug>.attentions.json`,
    /// the followups sentinel) call. Group reconciliation is identical: the
    /// batch joins the latest open / partially-answered group for its grouping
    /// key, or — if that group is already `actioned`/`dismissed` (terminal) —
    /// starts a fresh generation. On top of the group's
    /// `(grouping_key, generation)` idempotency, member-level dedup keys on
    /// [`content_key`] so re-running the same source (a re-detected PR, a
    /// re-emitted block) does **not** append duplicate members.
    ///
    /// All `inputs` must share the same grouping identity
    /// (kind + association + source); the group is resolved from the first
    /// input. Returns the group plus the members **newly inserted on this call**
    /// (an empty `Vec` when every member already existed), or `Ok(None)` for an
    /// empty batch so callers can skip event publishing without a special case.
    pub fn reconcile_attentions(
        &self,
        inputs: Vec<CreateAttentionInput>,
    ) -> Result<Option<(AttentionGroup, Vec<Attention>)>> {
        let Some(first) = inputs.first() else {
            return Ok(None);
        };

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;

        let group = resolve_or_create_group(&tx, first)?;
        // `resolve_or_create_group` always returns a non-terminal group (it
        // bumps past a closed one), so members can be appended safely.
        debug_assert!(!group_is_terminal(&group.state));

        // Seed the dedup set + ordinal counter from the group's existing
        // members so re-runs are no-ops and ordinals stay monotonic.
        let existing = {
            let mut stmt = tx.prepare(&format!("SELECT {ATTN_COLS} FROM attentions WHERE group_id = ?1"))?;
            collect_rows(stmt.query_map([group.id.as_str()], map_attention)?)?
        };
        let mut seen: HashSet<String> = existing
            .iter()
            .map(|a| {
                content_key(
                    &group.kind,
                    a.question_type.as_deref(),
                    a.prompt_text.as_deref(),
                    a.source_anchor.as_deref(),
                    a.proposed_name.as_deref(),
                )
            })
            .collect();
        let mut ordinal = existing.iter().map(|a| a.ordinal).max().unwrap_or(0);

        let mut created = Vec::new();
        for input in &inputs {
            if input.kind != group.kind {
                bail!(
                    "attention kind {:?} does not match group {} kind {:?}",
                    input.kind,
                    group.id,
                    group.kind
                );
            }
            validate_member_input(input)?;
            let key = content_key(
                &group.kind,
                input.question_type.as_deref(),
                input.prompt_text.as_deref(),
                input.source_anchor.as_deref(),
                input.proposed_name.as_deref(),
            );
            // Skips both members already in the group and intra-batch dupes.
            if !seen.insert(key) {
                continue;
            }
            ordinal += 1;
            created.push(insert_member(&tx, &group.id, ordinal, input)?);
        }

        recompute_group_state(&tx, &group.id)?;
        let group = query_attention_group(&tx, &group.id)?
            .with_context(|| format!("missing attention group after reconcile: {}", group.id))?;
        tx.commit()?;
        Ok(Some((group, created)))
    }

    /// List groups for `product_id`, newest first. Optional filters narrow
    /// by association (project/task), `kind`, and `state`. With no `state`
    /// filter the default is the actionable set: `open` + `partially_answered`.
    pub fn list_attention_groups(
        &self,
        product_id: &str,
        project_id: Option<&str>,
        task_id: Option<&str>,
        kind: Option<&str>,
        state: Option<&str>,
    ) -> Result<Vec<AttentionGroup>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {GROUP_COLS} FROM attention_groups
             WHERE product_id = ?1
               AND (?2 IS NULL OR association_project_id = ?2)
               AND (?3 IS NULL OR association_task_id = ?3)
               AND (?4 IS NULL OR kind = ?4)
               AND (
                    (?5 IS NULL AND state IN ('open', 'partially_answered'))
                    OR state = ?5
               )
             ORDER BY created_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map(
            params![product_id, project_id, task_id, kind, state],
            map_attention_group,
        )?;
        collect_rows(rows)
    }

    /// Fetch one group by `atg_…` id or `A<n>` short id.
    pub fn get_attention_group(&self, id: &str) -> Result<AttentionGroup> {
        let conn = self.connect()?;
        resolve_group(&conn, id).require("attention group", id)
    }

    /// List the members of a group in display order. Validates the group id
    /// (rejecting a typo with an error rather than an empty list).
    pub fn list_attentions_for_group(&self, group_id: &str) -> Result<Vec<Attention>> {
        let conn = self.connect()?;
        let group = resolve_group(&conn, group_id).require("attention group", group_id)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {ATTN_COLS} FROM attentions \
             WHERE group_id = ?1 ORDER BY ordinal ASC, id ASC"
        ))?;
        let rows = stmt.query_map([group.id.as_str()], map_attention)?;
        collect_rows(rows)
    }

    /// Record the human's resolution of one member (`atn_…`) and return the
    /// owning group with its recomputed `state`.
    ///
    /// Precedence mirrors the wire: `dismiss` ⇒ `skip` ⇒ answer. A `dismiss`
    /// or `skip` clears any captured answer; answering a *question* requires
    /// a value, while answering a *followup* (an "accept") does not.
    pub fn answer_attention(
        &self,
        id: &str,
        answer: Option<String>,
        skip: bool,
        dismiss: bool,
    ) -> Result<AttentionGroup> {
        let new_state = if dismiss {
            "dismissed"
        } else if skip {
            "skipped"
        } else {
            "answered"
        };
        // Only an `answered` transition carries an answer value.
        let answer = if new_state == "answered" {
            answer.filter(|s| !s.is_empty())
        } else {
            None
        };
        self.set_member_answer_state(id, new_state, answer, LAST_STATUS_ACTOR_HUMAN)
    }

    /// Dismiss without producing anything. `atg_…` / `A<n>` dismisses the
    /// whole group (terminal); `atn_…` dismisses a single member. `reason`
    /// has no column in the store and is accepted only for wire/CLI parity.
    pub fn dismiss_attention(&self, id: &str, reason: Option<String>) -> Result<AttentionGroup> {
        self.dismiss_attention_as_actor(id, reason, LAST_STATUS_ACTOR_HUMAN)
    }

    /// Like [`Self::dismiss_attention`] but attributes the dismissal to
    /// `actor`, auditing it as a `boothby_actions` row when that actor is
    /// Boothby — this is the path Boothby's attention-tidying takes, and
    /// the pre-image is what lets a human put a wrongly-dismissed group
    /// (or member) back.
    ///
    /// Both the group path and the `atn_…` member path (via
    /// [`Self::set_member_answer_state`], journalled under
    /// [`boss_protocol::BOOTHBY_TARGET_ATTENTION_ITEM`]) are audited.
    pub fn dismiss_attention_as_actor(&self, id: &str, _reason: Option<String>, actor: &str) -> Result<AttentionGroup> {
        if id.starts_with("atn_") {
            return self.set_member_answer_state(id, "dismissed", None, actor);
        }

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let group = resolve_group(&tx, id).require("attention group", id)?;
        let before = group.clone();
        match group.state.as_str() {
            // Idempotent: dismissing an already-dismissed group is a no-op.
            "dismissed" => {
                tx.commit()?;
                return Ok(group);
            }
            "actioned" => bail!(
                "attention group {} is already actioned; an actioned group cannot be dismissed",
                group.id
            ),
            _ => {}
        }
        let now = now_string();
        tx.execute(
            "UPDATE attention_groups SET state = 'dismissed', dismissed_at = ?2 WHERE id = ?1",
            params![group.id, now],
        )?;
        let group = query_attention_group(&tx, &group.id)?
            .with_context(|| format!("missing attention group after dismiss: {}", group.id))?;
        // Audit inside `tx`. Inert unless `actor` is Boothby. Note the
        // idempotent already-dismissed arm above returns before reaching
        // here, so a re-dismiss appends no second action row.
        boothby::capture_attention_group_update(&tx, self, actor, &before, &group, &now)?;
        tx.commit()?;
        Ok(group)
    }

    /// Shared member-state transition for `answer_attention` /
    /// `dismiss_attention`. Refuses to mutate a member whose group is
    /// terminal, then recomputes and returns the group. Audits the member
    /// mutation in-transaction when `actor` is Boothby (inert otherwise) —
    /// see [`boothby::capture_attention_member_update`].
    fn set_member_answer_state(
        &self,
        member_id: &str,
        new_state: &str,
        answer: Option<String>,
        actor: &str,
    ) -> Result<AttentionGroup> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let before = query_attention(&tx, member_id).require("attention", member_id)?;
        let group = query_attention_group(&tx, &before.group_id)?
            .with_context(|| format!("attention {member_id} references a missing group"))?;
        if group_is_terminal(&group.state) {
            bail!(
                "attention group {} is {} (terminal); its members can no longer be changed",
                group.id,
                group.state
            );
        }
        if new_state == "answered" && group.kind == "question" && answer.is_none() {
            bail!("answering a question attention requires an answer value");
        }
        let answered_at = if new_state == "answered" {
            Some(now_string())
        } else {
            None
        };
        let now = answered_at.clone().unwrap_or_else(now_string);
        tx.execute(
            "UPDATE attentions
                SET answer_state = ?2, answer = ?3, answered_at = ?4
              WHERE id = ?1",
            params![member_id, new_state, answer, answered_at],
        )?;
        let after = query_attention(&tx, member_id)
            .require("attention", member_id)
            .with_context(|| format!("missing attention after update: {member_id}"))?;
        // Audit inside `tx`. Inert unless `actor` is Boothby.
        boothby::capture_attention_member_update(&tx, self, actor, &before, &after, &now)?;
        recompute_group_state(&tx, &group.id)?;
        let group = query_attention_group(&tx, &group.id)?
            .with_context(|| format!("missing attention group after update: {}", group.id))?;
        tx.commit()?;
        Ok(group)
    }
}

// ===========================================================================
// ActionAttentionGroup — the single terminal producer (design §"Engine
// behaviour and take action per kind"). One entry point so the Notifications
// window and the inline doc surface produce identical effects.
// ===========================================================================

/// Outcome of [`WorkDb::action_attention_group`]: the now-`actioned` group
/// plus the ids of the work items the action produced. The RPC handler emits
/// [`boss_protocol::FrontendEvent::AttentionGroupActioned`] with the group and
/// publishes a work-tree invalidation for the produced ids so the kanban
/// reflects the new revision / tasks without a manual reload.
#[derive(Debug, Clone)]
pub struct ActionedAttentionGroup {
    pub group: AttentionGroup,
    pub produced_work_item_ids: Vec<String>,
}

/// Load a group's members in display order within an open transaction.
fn members_in_tx(conn: &Connection, group_id: &str) -> Result<Vec<Attention>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ATTN_COLS} FROM attentions WHERE group_id = ?1 ORDER BY ordinal ASC, id ASC"
    ))?;
    collect_rows(stmt.query_map([group_id], map_attention)?)
}

/// Map a `proposed_effort` hint (`"trivial"`…`"max"`) to an [`EffortLevel`].
/// Unrecognised / empty values yield `None`, letting the dispatcher fall
/// through to the product / engine default.
fn parse_effort(raw: Option<&str>) -> Option<EffortLevel> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some("trivial") => Some(EffortLevel::Trivial),
        Some("small") => Some(EffortLevel::Small),
        Some("medium") => Some(EffortLevel::Medium),
        Some("large") => Some(EffortLevel::Large),
        Some("max") => Some(EffortLevel::Max),
        _ => None,
    }
}

/// A concise card title for the revision / design task produced from a
/// question group — derived from the source doc's basename when known.
fn question_artifact_name(group: &AttentionGroup) -> String {
    match group.source_doc_path.as_deref().filter(|s| !s.is_empty()) {
        Some(path) => {
            let base = path.rsplit('/').next().unwrap_or(path);
            format!("Apply answered questions to {base}")
        }
        None => "Apply answered design questions".to_owned(),
    }
}

/// Render the `answered` question/answer pairs into a markdown brief handed
/// to the revision / design worker. Skipped and dismissed members contribute
/// nothing (the design: "produces one downstream artifact from the answered
/// set").
fn build_qa_brief(group: &AttentionGroup, answered: &[&Attention]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    match group.source_doc_path.as_deref().filter(|s| !s.is_empty()) {
        Some(path) => {
            let _ = writeln!(
                out,
                "The operator answered open questions about the design doc `{path}`. \
                 Incorporate every answer below into the doc."
            );
        }
        None => {
            let _ = writeln!(
                out,
                "The operator answered open questions about this design. \
                 Incorporate every answer below into the doc."
            );
        }
    }
    out.push_str("\n## Answered questions\n");
    for m in answered {
        let prompt = m.prompt_text.as_deref().unwrap_or("(question)");
        let _ = write!(out, "\n### {prompt}\n");
        if let Some(anchor) = m.source_anchor.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(out, "_Section: {anchor}_");
        }
        let _ = writeln!(out, "\n**Answer:** {}", m.answer.as_deref().unwrap_or(""));
    }
    out
}

/// Insert a fresh `kind = 'design'` task seeded with the answered-questions
/// brief. Used when a question group's source doc has already merged, so a
/// revision (which needs an open PR) is impossible: a new design task opens a
/// new PR instead. Mirrors [`insert_design_task_for_project_in_tx`] but
/// carries a real description, a normal ordinal (the project's original
/// design task occupies ordinal 0), and `created_via = attention`.
fn insert_seeded_design_task_in_tx(
    conn: &Connection,
    product_id: &str,
    project_id: &str,
    name: &str,
    description: &str,
) -> Result<Task> {
    let id = next_id("task");
    let now = now_string();
    let ordinal = next_task_ordinal(conn, project_id)?;
    let short_id = allocate_short_id(conn, product_id)?;
    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, \
         pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, short_id) \
         VALUES (?1, ?2, ?3, 'design', ?4, ?5, 'todo', ?6, NULL, NULL, ?7, ?7, 1, 'medium', ?8, ?9)",
        params![
            id,
            product_id,
            project_id,
            name,
            description,
            ordinal,
            now,
            CREATED_VIA_ATTENTION,
            short_id,
        ],
    )?;
    query_task(conn, &id)?.with_context(|| format!("missing seeded design task after insert: {id}"))
}

/// Produce the downstream artifact for a **question** group. Returns
/// `(produced_artifact_kind, produced_artifact_ref_json, produced_ids)`.
///
/// Prefer a revision against the source doc's still-open PR. The revision
/// gate (parent PR open and unmerged) is the exact condition the design forks
/// on, so we attempt the revision and fall back to a fresh design task
/// *precisely* when the gate refuses (no PR / merged / closed). Any other
/// failure (e.g. a `gh` probe error) is a real error and propagates.
fn action_question_group(
    conn: &Connection,
    group: &AttentionGroup,
    members: &[Attention],
    pr_checker: &dyn PrStateChecker,
) -> Result<(String, String, Vec<String>)> {
    let answered: Vec<&Attention> = members.iter().filter(|m| m.answer_state == "answered").collect();
    if answered.is_empty() {
        bail!(
            "attention group {} has no answered questions to act on; \
             dismiss it instead of actioning",
            group.id
        );
    }
    let brief = build_qa_brief(group, &answered);
    let name = question_artifact_name(group);

    if let Some(parent_task_id) = group.source_task_id.as_deref().filter(|s| !s.is_empty()) {
        let input = CreateRevisionInput::builder()
            .parent_task_id(parent_task_id)
            .description(brief.clone())
            .name(name.clone())
            .created_via(CREATED_VIA_ATTENTION)
            .build();
        match assert_parent_revisable_and_insert(conn, input, pr_checker) {
            Ok(revision) => {
                let reference = serde_json::json!({
                    "task_id": revision.id,
                    "short_id": revision.short_id,
                })
                .to_string();
                return Ok(("revision".to_owned(), reference, vec![revision.id]));
            }
            Err(err) => {
                // Fall back to a fresh design task only when the gate refused
                // (the source doc has no open PR to revise). The gate's checks
                // run before it inserts anything, so the transaction is still
                // clean here.
                if err.downcast_ref::<RevisionGateError>().is_none() {
                    return Err(err);
                }
            }
        }
    }

    // Merged doc (or no source task / open PR): a fresh design task opens a
    // new PR seeded with the Q&A.
    let project_id = group
        .association_project_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .with_context(|| {
            format!(
                "question group {} has no associated project; cannot create a design task",
                group.id
            )
        })?;
    let task = insert_seeded_design_task_in_tx(conn, &group.product_id, project_id, &name, &brief)?;
    let reference = serde_json::json!({
        "task_id": task.id,
        "short_id": task.short_id,
    })
    .to_string();
    Ok(("design_task".to_owned(), reference, vec![task.id]))
}

/// Produce the downstream artifact for a **followup** group: one task/chore
/// per accepted (answered) member, created in the originating task's
/// product/project. Skipped/dismissed members contribute nothing. Returns
/// `(produced_artifact_kind, produced_artifact_ref_json, produced_ids)`.
///
/// `proposed_work_kind` is honoured as `chore` vs (project-)`task`; a
/// `project` hint is materialised as a task in the originating project (v1
/// produces tasks/chores, per the Attn-3 scope). When the originating work
/// item has no project (it is itself a chore), the followup is created as a
/// product-level chore.
fn action_followup_group(
    conn: &Connection,
    group: &AttentionGroup,
    members: &[Attention],
) -> Result<(String, String, Vec<String>)> {
    let accepted: Vec<&Attention> = members.iter().filter(|m| m.answer_state == "answered").collect();
    if accepted.is_empty() {
        bail!(
            "attention group {} has no accepted followups to create; \
             dismiss it instead of actioning",
            group.id
        );
    }

    // New work items inherit the originating task's product + project.
    let origin_id = group
        .association_task_id
        .as_deref()
        .or(group.source_task_id.as_deref())
        .filter(|s| !s.is_empty())
        .with_context(|| format!("followup group {} has no originating task", group.id))?;
    let origin = query_task(conn, origin_id)?
        .with_context(|| format!("followup group {} references a missing task {origin_id}", group.id))?;
    let product_id = origin.product_id.clone();
    let project_id = origin.project_id.clone();

    let mut created_ids = Vec::with_capacity(accepted.len());
    let mut created_refs = Vec::with_capacity(accepted.len());
    for m in accepted {
        let name = m
            .proposed_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .with_context(|| format!("accepted followup {} has no proposed_name", m.id))?;
        let effort = parse_effort(m.proposed_effort.as_deref());
        // A `chore` hint, or the absence of a project to file into, lands a
        // product-level chore; everything else becomes a project task. The
        // duplicate guard is bypassed: actioning is an explicit human gesture.
        let as_chore = m.proposed_work_kind.as_deref() == Some("chore") || project_id.is_none();

        let created = if as_chore {
            insert_chore_in_tx(
                conn,
                CreateChoreInput::builder()
                    .product_id(product_id.clone())
                    .name(name)
                    .maybe_description(m.proposed_description.clone())
                    .maybe_effort_level(effort)
                    .created_via(CREATED_VIA_ATTENTION)
                    .force_duplicate(true)
                    .build(),
            )?
        } else {
            let project_id = project_id
                .clone()
                .expect("project_id is present when as_chore is false");
            insert_task_in_tx(
                conn,
                CreateTaskInput::builder()
                    .product_id(product_id.clone())
                    .project_id(project_id)
                    .name(name)
                    .maybe_description(m.proposed_description.clone())
                    .maybe_effort_level(effort)
                    .created_via(CREATED_VIA_ATTENTION)
                    .force_duplicate(true)
                    .build(),
            )?
        };
        created_refs.push(serde_json::json!({
            "task_id": created.id,
            "short_id": created.short_id,
            "kind": created.kind,
        }));
        created_ids.push(created.id);
    }

    let reference = serde_json::json!({ "tasks": created_refs }).to_string();
    Ok(("tasks".to_owned(), reference, created_ids))
}

impl WorkDb {
    /// Action an open / partially-answered attention group: produce the single
    /// downstream artifact and transition the group to `actioned` (terminal),
    /// recording `produced_artifact_kind` + `produced_artifact_ref`. All of it
    /// — the artifact insert and the group flip — happens in one transaction
    /// so a re-action can never spawn a second artifact.
    ///
    /// `skip_unanswered` marks every still-`open` member `skipped` first, so
    /// the caller does not have to touch every row. After that, *every* member
    /// must be in a terminal answer-state (`answered` / `skipped` / `dismissed`)
    /// — otherwise the action is refused.
    ///
    /// - **question** group → a revision on the source doc's open PR, or a
    ///   fresh `design` task when the doc has already merged.
    /// - **followup** group → a batch of tasks/chores from the accepted members.
    ///
    /// `pr_checker` supplies the live PR state for the question→revision gate;
    /// pass `&GhPrStateChecker` in production, `&FakePrStateChecker` in tests.
    pub fn action_attention_group(
        &self,
        id: &str,
        skip_unanswered: bool,
        pr_checker: &dyn PrStateChecker,
    ) -> Result<ActionedAttentionGroup> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let group = resolve_group(&tx, id).require("attention group", id)?;
        match group.state.as_str() {
            "actioned" => bail!(
                "attention group {} is already actioned; an actioned group is terminal",
                group.id
            ),
            "dismissed" => bail!(
                "attention group {} is dismissed; a dismissed group cannot be actioned",
                group.id
            ),
            _ => {}
        }

        if skip_unanswered {
            tx.execute(
                "UPDATE attentions SET answer_state = 'skipped' \
                 WHERE group_id = ?1 AND answer_state = 'open'",
                params![group.id],
            )?;
        }

        let members = members_in_tx(&tx, &group.id)?;
        if members.is_empty() {
            bail!("attention group {} has no members to action", group.id);
        }
        let unanswered = members.iter().filter(|m| m.answer_state == "open").count();
        if unanswered > 0 {
            bail!(
                "attention group {} has {unanswered} unanswered member(s); answer or skip them \
                 (or pass skip_unanswered) before actioning",
                group.id
            );
        }

        // A question group where every member was skipped (none answered) has
        // no content to produce a revision or design task from. Treat this as
        // "I don't want to act on any of these" and dismiss the group so the
        // card disappears — matching the existing auto-dismiss path for
        // followup groups where all members are rejected.
        if group.kind == "question" && members.iter().all(|m| m.answer_state != "answered") {
            let now = now_string();
            tx.execute(
                "UPDATE attention_groups SET state = 'dismissed', dismissed_at = ?2 WHERE id = ?1",
                params![group.id, now],
            )?;
            let group = query_attention_group(&tx, &group.id)?
                .with_context(|| format!("missing attention group after auto-dismiss: {}", group.id))?;
            tx.commit()?;
            return Ok(ActionedAttentionGroup {
                group,
                produced_work_item_ids: vec![],
            });
        }

        let (produced_kind, produced_ref, produced_work_item_ids) = match group.kind.as_str() {
            "question" => action_question_group(&tx, &group, &members, pr_checker)?,
            "followup" => action_followup_group(&tx, &group, &members)?,
            other => bail!("cannot action attention group {} of kind {other:?}", group.id),
        };

        let now = now_string();
        tx.execute(
            "UPDATE attention_groups \
                SET state = 'actioned', produced_artifact_kind = ?2, \
                    produced_artifact_ref = ?3, actioned_at = ?4 \
              WHERE id = ?1",
            params![group.id, produced_kind, produced_ref, now],
        )?;
        let group = query_attention_group(&tx, &group.id)?
            .with_context(|| format!("missing attention group after action: {}", group.id))?;
        tx.commit()?;
        Ok(ActionedAttentionGroup {
            group,
            produced_work_item_ids,
        })
    }
}

// ===========================================================================
// attention_merges — provenance ledger (P1203 task 1). `AttentionMerge`
// itself lives in `boss_protocol` so it can ride the wire (P1203 task 8 —
// the Notifications UI reads it via `ListAttentionMerges`).
// ===========================================================================

/// Input for inserting one `attention_merges` row.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct InsertAttentionMergeInput {
    pub canonical_attention_id: Option<String>,
    pub canonical_work_item_id: Option<String>,
    pub product_id: String,
    pub trigger: String,
    pub duplicate_attention_id: Option<String>,
    pub candidate_summary: String,
    pub candidate_source: Option<String>,
    pub model: String,
    pub decision_rationale: Option<String>,
    pub edits_applied: Option<String>,
}

fn map_attention_merge(row: &Row<'_>) -> rusqlite::Result<AttentionMerge> {
    Ok(AttentionMerge {
        id: row.get(0)?,
        canonical_attention_id: row.get(1)?,
        canonical_work_item_id: row.get(2)?,
        product_id: row.get(3)?,
        trigger: row.get(4)?,
        duplicate_attention_id: row.get(5)?,
        candidate_summary: row.get(6)?,
        candidate_source: row.get(7)?,
        model: row.get(8)?,
        decision_rationale: row.get(9)?,
        edits_applied: row.get(10)?,
        created_at: row.get(11)?,
    })
}

impl WorkDb {
    /// Append one row to the `attention_merges` provenance ledger. Returns the
    /// generated `id` of the new row.
    ///
    /// The pair-unique index on `(canonical_attention_id, duplicate_attention_id)`
    /// enforces sweep idempotency at the DB level: inserting the same
    /// (canonical, duplicate) pair a second time returns a constraint error
    /// rather than silently double-counting the score. Callers that need
    /// idempotent behaviour should use `INSERT OR IGNORE` semantics at the SQL
    /// level (task 5/7 will do this via a dedicated sweep helper).
    pub fn insert_attention_merge(&self, input: InsertAttentionMergeInput) -> Result<String> {
        let conn = self.connect()?;
        let id = next_id("merge");
        let now = now_string();
        conn.execute(
            "INSERT INTO attention_merges (
                 id, canonical_attention_id, canonical_work_item_id,
                 product_id, trigger, duplicate_attention_id,
                 candidate_summary, candidate_source, model,
                 decision_rationale, edits_applied, created_at
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12
             )",
            params![
                id,
                input.canonical_attention_id,
                input.canonical_work_item_id,
                input.product_id,
                input.trigger,
                input.duplicate_attention_id,
                input.candidate_summary,
                input.candidate_source,
                input.model,
                input.decision_rationale,
                input.edits_applied,
                now,
            ],
        )?;
        Ok(id)
    }

    /// List all `attention_merges` rows for a canonical `Attention` id,
    /// ordered chronologically. Used to render the merge-provenance affordance
    /// in the Notifications UI ("folded N duplicate reports").
    pub fn list_attention_merges_for_canonical(&self, canonical_attention_id: &str) -> Result<Vec<AttentionMerge>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, canonical_attention_id, canonical_work_item_id,
                    product_id, trigger, duplicate_attention_id,
                    candidate_summary, candidate_source, model,
                    decision_rationale, edits_applied, created_at
             FROM attention_merges
             WHERE canonical_attention_id = ?1
             ORDER BY created_at ASC",
        )?;
        collect_rows(stmt.query_map([canonical_attention_id], map_attention_merge)?)
    }

    /// Count `attention_merges` rows whose `canonical_work_item_id` matches
    /// the given work-item id. Provides the "N related attentions suppressed"
    /// signal for a work item (design §R11 — deferred count, available here
    /// for callers that want it).
    pub fn count_attention_merges_by_work_item(&self, work_item_id: &str) -> Result<i64> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT COUNT(*) FROM attention_merges WHERE canonical_work_item_id = ?1",
            [work_item_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// Empty-card cleanup: if the group identified by `group_id` has no
    /// remaining open members (i.e. every member's `answer_state` is not
    /// `'open'`), retire the group by setting `state = 'dismissed'` and
    /// `dismissed_at = now()`. Returns `true` if the group was retired.
    ///
    /// Called inside the sweep fold transaction after retiring each loser
    /// `Attention` item. Only affects non-terminal groups — a group that is
    /// already `actioned` or `dismissed` is left unchanged (returns `false`).
    pub fn retire_group_if_empty(&self, group_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        self.retire_group_if_empty_in_tx(&conn, group_id)
    }

    /// Transaction-aware variant of [`Self::retire_group_if_empty`] for use
    /// inside an existing transaction.
    pub(crate) fn retire_group_if_empty_in_tx(&self, conn: &Connection, group_id: &str) -> Result<bool> {
        let group = match query_attention_group(conn, group_id)? {
            Some(g) => g,
            None => return Ok(false),
        };
        if group_is_terminal(&group.state) {
            return Ok(false);
        }
        let open_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM attentions WHERE group_id = ?1 AND answer_state = 'open'",
            [group_id],
            |row| row.get(0),
        )?;
        if open_count > 0 {
            return Ok(false);
        }
        let now = now_string();
        conn.execute(
            "UPDATE attention_groups SET state = 'dismissed', dismissed_at = ?2 WHERE id = ?1",
            params![group_id, now],
        )?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `CreateAttentionInput` with only `kind` set; all other fields default
    /// to `None`. Individual tests set just the fields they exercise.
    fn input(kind: &str) -> CreateAttentionInput {
        CreateAttentionInput {
            kind: kind.to_owned(),
            ..Default::default()
        }
    }

    /// A minimal question group. `source_doc_path` is the only content the
    /// pure producers (`question_artifact_name`, `build_qa_brief`) read.
    fn question_group(source_doc_path: Option<&str>) -> AttentionGroup {
        AttentionGroup::builder()
            .id("atg_q")
            .product_id("prod_1")
            .created_at("2026-01-01T00:00:00Z")
            .grouping_key("question|proj_1|doc:x")
            .kind("question")
            .source_kind("design_doc")
            .maybe_source_doc_path(source_doc_path.map(str::to_owned))
            .build()
    }

    /// A minimal answered question member carrying a prompt/answer/anchor.
    fn answered_member(prompt: &str, answer: &str, anchor: Option<&str>) -> Attention {
        Attention::builder()
            .id("atn_1")
            .group_id("atg_q")
            .ordinal(1)
            .answer_state("answered")
            .created_at("2026-01-01T00:00:00Z")
            .prompt_text(prompt)
            .answer(answer)
            .maybe_source_anchor(anchor.map(str::to_owned))
            .build()
    }

    // --- group_is_terminal ---------------------------------------------------

    #[test]
    fn group_is_terminal_only_for_actioned_and_dismissed() {
        assert!(group_is_terminal("actioned"));
        assert!(group_is_terminal("dismissed"));
        // Open / in-progress states are not terminal.
        assert!(!group_is_terminal("open"));
        assert!(!group_is_terminal("partially_answered"));
        assert!(!group_is_terminal("answered"));
        // Arbitrary / unknown strings are treated as non-terminal.
        assert!(!group_is_terminal(""));
        assert!(!group_is_terminal("Actioned"));
        assert!(!group_is_terminal("whatever"));
    }

    // --- content_key ---------------------------------------------------------

    #[test]
    fn content_key_question_keys_on_type_prompt_and_anchor() {
        let base = content_key("question", Some("prompt"), Some("Ship it?"), Some("§1"), None);
        // Same prompt asked about a different anchor is a *different* member.
        let other_anchor = content_key("question", Some("prompt"), Some("Ship it?"), Some("§2"), None);
        assert_ne!(base, other_anchor);
        // A different question_type also changes identity.
        let other_type = content_key("question", Some("yes_no"), Some("Ship it?"), Some("§1"), None);
        assert_ne!(base, other_type);
        // Same inputs collapse to the same key.
        assert_eq!(
            base,
            content_key("question", Some("prompt"), Some("Ship it?"), Some("§1"), None)
        );
        // `proposed_name` is irrelevant on the question branch.
        assert_eq!(
            base,
            content_key(
                "question",
                Some("prompt"),
                Some("Ship it?"),
                Some("§1"),
                Some("ignored")
            )
        );
    }

    #[test]
    fn content_key_followup_keys_only_on_proposed_name() {
        let base = content_key("followup", None, None, None, Some("Add retries"));
        // Re-phrased description / other fields don't matter — only the name.
        assert_eq!(
            base,
            content_key(
                "followup",
                Some("prompt"),
                Some("different"),
                Some("anchor"),
                Some("Add retries")
            )
        );
        // A different name is a different followup.
        assert_ne!(base, content_key("followup", None, None, None, Some("Add backoff")));
    }

    #[test]
    fn content_key_fallback_branch_keys_on_prompt_text() {
        // Unknown kinds fall through to the `{other}` + prompt_text branch.
        let a = content_key("weird", None, Some("hello"), None, None);
        assert_eq!(a, content_key("weird", Some("q"), Some("hello"), Some("x"), Some("y")));
        assert_ne!(a, content_key("weird", None, Some("world"), None, None));
        // The kind is part of the fallback key.
        assert_ne!(a, content_key("odd", None, Some("hello"), None, None));
    }

    #[test]
    fn content_key_none_fields_default_to_empty() {
        assert_eq!(content_key("question", None, None, None, None), "q\u{1f}\u{1f}\u{1f}");
        assert_eq!(content_key("followup", None, None, None, None), "f\u{1f}");
    }

    #[test]
    fn content_key_separator_keeps_field_boundaries_unambiguous() {
        // Without a separator, ("ab","c") and ("a","bc") would collide. The
        // unit separator between fields keeps them distinct.
        let split_a = content_key("question", Some("ab"), Some("c"), Some(""), None);
        let split_b = content_key("question", Some("a"), Some("bc"), Some(""), None);
        assert_ne!(split_a, split_b);
    }

    // --- derive_grouping_key -------------------------------------------------

    #[test]
    fn derive_grouping_key_question_shape() {
        let mut i = input("question");
        i.association_project_id = Some("proj_9".to_owned());
        i.source_doc_path = Some("docs/plan.md".to_owned());
        assert_eq!(derive_grouping_key(&i).unwrap(), "question|proj_9|doc:docs/plan.md");
    }

    #[test]
    fn derive_grouping_key_question_requires_project_and_doc_path() {
        // Missing project id.
        let mut i = input("question");
        i.source_doc_path = Some("docs/plan.md".to_owned());
        assert!(derive_grouping_key(&i).is_err());

        // Empty project id is treated as missing.
        let mut i = input("question");
        i.association_project_id = Some(String::new());
        i.source_doc_path = Some("docs/plan.md".to_owned());
        assert!(derive_grouping_key(&i).is_err());

        // Missing doc path.
        let mut i = input("question");
        i.association_project_id = Some("proj_9".to_owned());
        assert!(derive_grouping_key(&i).is_err());

        // Empty doc path is treated as missing.
        let mut i = input("question");
        i.association_project_id = Some("proj_9".to_owned());
        i.source_doc_path = Some(String::new());
        assert!(derive_grouping_key(&i).is_err());
    }

    #[test]
    fn derive_grouping_key_followup_prefers_source_task_then_association() {
        // Prefers source_task_id when both are present.
        let mut i = input("followup");
        i.source_task_id = Some("task_src".to_owned());
        i.association_task_id = Some("task_assoc".to_owned());
        assert_eq!(derive_grouping_key(&i).unwrap(), "followup|task_src");

        // Falls back to association_task_id when source is absent.
        let mut i = input("followup");
        i.association_task_id = Some("task_assoc".to_owned());
        assert_eq!(derive_grouping_key(&i).unwrap(), "followup|task_assoc");

        // An empty source_task_id falls through to association_task_id.
        let mut i = input("followup");
        i.source_task_id = Some(String::new());
        i.association_task_id = Some("task_assoc".to_owned());
        assert_eq!(derive_grouping_key(&i).unwrap(), "followup|task_assoc");
    }

    #[test]
    fn derive_grouping_key_followup_requires_a_task() {
        // Neither source nor association task id.
        assert!(derive_grouping_key(&input("followup")).is_err());

        // Both empty is still an error.
        let mut i = input("followup");
        i.source_task_id = Some(String::new());
        i.association_task_id = Some(String::new());
        assert!(derive_grouping_key(&i).is_err());
    }

    #[test]
    fn derive_grouping_key_unknown_kind_errors() {
        assert!(derive_grouping_key(&input("mystery")).is_err());
    }

    // --- validate_member_input -----------------------------------------------

    #[test]
    fn validate_question_accepts_each_valid_type() {
        for qt in ["yes_no", "prompt"] {
            let mut i = input("question");
            i.question_type = Some(qt.to_owned());
            i.prompt_text = Some("Proceed?".to_owned());
            assert!(validate_member_input(&i).is_ok(), "type {qt} should validate");
        }
        // multiple_choice needs choice_options too.
        let mut i = input("question");
        i.question_type = Some("multiple_choice".to_owned());
        i.prompt_text = Some("Pick one".to_owned());
        i.choice_options = Some("[\"a\",\"b\"]".to_owned());
        assert!(validate_member_input(&i).is_ok());
    }

    #[test]
    fn validate_question_rejects_bad_type_and_missing_prompt() {
        // Missing question_type.
        let mut i = input("question");
        i.prompt_text = Some("Proceed?".to_owned());
        assert!(validate_member_input(&i).is_err());

        // Invalid question_type.
        let mut i = input("question");
        i.question_type = Some("freeform".to_owned());
        i.prompt_text = Some("Proceed?".to_owned());
        assert!(validate_member_input(&i).is_err());

        // Valid type but empty prompt_text.
        let mut i = input("question");
        i.question_type = Some("prompt".to_owned());
        i.prompt_text = Some(String::new());
        assert!(validate_member_input(&i).is_err());

        // Missing prompt_text entirely.
        let mut i = input("question");
        i.question_type = Some("prompt".to_owned());
        assert!(validate_member_input(&i).is_err());
    }

    #[test]
    fn validate_multiple_choice_requires_choice_options() {
        let mut i = input("question");
        i.question_type = Some("multiple_choice".to_owned());
        i.prompt_text = Some("Pick one".to_owned());
        // No choice_options.
        assert!(validate_member_input(&i).is_err());

        // Empty choice_options is treated as missing.
        i.choice_options = Some(String::new());
        assert!(validate_member_input(&i).is_err());
    }

    #[test]
    fn validate_followup_accepts_valid_and_rejects_bad_work_kind() {
        // Bare name is enough (work kind is optional).
        let mut i = input("followup");
        i.proposed_name = Some("Add retries".to_owned());
        assert!(validate_member_input(&i).is_ok());

        // Each valid work kind is accepted.
        for wk in ["task", "chore", "project"] {
            let mut i = input("followup");
            i.proposed_name = Some("Add retries".to_owned());
            i.proposed_work_kind = Some(wk.to_owned());
            assert!(validate_member_input(&i).is_ok(), "work kind {wk} should validate");
        }

        // Missing / empty proposed_name is rejected.
        assert!(validate_member_input(&input("followup")).is_err());
        let mut i = input("followup");
        i.proposed_name = Some(String::new());
        assert!(validate_member_input(&i).is_err());

        // A name but an invalid work kind is rejected.
        let mut i = input("followup");
        i.proposed_name = Some("Add retries".to_owned());
        i.proposed_work_kind = Some("epic".to_owned());
        assert!(validate_member_input(&i).is_err());
    }

    #[test]
    fn validate_unknown_kind_errors() {
        assert!(validate_member_input(&input("mystery")).is_err());
    }

    // --- parse_effort --------------------------------------------------------

    #[test]
    fn parse_effort_maps_each_level() {
        assert_eq!(parse_effort(Some("trivial")), Some(EffortLevel::Trivial));
        assert_eq!(parse_effort(Some("small")), Some(EffortLevel::Small));
        assert_eq!(parse_effort(Some("medium")), Some(EffortLevel::Medium));
        assert_eq!(parse_effort(Some("large")), Some(EffortLevel::Large));
        assert_eq!(parse_effort(Some("max")), Some(EffortLevel::Max));
    }

    #[test]
    fn parse_effort_trims_whitespace() {
        assert_eq!(parse_effort(Some("  large  ")), Some(EffortLevel::Large));
    }

    #[test]
    fn parse_effort_none_for_empty_or_unknown() {
        assert_eq!(parse_effort(None), None);
        assert_eq!(parse_effort(Some("")), None);
        assert_eq!(parse_effort(Some("   ")), None);
        assert_eq!(parse_effort(Some("humongous")), None);
        // Case-sensitive: only lowercase variants match.
        assert_eq!(parse_effort(Some("Large")), None);
    }

    // --- question_artifact_name ---------------------------------------------

    #[test]
    fn question_artifact_name_uses_doc_basename() {
        let g = question_group(Some("docs/designs/plan.md"));
        assert_eq!(question_artifact_name(&g), "Apply answered questions to plan.md");
    }

    #[test]
    fn question_artifact_name_handles_path_without_slash() {
        let g = question_group(Some("plan.md"));
        assert_eq!(question_artifact_name(&g), "Apply answered questions to plan.md");
    }

    #[test]
    fn question_artifact_name_falls_back_without_doc_path() {
        assert_eq!(
            question_artifact_name(&question_group(None)),
            "Apply answered design questions"
        );
        // An empty path is treated as absent.
        assert_eq!(
            question_artifact_name(&question_group(Some(""))),
            "Apply answered design questions"
        );
    }

    // --- build_qa_brief ------------------------------------------------------

    #[test]
    fn build_qa_brief_with_no_answered_has_header_but_no_entries() {
        let g = question_group(Some("docs/plan.md"));
        let brief = build_qa_brief(&g, &[]);
        assert!(brief.contains("docs/plan.md"));
        assert!(brief.contains("## Answered questions"));
        // No per-question entries.
        assert!(!brief.contains("### "));
        assert!(!brief.contains("**Answer:**"));
    }

    #[test]
    fn build_qa_brief_renders_each_answered_question() {
        let g = question_group(Some("docs/plan.md"));
        let m1 = answered_member("Ship on Friday?", "Yes", Some("Rollout"));
        let m2 = answered_member("Use feature flags?", "No", None);
        let answered = [&m1, &m2];
        let brief = build_qa_brief(&g, &answered);

        assert!(brief.contains("### Ship on Friday?"));
        assert!(brief.contains("**Answer:** Yes"));
        assert!(brief.contains("_Section: Rollout_"));
        assert!(brief.contains("### Use feature flags?"));
        assert!(brief.contains("**Answer:** No"));
    }

    #[test]
    fn build_qa_brief_intro_differs_when_doc_path_absent() {
        let with_path = build_qa_brief(&question_group(Some("docs/plan.md")), &[]);
        assert!(with_path.contains("`docs/plan.md`"));

        let without_path = build_qa_brief(&question_group(None), &[]);
        assert!(without_path.contains("about this design"));
        assert!(!without_path.contains('`'));
    }
}
