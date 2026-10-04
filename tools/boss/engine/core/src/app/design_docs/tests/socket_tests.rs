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
    let source = Arc::new(GatedSource {
        inner: FakeSource::new(),
        gate: Arc::new(tokio::sync::Semaphore::new(1)),
        started: AtomicUsize::new(0),
    });
    let svc = Arc::new(DesignDocsService::with_source(source.clone()));
    svc.open_markdown_doc(FLUNGE, PATH, GIT_REF).await;
    let (state, _dir) =
        crate::app::tests::test_server_state_with_overrides(ServerStateOverrides::builder().design_docs(svc).build());
    let registry = state.design_doc_revalidation.clone();
    let (client, server) = UnixStream::pair().unwrap();
    let handler = tokio::spawn(handle_frontend_connection(server, state, None));
    let (read, mut write) = client.into_split();
    let mut lines = BufReader::new(read).lines();
    for index in 0..32 {
        send(&mut write, &format!("cached-{index}"), get(PATH, GIT_REF)).await;
    }
    let mut ids = std::collections::HashSet::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while ids.len() < 32 {
            let env: FrontendEventEnvelope = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if let Some(id) = env.request_id {
                assert!(matches!(env.payload, FrontendEvent::ProductDesignDocContent { .. }));
                assert!(ids.insert(id));
            }
        }
    })
    .await
    .unwrap();
    wait_started(&source, 2).await;
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
    drop(lines);
    drop(write);
    handler.await.unwrap().unwrap();
}

#[test]
fn revalidation_capacity_is_global_and_released_on_drop() {
    let registry = Arc::new(RevalidationRegistry::default());
    let mut guards = Vec::new();
    for index in 0..4 {
        guards.push(
            registry
                .try_begin((FLUNGE.into(), format!("{index}.md"), GIT_REF.into()))
                .unwrap(),
        );
    }
    assert!(
        registry
            .try_begin((FLUNGE.into(), "extra.md".into(), GIT_REF.into()))
            .is_none()
    );
    guards.pop();
    assert!(
        registry
            .try_begin((FLUNGE.into(), "extra.md".into(), GIT_REF.into()))
            .is_some()
    );
    drop(guards);
    assert!(registry.in_flight.lock().unwrap().is_empty());
}
