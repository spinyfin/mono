//! `architectural_mismatch` on a merge-conflict attempt: instead of a dead end,
//! the repo owner is asked to choose between rewriting onto the new base and
//! forcing a merge.
//!
//! Lifecycle:
//!
//! 1. The conflict worker runs `mark-failed --reason architectural_mismatch`
//!    with evidence. [`WorkDb::record_conflict_architectural_mismatch`]
//!    validates it and stores it on the *live* attempt. The attempt is not
//!    marked failed: nothing is final until the repo owner decides.
//! 2. The worker declares itself blocked. Applying that declaration turns the
//!    stored evidence into an ordinary multiple-choice operator question on the
//!    conflict revision ([`take_pending_question`] / [`link_question`]) and the
//!    revision parks awaiting the answer, exactly like any other worker question.
//! 3. [`answer_in_tx`] runs inside the answer transaction. *Force a merge*
//!    re-dispatches the same revision with an in-place instruction; *Rewrite*
//!    archives it and mints a port-sized revision on the same PR. Either way the
//!    attempt's `architectural_mismatch` exit is closed.

use super::*;
use rusqlite::{Transaction, TransactionBehavior};

use boss_protocol::{
    ArchitecturalMismatchEvidence, CONFLICT_MISMATCH_CHOICE_FORCE_MERGE, CONFLICT_MISMATCH_CHOICE_REWRITE,
    OperatorAnswerType, OperatorQuestion, OperatorQuestionView,
};

pub(super) const MIGRATION_ID: &str = "20261102_1500_conflict_architectural_mismatch";

/// Persist the human's operator-forced admission on an execution an answer
/// created, so a writer wait, pool cap, preflight, or drain race cannot drop
/// it. Independent of [`MIGRATION_ID`]: that migration may already have
/// applied on databases this binary upgrades.
pub(super) const OPERATOR_FORCED_ADMISSION_MIGRATION_ID: &str = "20261103_1800_operator_forced_admission";

const TERMINAL_EXECUTION_STATUSES: &str = "'completed', 'failed', 'cancelled', 'orphaned', 'abandoned'";

/// Archive mechanism recorded on the conflict revision a *Rewrite* answer
/// replaces with a port revision.
pub const ARCHIVE_MECHANISM_CONFLICT_REWRITE_ANSWER: &str = "conflict_rewrite_answer";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    for (table, column, ddl) in [
        (
            "conflict_resolutions",
            "mismatch_evidence_json",
            "ALTER TABLE conflict_resolutions ADD COLUMN mismatch_evidence_json TEXT",
        ),
        (
            "conflict_resolutions",
            "mismatch_exit_barred",
            "ALTER TABLE conflict_resolutions ADD COLUMN mismatch_exit_barred INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "operator_questions",
            "conflict_resolution_id",
            "ALTER TABLE operator_questions ADD COLUMN conflict_resolution_id TEXT",
        ),
    ] {
        if !super::schema_init::table_has_column(conn, table, column)? {
            conn.execute_batch(ddl)?;
        }
    }
    Ok(())
}

