//! `work_comments` persistence + anchor-resolution side-effects for the
//! comments-in-markdown-viewer feature (Phase 2). Design:
//! `tools/boss/docs/designs/comments-in-markdown-viewer.md`.
//!
//! The pure `TextQuoteSelector` resolver lives in [`crate::comments_anchor`];
//! this module wires it to the database — persisting fuzzy re-anchors and
//! orphan flips on resolve, and re-keying work-item comments to a `pr_doc:*`
//! artifact when a design doc graduates to a PR.

use super::*;
use crate::comments_anchor::{AnchorResolution, CommentFuzzyConfig, OrphanReason, resolve_anchor};

/// Default prefix/suffix length (chars) when (re-)extracting an anchor from
/// plain text. 64 each per design § "Anchoring model" (prefix/suffix length).
const ANCHOR_CONTEXT_CHARS: usize = 64;

/// Emits the one trace event a comment's `active`/`orphaned` → `orphaned`
/// transition previously produced none of: before this, diagnosing an orphan
/// meant reading `anchor_json`/`plain_text_projection_version` out of
/// `state.db` by hand and re-deriving why `resolve_anchor` gave up. Classifies
/// [`OrphanReason::NoConfidentMatch`] further into `not_found` (best score
/// never cleared the bar) vs the two ambiguous cases (a non-unique verbatim
/// hit, or a fuzzy winner too close to its runner-up) so "the quote is gone"
/// and "the quote is ambiguous" don't require re-deriving from raw scores.
fn log_orphan_transition(
    comment_id: &str,
    doc_version: &str,
    anchor_exact: &str,
    reason: OrphanReason,
    config: &CommentFuzzyConfig,
) {
    let anchor_exact: String = anchor_exact.chars().take(80).collect();
    match reason {
        OrphanReason::EmptyExact => tracing::warn!(
            comment_id,
            doc_version,
            anchor_exact,
            why = "empty_exact",
            "comment orphaned: anchor has no exact text to locate",
        ),
        OrphanReason::ContextTooShort => tracing::warn!(
            comment_id,
            doc_version,
            anchor_exact,
            why = "context_too_short",
            "comment orphaned: prefix+exact+suffix context is too short to fuzzy-match, and \
             didn't match verbatim either",
        ),
        OrphanReason::NoConfidentMatch {
            exact_hits,
            best_score,
            second_best_score,
        } => {
            let why = if exact_hits > 1 {
                "ambiguous_verbatim"
            } else if best_score < config.score_threshold {
                "not_found"
            } else {
                "ambiguous_fuzzy"
            };
            tracing::warn!(
                comment_id,
                doc_version,
                anchor_exact,
                why,
                exact_hits,
                best_score,
                second_best_score,
                "comment orphaned: no confident anchor match",
            );
        }
    }
}

/// Column list shared by every `work_comments` SELECT. Order must match
/// [`map_comment`].
const COMMENT_COLUMNS: &str = "id, artifact_kind, artifact_id, doc_version, anchor_json, body, \
     author, status, status_actor, last_resolved_with, plain_text_projection_version, \
     created_at, updated_at, dismissed_at, intent, intent_confidence, intent_classified_at, \
     intent_overridden_by, revise_task_id, intent_classification_failed_at, \
     intent_classification_error";

/// The one definition of "this comment is a `[Revise]` candidate": `active`
/// status **and** a `directive`/`larger_change` intent. A SQL fragment rather
/// than a Rust predicate because all three consumers are queries:
/// [`WorkDb::comments_banner_state`]'s `unresolved_count`,
/// [`query_revisable_comments`] (the batch's candidate read), and the guarded
/// claim UPDATE in [`super::revise_doc`]. Those three were previously three
/// independently-formatted string literals that happened to agree; a divergence
/// between them shows up as the banner counting a comment `[Revise]` then
/// silently drops, so they share one const.
///
/// `intent` is deliberately not the only term. A comment badged
/// `larger_change` whose `status` is `answering`/`in_revision`/`orphaned`
/// genuinely is not ready for a batch; the fix for the reclassification bug is
/// to re-home `status` on override (see [`WorkDb::override_comment_intent`]),
/// never to loosen this predicate.
pub(crate) fn revisable_comment_predicate() -> String {
    format!("status = '{COMMENT_STATUS_ACTIVE}' AND intent IN ('{INTENT_DIRECTIVE}', '{INTENT_LARGER_CHANGE}')")
}

/// The complement of [`REVISABLE_COMMENT_PREDICATE`] over the same intent set:
/// comments the sidebar badges `directive`/`larger_change` whose `status`
/// disqualifies them from a batch. Drives `ReviseDocOutcome::Created`'s
/// `excluded_comment_ids` so the operator is told what the batch left behind
/// instead of seeing an unqualified success.
pub(crate) fn excluded_revisable_comment_predicate() -> String {
    format!(
        "status NOT IN ('{COMMENT_STATUS_ACTIVE}', '{COMMENT_STATUS_RESOLVED}', '{COMMENT_STATUS_DISMISSED}') \
         AND intent IN ('{INTENT_DIRECTIVE}', '{INTENT_LARGER_CHANGE}')"
    )
}

const COMMENT_INSERT_SQL: &str = "INSERT INTO work_comments \
     (id, artifact_kind, artifact_id, doc_version, anchor_json, body, author, status, \
      status_actor, last_resolved_with, plain_text_projection_version, created_at, updated_at, \
      dismissed_at) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)";

impl WorkDb {
    /// Create an `active` comment. Returns the inserted row.
    pub fn create_comment(&self, input: CreateCommentInput) -> Result<WorkComment> {
        if input.body.trim().is_empty() {
            bail!("comment body may not be empty");
        }
        if input.anchor.exact.is_empty() {
            bail!("comment anchor.exact may not be empty");
        }
        if input.artifact_id.trim().is_empty() {
            bail!("comment artifact_id may not be empty");
        }
        let conn = self.connect()?;
        let id = next_id("cmt");
        let now = now_string();
        let anchor_json = serde_json::to_string(&input.anchor)?;
        conn.execute(
            COMMENT_INSERT_SQL,
            params![
                id,
                input.artifact_kind,
                input.artifact_id,
                input.doc_version,
                anchor_json,
                input.body,
                input.author,
                COMMENT_STATUS_ACTIVE,
                Option::<String>::None,
                Option::<String>::None,
                input.plain_text_projection_version,
                now,
                now,
                Option::<String>::None,
            ],
        )?;
        query_comment(&conn, &id)?.with_context(|| format!("missing comment after insert: {id}"))
    }

    /// List comments for an artifact in document-creation order. Excludes
    /// `resolved` / `dismissed` unless `include_resolved`. `orphaned`
    /// comments are always included (the sidebar surfaces them).
    pub fn list_comments(
        &self,
        artifact_kind: &str,
        artifact_id: &str,
        include_resolved: bool,
    ) -> Result<Vec<WorkComment>> {
        let conn = self.connect()?;
        query_comments(&conn, artifact_kind, artifact_id, include_resolved)
    }

    /// Fetch a single comment by id.
    pub fn get_comment(&self, comment_id: &str) -> Result<Option<WorkComment>> {
        let conn = self.connect()?;
        query_comment(&conn, comment_id)
    }

    /// [`Self::list_comments`], with each comment paired with its
    /// [`CommentThreadEntry`] rows and whether an answer-agent run is
    /// currently `running` (or has terminally `failed`) for it — the
    /// `CommentsList` read-path shape the design specifies
    /// (`comment-triggered-document-revisions.md` §"UI / thread behavior").
    pub fn list_comments_with_thread(
        &self,
        artifact_kind: &str,
        artifact_id: &str,
        include_resolved: bool,
    ) -> Result<Vec<CommentWithThread>> {
        let comments = self.list_comments(artifact_kind, artifact_id, include_resolved)?;
        comments
            .into_iter()
            .map(|comment| {
                let thread_entries = self.list_comment_thread_entries(&comment.id)?;
                let answer_agent_running = self.running_answer_agent_run_for_comment(&comment.id)?.is_some();
                let answer_agent_failed = self
                    .latest_answer_agent_run_for_comment(&comment.id)?
                    .is_some_and(|run| run.status == ANSWER_AGENT_RUN_STATUS_FAILED);
                Ok(CommentWithThread {
                    comment,
                    thread_entries,
                    answer_agent_running,
                    answer_agent_failed,
                })
            })
            .collect()
    }

