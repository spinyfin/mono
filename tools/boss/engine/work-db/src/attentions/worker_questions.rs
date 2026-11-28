//! Questions from workers whose parent lifecycle cannot be parked.
use super::*;
use boss_protocol::{
    FileCountBypassSpec, OperatorAnswerType, OperatorQuestion, OperatorQuestionRecord, OperatorQuestionView,
};

/// Whether a successor continues the PR covered by the original question.
pub(super) enum QuestionSuccessor {
    Revision,
    NoPr,
    FinishedPr,
}

pub fn create_in_tx(
    conn: &Connection,
    task: &Task,
    execution_id: &str,
    question: &OperatorQuestion,
) -> Result<(Attention, AttentionGroup)> {
    let mut prompt = format!(
        "Question from {} {}\n\n{}\n\n{}",
        task.short_label(),
        task.name,
        question.text,
        question.explanation
    );
    let (question_type, choices) = match &question.answer_type {
        OperatorAnswerType::YesNo => ("yes_no", None),
        OperatorAnswerType::MultipleChoice { choices, .. } => {
            ("multiple_choice", Some(serde_json::to_string(choices)?))
        }
        OperatorAnswerType::CreatePrerequisiteTask { name, brief, .. } => {
            let (yes, no) = question
                .answer_type
                .option_labels()
                .map_or(("Yes", "No"), |labels| (labels.yes.as_str(), labels.no.as_str()));
            prompt.push_str(&format!("\n\nPrerequisite: {name}\n\n{brief}\n\n{yes} creates or links this prerequisite for the work that handles this answer."));
            if let Some(spec) = question.answer_type.file_count_bypass() {
                prompt.push_str(&format!(
                    " {no} authorizes a change/file-count exception for this PR only, up to {} files (limit {}), and the work continues in one PR.",
                    spec.files, spec.limit
                ));
            }
            (
                "yes_no",
                question
                    .answer_type
                    .option_labels()
                    .map(serde_json::to_string)
                    .transpose()?,
            )
        }
        OperatorAnswerType::LinkPrerequisiteTask {
            name,
            pr_url,
            work_item_id,
            ..
        } => {
            let mut existing = format!("\n\nExisting prerequisite: {name}");
            if let Some(pr_url) = pr_url.as_deref().filter(|url| !url.is_empty()) {
                existing.push_str(&format!("\n\nPull request: {pr_url}"));
            }
            if !work_item_id.is_empty() {
                existing.push_str(&format!("\n\nTask id: {work_item_id}"));
            }
            existing
                .push_str("\n\nYes links this task as a prerequisite and waits for it. It does not create a new task.");
            prompt.push_str(&existing);
            ("yes_no", None)
        }
    };
    create_attention_in_tx(
        conn,
        CreateAttentionInput::builder()
            .kind("question")
            .group_key(format!("worker_question|{execution_id}"))
            .maybe_association_project_id(task.project_id.clone())
            .maybe_association_task_id(task.project_id.is_none().then(|| task.id.clone()))
            .source_task_id(&task.id)
            .source_kind("task_transcript")
            .question_type(question_type)
            .maybe_choice_options(choices)
            .prompt_text(prompt)
            .build(),
    )
}

/// Insert the answer consumer immediately before the blocked asking revision.
/// The consumer inherits its prerequisites, while later revisions retain their
/// existing order behind the source. The source's domain blocker is untouched.
/// Creation uses autostart=false so no execution observes the temporary tail gate.
pub(super) fn order_recovery_in_tx(
    pending: &mut PendingEvents,
    conn: &Connection,
    source: &Task,
    recovery: &Task,
    actor: &str,
    provenance: Option<&str>,
) -> Result<()> {
    let now = now_string();
    let prefix = boss_protocol::CREATED_VIA_CONFLICT_RECOVERY_PREFIX;
    if source.created_via.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX) || source.created_via.starts_with(prefix) {
        let source_id = source.created_via.strip_prefix(prefix).unwrap_or(&source.id);
        conn.execute(
            "UPDATE tasks SET created_via = ?2 WHERE id = ?1",
            params![recovery.id, format!("{prefix}{source_id}")],
        )?;
    }
    for edge in deps::prerequisites_of(conn, &recovery.id, Some(RELATION_BLOCKS))? {
        deps::delete_edge(conn, &recovery.id, &edge.prerequisite_id, RELATION_BLOCKS)?;
    }
    for edge in deps::prerequisites_of(conn, &source.id, Some(RELATION_BLOCKS))? {
        deps::insert_edge(conn, &recovery.id, &edge.prerequisite_id, RELATION_BLOCKS, &now)?;
    }
    deps::insert_edge(conn, &source.id, &recovery.id, RELATION_BLOCKS, &now)?;
    crate::task_status_audit::execute(
        conn,
        &crate::task_status_audit::compose_basis("order_recovery_in_tx", provenance),
        actor,
        "UPDATE tasks SET autostart = 1 WHERE id = ?1",
        [&recovery.id],
    )?;
    maybe_engine_unblock_dependent(pending, conn, &recovery.id, &now)?;
    let task = query_task(conn, &recovery.id).require("task", &recovery.id)?;
    reconcile_revision_execution(pending, conn, &mut ExecutionReconcileResult::default(), &task)?;
    Ok(())
}

