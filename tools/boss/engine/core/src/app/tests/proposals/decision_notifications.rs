use super::*;

#[tokio::test]
async fn open_question_is_hidden_until_task_is_awaiting_an_answer() {
    let (state, _dir, _execution, task_id) = parked_question_task().await;
    let (tx, _rx) = tokio::sync::oneshot::channel::<()>();
    let sink = Arc::new(SessionSink::new(tx, crate::app::frontend_metrics::test_registry()));
    state.topic_broker.register_session("decisions", sink.clone()).await;
    state
        .topic_broker
        .subscribe("decisions", &["work.decisions".into()])
        .await;
    for (status, reason, expected) in [
        ("active", None, 0),
        ("done", None, 0),
        ("blocked", Some("dependencies"), 0),
        ("blocked", Some("awaiting_operator_answer"), 1),
    ] {
        let conn = state.work_db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = ?1, blocked_reason = ?2 WHERE id = ?3",
            rusqlite::params![status, reason, task_id],
        )
        .unwrap();
        // Recreate the proposal-before-finalization window: status changes
        // normally withdraw the fixture's already-parked question.
        conn.execute(
            "UPDATE operator_questions SET status = 'open', withdrawn_reason = NULL WHERE work_item_id = ?1",
            [&task_id],
        )
        .unwrap();
        drop(conn);
        state.topic_broker.refresh_decisions(&state.work_db, true).await;
        assert_eq!(next_snapshot(&sink).await.len(), expected, "{status}: {reason:?}");
    }
}

async fn next_snapshot(sink: &SessionSink) -> Vec<boss_protocol::OperatorDecisionNotification> {
    let envelope = tokio::time::timeout(std::time::Duration::from_secs(2), sink.next())
        .await
        .unwrap()
        .unwrap();
    let FrontendEvent::OperatorDecisionSnapshot { questions } = envelope.payload else {
        panic!("expected decision snapshot: {envelope:?}");
    };
    questions
}

#[tokio::test]
async fn global_feed_discovers_question_and_cancels_after_answer() {
    let (state, _dir, _execution, task_id) = parked_question_task().await;
    let (tx, _rx) = tokio::sync::oneshot::channel::<()>();
    let sink = Arc::new(SessionSink::new(tx, crate::app::frontend_metrics::test_registry()));
    state.topic_broker.register_session("session-test", sink.clone()).await;
    crate::app::subscriptions::handle_subscribe(
        dispatch_with_peer(&state, &sink, None),
        FrontendRequest::Subscribe {
            topics: vec!["work.decisions".into()],
        },
    )
    .await;
    let questions = next_snapshot(&sink).await;
    let task = question_task(&state, &task_id);
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].work_item_id, task_id);
    assert_eq!(questions[0].product_id, task.product_id);
    assert_eq!(questions[0].question_id, task.operator_question.unwrap().id);
    assert_eq!(
        questions[0].label,
        format!("#{}: {}", task.short_id.unwrap(), task.name)
    );
    assert!(matches!(
        sink.next().await.unwrap().payload,
        FrontendEvent::Subscribed { .. }
    ));
    // Resync re-subscribes an existing topic: it must still replay state.
    crate::app::subscriptions::handle_subscribe(
        dispatch_with_peer(&state, &sink, None),
        FrontendRequest::Subscribe {
            topics: vec!["work.decisions".into()],
        },
    )
    .await;
    assert_eq!(next_snapshot(&sink).await, questions);
    assert!(matches!(
        sink.next().await.unwrap().payload,
        FrontendEvent::Subscribed { .. }
    ));
    // The answer handler's normal work publisher must also refresh the global
    // feed, without a subscription to this task's product.
    answer_question(&state, &task_id, false).await;
    assert!(next_snapshot(&sink).await.is_empty());
}

#[tokio::test]
async fn execution_publisher_covers_creation_and_status_override_withdrawal() {
    let (state, _dir, _execution, task_id) = parked_question_task().await;
    let task = question_task(&state, &task_id);
    let (tx, _rx) = tokio::sync::oneshot::channel::<()>();
    let sink = Arc::new(SessionSink::new(tx, crate::app::frontend_metrics::test_registry()));
    state.topic_broker.register_session("decisions", sink.clone()).await;
    state
        .topic_broker
        .subscribe("decisions", &["work.decisions".into()])
        .await;
    state
        .publisher
        .publish_work_item_changed(&task.product_id, &task_id, "question_created")
        .await;
    assert_eq!(next_snapshot(&sink).await.len(), 1);
    state
        .work_db
        .update_work_item(
            &task_id,
            boss_protocol::WorkItemPatch {
                status: Some("todo".into()),
                ..Default::default()
            },
        )
        .unwrap();
    state
        .publisher
        .publish_work_item_changed(&task.product_id, &task_id, "status_edited")
        .await;
    assert!(next_snapshot(&sink).await.is_empty());
}