/// Persist operator-forced admission on `work_executions` and copy it onto a
/// later non-terminal execution of the same work item (a restart). Consume it
/// when the row becomes terminal so a cancelled wait does not keep forcing.
pub(super) fn migrate_operator_forced_admission(conn: &Connection) -> Result<()> {
    if !super::schema_init::table_has_column(conn, "work_executions", "operator_forced_admission")? {
        conn.execute_batch(
            "ALTER TABLE work_executions ADD COLUMN operator_forced_admission INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS work_executions_operator_forced_idx
           ON work_executions(work_item_id) WHERE operator_forced_admission = 1;
         CREATE TRIGGER IF NOT EXISTS work_executions_copy_operator_forced_admission
         AFTER INSERT ON work_executions
         BEGIN
           UPDATE work_executions SET operator_forced_admission = 1 WHERE id = NEW.id
             AND EXISTS (
               SELECT 1 FROM work_executions AS prev
               WHERE prev.work_item_id = NEW.work_item_id
                 AND prev.id != NEW.id
                 AND prev.operator_forced_admission = 1
                 AND prev.status NOT IN ('completed', 'failed', 'cancelled', 'orphaned', 'abandoned')
             );
         END;
         CREATE TRIGGER IF NOT EXISTS work_executions_clear_operator_forced_on_terminal
         AFTER UPDATE OF status ON work_executions
         WHEN NEW.operator_forced_admission = 1
          AND NEW.status IN ('completed', 'failed', 'cancelled', 'orphaned', 'abandoned')
         BEGIN
           UPDATE work_executions SET operator_forced_admission = 0 WHERE id = NEW.id;
         END;",
    )?;
    Ok(())
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Record a worker's `architectural_mismatch` stop on its live attempt.
    ///
    /// Errors (with text the worker sees) when the evidence does not clear the
    /// bar, when the repo owner already closed this exit for the attempt, or when
    /// the attempt has no conflict revision to ask on behalf of. `Ok(None)` when
    /// the attempt is unknown or already terminal. The attempt stays live.
    pub fn record_conflict_architectural_mismatch(
        &self,
        attempt_id: &str,
        evidence: Option<&ArchitecturalMismatchEvidence>,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(attempt) = query_conflict_resolution(&tx, attempt_id)? else {
            return Ok(None);
        };
        if !matches!(attempt.status.as_str(), "pending" | "running") {
            return Ok(None);
        }
        if attempt.mismatch_exit_barred {
            bail!(
                "architectural_mismatch is no longer an allowed exit for conflict resolution {attempt_id}: the repo \
                 owner has decided how to proceed. Resolve the conflicts against the refactored code, porting the \
                 change's intent onto the new layout, and push. If the work is bigger than this attempt's sizing, \
                 keep going and emit the effort-escalation marker"
            );
        }
        let evidence = evidence.ok_or_else(|| {
            anyhow::anyhow!(
                "architectural_mismatch requires evidence: --basis, --conflicting-commits, --main-changes, \
                 --attempted-port, --contradiction and --recommendation. It is only for a real behavioural conflict \
                 after a genuine attempt to port the change onto main's new layout. Moved or deleted files, split or \
                 renamed modules, newly private helpers and every commit conflicting are normal conflict work"
            )
        })?;
        evidence.validate().map_err(anyhow::Error::msg)?;
        if attempt.revision_task_id.is_none() {
            bail!(
                "conflict resolution {attempt_id} has no conflict revision to ask the repo owner through; use \
                 `product_decision_required` to escalate it"
            );
        }
        tx.execute(
            "UPDATE conflict_resolutions SET mismatch_evidence_json = ?2 WHERE id = ?1",
            params![attempt_id, serde_json::to_string(evidence)?],
        )?;
        let updated = query_conflict_resolution(&tx, attempt_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Record that the human's answer created this work and it must dispatch
    /// like `bossctl agents launch`: through the pause and the focus set.
    /// Applied to the latest non-terminal execution of `work_item_id`.
    pub fn mark_operator_forced_admission(&self, work_item_id: &str) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        mark_operator_forced_admission_in_tx(&tx, work_item_id)?;
        tx.commit()?;
        Ok(())
    }

    /// Consume operator-forced admission after the execution is admitted.
    /// Terminal rows are cleared by the schema trigger as well.
    pub fn clear_operator_forced_admission(&self, execution_id: &str) -> Result<()> {
        self.connect()?.execute(
            "UPDATE work_executions SET operator_forced_admission = 0 WHERE id = ?1",
            [execution_id],
        )?;
        Ok(())
    }

    /// Ready executions that still carry operator-forced admission, so the
    /// drain can admit them through the pause and the focus set.
    pub fn operator_forced_ready_execution_ids(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM work_executions
             WHERE operator_forced_admission = 1 AND status = 'ready'",
        )?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
        Ok(ids)
    }

    /// Whether `execution_id` currently carries operator-forced admission.
    pub fn execution_has_operator_forced_admission(&self, execution_id: &str) -> Result<bool> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT operator_forced_admission FROM work_executions WHERE id = ?1",
                [execution_id],
                |row| {
                    let flag: i64 = row.get(0)?;
                    Ok(flag != 0)
                },
            )
            .optional()?
            .unwrap_or(false))
    }
}