/// Stored question for a `worker_question|` group, when the asking run
/// recorded one.
pub(super) fn stored_question(conn: &Connection, group: &AttentionGroup) -> Result<Option<OperatorQuestionRecord>> {
    let Some(execution_id) = group.grouping_key.strip_prefix("worker_question|") else {
        return Ok(None);
    };
    conn.query_row(
        &format!(
            "{} WHERE execution_id = ?1",
            super::super::pr_flow::OPERATOR_QUESTION_RECORD_SELECT
        ),
        [execution_id],
        super::super::pr_flow::map_question,
    )
    .optional()
    .map_err(Into::into)
}

/// Optional per-option addendum stored on a `worker_question|` group's question.
pub(super) fn choice_addendum(conn: &Connection, group: &AttentionGroup, choice: &str) -> Result<Option<String>> {
    let Some(record) = stored_question(conn, group)? else {
        return Ok(None);
    };
    Ok(record
        .question
        .answer_type
        .choice_option(choice)
        .and_then(|opt| opt.addendum.clone()))
}

/// When the repo owner actions a `worker_question|` group whose answered choice
/// carries an `archive` or `park` disposition, execute it on the source item in
/// `tx` instead of minting a follow-up revision or chore. Non-owner actors
/// return `Ok(None)` so only the repo owner can archive or park.
pub(super) fn apply_owner_disposition_in_tx(
    db: &WorkDb,
    pending: &mut PendingEvents,
    tx: &rusqlite::Transaction<'_>,
    group: &AttentionGroup,
    members: &[Attention],
    actor: &str,
    provenance: Option<&str>,
) -> Result<Option<(String, String, Vec<String>)>> {
    if !group.grouping_key.starts_with("worker_question|") {
        return Ok(None);
    }
    let Some(choice) = members
        .iter()
        .find(|m| m.answer_state == "answered")
        .and_then(|m| m.answer.as_deref())
    else {
        return Ok(None);
    };
    let Some(record) = stored_question(tx, group)? else {
        return Ok(None);
    };
    let Some(disposition) = crate::operator_disposition::owner_choice_disposition(actor, &record.question, choice)
    else {
        return Ok(None);
    };
    if disposition == boss_protocol::ChoiceDisposition::Resume {
        return Ok(None);
    }
    let Some(source_id) = group.source_task_id.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let Some(task) = query_task(tx, source_id)?.filter(|t| t.deleted_at.is_none()) else {
        return Ok(None);
    };
    let reference = serde_json::json!({
        "task_id": task.id,
        "short_id": task.short_id,
        "kind": task.kind,
    })
    .to_string();
    match disposition {
        boss_protocol::ChoiceDisposition::Archive => {
            db.update_task_in_tx(
                tx,
                pending,
                &task.id,
                WorkItemPatch::builder()
                    .status("archived")
                    .archived_reason("the owner chose to archive this item in an operator question")
                    .build(),
                crate::task_status_audit::WriteAttribution {
                    actor,
                    provenance: Some(provenance.unwrap_or("operator_question_archive")),
                },
            )?;
            Ok(Some(("archived".to_owned(), reference, vec![task.id])))
        }
        boss_protocol::ChoiceDisposition::Park => {
            let now = now_string();
            let answered_at = chrono::DateTime::from_timestamp(now.parse::<i64>()?, 0)
                .context("answer timestamp is outside the supported range")?;
            let timestamp = answered_at.format("%Y-%m-%d %H:%M UTC").to_string();
            let block = super::super::proposals::choice_block(&record.question, choice, &timestamp);
            // Audited update path: a rejected description guard surfaces as an
            // error instead of silently dropping the recorded answer.
            db.update_task_in_tx(
                tx,
                pending,
                &task.id,
                WorkItemPatch::builder()
                    .description(format!("{}{block}", task.description))
                    .build(),
                crate::task_status_audit::WriteAttribution {
                    actor,
                    provenance: Some(provenance.unwrap_or("operator_question_park")),
                },
            )?;
            let touched = crate::operator_disposition::park_chain_root_in_tx(tx, pending, &task, actor, &now)?;
            Ok(Some(("parked".to_owned(), reference, touched)))
        }
        // Engine-reserved dispositions only appear on engine-built cards
        // answered through their own handler, never on a worker question.
        boss_protocol::ChoiceDisposition::Resume
        | boss_protocol::ChoiceDisposition::ForceMerge
        | boss_protocol::ChoiceDisposition::RewriteOntoBase => Ok(None),
    }
}

