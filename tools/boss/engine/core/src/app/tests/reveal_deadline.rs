use super::*;

async fn delayed_reveal_response(visible: bool) {
    let (server, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&server.work_db, "delayed reveal");
    let task = crate::test_support::create_test_chore(&server.work_db, &product.id, "target card");
    let sink = make_session_sink();
    server.register_app_session("app".into(), sink.clone()).await;
    let sender = server.clone();
    let id = task.id.clone();
    let reveal = tokio::spawn(async move { sender.reveal_work_item(&id).await });
    let envelope = sink.next().await.expect("reveal request");
    let FrontendEvent::EngineRequest { request_id, request } = envelope.payload else {
        panic!("expected engine request");
    };
    let EngineToAppRequest::RevealWorkItem(input) = request else {
        panic!("expected reveal request");
    };
    assert_eq!(input.work_item_id, task.id);
    assert_eq!(input.product_id, product.id);

    // Model the app's deferred tree arriving just before its first deadline,
    // followed by almost the full, freshly armed viewport confirmation budget.
    tokio::time::sleep(Duration::from_millis(2800)).await;
    assert!(!reveal.is_finished(), "tree wait must remain pending");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!reveal.is_finished(), "transport must outlive both app phases");
    let reason = format!(
        "could not reveal {}: target card did not become visible in the board viewport",
        task.id
    );
    let response = EngineToAppResponse::RevealWorkItem {
        result: if visible {
            Ok(boss_protocol::RevealWorkItemResult::default())
        } else {
            Err(EngineToAppError::Internal {
                message: reason.clone(),
            })
        },
    };
    server.deliver_app_response("app", &request_id, response).await;
    let result = reveal.await.expect("reveal task");
    if visible {
        assert_eq!(result.expect("late app confirmation must succeed"), task.id);
    } else {
        assert!(
            matches!(result, Err(pane_ops::RevealItemError::App(EngineToAppError::Internal { message })) if message == reason)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn reveal_accepts_confirmation_after_five_seconds() {
    delayed_reveal_response(true).await;
}

#[tokio::test(start_paused = true)]
async fn reveal_preserves_named_visibility_failure_after_five_seconds() {
    delayed_reveal_response(false).await;
}