/// Whether a conflict revision is parked on an open architectural-mismatch
/// question. Such a revision is still the live fix vehicle for its attempt.
pub(super) fn conflict_revision_awaits_mismatch_answer(conn: &Connection, revision_task_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM operator_questions
         WHERE work_item_id = ?1 AND status = 'open' AND conflict_resolution_id IS NOT NULL)",
        [revision_task_id],
        |row| row.get(0),
    )?)
}

fn mark_operator_forced_admission_in_tx(tx: &Transaction<'_>, work_item_id: &str) -> Result<()> {
    let id: Option<String> = tx
        .query_row(
            &format!(
                "SELECT id FROM work_executions
                 WHERE work_item_id = ?1
                   AND status NOT IN ({TERMINAL_EXECUTION_STATUSES})
                 ORDER BY created_at DESC, id DESC LIMIT 1"
            ),
            [work_item_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = id {
        tx.execute(
            "UPDATE work_executions SET operator_forced_admission = 1 WHERE id = ?1",
            [&id],
        )?;
    }
    Ok(())
}

/// The question to ask for the attempt this execution's conflict revision
/// belongs to, when the worker has recorded a mismatch and not yet been
/// asked about it. The stored evidence stays until the question is answered,
/// so a later blocked declaration can re-raise it if the first question was
/// withdrawn.
pub(super) fn take_pending_question(
    tx: &Transaction<'_>,
    execution_id: &str,
) -> Result<Option<(String, OperatorQuestion)>> {
    let row: Option<(String, i64, String)> = tx
        .query_row(
            "SELECT cr.id, cr.pr_number, cr.mismatch_evidence_json
             FROM conflict_resolutions cr
             JOIN work_executions we ON we.work_item_id = cr.revision_task_id
             WHERE we.id = ?1 AND cr.status IN ('pending', 'running')
               AND cr.mismatch_evidence_json IS NOT NULL",
            [execution_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((attempt_id, pr_number, evidence_json)) = row else {
        return Ok(None);
    };
    let evidence: ArchitecturalMismatchEvidence = serde_json::from_str(&evidence_json)?;
    Ok(Some((attempt_id, question_for(pr_number, &evidence))))
}

/// Whether this execution already has an open architectural-mismatch question.
/// A later blocked declaration without a question of its own keeps that
/// question instead of withdrawing it.
pub(super) fn execution_has_open_mismatch_question(conn: &Connection, execution_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM operator_questions
         WHERE execution_id = ?1 AND status = 'open' AND conflict_resolution_id IS NOT NULL)",
        [execution_id],
        |row| row.get(0),
    )?)
}

/// Tie the question inserted for `proposal_id` to its attempt.
pub(super) fn link_question(
    tx: &Transaction<'_>,
    execution_id: &str,
    proposal_id: &str,
    attempt_id: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE operator_questions SET conflict_resolution_id = ?3 WHERE execution_id = ?1 AND proposal_id = ?2",
        params![execution_id, proposal_id, attempt_id],
    )?;
    Ok(())
}

