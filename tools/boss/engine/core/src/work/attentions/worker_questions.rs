//! Questions from workers whose parent lifecycle cannot be parked.
use super::*;
use boss_protocol::{OperatorAnswerType, OperatorQuestion};

pub(crate) fn create_in_tx(
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
        OperatorAnswerType::MultipleChoice { choices } => ("multiple_choice", Some(serde_json::to_string(choices)?)),
        OperatorAnswerType::CreatePrerequisiteTask { name, brief } => {
            prompt.push_str(&format!("\n\nPrerequisite: {name}\n\n{brief}\n\nYes creates or links this prerequisite for the work that handles this answer."));
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
    conn.execute("UPDATE tasks SET autostart = 1 WHERE id = ?1", [&recovery.id])?;
    maybe_engine_unblock_dependent(pending, conn, &recovery.id, &now)?;
    let task = query_task(conn, &recovery.id).require("task", &recovery.id)?;
    reconcile_revision_execution(pending, conn, &mut ExecutionReconcileResult::default(), &task)?;
    Ok(())
}

/// Apply the original typed prerequisite to the successor, preserving the
/// source parent's merge/block/terminal lifecycle. This shares the ordinary
/// question approval's deduplication, dependency and dispatch behavior.
pub(super) fn apply_prerequisite_in_tx(
    pending: &mut PendingEvents,
    conn: &Connection,
    group: &AttentionGroup,
    members: &[Attention],
    produced_ids: &[String],
) -> Result<()> {
    let Some(execution_id) = group.grouping_key.strip_prefix("worker_question|") else {
        return Ok(());
    };
    if !members
        .iter()
        .any(|m| m.answer_state == "answered" && m.answer.as_deref() == Some("yes"))
    {
        return Ok(());
    }
    let record = conn
        .query_row(
            &format!(
                "{} WHERE execution_id = ?1",
                super::super::pr_flow::OPERATOR_QUESTION_RECORD_SELECT
            ),
            [execution_id],
            super::super::pr_flow::map_question,
        )
        .optional()?;
    let Some(record) = record else {
        return Ok(());
    };
    let OperatorAnswerType::CreatePrerequisiteTask { name, brief } = &record.question.answer_type else {
        return Ok(());
    };
    let now = now_string();
    let answered_at = chrono::DateTime::from_timestamp(now.parse::<i64>()?, 0)
        .context("answer timestamp is outside the supported range")?;
    for id in produced_ids {
        let task = query_task(conn, id).require("task", id)?;
        super::super::prerequisite_question::approve_in_tx(
            pending,
            conn,
            &task,
            &record.question,
            (name, brief),
            answered_at,
            &now,
        )?;
        if task.kind == TaskKind::Revision {
            let task = query_task(conn, id).require("task", id)?;
            reconcile_revision_execution(pending, conn, &mut ExecutionReconcileResult::default(), &task)?;
        }
    }
    Ok(())
}