/// File-count bypass spec when this group was answered No on a split-vs-bypass
/// question. The successor that continues the same PR needs the scoped
/// authorization block, not the generic No disclaimer.
pub(super) fn file_count_bypass_no_spec(
    conn: &Connection,
    group: &AttentionGroup,
    members: &[Attention],
) -> Result<Option<(OperatorQuestionView, FileCountBypassSpec)>> {
    if !members
        .iter()
        .any(|m| m.answer_state == "answered" && m.answer.as_deref() == Some("no"))
    {
        return Ok(None);
    }
    let Some(record) = stored_question(conn, group)? else {
        return Ok(None);
    };
    Ok(record
        .question
        .answer_type
        .file_count_bypass()
        .map(|spec| (record.question, spec)))
}

/// Apply a typed answer to the successor: Yes creates or links a prerequisite;
/// file-count No records authorization only for a revision of the asking PR.
/// Fresh work retains the answer as history and requires fresh approval.
/// Preserve the source parent's merge/block/terminal lifecycle.
pub(super) fn apply_typed_answer_in_tx(
    pending: &mut PendingEvents,
    conn: &Connection,
    group: &AttentionGroup,
    members: &[Attention],
    produced_ids: &[String],
    successor: QuestionSuccessor,
    attribution: crate::task_status_audit::WriteAttribution<'_>,
) -> Result<()> {
    let answered_yes = members
        .iter()
        .any(|m| m.answer_state == "answered" && m.answer.as_deref() == Some("yes"));
    if answered_yes {
        let Some(record) = stored_question(conn, group)? else {
            return Ok(());
        };
        let now = now_string();
        let answered_at = chrono::DateTime::from_timestamp(now.parse::<i64>()?, 0)
            .context("answer timestamp is outside the supported range")?;
        for id in produced_ids {
            let task = query_task(conn, id).require("task", id)?;
            match &record.question.answer_type {
                OperatorAnswerType::CreatePrerequisiteTask { name, brief, .. } => {
                    super::super::prerequisite_question::approve_in_tx(
                        pending,
                        conn,
                        &task,
                        super::super::prerequisite_question::Approval {
                            question: &record.question,
                            proposed: (name, brief),
                            answered_at,
                            now: &now,
                        },
                        attribution,
                    )?;
                }
                OperatorAnswerType::LinkPrerequisiteTask { work_item_id, .. } => {
                    super::super::prerequisite_question::approve_existing_in_tx(
                        pending,
                        conn,
                        &task,
                        super::super::prerequisite_question::ExistingApproval {
                            question: &record.question,
                            existing_id: work_item_id,
                            answered_at,
                            now: &now,
                        },
                        attribution,
                    )?;
                }
                _ => return Ok(()),
            }
            if task.kind == TaskKind::Revision {
                let task = query_task(conn, id).require("task", id)?;
                reconcile_revision_execution(pending, conn, &mut ExecutionReconcileResult::default(), &task)?;
            }
        }
        return Ok(());
    }
    let Some((question, spec)) = file_count_bypass_no_spec(conn, group, members)? else {
        return Ok(());
    };
    let now = now_string();
    let answered_at = chrono::DateTime::from_timestamp(now.parse::<i64>()?, 0)
        .context("answer timestamp is outside the supported range")?;
    let timestamp = answered_at.format("%Y-%m-%d %H:%M UTC").to_string();
    let block = match successor {
        QuestionSuccessor::Revision => {
            super::super::pr_flow::file_count_bypass_authorization_block(&question, spec, &timestamp)
        }
        QuestionSuccessor::FinishedPr | QuestionSuccessor::NoPr => format!(
            "\n\n## Recorded answer ({timestamp})\n\nThe operator answered {} to: {}\n\nThis successor does not continue an open PR from the original work. This answer is context only and grants no authorization for a new PR; exceeding the file-count limit requires fresh approval.\n",
            question
                .answer_type
                .option_labels()
                .map_or("No", |labels| labels.no.as_str()),
            question.text
        ),
    };
    for id in produced_ids {
        let task = query_task(conn, id).require("task", id)?;
        let description = format!("{}{block}", task.description);
        super::super::description_guard::validate_description_update(&task.description, &description, false)?;
        conn.execute(
            "UPDATE tasks SET description = ?2, updated_at = ?3 WHERE id = ?1",
            params![task.id, description, now],
        )?;
    }
    Ok(())
}