    /// Read-only `[Revise]`-banner summary for an artifact: `revisable`,
    /// `unresolved_count` (active `directive`/`larger_change` comments —
    /// same candidate set as `query_revisable_comments`), `in_revision_count`,
    /// and the doc owner's `TaskKind` (`None` when `resolve_doc_owner` finds
    /// no design/investigation owner). Design:
    /// `tools/boss/docs/designs/comment-triggered-document-revisions.md`
    /// §"2d. Banner state on the comment read path".
    pub fn comments_banner_state(&self, artifact_kind: &str, artifact_id: &str) -> Result<CommentsBannerState> {
        let owner = self.resolve_doc_owner(artifact_kind, artifact_id)?;
        let conn = self.connect()?;
        let revisable = revisable_comment_predicate();
        let unresolved_count: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM work_comments
                 WHERE artifact_kind = ?1 AND artifact_id = ?2
                   AND {revisable}"
            ),
            params![artifact_kind, artifact_id],
            |row| row.get(0),
        )?;
        let in_revision_count: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM work_comments
                 WHERE artifact_kind = ?1 AND artifact_id = ?2
                   AND status = '{COMMENT_STATUS_IN_REVISION}'"
            ),
            params![artifact_kind, artifact_id],
            |row| row.get(0),
        )?;
        Ok(CommentsBannerState {
            revisable: owner.is_some() && unresolved_count > 0,
            unresolved_count,
            in_revision_count,
            doc_kind: owner.map(|o| o.task_kind),
        })
    }

    /// Transition a comment's status. Accepts `active` / `resolved` /
    /// `orphaned` / `dismissed`; stamps `dismissed_at` when entering
    /// `resolved` / `dismissed` and clears it otherwise (re-activation).
    pub fn set_comment_status(&self, comment_id: &str, status: &str, actor: Option<&str>) -> Result<WorkComment> {
        match status {
            COMMENT_STATUS_ACTIVE | COMMENT_STATUS_RESOLVED | COMMENT_STATUS_ORPHANED | COMMENT_STATUS_DISMISSED => {}
            other => bail!("invalid comment status: {other}"),
        }
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE work_comments
             SET status = ?2,
                 status_actor = ?3,
                 updated_at = ?4,
                 dismissed_at = CASE WHEN ?2 IN ('resolved', 'dismissed') THEN ?4 ELSE NULL END
             WHERE id = ?1",
            params![comment_id, status, actor, now],
        )?;
        if n == 0 {
            bail!("unknown comment: {comment_id}");
        }
        query_comment(&conn, comment_id)?.with_context(|| format!("missing comment after status update: {comment_id}"))
    }

    /// Soft-dismiss: transition a comment to `resolved`. Recoverable via
    /// `set_comment_status(.., "active", ..)`.
    pub fn dismiss_comment(&self, comment_id: &str, actor: Option<&str>) -> Result<WorkComment> {
        self.set_comment_status(comment_id, COMMENT_STATUS_RESOLVED, actor)
    }

    /// Bucket-2 track (P3b): `active → answering`, fired once the classifier
    /// resolves `intent = question` and the engine has spawned an
    /// answer-agent run for the comment. Guarded on `status = 'active'` — the
    /// state machine's "classifying → answering" row (design §"Comment/thread
    /// state machine"); a comment isn't classified with a `status` of its
    /// own (`intent IS NULL` *is* "classifying" — see 1a), so every comment
    /// sits `active` throughout classification and this is the first status
    /// transition bucket 2 makes. Not exposed via the general
    /// [`Self::set_comment_status`] / `CommentsSetStatus` RPC — this is an
    /// engine-internal transition.
    pub fn transition_comment_to_answering(&self, comment_id: &str) -> Result<WorkComment> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE work_comments
             SET status = ?2, status_actor = 'engine', updated_at = ?3
             WHERE id = ?1 AND status = ?4",
            params![comment_id, COMMENT_STATUS_ANSWERING, now, COMMENT_STATUS_ACTIVE],
        )?;
        if n == 0 {
            bail!("comment {comment_id} not found, or not 'active' (expected active → answering)");
        }
        query_comment(&conn, comment_id)?
            .with_context(|| format!("missing comment after answering transition: {comment_id}"))
    }

    /// Compensation for a spawn that flipped a comment to `answering` and
    /// then failed to finish creating its tracking rows (a DB write error
    /// creating the `answer_agent_runs` row or the bound execution) — see
    /// `spawn_answer_agent`. Without this, the comment would sit `answering`
    /// with no execution that will ever reach `finalize_answer_agent`, so no
    /// `Stop` event would ever recover it. Guarded on `status = 'answering'`
    /// so it's a no-op error if the state already moved on.
    pub fn transition_comment_answering_to_active(&self, comment_id: &str) -> Result<WorkComment> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE work_comments
             SET status = ?2, status_actor = 'engine', updated_at = ?3
             WHERE id = ?1 AND status = ?4",
            params![comment_id, COMMENT_STATUS_ACTIVE, now, COMMENT_STATUS_ANSWERING],
        )?;
        if n == 0 {
            bail!("comment {comment_id} not found, or not 'answering' (expected answering → active)");
        }
        query_comment(&conn, comment_id)?
            .with_context(|| format!("missing comment after answering->active transition: {comment_id}"))
    }

    /// Bucket-2 track (P3b): `answering → answered`, fired when the answer
    /// agent posts its reply (`CommentsPostAnswer`) or when its run ends
    /// without one (`finalize_answer_agent`'s no-reply-posted path — an
    /// apology thread entry stands in for the missing answer so the thread
    /// isn't left silently stuck). Guarded on `status = 'answering'`,
    /// mirroring the design's idempotency table.
    ///
    /// Reverse-edge case: [`Self::override_comment_intent`] has no status
    /// guard, so a user can reclassify a comment away from `question` while
    /// its answer-agent run is still in flight (still `answering`). When
    /// that run finishes, landing on `answered` would strand the comment
    /// off the `[Revise]` candidate pool exactly like the forward-direction
    /// bug this module fixes — there's no question left to await a
    /// follow-up on. So if `intent` is already `directive`/`larger_change`
    /// by the time this fires, skip `answered` entirely and land on
    /// `active`, folding the comment straight into the revise pool (the
    /// answer-agent's reply is still available as thread context via
    /// [`Self::latest_answer_agent_run_for_comment`]).
    pub fn transition_comment_to_answered(&self, comment_id: &str) -> Result<WorkComment> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE work_comments
             SET status = CASE WHEN intent IN (?5, ?6) THEN ?7 ELSE ?2 END,
                 status_actor = 'engine', updated_at = ?3
             WHERE id = ?1 AND status = ?4",
            params![
                comment_id,
                COMMENT_STATUS_ANSWERED,
                now,
                COMMENT_STATUS_ANSWERING,
                INTENT_DIRECTIVE,
                INTENT_LARGER_CHANGE,
                COMMENT_STATUS_ACTIVE,
            ],
        )?;
        if n == 0 {
            bail!("comment {comment_id} not found, or not 'answering' (expected answering → answered)");
        }
        query_comment(&conn, comment_id)?
            .with_context(|| format!("missing comment after answered transition: {comment_id}"))
    }

    /// Bucket-2 track (P3c): `answered → awaiting_followup`, fired when an
    /// operator posts a reply in the thread (`CommentsPostFollowup`).
    /// Guarded on `status = 'answered'` — in particular, a comment still
    /// `answering` (a run already in flight) rejects a second follow-up
    /// rather than queuing it (design §"Concurrency/idempotency" describes
    /// queuing as the eventual UX; not yet implemented).
    pub fn transition_comment_to_awaiting_followup(&self, comment_id: &str) -> Result<WorkComment> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE work_comments
             SET status = ?2, status_actor = 'engine', updated_at = ?3
             WHERE id = ?1 AND status = ?4",
            params![
                comment_id,
                COMMENT_STATUS_AWAITING_FOLLOWUP,
                now,
                COMMENT_STATUS_ANSWERED
            ],
        )?;
        if n == 0 {
            bail!("comment {comment_id} not found, or not 'answered' (expected answered → awaiting_followup)");
        }
        query_comment(&conn, comment_id)?
            .with_context(|| format!("missing comment after awaiting_followup transition: {comment_id}"))
    }

    /// Bucket-2 re-entry (P3c): `awaiting_followup → answering`, fired when a
    /// follow-up reply reclassifies as `question` — the answer agent runs
    /// again with the accumulated thread as context (design
    /// §"Reclassifying follow-ups").
    pub fn transition_comment_awaiting_followup_to_answering(&self, comment_id: &str) -> Result<WorkComment> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE work_comments
             SET status = ?2, status_actor = 'engine', updated_at = ?3
             WHERE id = ?1 AND status = ?4",
            params![
                comment_id,
                COMMENT_STATUS_ANSWERING,
                now,
                COMMENT_STATUS_AWAITING_FOLLOWUP
            ],
        )?;
        if n == 0 {
            bail!(
                "comment {comment_id} not found, or not 'awaiting_followup' (expected awaiting_followup → answering)"
            );
        }
        query_comment(&conn, comment_id)?
            .with_context(|| format!("missing comment after awaiting_followup->answering transition: {comment_id}"))
    }

    /// Compensation for a follow-up re-entry spawn that flipped a comment to
    /// `answering` and then failed to finish creating its tracking rows —
    /// the follow-up analogue of [`Self::transition_comment_answering_to_active`].
    /// Puts the comment back in `awaiting_followup` (its state before the
    /// failed re-spawn attempt) rather than `active`, so a subsequent
    /// `[Revise]` batch doesn't pick it up prematurely. Guarded on
    /// `status = 'answering'`.
    pub fn transition_comment_answering_to_awaiting_followup(&self, comment_id: &str) -> Result<WorkComment> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE work_comments
             SET status = ?2, status_actor = 'engine', updated_at = ?3
             WHERE id = ?1 AND status = ?4",
            params![
                comment_id,
                COMMENT_STATUS_AWAITING_FOLLOWUP,
                now,
                COMMENT_STATUS_ANSWERING
            ],
        )?;
        if n == 0 {
            bail!("comment {comment_id} not found, or not 'answering' (expected answering → awaiting_followup)");
        }
        query_comment(&conn, comment_id)?
            .with_context(|| format!("missing comment after answering->awaiting_followup transition: {comment_id}"))
    }

    /// The bucket-1&3 bridge (P3c): `awaiting_followup → active`, fired when
    /// a follow-up reply reclassifies as `directive`/`larger_change`. The
    /// comment re-enters the `[Revise]` candidate pool exactly like any
    /// other `active` `directive`/`larger_change` comment (design
    /// §"Bridging a bucket-2 answer into a revision") — `revise_task_id`
    /// stays `NULL` since no batch has claimed it yet.
    pub fn transition_comment_awaiting_followup_to_active(&self, comment_id: &str) -> Result<WorkComment> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE work_comments
             SET status = ?2, status_actor = 'engine', updated_at = ?3
             WHERE id = ?1 AND status = ?4",
            params![comment_id, COMMENT_STATUS_ACTIVE, now, COMMENT_STATUS_AWAITING_FOLLOWUP],
        )?;
        if n == 0 {
            bail!("comment {comment_id} not found, or not 'awaiting_followup' (expected awaiting_followup → active)");
        }
        query_comment(&conn, comment_id)?
            .with_context(|| format!("missing comment after awaiting_followup->active transition: {comment_id}"))
    }

    /// Reclassify a comment's intent from a follow-up reply (P3c). Unlike
    /// [`Self::set_comment_intent`] (guarded to fire once, for the
    /// comment's original top-level classification), this has no
    /// `intent_classified_at IS NULL` guard: a follow-up is a fresh
    /// classification event on new thread content, so it always overwrites
    /// `intent`/`intent_confidence`/`intent_classified_at`. Distinct from
    /// [`Self::override_comment_intent`] in one way that matters: this is an
    /// **engine** classification, not a human correction, so it clears
    /// `intent_overridden_by` (any earlier manual override is superseded by
    /// the operator's new reply, not preserved as if the classifier never
    /// ran) rather than stamping `'user'`. Also clears
    /// `intent_classification_failed_at`/`intent_classification_error`,
    /// mirroring [`Self::set_comment_intent`] — a successful reclassification
    /// must not leave a stale failure record from an earlier attempt.
    pub fn reclassify_comment_intent(&self, comment_id: &str, intent: &str, confidence: f64) -> Result<WorkComment> {
        match intent {
            INTENT_DIRECTIVE | INTENT_QUESTION | INTENT_LARGER_CHANGE => {}
            other => bail!("invalid comment intent: {other}"),
        }
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE work_comments
             SET intent = ?2, intent_confidence = ?3, intent_classified_at = ?4, intent_overridden_by = NULL,
                 intent_classification_failed_at = NULL, intent_classification_error = NULL
             WHERE id = ?1",
            params![comment_id, intent, confidence, now],
        )?;
        if n == 0 {
            bail!("unknown comment: {comment_id}");
        }
        query_comment(&conn, comment_id)?
            .with_context(|| format!("missing comment after intent reclassification: {comment_id}"))
    }

    /// Persist the async classifier's output onto a comment's intent
    /// columns — called from the detached task spawned off `CommentsCreate`
    /// (comment-intent-classification design § "The classifier"). Guarded
    /// on `intent_classified_at IS NULL` so a comment is only ever
    /// engine-classified once; re-firing (a raced/duplicate completion) is
    /// a no-op error the caller logs and discards. A manual override
    /// (`CommentsSetIntent`) bypasses this guard by design — see
    /// [`Self::override_comment_intent`], which that RPC uses instead.
    pub fn set_comment_intent(&self, comment_id: &str, intent: &str, confidence: f64) -> Result<WorkComment> {
        match intent {
            INTENT_DIRECTIVE | INTENT_QUESTION | INTENT_LARGER_CHANGE => {}
            other => bail!("invalid comment intent: {other}"),
        }
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE work_comments
             SET intent = ?2, intent_confidence = ?3, intent_classified_at = ?4,
                 intent_classification_failed_at = NULL, intent_classification_error = NULL
             WHERE id = ?1 AND intent_classified_at IS NULL",
            params![comment_id, intent, confidence, now],
        )?;
        if n == 0 {
            bail!("comment {comment_id} not found, or already classified");
        }
        query_comment(&conn, comment_id)?.with_context(|| format!("missing comment after intent update: {comment_id}"))
    }

    /// Record that the async classifier permanently gave up on a comment
    /// after exhausting its retries (comment-intent-classification design —
    /// the classifier retry/terminal-state fix). Guarded on
    /// `intent_classified_at IS NULL`, mirroring [`Self::set_comment_intent`]:
    /// a comment that has already been successfully classified (or manually
    /// overridden) must never be knocked back into a failed state by a
    /// stale/late-arriving failure. Idempotent — re-recording the same
    /// failure (e.g. a second, independent classifier attempt elsewhere)
    /// just overwrites the error text and timestamp.
    pub fn record_comment_classification_failed(&self, comment_id: &str, error: &str) -> Result<WorkComment> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE work_comments
             SET intent_classification_failed_at = ?2, intent_classification_error = ?3
             WHERE id = ?1 AND intent_classified_at IS NULL",
            params![comment_id, now, error],
        )?;
        if n == 0 {
            bail!("comment {comment_id} not found, or already classified");
        }
        query_comment(&conn, comment_id)?
            .with_context(|| format!("missing comment after recording classification failure: {comment_id}"))
    }

    /// Manually reclassify a comment's intent (`CommentsSetIntent` RPC —
    /// comment-intent-classification design § "Misclassification /
    /// override"). Unlike [`Self::set_comment_intent`], this has no
    /// `intent_classified_at IS NULL` guard: it overwrites any prior engine
    /// classification (or lack thereof) and always stamps
    /// `intent_overridden_by = 'user'`, which is preserved permanently as an
    /// audit trail distinguishing engine calls from human corrections.
    /// `intent_confidence` is cleared (`NULL`) — a manual override has no
    /// numeric confidence, and the override itself doubles as the
    /// classification, so no re-classification LLM call is triggered.
    ///
    /// Also has no status guard — it can fire from any comment status,
    /// including mid-bucket-2 (`answering`/`answered`/`awaiting_followup`).
    /// If the new intent is revisable (`directive`/`larger_change`) and the
    /// comment is sitting in any of those three statuses, this also resets
    /// `status` back to `active` in the same write: all three only make
    /// sense for a `question` awaiting (or receiving) an answer, and leaving
    /// a revisable-intent comment stranded there silently excludes it from
    /// every consumer of [`revisable_comment_predicate`] — `[Revise]`'s
    /// candidate query, its claim UPDATE, and the banner's
    /// `unresolved_count` (comment-triggered-document-revisions.md
    /// §"Reclassifying an answered question mid-flight"). The answer-agent's
    /// prior reply is preserved as thread context regardless (it isn't
    /// touched by this transition) — `compose_doc_comment_directive` pulls
    /// it in via [`Self::latest_answer_agent_run_for_comment`] once the
    /// comment is claimed into a revision.
    ///
    /// `answering` is included deliberately, reversing this method's original
    /// behaviour. It used to be excluded on the theory that a live run must
    /// not be orphaned, with the reverse edge deferred to
    /// [`Self::transition_comment_to_answered`] — but "deferred to run
    /// termination" is exactly the operator-visible bug: the comment kept
    /// showing "Thinking…", stayed off the unresolved count, and `[Revise]`
    /// dropped it, for as long as the agent kept running. The status must
    /// re-home *immediately*; standing the live run down is the caller's job
    /// (`handle_comments_set_intent` supersedes the `answer_agent_runs` row
    /// and cancels the bound execution). The reverse edge in
    /// `transition_comment_to_answered` is retained as a backstop for any
    /// path that reclassifies without going through that handler.
    ///
    /// `in_revision` is NOT reset: the comment has already been claimed by a
    /// `[Revise]` batch whose directive was assembled once, at creation, and
    /// is immutable. Pulling the comment out from under a live task would
    /// leave that task addressing a comment it no longer owns and destroy the
    /// `revise_task_id` provenance. The new intent takes effect on the next
    /// cycle instead — reconciliation returns the comment to `active` (with
    /// the overridden intent already in place) if the batch is abandoned. The
    /// operator is told about this rather than left to infer it:
    /// [`excluded_revisable_comment_predicate`] surfaces such comments in
    /// `ReviseDocOutcome::Created`'s `excluded_comment_ids`.
    pub fn override_comment_intent(&self, comment_id: &str, intent: &str) -> Result<WorkComment> {
        match intent {
            INTENT_DIRECTIVE | INTENT_QUESTION | INTENT_LARGER_CHANGE => {}
            other => bail!("invalid comment intent: {other}"),
        }
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE work_comments
             SET intent = ?2, intent_confidence = NULL, intent_classified_at = ?3, intent_overridden_by = 'user',
                 intent_classification_failed_at = NULL, intent_classification_error = NULL,
                 status = CASE WHEN status IN (?4, ?5, ?9) AND ?2 IN (?6, ?7) THEN ?8 ELSE status END,
                 status_actor = CASE WHEN status IN (?4, ?5, ?9) AND ?2 IN (?6, ?7) THEN 'user' ELSE status_actor END,
                 updated_at = CASE WHEN status IN (?4, ?5, ?9) AND ?2 IN (?6, ?7) THEN ?3 ELSE updated_at END
             WHERE id = ?1",
            params![
                comment_id,
                intent,
                now,
                COMMENT_STATUS_ANSWERED,
                COMMENT_STATUS_AWAITING_FOLLOWUP,
                INTENT_DIRECTIVE,
                INTENT_LARGER_CHANGE,
                COMMENT_STATUS_ACTIVE,
                COMMENT_STATUS_ANSWERING,
            ],
        )?;
        if n == 0 {
            bail!("unknown comment: {comment_id}");
        }
        query_comment(&conn, comment_id)?
            .with_context(|| format!("missing comment after intent override: {comment_id}"))
    }

    /// Every comment currently sitting `answering`, oldest first — the
    /// candidate set for [`crate::stranded_answering_sweep`].
    ///
    /// `answering` is only ever meant to be transient: the answer agent runs,
    /// its Stop hook fires, and `finalize_answer_agent` moves the comment on.
    /// If that Stop never arrives (pane killed, engine restarted mid-run) the
    /// comment sits here forever — invisible to the `[Revise]` banner and to
    /// `query_revisable_comments`, with no operator-visible signal that
    /// anything is wrong. Unlike the execution-centric reapers this cannot be
    /// keyed on the in-memory live-worker registry (empty after a restart), so
    /// it reads the durable comment rows instead.
    pub fn list_answering_comments(&self) -> Result<Vec<WorkComment>> {
        let conn = self.connect()?;
        let sql = format!(
            "SELECT {COMMENT_COLUMNS} FROM work_comments
             WHERE status = ?1
             ORDER BY updated_at ASC, id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![COMMENT_STATUS_ANSWERING], map_comment)?;
        collect_rows(rows)
    }

    /// Persist a renderer-supplied re-anchor (the `comments_update_anchor`
    /// callback). Records the fuzzy outcome so the sidebar shows the ⚠ glyph
    /// and subsequent loads exact-match against the new shape.
    pub fn update_comment_anchor(
        &self,
        comment_id: &str,
        anchor: &CommentAnchor,
        new_doc_version: &str,
        plain_text_projection_version: i64,
    ) -> Result<WorkComment> {
        let conn = self.connect()?;
        let now = now_string();
        let anchor_json = serde_json::to_string(anchor)?;
        let n = conn.execute(
            "UPDATE work_comments
             SET anchor_json = ?2,
                 doc_version = ?3,
                 last_resolved_with = ?4,
                 plain_text_projection_version = ?5,
                 updated_at = ?6
             WHERE id = ?1",
            params![
                comment_id,
                anchor_json,
                new_doc_version,
                RESOLVED_WITH_FUZZY,
                plain_text_projection_version,
                now
            ],
        )?;
        if n == 0 {
            bail!("unknown comment: {comment_id}");
        }
        query_comment(&conn, comment_id)?.with_context(|| format!("missing comment after anchor update: {comment_id}"))
    }

    /// Resolve every active (or previously orphaned) comment on an artifact
    /// against `plain_text` — the renderer's current plain-text projection.
    ///
    /// Persists the resolution outcome: an `exact` hit marks the row
    /// `last_resolved_with = 'exact'`; a `fuzzy` hit re-extracts a fresh
    /// anchor around the match (so the next load exact-matches) and marks it
    /// `'fuzzy'`; an unresolvable comment flips to `status = 'orphaned'`. A
    /// previously orphaned comment that now resolves is revived to `active`.
    ///
    /// Resolution is a per-client read-with-side-effect (each client supplies
    /// its own projection), so it does **not** publish a topic event; the
    /// caller already receives the outcome in the reply.
    pub fn resolve_comments(
        &self,
        artifact_kind: &str,
        artifact_id: &str,
        plain_text: &str,
        plain_text_projection_version: i64,
        config: &CommentFuzzyConfig,
    ) -> Result<Vec<ResolvedComment>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        // Resolve active + orphaned (orphans can recover); resolved/dismissed
        // are intentionally not re-resolved.
        let comments = query_comments(&tx, artifact_kind, artifact_id, false)?;
        let now = now_string();
        let mut out = Vec::with_capacity(comments.len());
        for mut comment in comments {
            if comment.status != COMMENT_STATUS_ACTIVE && comment.status != COMMENT_STATUS_ORPHANED {
                continue;
            }
            let was_orphaned = comment.status == COMMENT_STATUS_ORPHANED;
            let resolution = resolve_anchor(plain_text, &comment.anchor, config);
            let wire = match resolution {
                AnchorResolution::Exact { start, length } => {
                    tx.execute(
                        "UPDATE work_comments
                         SET status = 'active', last_resolved_with = ?2, updated_at = ?3
                         WHERE id = ?1",
                        params![comment.id, RESOLVED_WITH_EXACT, now],
                    )?;
                    comment.status = COMMENT_STATUS_ACTIVE.to_owned();
                    comment.last_resolved_with = Some(RESOLVED_WITH_EXACT.to_owned());
                    comment.updated_at = now.clone();
                    CommentResolution {
                        kind: RESOLVED_WITH_EXACT.to_owned(),
                        start: Some(start as i64),
                        length: Some(length as i64),
                        score: None,
                    }
                }
                AnchorResolution::Fuzzy { start, length, score } => {
                    let new_anchor = extract_anchor(plain_text, start, length);
                    let anchor_json = serde_json::to_string(&new_anchor)?;
                    tx.execute(
                        "UPDATE work_comments
                         SET anchor_json = ?2, status = 'active', last_resolved_with = ?3,
                             plain_text_projection_version = ?4, updated_at = ?5
                         WHERE id = ?1",
                        params![
                            comment.id,
                            anchor_json,
                            RESOLVED_WITH_FUZZY,
                            plain_text_projection_version,
                            now
                        ],
                    )?;
                    comment.anchor = new_anchor;
                    comment.status = COMMENT_STATUS_ACTIVE.to_owned();
                    comment.last_resolved_with = Some(RESOLVED_WITH_FUZZY.to_owned());
                    comment.plain_text_projection_version = plain_text_projection_version;
                    comment.updated_at = now.clone();
                    CommentResolution {
                        kind: RESOLVED_WITH_FUZZY.to_owned(),
                        start: Some(start as i64),
                        length: Some(length as i64),
                        score: Some(score),
                    }
                }
                AnchorResolution::Orphan(reason) => {
                    tx.execute(
                        "UPDATE work_comments
                         SET status = 'orphaned', last_resolved_with = ?2, updated_at = ?3
                         WHERE id = ?1",
                        params![comment.id, RESOLVED_WITH_ORPHAN, now],
                    )?;
                    if !was_orphaned {
                        log_orphan_transition(&comment.id, &comment.doc_version, &comment.anchor.exact, reason, config);
                    }
                    comment.status = COMMENT_STATUS_ORPHANED.to_owned();
                    comment.last_resolved_with = Some(RESOLVED_WITH_ORPHAN.to_owned());
                    comment.updated_at = now.clone();
                    CommentResolution {
                        kind: RESOLVED_WITH_ORPHAN.to_owned(),
                        start: None,
                        length: None,
                        score: None,
                    }
                }
            };
            out.push(ResolvedComment {
                comment,
                resolution: wire,
            });
        }
        tx.commit()?;
        Ok(out)
    }

    /// Re-key the active `work_item:<task_id>` comments onto a `pr_doc:*`
    /// artifact when a design doc graduates to a PR (DesignDetector
    /// `in_review` transition). Each original is copied to a new row keyed to
    /// `new_artifact_id`; the original is then soft-resolved so the trail is
    /// visible (design § "Comments on PR-backed docs").
    ///
    /// When `new_plain_text` is supplied, each migrated anchor is immediately
    /// re-resolved against it (fuzzy re-anchors are persisted; comments that
    /// can't re-anchor land as `orphaned` on the pr_doc side). When `None`,
    /// the anchors are copied verbatim and resolution is deferred to the
    /// renderer's next load — the engine cannot itself render markdown to
    /// plain text. Returns the number of comments migrated.
    pub fn migrate_work_item_comments_to_pr_doc(
        &self,
        task_id: &str,
        new_artifact_id: &str,
        new_plain_text: Option<&str>,
        plain_text_projection_version: i64,
        config: &CommentFuzzyConfig,
    ) -> Result<usize> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let originals: Vec<WorkComment> = query_comments(&tx, "work_item", task_id, false)?
            .into_iter()
            .filter(|c| c.status == COMMENT_STATUS_ACTIVE)
            .collect();
        let now = now_string();
        let actor = crate::work::AUDIT_ACTOR_DESIGN_DETECTOR;
        let mut migrated = 0usize;
        for original in &originals {
            let new_id = next_id("cmt");
            let mut anchor = original.anchor.clone();
            let mut status = COMMENT_STATUS_ACTIVE;
            let mut last_resolved: Option<&str> = None;
            let mut proj_ver = original.plain_text_projection_version;
            if let Some(text) = new_plain_text {
                match resolve_anchor(text, &original.anchor, config) {
                    AnchorResolution::Exact { .. } => last_resolved = Some(RESOLVED_WITH_EXACT),
                    AnchorResolution::Fuzzy { start, length, .. } => {
                        anchor = extract_anchor(text, start, length);
                        last_resolved = Some(RESOLVED_WITH_FUZZY);
                        proj_ver = plain_text_projection_version;
                    }
                    AnchorResolution::Orphan(reason) => {
                        status = COMMENT_STATUS_ORPHANED;
                        last_resolved = Some(RESOLVED_WITH_ORPHAN);
                        log_orphan_transition(&new_id, &original.doc_version, &original.anchor.exact, reason, config);
                    }
                }
            }
            let anchor_json = serde_json::to_string(&anchor)?;
            tx.execute(
                COMMENT_INSERT_SQL,
                params![
                    new_id,
                    "pr_doc",
                    new_artifact_id,
                    original.doc_version,
                    anchor_json,
                    original.body,
                    original.author,
                    status,
                    actor,
                    last_resolved,
                    proj_ver,
                    now,
                    now,
                    Option::<String>::None,
                ],
            )?;
            tx.execute(
                "UPDATE work_comments
                 SET status = 'resolved', status_actor = ?2, updated_at = ?3, dismissed_at = ?3
                 WHERE id = ?1",
                params![original.id, actor, now],
            )?;
            migrated += 1;
        }
        tx.commit()?;
        Ok(migrated)
    }
}

