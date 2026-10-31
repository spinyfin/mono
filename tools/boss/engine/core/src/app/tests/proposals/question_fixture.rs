use super::*;

pub(super) async fn parked_task_with_question_kind(
    payload: Value,
    revision: bool,
) -> (Arc<ServerState>, tempfile::TempDir, String, String) {
    parked_task_with_question_route(payload, revision, None).await
}

pub(super) async fn parked_task_with_question_route(
    payload: Value,
    revision: bool,
    project_attention_route: Option<bool>,
) -> (Arc<ServerState>, tempfile::TempDir, String, String) {
    let (state, dir, execution, mut task) = live_chore_execution();
    prepare_workspace_teardown(&state, &execution);
    let project = project_attention_route.map(|_| {
        let product: String = state
            .work_db
            .connect()
            .unwrap()
            .query_row("SELECT product_id FROM tasks WHERE id = ?1", [&task], |row| row.get(0))
            .unwrap();
        let project = state
            .work_db
            .create_project(
                boss_protocol::CreateProjectInput::builder()
                    .product_id(product)
                    .name("Revision project")
                    .no_design_task(true)
                    .build(),
            )
            .unwrap();
        state
            .work_db
            .update_work_item(
                &task,
                boss_protocol::WorkItemPatch {
                    project_id: Some(project.id.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        project
    });
    if revision {
        state
            .work_db
            .update_work_item(
                &task,
                boss_protocol::WorkItemPatch {
                    status: Some("in_review".into()),
                    pr_url: Some("https://github.com/example/repo/pull/1".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let child = if project_attention_route == Some(true) {
            let (attention, _) = state
                .work_db
                .create_attention(
                    boss_protocol::CreateAttentionInput::builder()
                        .kind("question")
                        .association_project_id(&project.as_ref().unwrap().id)
                        .source_kind("design_doc")
                        .source_task_id(&task)
                        .source_doc_path("docs/design.md")
                        .question_type("prompt")
                        .prompt_text("Which layout?")
                        .build(),
                )
                .unwrap();
            let group = state
                .work_db
                .answer_attention(&attention.id, Some("Two tables".into()), false, false)
                .unwrap();
            let result = state
                .work_db
                .action_attention_group(
                    &group.id,
                    false,
                    &Default::default(),
                    &crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open),
                )
                .unwrap();
            question_task(&state, &result.produced_work_item_ids[0])
        } else {
            state
                .work_db
                .create_revision(
                    boss_protocol::CreateRevisionInput::builder()
                        .parent_task_id(&task)
                        .description("Original brief")
                        .autostart(false)
                        .build(),
                    &crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open),
                )
                .unwrap()
        };
        task = child.id;
        state
            .work_db
            .connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET work_item_id = ?2 WHERE id = ?1",
                rusqlite::params![execution, task],
            )
            .unwrap();
    }
    {
        let conn = state.work_db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET kind = ?2, description = 'Original brief' WHERE id = ?1",
            rusqlite::params![task, if revision { "revision" } else { "project_task" }],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET kind = ?2 WHERE id = ?1",
            rusqlite::params![
                execution,
                if revision {
                    "revision_implementation"
                } else {
                    "task_implementation"
                }
            ],
        )
        .unwrap();
    }
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&execution, ProposalKind::RunDone, payload),
        )
        .await,
    );
    (state, dir, execution, task)
}
