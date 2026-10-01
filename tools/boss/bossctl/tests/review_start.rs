//! Assert review-start output and exit status against a private RPC fixture.

use boss_engine::work::PRE_MERGE_BATCH_RESERVATION_UNITS;
use boss_protocol::{FrontendEvent, FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn review_start(response: FrontendEvent) -> std::process::Output {
    // Each invocation owns a distinct socket, including concurrent tests.
    let socket = std::env::temp_dir().join(format!("review-{}.sock", unique_id()));
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (connected_tx, mut connected_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut connected_tx = Some(connected_tx);
        // Discovery first probes connectivity without writing a request.
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            if let Some(connected_tx) = connected_tx.take() {
                let _ = connected_tx.send(());
            }
            let (reader, mut writer) = stream.into_split();
            let Some(line) = BufReader::new(reader).lines().next_line().await.unwrap() else {
                continue;
            };
            let request: FrontendRequestEnvelope = serde_json::from_str(&line).unwrap();
            assert!(matches!(request.payload, FrontendRequest::TriggerPrReview {
                pr_number: 42, repo: Some(ref repo),
            } if repo == "example/repo"));
            let response = FrontendEventEnvelope::response(request.request_id, response);
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            writer.write_all(&bytes).await.unwrap();
            break;
        }
    });
    let mut command = tokio::process::Command::new(std::env::var("BOSSCTL_TEST_BINARY").unwrap());
    command.args([
        "--socket-path",
        socket.to_str().unwrap(),
        "review",
        "start",
        "--pr",
        "42",
        "--repo",
        "example/repo",
    ]);
    command.kill_on_drop(true);
    let output = command.output();
    tokio::pin!(output);
    // The deadline bounds the RPC exchange, so it starts when the client
    // reaches the fixture rather than at spawn. The first exec of a freshly
    // linked bossctl is an OS cold start that, on a host running many other
    // fresh test binaries, has been measured above 15s before `main` runs;
    // that latency is not the behaviour under test.
    let exited_early = tokio::select! {
        biased;
        _ = &mut connected_rx => None,
        result = &mut output => Some(result.unwrap()),
    };
    let output = match exited_early {
        // The client exited without reaching the fixture; the caller's
        // assertions report its status and stderr.
        Some(output) => {
            server.abort();
            output
        }
        None => {
            let output = tokio::time::timeout(std::time::Duration::from_secs(15), output)
                .await
                .unwrap()
                .unwrap();
            server.await.unwrap();
            output
        }
    };
    std::fs::remove_file(socket).unwrap();
    output
}

fn unique_id() -> usize {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[tokio::test]
async fn review_start_capacity_error_exits_nonzero_and_names_the_reason() {
    let reason = format!(
        "cannot start review batch: review-pool reservation capacity exhausted \
         (requires {PRE_MERGE_BATCH_RESERVATION_UNITS} units)"
    );
    let output = review_start(FrontendEvent::WorkError {
        message: reason.clone(),
    })
    .await;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(&reason), "{stderr}");
}

#[tokio::test]
async fn review_start_renders_admitted_active_and_legacy_reviews() {
    for (batch, active, heading) in [
        (
            true,
            false,
            "admitted review batch batch-example (generation 2) - 3 reviewers",
        ),
        (
            true,
            true,
            "an active review batch already covers this head (batch batch-example, generation 2); nothing started",
        ),
        (false, false, "re-enqueued review for PR #42"),
    ] {
        let execution = boss_protocol::WorkExecution::builder()
            .id("exec-one")
            .work_item_id("chore-example")
            .repo_remote_url("https://github.com/example/repo")
            .kind(boss_protocol::ExecutionKind::PrReview)
            .status(boss_protocol::ExecutionStatus::Ready)
            .created_at("1700000000")
            .branch_naming(boss_protocol::BranchNaming::BossExecPrefix)
            .build();
        let output = review_start(FrontendEvent::PrReviewTriggered {
            execution,
            work_item_id: "chore-example".into(),
            pr_url: "https://github.com/example/repo/pull/42".into(),
            batch_id: batch.then(|| "batch-example".into()),
            batch_generation: batch.then_some(2),
            batch_execution_ids: if batch {
                vec!["exec-one".into(), "exec-two".into(), "exec-three".into()]
            } else {
                vec![]
            },
            already_active: active,
        })
        .await;
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let executions = if batch {
            "  execution: exec-one\n  execution: exec-two\n  execution: exec-three\n"
        } else {
            "  execution: exec-one  [ready]\n"
        };
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "{heading}\n  work item: chore-example\n  pr url:    https://github.com/example/repo/pull/42\n{executions}"
            )
        );
    }
}