/// Re-extract a 64/exact/64-char anchor around `[start, start+length)` in the
/// plain text, trimmed to text bounds. Used after a fuzzy resolve so the
/// stored anchor reflects the current doc and the next load exact-matches.
fn extract_anchor(plain_text: &str, start: usize, length: usize) -> CommentAnchor {
    let chars: Vec<char> = plain_text.chars().collect();
    let n = chars.len();
    let start = start.min(n);
    let end = (start + length).min(n);
    let prefix_start = start.saturating_sub(ANCHOR_CONTEXT_CHARS);
    let suffix_end = (end + ANCHOR_CONTEXT_CHARS).min(n);
    CommentAnchor {
        exact: chars[start..end].iter().collect(),
        prefix: chars[prefix_start..start].iter().collect(),
        suffix: chars[end..suffix_end].iter().collect(),
    }
}

pub(crate) fn query_comment(conn: &Connection, id: &str) -> Result<Option<WorkComment>> {
    let sql = format!("SELECT {COMMENT_COLUMNS} FROM work_comments WHERE id = ?1");
    conn.query_row(&sql, [id], map_comment).optional().map_err(Into::into)
}

pub(crate) fn query_comments(
    conn: &Connection,
    artifact_kind: &str,
    artifact_id: &str,
    include_resolved: bool,
) -> Result<Vec<WorkComment>> {
    let filter = if include_resolved {
        ""
    } else {
        " AND status NOT IN ('resolved', 'dismissed')"
    };
    let sql = format!(
        "SELECT {COMMENT_COLUMNS} FROM work_comments
         WHERE artifact_kind = ?1 AND artifact_id = ?2{filter}
         ORDER BY created_at ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![artifact_kind, artifact_id], map_comment)?;
    collect_rows(rows)
}

