//! Assert the real CLI process's failure contract against a private RPC fixture.

use boss_engine::work::PRE_MERGE_BATCH_RESERVATION_UNITS;
use boss_protocol::{FrontendEvent, FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn review_start_capacity_error_exits_nonzero_and_names_the_reason() {
    // Bazel's hermetic wrapper gives this test action its own temporary root.
    let socket = std::env::temp_dir().join("review.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let reason = format!(
        "cannot start review batch: review-pool reservation capacity exhausted \
         (requires {PRE_MERGE_BATCH_RESERVATION_UNITS} units)"
    );
    let server_reason = reason.clone();
    let server = tokio::spawn(async move {
        // Discovery first probes connectivity without writing a request.
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let Some(line) = BufReader::new(reader).lines().next_line().await.unwrap() else {
                continue;
            };
            let request: FrontendRequestEnvelope = serde_json::from_str(&line).unwrap();
            assert!(matches!(request.payload, FrontendRequest::TriggerPrReview {
                pr_number: 42, repo: Some(ref repo),
            } if repo == "example/repo"));
            let response = FrontendEventEnvelope::response(
                request.request_id,
                FrontendEvent::WorkError {
                    message: server_reason.clone(),
                },
            );
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
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), command.output())
        .await
        .unwrap()
        .unwrap();
    server.await.unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(&reason), "{stderr}");
}
