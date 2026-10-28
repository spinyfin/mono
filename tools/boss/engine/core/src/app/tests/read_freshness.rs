use super::*;

#[tokio::test]
async fn newer_domain_failures_do_not_replace_successful_snapshots() {
    use boss_protocol::{DesignDocContent, DesignDocTreeState};
    let failures = [
        FrontendEvent::OperatorQuestionError {
            error: boss_protocol::OperatorQuestionError::NotFound,
        },
        FrontendEvent::ProductDesignDocContent {
            repo_remote_url: "repo".into(),
            path: "doc.md".into(),
            git_ref: "main".into(),
            content: DesignDocContent::Failed {
                reason: "offline".into(),
                retryable: true,
            },
        },
        FrontendEvent::ProductDesignDocsList {
            product_id: "product".into(),
            state: DesignDocTreeState::Unreachable {
                repo_remote_url: "repo".into(),
                reason: "offline".into(),
            },
        },
        FrontendEvent::ProductDesignDocsList {
            product_id: "product".into(),
            state: DesignDocTreeState::RateLimited {
                repo_remote_url: "repo".into(),
                reason: "limited".into(),
            },
        },
        FrontendEvent::ExecutionTranscriptUnavailable {
            execution_id: "execution".into(),
            reason: "missing".into(),
        },
    ];
    for failure in failures {
        let sink = make_session_sink();
        {
            let mut reads = sink.view_reads.lock().unwrap();
            reads.begin("old", "view".into());
            reads.begin("new", "view".into());
        }
        let expected_failure = serde_json::to_value(&failure).unwrap();
        send_response(&sink, "new", failure);
        let newer = sink.next().await.unwrap();
        assert_eq!(newer.request_id.as_deref(), Some("new"));
        assert_eq!(serde_json::to_value(newer.payload).unwrap(), expected_failure);
        // Snapshot contents are opaque to the ledger; this sentinel must survive.
        send_response(&sink, "old", FrontendEvent::ProductsList { products: vec![] });
        let older = sink.next().await.unwrap();
        assert_eq!(older.request_id.as_deref(), Some("old"));
        assert!(matches!(older.payload, FrontendEvent::ProductsList { .. }));
    }
}

fn context(state: &Arc<ServerState>, sink: &Arc<SessionSink>, id: &str) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(Arc::new(state.work_db.query_connection().unwrap()))
        .sink(sink.clone())
        .session_id("lists")
        .request_id(id)
        .recv_instant(Instant::now())
        .decode_ms(0.0)
        .build()
}

#[tokio::test]
async fn overtaken_product_list_replies_with_latest_rows_and_original_correlation() {
    let (state, _dir) = test_server_state();
    let sink = make_session_sink();
    let request = FrontendRequest::ListProducts;
    // Pause the older handler at its read/enqueue boundary. The newer handler
    // runs completely while that captured pre-mutation snapshot is held.
    sink.view_reads
        .lock()
        .unwrap()
        .begin("old", read_admission::key(&request));
    let old = state.work_db.list_products().unwrap();
    let added = crate::test_support::create_test_product_named(&state.work_db, "added");
    dispatch::dispatch_request(context(&state, &sink, "new"), request, true).await;
    let newer = sink.next().await.unwrap();
    assert_eq!(newer.request_id.as_deref(), Some("new"));
    send_response(&sink, "old", FrontendEvent::ProductsList { products: old });
    let older = sink.next().await.unwrap();
    assert_eq!(older.request_id.as_deref(), Some("old"));
    for reply in [newer, older] {
        let FrontendEvent::ProductsList { products } = reply.payload else {
            panic!("expected products")
        };
        assert!(products.iter().any(|product| product.id == added.id));
    }
}

#[tokio::test]
async fn writable_bulk_read_handlers_do_not_reserve_the_writer() {
    let (state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product_named(&state.work_db, "reads");
    let writer = state.work_db.query_connection().unwrap();
    writer.connect().unwrap().execute_batch("BEGIN IMMEDIATE").unwrap();
    let sink = make_session_sink();
    for (id, request) in [
        ("products", FrontendRequest::ListProducts),
        (
            "projects",
            FrontendRequest::ListProjects {
                product_id: product.id.clone(),
                dep_filter: None,
            },
        ),
        (
            "ideas",
            FrontendRequest::ListIdeas {
                product_id: product.id.clone(),
                status: None,
            },
        ),
    ] {
        let ctx = context(&state, &sink, id);
        ctx.work_db.connect().unwrap().busy_timeout(Duration::ZERO).unwrap();
        match request {
            r @ FrontendRequest::ListProducts => products::handle_list_products(ctx, r).await,
            r @ FrontendRequest::ListProjects { .. } => projects::handle_list_projects(ctx, r).await,
            r @ FrontendRequest::ListIdeas { .. } => ideas::handle_list_ideas(ctx, r).await,
            _ => unreachable!(),
        }
        let reply = sink.next().await.unwrap();
        assert_eq!(reply.request_id.as_deref(), Some(id));
        assert!(
            !matches!(
                reply.payload,
                FrontendEvent::WorkError { .. } | FrontendEvent::Error { .. }
            ),
            "{reply:?}"
        );
    }
    writer.connect().unwrap().execute_batch("ROLLBACK").unwrap();
}