/// Comments eligible for a `[Revise]` batch (`CommentsReviseDoc`): `active`
/// status, classified `directive`/`larger_change`. `comment_ids` narrows to
/// that id set when supplied — v1 always passes `None` (reserved for a
/// future subset-selection UI, design §"Batch scope").
pub(crate) fn query_revisable_comments(
    conn: &Connection,
    artifact_kind: &str,
    artifact_id: &str,
    comment_ids: Option<&[String]>,
) -> Result<Vec<WorkComment>> {
    if matches!(comment_ids, Some(ids) if ids.is_empty()) {
        return Ok(Vec::new());
    }
    let revisable = revisable_comment_predicate();
    let mut sql = format!(
        "SELECT {COMMENT_COLUMNS} FROM work_comments
         WHERE artifact_kind = ? AND artifact_id = ?
           AND {revisable}"
    );
    let mut bind_params: Vec<&dyn rusqlite::ToSql> = vec![&artifact_kind, &artifact_id];
    if let Some(ids) = comment_ids {
        let placeholders = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(",");
        sql.push_str(&format!(" AND id IN ({placeholders})"));
        for id in ids {
            bind_params.push(id);
        }
    }
    sql.push_str(" ORDER BY created_at ASC, id ASC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(bind_params.as_slice(), map_comment)?;
    collect_rows(rows)
}

/// The comments a `[Revise]` batch on this artifact leaves behind: badged
/// `directive`/`larger_change` (so the operator reads them as revisable) but
/// disqualified by `status` — `in_revision` (claimed by an earlier batch),
/// `orphaned` (the anchor no longer resolves), or `answering` (a live
/// answer-agent run). Exactly the complement of [`query_revisable_comments`]
/// over the same intent set, minus the `resolved`/`dismissed` comments the
/// sidebar already hides. Reported as `ReviseDocOutcome::Created`'s
/// `excluded_comment_ids` so `[Revise]` never returns an unqualified success
/// while quietly dropping comments the operator can see marked revisable.
pub(crate) fn query_excluded_revisable_comment_ids(
    conn: &Connection,
    artifact_kind: &str,
    artifact_id: &str,
) -> Result<Vec<String>> {
    let excluded = excluded_revisable_comment_predicate();
    let sql = format!(
        "SELECT id FROM work_comments
         WHERE artifact_kind = ?1 AND artifact_id = ?2
           AND {excluded}
         ORDER BY created_at ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![artifact_kind, artifact_id], |row| row.get(0))?;
    collect_rows(rows)
}

/// Outcome driving [`reconcile_comments_for_task`] — whether the task that
/// owns a batch of `in_revision` comments reached a terminal "shipped"
/// state (`Resolved`) or a terminal "did not ship" state (`Reopened`).
/// Comment-intent-classification design §"Reconciliation".
pub(crate) enum CommentReconcileOutcome {
    Resolved,
    /// `include_resolved`: also match comments already `resolved` (not just
    /// `in_revision`) — for a revision whose commit landed and resolved its
    /// comments ahead of the chain root's own terminal state (see the
    /// module-level doc on this transition in `record_worker_pr_completion`),
    /// a later close-unmerged event must still reopen them. `false` for the
    /// plain-chore vehicle: its own comments only ever resolve on a genuine
    /// merge, so a late/duplicate close-unmerged sweep for that same task
    /// must never undo it (`reopen_is_noop_once_already_resolved`).
    Reopened {
        include_resolved: bool,
    },
}

/// Reconcile every comment claimed by `task_id`'s `[Revise]` batch
/// (`revise_task_id = task_id`, `status = 'in_revision'`) when that task
/// reaches a terminal state. Design
/// `comment-triggered-document-revisions.md` §"Reconciliation":
///
/// - `Resolved` — for a revision, fired the moment its commit lands on the
///   chain root's PR branch (the vehicle reaches `in_review`); for the
///   plain-chore vehicle, fired when the chore's own PR merges. Either way
///   the requested change rode a real commit, so mark the comment
///   `resolved`. `revise_task_id` is deliberately left in place — it is the
///   provenance trail of which batch addressed the comment (see
///   [`WorkComment::revise_task_id`]'s doc comment).
/// - `Reopened` (the task was abandoned / its PR closed unmerged): the
///   requested change never shipped, so put the comment back on the
///   `[Revise]` banner — `status='active'`, `revise_task_id` cleared. With
///   `include_resolved: true`, also matches comments already `resolved` by
///   a revision's commit landing: if the chain root's PR later closes
///   unmerged, that commit never made it to `main` either, so a comment
///   resolved-by-landing must reopen just like one still sitting
///   `in_revision` — the `revise_task_id` provenance this outcome never
///   clears on the resolve side is exactly what makes these rows findable
///   here.
///
/// Deliberately does **not** touch `last_resolved_with`: despite the
/// design doc's SQL sketch proposing `last_resolved_with='revise:<task_id>'`,
/// that column is already the anchor-resolution-mode field
/// (`exact`/`fuzzy`/`orphan`, driving the sidebar's ⚠ glyph —
/// `migrate_work_comments_table`) for every comment in production today;
/// stomping it here would destroy that history for no benefit, since
/// `revise_task_id` already carries the "which batch resolved this"
/// provenance the design SQL was reaching for.
///
/// `Resolved` is always guarded on `status = 'in_revision'`. Either way,
/// calling this on a task that never claimed any comments — or re-firing on
/// an already-reconciled task — is a no-op. Returns the number of comment
/// rows changed (tests / logging).
pub(crate) fn reconcile_comments_for_task(
    conn: &Connection,
    task_id: &str,
    outcome: CommentReconcileOutcome,
    now: &str,
) -> Result<usize> {
    let affected = match outcome {
        CommentReconcileOutcome::Resolved => conn.execute(
            &format!(
                "UPDATE work_comments
                 SET status = '{COMMENT_STATUS_RESOLVED}',
                     status_actor = 'engine',
                     updated_at = ?2,
                     dismissed_at = ?2
                 WHERE revise_task_id = ?1 AND status = '{COMMENT_STATUS_IN_REVISION}'"
            ),
            params![task_id, now],
        )?,
        CommentReconcileOutcome::Reopened { include_resolved } => {
            let status_filter = if include_resolved {
                format!("status IN ('{COMMENT_STATUS_IN_REVISION}', '{COMMENT_STATUS_RESOLVED}')")
            } else {
                format!("status = '{COMMENT_STATUS_IN_REVISION}'")
            };
            conn.execute(
                &format!(
                    "UPDATE work_comments
                     SET status = '{COMMENT_STATUS_ACTIVE}',
                         revise_task_id = NULL,
                         status_actor = 'engine',
                         updated_at = ?2,
                         dismissed_at = NULL
                     WHERE revise_task_id = ?1
                       AND {status_filter}"
                ),
                params![task_id, now],
            )?
        }
    };
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use crate::comments_anchor::CommentFuzzyConfig;
    use crate::work::WorkDb;
    use boss_protocol::{CommentAnchor, CreateCommentInput, WorkComment};
    use std::path::PathBuf;

    /// Per-test named shared-cache in-memory db (see `work::tests`).
    fn mem_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).unwrap()
    }

    fn input(artifact_id: &str, exact: &str, prefix: &str, suffix: &str) -> CreateCommentInput {
        CreateCommentInput {
            artifact_kind: "work_item".to_owned(),
            artifact_id: artifact_id.to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: exact.to_owned(),
                prefix: prefix.to_owned(),
                suffix: suffix.to_owned(),
            },
            body: "a comment body".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 1,
        }
    }

    fn cfg() -> CommentFuzzyConfig {
        CommentFuzzyConfig::default()
    }

    #[test]
    fn create_and_list_round_trip() {
        let db = mem_db();
        let c1 = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        let _c2 = db.create_comment(input("t1", "beta", "", "")).unwrap();
        let list = db.list_comments("work_item", "t1", false).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(c1.status, "active");
        assert_eq!(c1.author, "user:test@example.com");
        assert_eq!(c1.plain_text_projection_version, 1);
        assert!(list.iter().any(|c| c.anchor.exact == "alpha"));
        assert!(list.iter().any(|c| c.anchor.exact == "beta"));
        // Other artifacts are isolated.
        assert!(db.list_comments("work_item", "other", false).unwrap().is_empty());
    }

    #[test]
    fn banner_state_never_revisable_without_a_doc_owner() {
        let db = mem_db();
        // No project/design task points at this artifact, so
        // `resolve_doc_owner` returns `None`. `revisable` must be false even
        // though there is an unresolved directive comment sitting on it —
        // `doc_kind` being absent is the gate, not the comment count.
        let artifact_id = "pr_doc:git@github.com:o/r.git:main:x.md";
        let mut create_input = input(artifact_id, "alpha", "", "");
        create_input.artifact_kind = "pr_doc".to_owned();
        let c = db.create_comment(create_input).unwrap();
        db.set_comment_intent(&c.id, "directive", 0.9).unwrap();
        let state = db.comments_banner_state("pr_doc", artifact_id).unwrap();
        assert!(!state.revisable);
        assert_eq!(state.unresolved_count, 1);
        assert_eq!(state.in_revision_count, 0);
        assert!(state.doc_kind.is_none());
    }

    #[test]
    fn banner_state_zero_for_untouched_artifact() {
        let db = mem_db();
        let state = db.comments_banner_state("work_item", "task_x").unwrap();
        assert!(!state.revisable);
        assert_eq!(state.unresolved_count, 0);
        assert_eq!(state.in_revision_count, 0);
        assert!(state.doc_kind.is_none());
    }

    #[test]
    fn empty_body_and_empty_exact_are_rejected() {
        let db = mem_db();
        let mut bad = input("t1", "alpha", "", "");
        bad.body = "   ".to_owned();
        assert!(db.create_comment(bad).is_err());
        let mut bad2 = input("t1", "", "", "");
        bad2.body = "ok".to_owned();
        assert!(db.create_comment(bad2).is_err());
    }

    #[test]
    fn soft_dismiss_hides_then_show_resolved_reveals_then_reactivate() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();

        let dismissed = db.dismiss_comment(&c.id, Some("user:me")).unwrap();
        assert_eq!(dismissed.status, "resolved");
        assert!(dismissed.dismissed_at.is_some());
        assert_eq!(dismissed.status_actor.as_deref(), Some("user:me"));

        // Hidden from the default list, revealed by include_resolved.
        assert!(db.list_comments("work_item", "t1", false).unwrap().is_empty());
        let revealed = db.list_comments("work_item", "t1", true).unwrap();
        assert_eq!(revealed.len(), 1);
        assert_eq!(revealed[0].status, "resolved");

        // Recoverable: re-activate clears dismissed_at.
        let reactivated = db.set_comment_status(&c.id, "active", Some("user:me")).unwrap();
        assert_eq!(reactivated.status, "active");
        assert!(reactivated.dismissed_at.is_none());
        assert_eq!(db.list_comments("work_item", "t1", false).unwrap().len(), 1);
    }

    #[test]
    fn resolve_exact_returns_span_and_persists_mode() {
        let db = mem_db();
        let doc = "Hello world, this is a sample document about anchoring.";
        let c = db
            .create_comment(input("t1", "sample document", "this is a ", " about anchoring"))
            .unwrap();
        let resolved = db.resolve_comments("work_item", "t1", doc, 2, &cfg()).unwrap();
        assert_eq!(resolved.len(), 1);
        let r = &resolved[0];
        assert_eq!(r.resolution.kind, "exact");
        let start = r.resolution.start.unwrap() as usize;
        let length = r.resolution.length.unwrap() as usize;
        let span: String = doc.chars().skip(start).take(length).collect();
        assert_eq!(span, "sample document");
        assert_eq!(r.comment.last_resolved_with.as_deref(), Some("exact"));
        // Persisted.
        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(reloaded.last_resolved_with.as_deref(), Some("exact"));
    }

    #[test]
    fn resolve_fuzzy_reanchors_and_next_load_is_exact() {
        let db = mem_db();
        let prefix = "The renderer maintains a mapping so the ";
        let exact = "engine never has to know about layout";
        let suffix = ", and the macOS app never round-trips";
        let c = db.create_comment(input("t1", exact, prefix, suffix)).unwrap();

        // A word ("carefully") was inserted inside the prefix region, so the
        // verbatim context no longer matches — but the region is ~identical.
        let edited = "Intro. The renderer carefully maintains a mapping so the engine never \
                      has to know about layout, and the macOS app never round-trips. Outro.";
        let resolved = db.resolve_comments("work_item", "t1", edited, 3, &cfg()).unwrap();
        let r = &resolved[0];
        assert_eq!(r.resolution.kind, "fuzzy");
        assert!(r.resolution.score.unwrap() >= 0.8);
        assert_eq!(r.comment.last_resolved_with.as_deref(), Some("fuzzy"));

        // The engine re-extracted and persisted a fresh anchor against the
        // edited text + recorded the projection version → a second load now
        // exact-matches.
        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(reloaded.last_resolved_with.as_deref(), Some("fuzzy"));
        assert_eq!(reloaded.plain_text_projection_version, 3);
        let again = db.resolve_comments("work_item", "t1", edited, 3, &cfg()).unwrap();
        assert_eq!(again[0].resolution.kind, "exact");
    }

    #[test]
    fn resolve_orphan_when_containing_element_deleted() {
        let db = mem_db();
        let c = db
            .create_comment(input(
                "t1",
                "The widget config lives here",
                "Section A. ",
                " Section B.",
            ))
            .unwrap();
        // The anchored span is removed entirely and replaced with unrelated text.
        let edited = "Section A. Section B. Totally different unrelated content now appears.";
        let resolved = db.resolve_comments("work_item", "t1", edited, 2, &cfg()).unwrap();
        let r = &resolved[0];
        assert_eq!(r.resolution.kind, "orphan");
        assert!(r.resolution.start.is_none());
        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(reloaded.status, "orphaned");
        assert_eq!(reloaded.last_resolved_with.as_deref(), Some("orphan"));
        // Orphans still appear in the default list (the sidebar surfaces them).
        assert_eq!(db.list_comments("work_item", "t1", false).unwrap().len(), 1);
    }

    #[test]
    fn update_anchor_persists_new_shape_and_marks_fuzzy() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "p", "s")).unwrap();
        let new_anchor = CommentAnchor {
            exact: "alpha-v2".to_owned(),
            prefix: "new-prefix".to_owned(),
            suffix: "new-suffix".to_owned(),
        };
        let updated = db.update_comment_anchor(&c.id, &new_anchor, "v2", 5).unwrap();
        assert_eq!(updated.anchor.exact, "alpha-v2");
        assert_eq!(updated.doc_version, "v2");
        assert_eq!(updated.last_resolved_with.as_deref(), Some("fuzzy"));
        assert_eq!(updated.plain_text_projection_version, 5);
        assert!(db.update_comment_anchor("nope", &new_anchor, "v2", 5).is_err());
    }

    #[test]
    fn cross_doc_migration_rekeys_and_resolves_originals() {
        let db = mem_db();
        db.create_comment(input("task1", "alpha", "", "")).unwrap();
        db.create_comment(input("task1", "beta", "", "")).unwrap();

        let pr_artifact = "pr_doc:git@github.com:o/r.git:boss/exec_x:doc.md";
        let migrated = db
            .migrate_work_item_comments_to_pr_doc("task1", pr_artifact, None, 0, &cfg())
            .unwrap();
        assert_eq!(migrated, 2);

        // Originals are soft-resolved (the trail) and gone from the default list.
        assert!(db.list_comments("work_item", "task1", false).unwrap().is_empty());
        let originals = db.list_comments("work_item", "task1", true).unwrap();
        assert_eq!(originals.len(), 2);
        assert!(originals.iter().all(|c| c.status == "resolved"));
        assert!(
            originals
                .iter()
                .all(|c| c.status_actor.as_deref() == Some("engine_design_detector"))
        );

        // The pr_doc artifact carries the migrated comments, active, with
        // anchors copied verbatim (resolution deferred to renderer load).
        let pr = db.list_comments("pr_doc", pr_artifact, false).unwrap();
        assert_eq!(pr.len(), 2);
        assert!(pr.iter().all(|c| c.status == "active"));
        assert!(pr.iter().any(|c| c.anchor.exact == "alpha"));
        assert!(pr.iter().any(|c| c.anchor.exact == "beta"));
    }

    #[test]
    fn cross_doc_migration_orphans_unanchorable_comments() {
        let db = mem_db();
        db.create_comment(input("task1", "present span", "", "")).unwrap();
        db.create_comment(input("task1", "absent span zzqq", "", "")).unwrap();

        let pr_artifact = "pr_doc:r:b:p.md";
        let pr_text = "This PR doc contains the present span among other unrelated words.";
        let migrated = db
            .migrate_work_item_comments_to_pr_doc("task1", pr_artifact, Some(pr_text), 9, &cfg())
            .unwrap();
        assert_eq!(migrated, 2);

        let pr = db.list_comments("pr_doc", pr_artifact, true).unwrap();
        let present = pr.iter().find(|c| c.anchor.exact == "present span").unwrap();
        assert_eq!(present.status, "active");
        assert_eq!(present.last_resolved_with.as_deref(), Some("exact"));

        let absent = pr.iter().find(|c| c.anchor.exact == "absent span zzqq").unwrap();
        assert_eq!(absent.status, "orphaned");
        assert_eq!(absent.last_resolved_with.as_deref(), Some("orphan"));
    }

    // --- Intent classification (P1a) ---

    #[test]
    fn new_comment_starts_unclassified() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        assert!(c.intent.is_none());
        assert!(c.intent_confidence.is_none());
        assert!(c.intent_classified_at.is_none());
        assert!(c.intent_overridden_by.is_none());
    }

    #[test]
    fn set_comment_intent_persists_and_round_trips() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        let classified = db.set_comment_intent(&c.id, "question", 0.87).unwrap();
        assert_eq!(classified.intent.as_deref(), Some("question"));
        assert_eq!(classified.intent_confidence, Some(0.87));
        assert!(classified.intent_classified_at.is_some());

        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(reloaded.intent.as_deref(), Some("question"));
        assert_eq!(reloaded.intent_confidence, Some(0.87));
    }

    #[test]
    fn set_comment_intent_rejects_unknown_intent() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        assert!(db.set_comment_intent(&c.id, "bogus", 0.5).is_err());
    }

    #[test]
    fn set_comment_intent_is_guarded_against_double_classification() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.set_comment_intent(&c.id, "directive", 0.9).unwrap();
        // A second call finds intent_classified_at already set, so it's a
        // no-op error rather than silently overwriting the classification.
        assert!(db.set_comment_intent(&c.id, "question", 0.5).is_err());
        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(reloaded.intent.as_deref(), Some("directive"));
    }

    #[test]
    fn record_comment_classification_failed_persists_error_and_timestamp() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        let failed = db
            .record_comment_classification_failed(&c.id, "classifier exhausted retries")
            .unwrap();
        assert_eq!(
            failed.intent_classification_error.as_deref(),
            Some("classifier exhausted retries")
        );
        assert!(failed.intent_classification_failed_at.is_some());
        assert!(failed.intent.is_none());
        assert!(failed.intent_classified_at.is_none());

        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(
            reloaded.intent_classification_error.as_deref(),
            Some("classifier exhausted retries")
        );
        assert!(reloaded.intent_classification_failed_at.is_some());
    }

    #[test]
    fn record_comment_classification_failed_does_not_clobber_a_classified_comment() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.set_comment_intent(&c.id, "directive", 0.9).unwrap();

        // A late-arriving failure (e.g. a raced/duplicate classifier attempt)
        // must not knock an already-classified comment back into a failed
        // state.
        assert!(db.record_comment_classification_failed(&c.id, "too late").is_err());

        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(reloaded.intent.as_deref(), Some("directive"));
        assert!(reloaded.intent_classification_error.is_none());
        assert!(reloaded.intent_classification_failed_at.is_none());
    }

    #[test]
    fn record_comment_classification_failed_does_not_clobber_an_override() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.override_comment_intent(&c.id, "question").unwrap();

        assert!(db.record_comment_classification_failed(&c.id, "too late").is_err());
        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(reloaded.intent.as_deref(), Some("question"));
        assert!(reloaded.intent_classification_error.is_none());
    }

    #[test]
    fn record_comment_classification_failed_is_idempotent_on_re_recording() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.record_comment_classification_failed(&c.id, "first error").unwrap();
        let second = db.record_comment_classification_failed(&c.id, "second error").unwrap();
        assert_eq!(second.intent_classification_error.as_deref(), Some("second error"));
    }

    #[test]
    fn set_comment_intent_clears_a_prior_classification_failure() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.record_comment_classification_failed(&c.id, "transient error")
            .unwrap();

        let classified = db.set_comment_intent(&c.id, "directive", 0.8).unwrap();
        assert!(classified.intent_classification_failed_at.is_none());
        assert!(classified.intent_classification_error.is_none());
    }

    #[test]
    fn reclassify_comment_intent_clears_a_prior_classification_failure() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.record_comment_classification_failed(&c.id, "transient error")
            .unwrap();

        // reclassify_comment_intent has no `intent_classified_at IS NULL`
        // guard, so it succeeds even after a recorded failure — and it must
        // clear the failure columns rather than leaving stale error state
        // alongside a successful classification.
        let reclassified = db.reclassify_comment_intent(&c.id, "larger_change", 0.7).unwrap();
        assert_eq!(reclassified.intent.as_deref(), Some("larger_change"));
        assert!(reclassified.intent_classification_failed_at.is_none());
        assert!(reclassified.intent_classification_error.is_none());

        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(reloaded.intent.as_deref(), Some("larger_change"));
        assert!(reloaded.intent_classification_failed_at.is_none());
        assert!(reloaded.intent_classification_error.is_none());
    }

    #[test]
    fn override_comment_intent_reclassifies_and_stamps_actor() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.set_comment_intent(&c.id, "question", 0.6).unwrap();

        let overridden = db.override_comment_intent(&c.id, "directive").unwrap();
        assert_eq!(overridden.intent.as_deref(), Some("directive"));
        assert!(overridden.intent_confidence.is_none());
        assert!(overridden.intent_classified_at.is_some());
        assert_eq!(overridden.intent_overridden_by.as_deref(), Some("user"));

        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(reloaded.intent.as_deref(), Some("directive"));
        assert_eq!(reloaded.intent_overridden_by.as_deref(), Some("user"));
    }

    #[test]
    fn override_comment_intent_works_even_when_unclassified() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        assert!(c.intent.is_none());

        let overridden = db.override_comment_intent(&c.id, "larger_change").unwrap();
        assert_eq!(overridden.intent.as_deref(), Some("larger_change"));
        assert_eq!(overridden.intent_overridden_by.as_deref(), Some("user"));
    }

    #[test]
    fn override_comment_intent_rejects_unknown_intent() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        assert!(db.override_comment_intent(&c.id, "bogus").is_err());
    }

    #[test]
    fn override_comment_intent_rejects_unknown_comment() {
        let db = mem_db();
        assert!(db.override_comment_intent("cmt_missing", "directive").is_err());
    }

    // --- Reclassified answered/awaiting_followup comments re-enter the
    // revise pool (T2265: intent override never reset status) ---

    #[test]
    fn override_comment_intent_resets_answered_status_to_active_for_revisable_intent() {
        let db = mem_db();
        let comment = seed_answered_comment(&db);
        assert_eq!(comment.status, "answered");

        let overridden = db.override_comment_intent(&comment.id, "larger_change").unwrap();
        assert_eq!(overridden.status, "active");
        assert_eq!(overridden.status_actor.as_deref(), Some("user"));
        assert_eq!(overridden.intent.as_deref(), Some("larger_change"));
        assert_eq!(overridden.intent_overridden_by.as_deref(), Some("user"));

        let reloaded = db.get_comment(&comment.id).unwrap().unwrap();
        assert_eq!(reloaded.status, "active");
    }

    #[test]
    fn override_comment_intent_resets_awaiting_followup_status_to_active_for_revisable_intent() {
        let db = mem_db();
        let comment = seed_answered_comment(&db);
        db.transition_comment_to_awaiting_followup(&comment.id).unwrap();

        let overridden = db.override_comment_intent(&comment.id, "directive").unwrap();
        assert_eq!(overridden.status, "active");
        assert_eq!(overridden.status_actor.as_deref(), Some("user"));
    }

    #[test]
    fn override_comment_intent_leaves_answered_status_alone_when_new_intent_is_still_question() {
        let db = mem_db();
        let comment = seed_answered_comment(&db);

        // Re-affirming 'question' isn't a bucket-1&3 bridge — status must
        // stay 'answered'.
        let overridden = db.override_comment_intent(&comment.id, "question").unwrap();
        assert_eq!(overridden.status, "answered");
    }

    #[test]
    fn override_comment_intent_leaves_active_status_alone() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        assert_eq!(comment.status, "active");

        // Already active — the CASE guard must be a no-op, not an error.
        let overridden = db.override_comment_intent(&comment.id, "directive").unwrap();
        assert_eq!(overridden.status, "active");
    }

    #[test]
    fn override_comment_intent_rehomes_status_immediately_while_a_run_is_still_answering() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.transition_comment_to_answering(&comment.id).unwrap();

        // This assertion is the inverse of what it used to be. Leaving a
        // reclassified comment `answering` until its run terminated was the
        // operator-visible bug: it kept showing "Thinking…", stayed off the
        // unresolved count, and `[Revise]` silently dropped it, for as long
        // as the agent kept running. Standing the live run down is
        // `handle_comments_set_intent`'s job; the status must move here.
        let overridden = db.override_comment_intent(&comment.id, "larger_change").unwrap();
        assert_eq!(overridden.status, "active");
        assert_eq!(overridden.status_actor.as_deref(), Some("user"));
        assert_eq!(overridden.intent.as_deref(), Some("larger_change"));
    }

    #[test]
    fn override_comment_intent_leaves_a_claimed_comment_in_revision() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.set_comment_intent(&comment.id, "directive", 0.9).unwrap();
        // `set_comment_status` rejects `in_revision` (it's an engine-internal
        // transition), so claim the comment the way a real batch does.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_comments SET status = 'in_revision', revise_task_id = 'task_1' WHERE id = ?1",
                [&comment.id],
            )
            .unwrap();
        }

        // A claimed comment must not be yanked out from under the live task
        // whose (immutable) directive already quotes it.
        let overridden = db.override_comment_intent(&comment.id, "question").unwrap();
        assert_eq!(overridden.status, "in_revision");
        assert_eq!(overridden.revise_task_id.as_deref(), Some("task_1"));
        assert_eq!(overridden.intent.as_deref(), Some("question"));
    }

    #[test]
    fn overridden_answering_comment_immediately_counts_and_is_revisable() {
        // The end-to-end assertion the unit-level status arithmetic never
        // made: after a reclassification, does the comment actually show up
        // in the two queries the banner and `[Revise]` read?
        let db = mem_db();
        let artifact_id = "pr_doc:git@github.com:o/r.git:main:x.md";
        let mut create_input = input(artifact_id, "alpha", "", "");
        create_input.artifact_kind = "pr_doc".to_owned();
        let comment = db.create_comment(create_input).unwrap();
        db.set_comment_intent(&comment.id, "question", 0.9).unwrap();
        db.transition_comment_to_answering(&comment.id).unwrap();

        // Mid-flight: invisible to both consumers, which is correct — a live
        // answer agent genuinely means "not ready".
        assert_eq!(
            db.comments_banner_state("pr_doc", artifact_id)
                .unwrap()
                .unresolved_count,
            0
        );
        {
            let conn = db.connect().unwrap();
            assert!(
                super::query_revisable_comments(&conn, "pr_doc", artifact_id, None)
                    .unwrap()
                    .is_empty()
            );
        }

        db.override_comment_intent(&comment.id, "larger_change").unwrap();

        assert_eq!(
            db.comments_banner_state("pr_doc", artifact_id)
                .unwrap()
                .unresolved_count,
            1,
            "a reclassified comment must count as unresolved immediately, not once its run ends",
        );
        let conn = db.connect().unwrap();
        let revisable = super::query_revisable_comments(&conn, "pr_doc", artifact_id, None).unwrap();
        assert_eq!(revisable.len(), 1);
        assert_eq!(revisable[0].id, comment.id);
    }

    #[test]
    fn excluded_revisable_query_reports_claimed_and_orphaned_comments_only() {
        let db = mem_db();
        let artifact_id = "pr_doc:git@github.com:o/r.git:main:x.md";
        let make = |exact: &str| {
            let mut create_input = input(artifact_id, exact, "", "");
            create_input.artifact_kind = "pr_doc".to_owned();
            db.create_comment(create_input).unwrap()
        };
        let active = make("active-one");
        let claimed = make("claimed-one");
        let orphaned = make("orphaned-one");
        let answering = make("answering-one");
        let resolved = make("resolved-one");
        let question = make("question-one");
        for c in [&active, &claimed, &orphaned, &answering, &resolved] {
            db.set_comment_intent(&c.id, "directive", 0.9).unwrap();
        }
        db.set_comment_intent(&question.id, "question", 0.9).unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_comments SET status = 'in_revision', revise_task_id = 't1' WHERE id = ?1",
                [&claimed.id],
            )
            .unwrap();
        }
        db.set_comment_status(&orphaned.id, "orphaned", Some("engine")).unwrap();
        db.transition_comment_to_answering(&answering.id).unwrap();
        db.set_comment_status(&resolved.id, "resolved", Some("user")).unwrap();

        let conn = db.connect().unwrap();
        let excluded = super::query_excluded_revisable_comment_ids(&conn, "pr_doc", artifact_id).unwrap();
        assert!(
            !excluded.contains(&active.id),
            "an addressable comment is not an exclusion"
        );
        assert!(!excluded.contains(&question.id), "a question is not badged revisable");
        assert!(
            !excluded.contains(&resolved.id),
            "a resolved comment is already hidden from the sidebar",
        );
        assert!(excluded.contains(&claimed.id));
        assert!(excluded.contains(&orphaned.id));
        assert!(excluded.contains(&answering.id));
    }

    #[test]
    fn transition_to_answered_lands_on_active_when_intent_is_revisable_mid_flight() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.transition_comment_to_answering(&comment.id).unwrap();

        // `override_comment_intent` now re-homes the status itself, so it
        // can't set this state up. `reclassify_comment_intent` (the engine's
        // follow-up reclassifier) writes `intent` without touching `status`,
        // which is exactly the residual path this reverse edge backstops.
        db.reclassify_comment_intent(&comment.id, "larger_change", 0.8).unwrap();
        assert_eq!(db.get_comment(&comment.id).unwrap().unwrap().status, "answering");

        // The run finishes: rather than landing on 'answered' (stranding
        // the comment off the revise pool exactly like the forward-direction
        // bug), it must skip straight to 'active'.
        let answered = db.transition_comment_to_answered(&comment.id).unwrap();
        assert_eq!(answered.status, "active");
        assert_eq!(answered.status_actor.as_deref(), Some("engine"));
        assert_eq!(answered.intent.as_deref(), Some("larger_change"));
    }

    #[test]
    fn transition_to_answered_lands_on_answered_when_intent_is_still_question() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.transition_comment_to_answering(&comment.id).unwrap();

        // No override happened — the ordinary path is unaffected.
        let answered = db.transition_comment_to_answered(&comment.id).unwrap();
        assert_eq!(answered.status, "answered");
    }

    #[test]
    fn migration_is_noop_when_no_active_comments() {
        let db = mem_db();
        let n = db
            .migrate_work_item_comments_to_pr_doc("task-empty", "pr_doc:r:b:p.md", None, 0, &cfg())
            .unwrap();
        assert_eq!(n, 0);
    }

    // --- Bucket-2 status transitions (P3b) ---

    #[test]
    fn transition_to_answering_from_active_succeeds() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        assert_eq!(comment.status, "active");

        let answering = db.transition_comment_to_answering(&comment.id).unwrap();
        assert_eq!(answering.status, "answering");
        assert_eq!(answering.status_actor.as_deref(), Some("engine"));
    }

    #[test]
    fn transition_to_answering_rejects_non_active_source() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.transition_comment_to_answering(&comment.id).unwrap();
        // Already 'answering' — a second call must not silently re-fire.
        assert!(db.transition_comment_to_answering(&comment.id).is_err());
    }

    #[test]
    fn transition_to_answered_from_answering_succeeds() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.transition_comment_to_answering(&comment.id).unwrap();

        let answered = db.transition_comment_to_answered(&comment.id).unwrap();
        assert_eq!(answered.status, "answered");
    }

    #[test]
    fn transition_to_answered_rejects_non_answering_source() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        // Still 'active' — never entered 'answering'.
        assert!(db.transition_comment_to_answered(&comment.id).is_err());
    }

    // --- Follow-up reclassification loop + bridge (P3c) ---

    fn seed_answered_comment(db: &WorkDb) -> WorkComment {
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.transition_comment_to_answering(&comment.id).unwrap();
        db.transition_comment_to_answered(&comment.id).unwrap()
    }

    #[test]
    fn transition_to_awaiting_followup_from_answered_succeeds() {
        let db = mem_db();
        let comment = seed_answered_comment(&db);

        let awaiting = db.transition_comment_to_awaiting_followup(&comment.id).unwrap();
        assert_eq!(awaiting.status, "awaiting_followup");
        assert_eq!(awaiting.status_actor.as_deref(), Some("engine"));
    }

    #[test]
    fn transition_to_awaiting_followup_rejects_non_answered_source() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        // Still 'active' — a follow-up on a comment that was never answered
        // (or is still 'answering') must be rejected, not silently queued.
        assert!(db.transition_comment_to_awaiting_followup(&comment.id).is_err());

        db.transition_comment_to_answering(&comment.id).unwrap();
        assert!(db.transition_comment_to_awaiting_followup(&comment.id).is_err());
    }

    #[test]
    fn awaiting_followup_loops_back_to_answering_for_a_question() {
        let db = mem_db();
        let comment = seed_answered_comment(&db);
        db.transition_comment_to_awaiting_followup(&comment.id).unwrap();

        let answering = db
            .transition_comment_awaiting_followup_to_answering(&comment.id)
            .unwrap();
        assert_eq!(answering.status, "answering");
    }

    #[test]
    fn awaiting_followup_to_answering_rejects_non_awaiting_source() {
        let db = mem_db();
        let comment = seed_answered_comment(&db);
        // Still 'answered' — never entered 'awaiting_followup'.
        assert!(
            db.transition_comment_awaiting_followup_to_answering(&comment.id)
                .is_err()
        );
    }

    #[test]
    fn answering_compensates_back_to_awaiting_followup_on_respawn_failure() {
        let db = mem_db();
        let comment = seed_answered_comment(&db);
        db.transition_comment_to_awaiting_followup(&comment.id).unwrap();
        db.transition_comment_awaiting_followup_to_answering(&comment.id)
            .unwrap();

        let compensated = db
            .transition_comment_answering_to_awaiting_followup(&comment.id)
            .unwrap();
        assert_eq!(compensated.status, "awaiting_followup");
    }

    #[test]
    fn awaiting_followup_bridges_to_active_for_a_directive() {
        let db = mem_db();
        let comment = seed_answered_comment(&db);
        db.transition_comment_to_awaiting_followup(&comment.id).unwrap();

        let bridged = db.transition_comment_awaiting_followup_to_active(&comment.id).unwrap();
        assert_eq!(bridged.status, "active");
        assert!(bridged.revise_task_id.is_none());
    }

    #[test]
    fn awaiting_followup_to_active_rejects_non_awaiting_source() {
        let db = mem_db();
        let comment = seed_answered_comment(&db);
        // Still 'answered' — never entered 'awaiting_followup'.
        assert!(db.transition_comment_awaiting_followup_to_active(&comment.id).is_err());
    }

    #[test]
    fn reclassify_comment_intent_overwrites_a_prior_classification() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.set_comment_intent(&c.id, "question", 0.9).unwrap();

        let reclassified = db.reclassify_comment_intent(&c.id, "directive", 0.8).unwrap();
        assert_eq!(reclassified.intent.as_deref(), Some("directive"));
        assert_eq!(reclassified.intent_confidence, Some(0.8));
        assert!(reclassified.intent_overridden_by.is_none());

        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(reloaded.intent.as_deref(), Some("directive"));
    }

    #[test]
    fn reclassify_comment_intent_clears_a_prior_manual_override() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.override_comment_intent(&c.id, "question").unwrap();
        assert_eq!(
            db.get_comment(&c.id).unwrap().unwrap().intent_overridden_by.as_deref(),
            Some("user")
        );

        // A fresh engine reclassification (a new follow-up reply) supersedes
        // the earlier manual override rather than preserving its audit trail
        // forever — the operator's new reply is the thing being classified.
        let reclassified = db.reclassify_comment_intent(&c.id, "larger_change", 0.7).unwrap();
        assert_eq!(reclassified.intent.as_deref(), Some("larger_change"));
        assert!(reclassified.intent_overridden_by.is_none());
    }

    #[test]
    fn reclassify_comment_intent_rejects_unknown_intent() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        assert!(db.reclassify_comment_intent(&c.id, "bogus", 0.5).is_err());
    }

    #[test]
    fn reclassify_comment_intent_rejects_unknown_comment() {
        let db = mem_db();
        assert!(db.reclassify_comment_intent("cmt_missing", "directive", 0.5).is_err());
    }

    // --- CommentsList read path: thread entries + answer_agent_running ---

    #[test]
    fn list_comments_with_thread_carries_answer_and_operator_followup_entries() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.transition_comment_to_answering(&c.id).unwrap();
        let run = db.create_answer_agent_run(&c.id, "work_item", "t1", "v0", 0).unwrap();
        db.create_comment_thread_entry(
            &c.id,
            boss_protocol::THREAD_ENTRY_KIND_ANSWER,
            boss_protocol::THREAD_ENTRY_AUTHOR_ENGINE,
            "The retry backoff is exponential because…",
            None,
            Some(&run.id),
        )
        .unwrap();
        db.complete_answer_agent_run(
            &run.id,
            "replied",
            Some("The retry backoff is exponential because…"),
            None,
        )
        .unwrap();
        db.transition_comment_to_answered(&c.id).unwrap();
        db.create_comment_thread_entry(
            &c.id,
            boss_protocol::THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP,
            "user:me",
            "Does that also apply to the retry-after header?",
            None,
            None,
        )
        .unwrap();

        let list = db.list_comments_with_thread("work_item", "t1", false).unwrap();
        assert_eq!(list.len(), 1);
        let wrapped = &list[0];
        assert_eq!(wrapped.comment.id, c.id);
        assert!(!wrapped.answer_agent_running);
        assert_eq!(wrapped.thread_entries.len(), 2);
        assert_eq!(wrapped.thread_entries[0].entry_kind, "answer");
        assert_eq!(
            wrapped.thread_entries[0].answer_agent_run_id.as_deref(),
            Some(run.id.as_str())
        );
        assert_eq!(wrapped.thread_entries[1].entry_kind, "operator_followup");
        assert_eq!(wrapped.thread_entries[1].author, "user:me");
    }

    #[test]
    fn list_comments_with_thread_reports_a_live_answer_agent_run() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        db.transition_comment_to_answering(&c.id).unwrap();
        db.create_answer_agent_run(&c.id, "work_item", "t1", "v0", 0).unwrap();

        let list = db.list_comments_with_thread("work_item", "t1", false).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].answer_agent_running);
        assert!(list[0].thread_entries.is_empty());
    }

    #[test]
    fn list_comments_with_thread_is_empty_for_a_plain_comment() {
        let db = mem_db();
        db.create_comment(input("t1", "alpha", "", "")).unwrap();
        let list = db.list_comments_with_thread("work_item", "t1", false).unwrap();
        assert_eq!(list.len(), 1);
        assert!(!list[0].answer_agent_running);
        assert!(list[0].thread_entries.is_empty());
    }
}
