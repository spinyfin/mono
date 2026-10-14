use super::*;
use crate::app::read_admission::ReadAdmission;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn send(write: &mut tokio::net::unix::OwnedWriteHalf, id: &str, payload: FrontendRequest) {
    let request = FrontendRequestEnvelope {
        request_id: id.into(),
        payload,
    };
    write
        .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
        .await
        .unwrap();
}

fn get(path: &str, git_ref: &str) -> FrontendRequest {
    FrontendRequest::GetProductDesignDoc {
        repo_remote_url: FLUNGE.into(),
        path: path.into(),
        git_ref: git_ref.into(),
    }
}

async fn wait_started(source: &GatedSource, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while source.started.load(Ordering::SeqCst) < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("source did not start");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_document_holds_real_dispatch_admission_until_response() {
    let source = Arc::new(GatedSource {
        inner: FakeSource::new(),
        gate: Arc::new(tokio::sync::Semaphore::new(0)),
        started: AtomicUsize::new(0),
    });
    let (state, _dir) = crate::app::tests::test_server_state_with_overrides(
        ServerStateOverrides::builder()
            .design_docs(Arc::new(DesignDocsService::with_source(source.clone())))
            .read_admission(Arc::new(ReadAdmission::new(1, 1, 4, Duration::from_secs(5))))
            .build(),
    );
    let product = crate::test_support::create_test_product_with_repo(&state.work_db, "Docs", Some(FLUNGE));
    let (client, server) = UnixStream::pair().unwrap();
    let handler = tokio::spawn(handle_frontend_connection(server, state, None));
    let (read, mut write) = client.into_split();
    let mut lines = BufReader::new(read).lines();
    let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    send(&mut write, "first", get("first.md", sha)).await;
    wait_started(&source, 1).await;
    send(&mut write, "second", get("second.md", sha)).await;
    send(
        &mut write,
        "listing",
        FrontendRequest::ListProductDesignDocs {
            product_id: product.id,
            refresh: true,
        },
    )
    .await;
    // A live response proves the reader dispatched both excess requests.
    send(&mut write, "barrier", FrontendRequest::ListWorkerLiveStates).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let env: FrontendEventEnvelope = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if env.request_id.as_deref() == Some("barrier") {
                break;
            }
            assert!(env.request_id.is_none(), "blocked document replied before gate release");
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        source.started.load(Ordering::SeqCst),
        1,
        "excess source work escaped admission"
    );
    source.gate.add_permits(1);
    wait_started(&source, 2).await;
    assert_eq!(source.started.load(Ordering::SeqCst), 2);
    source.gate.add_permits(2);
    let mut ids = std::collections::HashSet::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while ids.len() < 3 {
            let env: FrontendEventEnvelope = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if let Some(id) = env.request_id {
                assert!(
                    !matches!(env.payload, FrontendEvent::Error { .. }),
                    "document read returned an error"
                );
                assert!(ids.insert(id));
            }
        }
    })
    .await
    .expect("queued documents did not progress");
    assert_eq!(source.started.load(Ordering::SeqCst), 3);
    drop(lines);
    drop(write);
    handler.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cached_reads_start_one_revalidation_fetch() {
    cached_read_burst(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cached_read_burst_waits_for_slow_first_response_without_admission_errors() {
    cached_read_burst(true).await;
}

async fn cached_read_burst(slow_first: bool) {
    let source = Arc::new(GatedSource {
        inner: FakeSource::new(),
        gate: Arc::new(tokio::sync::Semaphore::new(usize::from(!slow_first))),
        started: AtomicUsize::new(0),
    });
    let svc = Arc::new(DesignDocsService::with_source(source.clone()));
    if !slow_first {
        svc.open_markdown_doc(FLUNGE, PATH, GIT_REF).await;
    }
    let (state, _dir) = crate::app::tests::test_server_state_with_overrides(
        ServerStateOverrides::builder()
            .design_docs(svc)
            .read_admission(Arc::new(ReadAdmission::new(32, 16, 128, Duration::from_millis(500))))
            .build(),
    );
    let registry = state.design_doc_revalidation.clone();
    let (client, server) = UnixStream::pair().unwrap();
    let handler = tokio::spawn(handle_frontend_connection(server, state, None));
    let (read, mut write) = client.into_split();
    let mut lines = BufReader::new(read).lines();
    for index in 0..32 {
        send(&mut write, &format!("cached-{index}"), get(PATH, GIT_REF)).await;
    }
    if slow_first {
        wait_started(&source, 1).await;
        // Exceed the explicit 500 ms admission wait while all 31
        // successors queue for their ordering turn on this same view.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(source.started.load(Ordering::SeqCst), 1);
        source.gate.add_permits(1);
    }
    let mut ids = std::collections::HashSet::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while ids.len() < 32 {
            let env: FrontendEventEnvelope = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if let Some(id) = env.request_id {
                assert!(
                    matches!(env.payload, FrontendEvent::ProductDesignDocContent { .. }),
                    "{id}: {:?}",
                    env.payload
                );
                assert!(ids.insert(id));
            }
        }
    })
    .await
    .unwrap();
    wait_started(&source, 2).await;
    // Responses are queued before handlers register with the revalidation
    // ladder. Keep its fetch blocked until every request has joined; otherwise
    // the last handler can start a second ladder after the first one finishes.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let registered = registry.in_flight.lock().unwrap()[&key(PATH)]
                .registered_requests
                .load(Ordering::SeqCst);
            if registered == 32 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("not all cached reads joined the revalidation ladder");
    assert_eq!(
        source.started.load(Ordering::SeqCst),
        2,
        "one prime and one background fetch"
    );
    assert_eq!(registry.in_flight.lock().unwrap().len(), 1);
    source.gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !registry.in_flight.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(source.started.load(Ordering::SeqCst), 2, "no second revalidation fetch");
    drop(lines);
    drop(write);
    handler.await.unwrap().unwrap();
}

