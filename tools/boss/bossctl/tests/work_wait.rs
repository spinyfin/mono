//! Exercise wait exit codes and reference routing through a private RPC socket.

use boss_protocol::{
    ExecutionKind, ExecutionStatus, FrontendEvent, FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope,
    WorkExecution,
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn wait(reference: &str, response: Option<FrontendEvent>, extra: &[&str]) -> std::process::Output {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let socket = std::env::temp_dir().join(format!(
        "wait-{}.sock",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let reference = reference.to_owned();
    let expected = reference.clone();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap() == 0 {
                continue; // Discovery's connectivity probe.
            }
            let request: FrontendRequestEnvelope = serde_json::from_str(&line).unwrap();
            match request.payload {
                FrontendRequest::GetExecution { id } => assert_eq!(id, expected),
                FrontendRequest::ListExecutions {
                    work_item_id,
                    include_revision_chain,
                } => {
                    assert_eq!(work_item_id.as_deref(), Some(expected.as_str()));
                    assert!(!include_revision_chain);
                }
                other => panic!("unexpected lookup: {other:?}"),
            }
            if let Some(response) = response {
                let response = FrontendEventEnvelope::response(request.request_id, response);
                let mut bytes = serde_json::to_vec(&response).unwrap();
                bytes.push(b'\n');
                writer.write_all(&bytes).await.unwrap();
                break;
            }
            // Keep the socket open without responding until the client times out.
            let mut trailing = String::new();
            reader.read_line(&mut trailing).await.unwrap();
            break;
        }
    });
    let mut command = tokio::process::Command::new(std::env::var("BOSSCTL_TEST_BINARY").unwrap());
    command.args([
        "--socket-path",
        socket.to_str().unwrap(),
        "--json",
        "work",
        "wait",
        &reference,
    ]);
    command.args(extra);
    command.env("BOSS_ENGINE_AUTOSTART", "0");
    command.kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(50), command.output())
        .await
        .unwrap()
        .unwrap();
    // Argument failures do not connect.
    if extra.contains(&"invalid") {
        server.abort();
    } else {
        server.await.unwrap();
    }
    std::fs::remove_file(socket).unwrap();
    output
}

fn execution(status: ExecutionStatus) -> WorkExecution {
    WorkExecution::builder()
        .id("exec_example")
        .work_item_id("task_example")
        .kind(ExecutionKind::ChoreImplementation)
        .status(status)
        .repo_remote_url("git@example.com:example/repo.git")
        .created_at("100")
        .build()
}

fn assert_result(output: std::process::Output, code: i32, result: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["result"], result);
}

#[tokio::test]
async fn work_reference_goes_directly_to_list_executions() {
    let response = FrontendEvent::ExecutionsList {
        executions: vec![execution(ExecutionStatus::Running)],
        work_item_id: Some("task_example".into()),
    };
    assert_result(wait("task_example", Some(response), &[]).await, 0, "started");
}

#[tokio::test]
async fn execution_failure_and_wait_errors_have_separate_exit_codes() {
    assert_result(
        wait(
            "exec_example",
            Some(FrontendEvent::ExecutionResult {
                execution: execution(ExecutionStatus::Failed),
            }),
            &[],
        )
        .await,
        1,
        "failed",
    );
    assert_result(
        wait(
            "exec_missing",
            Some(FrontendEvent::WorkError {
                message: "unknown execution".into(),
            }),
            &[],
        )
        .await,
        4,
        "error",
    );
    assert_result(
        wait("exec_example", None, &["--until-not", "invalid"]).await,
        4,
        "error",
    );
}

#[tokio::test]
async fn stalled_rpc_times_out_and_excluded_pre_run_does_not_start() {
    assert_result(wait("exec_example", None, &["--timeout", "1"]).await, 3, "timed_out");
    assert_result(
        wait(
            "exec_example",
            Some(FrontendEvent::ExecutionResult {
                execution: execution(ExecutionStatus::WaitingDependency),
            }),
            &["--until-not", "ready"],
        )
        .await,
        5,
        "not_started",
    );
}