/// The attempt a question was raised for, if it is a mismatch question.
pub(super) fn attempt_for_question(conn: &Connection, question_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT conflict_resolution_id FROM operator_questions WHERE id = ?1",
            [question_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

fn question_for(pr_number: i64, evidence: &ArchitecturalMismatchEvidence) -> OperatorQuestion {
    let choices = vec![
        CONFLICT_MISMATCH_CHOICE_REWRITE.to_owned(),
        CONFLICT_MISMATCH_CHOICE_FORCE_MERGE.to_owned(),
    ];
    let explanation = format!(
        "Context:\n\
A worker tried to resolve the merge conflict on PR #{pr_number} and stopped after studying main's new layout and attempting the port. Its finding: {} ({}).\n\
{CONFLICT_MISMATCH_CHOICE_REWRITE}: a new revision on the same PR ports the change onto current main, or rebuilds it there. It keeps the PR's behaviour and every earlier review fix, puts code where it now lives, and is sized for a port rather than a conflict fix.\n\
{CONFLICT_MISMATCH_CHOICE_FORCE_MERGE}: the conflict fix is run again with an instruction to resolve the conflicts in place against the refactored code, and stopping on this ground is no longer allowed.\n\
Recommendation:\n\
The worker recommends: {}\n\
Details:\n\
Commits that conflict: {}\n\
Changed on main: {}\n\
What was attempted: {}",
        evidence.contradiction.trim(),
        evidence.basis.describe(),
        evidence.recommendation.trim(),
        evidence.conflicting_commits.trim(),
        evidence.main_changes.trim(),
        evidence.attempted_port.trim(),
    );
    OperatorQuestion {
        text: format!("Rewrite PR #{pr_number} onto the new base, or force a merge?"),
        answer_type: OperatorAnswerType::MultipleChoice { choices },
        explanation,
    }
}

/// What an answer did, for the RPC layer.
#[derive(Debug, Default)]
pub(super) struct AnswerEffect {
    /// Tasks the answer created, changed or retired, for board invalidation.
    pub touched: Vec<String>,
    /// The task whose ready execution must dispatch now, as an explicit launch.
    pub dispatch: Option<String>,
    /// `true` when the answer minted a ready execution directly.
    pub minted_execution: bool,
}

/// What the repo owner answered, and the context to apply it in.
pub(super) struct Answer<'a> {
    pub attempt_id: &'a str,
    pub choice: &'a str,
    pub pr_checker: &'a dyn PrStateChecker,
    pub answered_at: chrono::DateTime<chrono::Utc>,
    pub now: &'a str,
}

/// Apply the repo owner's choice for `answer.attempt_id`, inside the answer
/// transaction. `asker` is the parked conflict revision.
pub(super) fn answer_in_tx(
    pending: &mut PendingEvents,
    tx: &Transaction<'_>,
    asker: &Task,
    question: &OperatorQuestionView,
    answer: &Answer<'_>,
) -> Result<AnswerEffect> {
    let Answer {
        attempt_id,
        choice,
        pr_checker,
        answered_at,
        now,
    } = *answer;
    let attempt = query_conflict_resolution(tx, attempt_id).require("conflict resolution", attempt_id)?;
    let timestamp = answered_at.format("%Y-%m-%d %H:%M UTC").to_string();
    tx.execute(
        "UPDATE conflict_resolutions SET mismatch_exit_barred = 1, mismatch_evidence_json = NULL WHERE id = ?1",
        [attempt_id],
    )?;
    match choice {
        CONFLICT_MISMATCH_CHOICE_FORCE_MERGE => {
            let block = force_merge_block(question, &timestamp);
            let minted_execution = super::pr_flow::restart_after_answer(pending, tx, asker, question, &block, now)?;
            mark_operator_forced_admission_in_tx(tx, &asker.id)?;
            Ok(AnswerEffect {
                touched: vec![asker.id.clone()],
                dispatch: Some(asker.id.clone()),
                minted_execution,
            })
        }
        CONFLICT_MISMATCH_CHOICE_REWRITE => {
            let Some(parent_id) = asker.parent_task_id.clone() else {
                bail!("conflict revision {} has no parent PR to rewrite", asker.id);
            };
            // Retire the parked fix first so the port revision is sequenced
            // against the rest of the chain, not against a revision about to
            // disappear. Review-finding revisions queued on the PR wait
            // behind the port.
            archive_revision_task(
                tx,
                &asker.id,
                now,
                ArchiveProvenance::new(
                    ARCHIVE_MECHANISM_CONFLICT_REWRITE_ANSWER,
                    "replaced by a port revision after the repo owner chose to rewrite onto the new base",
                ),
            )?;
            let port = assert_parent_revisable_and_insert_as_actor(
                pending,
                tx,
                CreateRevisionInput::builder()
                    .parent_task_id(parent_id)
                    .name(format!("Port PR #{} onto the new base", attempt.pr_number))
                    .description(port_brief(&attempt, question, &timestamp))
                    // Same provenance as the fix it replaces: the PR is still
                    // conflicting, this revision is what resolves it, and it
                    // queues ahead of the PR's other revisions the same way.
                    .created_via(format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}{attempt_id}"))
                    // Allocate enough capacity to port the change onto the new base.
                    .effort_level(EffortLevel::Large)
                    .reasoning(ReasoningMode::Standard)
                    .build(),
                pr_checker,
                "human",
            )?;
            tx.execute(
                "UPDATE conflict_resolutions SET revision_task_id = ?2 WHERE id = ?1",
                params![attempt_id, port.id],
            )?;
            mark_operator_forced_admission_in_tx(tx, &port.id)?;
            Ok(AnswerEffect {
                touched: vec![asker.id.clone(), port.id.clone()],
                dispatch: Some(port.id),
                minted_execution: false,
            })
        }
        other => bail!("`{other}` is not an offered answer for this question"),
    }
}

fn force_merge_block(question: &OperatorQuestionView, timestamp: &str) -> String {
    format!(
        "\n\n---\n\n## Recorded answer ({timestamp})\n\n\
- **Question the previous worker asked:** {}\n\
- **The repo owner chose:** {CONFLICT_MISMATCH_CHOICE_FORCE_MERGE}\n\
- **The worker's explanation:** {}\n\n\
Resolve the conflicts in place against the refactored code on the base branch: study where the code now lives and apply this PR's change there, preserving the PR's behaviour and every earlier review fix. `architectural_mismatch` is no longer an allowed exit for this attempt, and the engine will refuse it. If the work is bigger than this attempt's sizing, keep going and emit the effort-escalation marker. This decides exactly the question above and nothing broader: it does not authorize bypassing any check.\n",
        question.text, question.explanation
    )
}

fn port_brief(attempt: &ConflictResolution, question: &OperatorQuestionView, timestamp: &str) -> String {
    format!(
        "Port PR #{pr} onto the current `{base}` branch.\n\n\
## Why this revision exists\n\n\
`{base}` moved under this PR and the conflict cannot be resolved textually. The repo owner chose to rewrite onto the new base ({timestamp}).\n\n\
## What to do\n\n\
- Study how `{base}` is laid out now: where the code this PR changes lives, which modules, crates and files moved, were split, renamed or became private, and what replaced anything that was deleted.\n\
- Port this PR's change onto that layout, or rebuild it there, putting code where it now lives. Rebasing and mapping the moved code commit by commit is a good way to keep the history reviewable. Push the result to the existing PR branch.\n\
- Preserve the PR's behaviour and every earlier review fix already on the PR. Read the PR's review history and make sure each fix survives the port.\n\
- Moved or deleted files, split or renamed modules and crates, newly private helpers, migrations that must move to another crate's sequence, and every commit conflicting are all part of this work. They are never a reason to stop.\n\
- This is a port, so it is sized as one. Do not stop on the ground of architectural mismatch: the engine refuses that exit for this attempt. If the work turns out to be bigger than its sizing, keep going and emit the effort-escalation marker.\n\
- Verify GitHub reports the PR mergeable after you push.\n\n\
## What the previous worker found\n\n\
{}\n",
        question.explanation,
        pr = attempt.pr_number,
        base = attempt.base_branch,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_question_offers_exactly_two_answers_and_carries_the_evidence() {
        let evidence = ArchitecturalMismatchEvidence {
            basis: boss_protocol::MismatchBasis::SubsystemDeleted,
            conflicting_commits: "abc123, def456".into(),
            main_changes: "the queue subsystem was deleted by the queue removal PR".into(),
            attempted_port: "looked for a replacement and found none".into(),
            contradiction: "Main deleted the queue, so the PR's new queue priority has nowhere to live.".into(),
            recommendation: "Rewrite around the dispatcher.".into(),
        };
        let question = question_for(3405, &evidence);
        let OperatorAnswerType::MultipleChoice { choices } = &question.answer_type else {
            panic!("expected a multiple-choice question");
        };
        assert_eq!(
            choices,
            &[
                CONFLICT_MISMATCH_CHOICE_REWRITE.to_owned(),
                CONFLICT_MISMATCH_CHOICE_FORCE_MERGE.to_owned()
            ]
        );
        for needle in [
            "abc123, def456",
            "queue removal PR",
            "found none",
            "Rewrite around the dispatcher.",
        ] {
            assert!(question.explanation.contains(needle), "{needle}");
        }
        crate::prose_vocabulary::assert_no_forbidden_prose("mismatch question", &question.explanation);
    }
}