#[test]
fn revalidation_capacity_is_global_and_released_on_drop() {
    let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
    let sink = Arc::new(SessionSink::new(shutdown_tx));
    let registry = Arc::new(RevalidationRegistry::default());
    let mut guards = Vec::new();
    for index in 0..4 {
        guards.push(owner(&registry, &format!("{index}.md"), &sink));
    }
    assert!(matches!(registry.try_begin(key("extra.md"), &sink), Begin::AtCapacity));
    assert!(matches!(registry.try_begin(key("0.md"), &sink), Begin::Coalesced));
    guards.pop();
    assert!(matches!(registry.try_begin(key("extra.md"), &sink), Begin::Owner(_)));
    drop(guards);
    assert!(registry.in_flight.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guidance_fetch_holds_connection_admission_and_identical_reply_order() {
    let source = Arc::new(GatedSource {
        inner: FakeSource::new(),
        gate: Arc::new(tokio::sync::Semaphore::new(0)),
        started: AtomicUsize::new(0),
    });
    let (state, _dir) = crate::app::tests::test_server_state_with_overrides(
        ServerStateOverrides::builder()
            .design_docs(Arc::new(DesignDocsService::with_source(source.clone())))
            .read_admission(Arc::new(ReadAdmission::new(4, 1, 8, Duration::from_secs(5))))
            .build(),
    );
    let product = crate::test_support::create_test_product_with_repo(&state.work_db, "Guidance", Some(FLUNGE));
    let other = crate::test_support::create_test_product_with_repo(&state.work_db, "Other", Some(FLUNGE));
    let (client, server) = UnixStream::pair().unwrap();
    let handler = tokio::spawn(handle_frontend_connection(server, state, None));
    let (read, mut write) = client.into_split();
    let mut lines = BufReader::new(read).lines();
    let request = |product_id: &str| FrontendRequest::ListCoordinatorGuidance {
        product_id: Some(product_id.into()),
    };
    send(&mut write, "first", request(&product.id)).await;
    wait_started(&source, 1).await;
    send(&mut write, "second", request(&product.id)).await;
    send(&mut write, "other", request(&other.id)).await;
    send(&mut write, "barrier", FrontendRequest::ListWorkerLiveStates).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let env: FrontendEventEnvelope = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if env.request_id.as_deref() == Some("barrier") {
                break;
            }
            assert!(env.request_id.is_none(), "{env:?}");
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        source.started.load(Ordering::SeqCst),
        1,
        "fetch escaped the connection limit"
    );
    source.gate.add_permits(1);
    wait_started(&source, 2).await;
    assert_eq!(source.started.load(Ordering::SeqCst), 2);
    source.gate.add_permits(2);
    let mut ids = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while ids.len() < 3 {
            let env: FrontendEventEnvelope = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if let Some(id) = env.request_id {
                assert!(
                    matches!(env.payload, FrontendEvent::CoordinatorGuidanceList { .. }),
                    "{id}: {:?}",
                    env.payload
                );
                ids.push(id);
            }
        }
    })
    .await
    .unwrap();
    assert!(ids.iter().position(|id| id == "first").unwrap() < ids.iter().position(|id| id == "second").unwrap());
    assert_eq!(source.started.load(Ordering::SeqCst), 3);
    drop(lines);
    drop(write);
    handler.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fetch_beyond_the_global_limit_gets_the_busy_error_and_frees_on_release() {
    let source = Arc::new(GatedSource {
        inner: FakeSource::new(),
        gate: Arc::new(tokio::sync::Semaphore::new(0)),
        started: AtomicUsize::new(0),
    });
    let limiter = Arc::new(tokio::sync::Semaphore::new(
        crate::app::coordinator_guidance::MAX_CONCURRENT_FETCHES,
    ));
    let (state, _dir) = crate::app::tests::test_server_state_with_overrides(
        ServerStateOverrides::builder()
            .design_docs(Arc::new(DesignDocsService::with_source(source.clone())))
            .guidance_fetches(limiter.clone())
            .read_admission(Arc::new(ReadAdmission::new(8, 8, 16, Duration::from_secs(5))))
            .build(),
    );
    let products: Vec<_> = (0..5)
        .map(|index| {
            crate::test_support::create_test_product_with_repo(
                &state.work_db,
                &format!("Guidance {index}"),
                Some(FLUNGE),
            )
        })
        .collect();
    let (client, server) = UnixStream::pair().unwrap();
    let handler = tokio::spawn(handle_frontend_connection(server, state, None));
    let (read, mut write) = client.into_split();
    let mut lines = BufReader::new(read).lines();
    let request = |index: usize| FrontendRequest::ListCoordinatorGuidance {
        product_id: Some(products[index].id.clone()),
    };
    for index in 0..4 {
        send(&mut write, &format!("fetch-{index}"), request(index)).await;
    }
    wait_started(&source, 4).await;
    assert_eq!(limiter.available_permits(), 0);
    send(&mut write, "excess", request(4)).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let env: FrontendEventEnvelope = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if let Some(id) = env.request_id {
                assert_eq!(id, "excess", "gated fetch replied before release");
                match env.payload {
                    FrontendEvent::WorkError { message } => assert_eq!(message, crate::app::read_admission::BUSY),
                    other => panic!("expected busy WorkError, got {other:?}"),
                }
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(source.started.load(Ordering::SeqCst), 4);
    source.gate.add_permits(4);
    let mut ids = std::collections::HashSet::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while ids.len() < 4 {
            let env: FrontendEventEnvelope = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if let Some(id) = env.request_id {
                let index = (0..4)
                    .find(|index| id == format!("fetch-{index}"))
                    .expect("original request id");
                match env.payload {
                    FrontendEvent::CoordinatorGuidanceList { guidance } => {
                        assert_eq!(guidance.len(), 1);
                        assert_eq!(guidance[0].product_id, products[index].id);
                        assert!(
                            matches!(
                                guidance[0].state,
                                boss_protocol::CoordinatorGuidanceState::Loaded { .. }
                            ),
                            "{guidance:?}"
                        );
                    }
                    other => panic!("expected guidance response, got {other:?}"),
                }
                assert!(ids.insert(id));
            }
        }
    })
    .await
    .unwrap();
    drop(lines);
    drop(write);
    handler.await.unwrap().unwrap();
    assert_eq!(limiter.available_permits(), 4);
}
