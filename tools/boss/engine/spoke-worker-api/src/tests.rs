use crate::{Gateway, identity};
use boss_host_execution::{
    process::process_snapshot,
    worker::{ExecutionRecord, ObservationConfig, Phase},
};
use boss_protocol::{
    FrontendEvent, PrBodyView, Product, Task, TaskKind, TaskStatus, WorkItemDependencyDetail, WorkerContextBundle,
};
use boss_protocol_spoke::v1::{self, command, record};
use boss_spoke_journal::{Journal, Limits};
use prost::Message;
use serde_json::Value;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::UnixListener;

fn stamp() -> anyhow::Result<v1::Stamp> {
    Ok(v1::Stamp {
        boot_id: "boot".into(),
        wall_unix_ms: 1234,
        awake_ns: 1,
        elapsed_ns: 1,
    })
}

/// Reports the size of `<root>/transcript.log` as the observed source, or
/// that no transcript exists, independent of anything ingested.
fn probe(root: &Path) -> crate::TranscriptProbe {
    let path = root.join("transcript.log");
    Arc::new(move |_| {
        let path = path.clone();
        Box::pin(async move { Ok(("1-0".to_owned(), std::fs::metadata(path)?.len())) })
    })
}

fn open_with(root: &Path, limits: Limits) -> Gateway {
    Gateway {
        journal: Arc::new(Mutex::new(Journal::open(&root.join("state"), "host", limits).unwrap())),
        state_dir: root.join("state"),
        probe: probe(root),
    }
}

fn open(root: &Path) -> Gateway {
    open_with(root, Limits::default())
}

fn execution(root: &Path) -> ExecutionRecord {
    ExecutionRecord::builder()
        .execution_id("execution")
        .run_id("run")
        .spawn_token("token")
        .assignment_generation(1)
        .lease_id("lease")
        .workspace_id("workspace")
        .session("session")
        .phase(Phase::Alive)
        .boot_id("boot")
        .pid(std::process::id() as u64)
        .driver_id("claude")
        .observation(ObservationConfig {
            workspace: root.into(),
            pending: root.into(),
            file: None,
            pid_start: Some(process_snapshot(std::process::id() as i32).unwrap().start),
        })
        .build()
}

fn save(gateway: &Gateway, execution: &ExecutionRecord) {
    gateway
        .journal
        .lock()
        .unwrap()
        .save_execution(
            &execution.run_id,
            &execution.spawn_token,
            &serde_json::to_vec(execution).unwrap(),
        )
        .unwrap();
}

fn records(gateway: &Gateway) -> Vec<v1::Record> {
    let journal = gateway.journal.lock().unwrap();
    let mut result = Vec::new();
    let mut after = 0;
    while let Some(record) = journal.next_record(after).unwrap() {
        after = record.sequence;
        result.push(v1::Record::decode(record.bytes.as_slice()).unwrap());
    }
    result
}

async fn cli(socket: &Path, run: &str, args: &[&str]) -> std::process::Output {
    cli_format(socket, run, args, true).await
}

async fn cli_format(socket: &Path, run: &str, args: &[&str], json: bool) -> std::process::Output {
    let bin = std::env::current_dir()
        .unwrap()
        .join(std::env::var("BOSS_TEST_CLI").unwrap());
    tokio::process::Command::new(bin)
        .env("BOSS_SOCKET_PATH", socket)
        .env("BOSS_RUN_ID", run)
        .args(args)
        .args(json.then_some("--json"))
        .output()
        .await
        .unwrap()
}

fn success(output: std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn server(gateway: &Gateway, socket: &Path) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let listener = UnixListener::bind(socket).unwrap();
    let gateway = gateway.clone();
    tokio::spawn(async move { gateway.serve(listener, stamp).await })
}

fn command(run: &v1::Run, body: command::Body) -> v1::Command {
    v1::Command {
        sequence: 1,
        command_id: "command".into(),
        run: Some(run.clone()),
        not_after_unix_ms: 99999,
        body: Some(body),
    }
}

#[tokio::test]
async fn actual_cli_queues_done_preserves_assignment_and_replays_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let mut execution = execution(root.path());
    let output = root.path().join("pr-url.json");
    std::fs::write(&output, br#"{"pr_url":"https://github.com/spinyfin/mono/pull/123"}"#).unwrap();
    execution
        .worker_outputs
        .insert("BOSS_PR_URL_OUTPUT".into(), output.display().to_string());
    let gateway = open(root.path());
    save(&gateway, &execution);
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let args = [
        "propose",
        "done",
        "--outcome",
        "delivered",
        "--summary",
        "Implemented the gateway",
    ];
    let first = success(cli(&socket, "execution", &args).await);
    assert_eq!(first["status"], "queued");
    assert_eq!(first["value"]["already_submitted"], false);
    let id = first["value"]["receipt"]["id"].as_str().unwrap().to_owned();
    let second = success(cli(&socket, "execution", &args).await);
    assert_eq!(second["value"]["already_submitted"], true);
    assert_eq!(
        records(&gateway)
            .iter()
            .filter(|r| matches!(r.body, Some(record::Body::Proposal(_))))
            .count(),
        1
    );
    let stored: ExecutionRecord = serde_json::from_slice(
        &gateway
            .journal
            .lock()
            .unwrap()
            .load_execution("run", "token")
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stored.phase, Phase::Alive);
    assert!(!stored.reclaimed);
    assert_eq!(stored.lease_id, "lease");
    task.abort();
    let _ = task.await;
    drop(gateway);
    std::fs::remove_file(&socket).unwrap();
    std::fs::remove_file(&output).unwrap();
    let gateway = open(root.path());
    gateway.pump(&identity(&execution), &stamp().unwrap()).unwrap();
    assert!(records(&gateway).iter().any(|r| matches!(&r.body, Some(record::Body::Artifact(a)) if a.kind == v1::ArtifactKind::PrUrl as i32 && a.eof && String::from_utf8_lossy(&a.data).contains("/pull/123"))));
    let task = server(&gateway, &socket);
    let replay = success(cli(&socket, "execution", &args).await);
    assert_eq!(replay["value"]["receipt"]["id"], id);
    assert_eq!(replay["value"]["already_submitted"], true);
    let decision = command(
        &identity(&execution),
        command::Body::ProposalDecision(v1::ProposalDecision {
            proposal_id: id,
            code: v1::ResultCode::Rejected as i32,
            reason: "validation evidence missing".into(),
            result_json: b"{}".to_vec(),
        }),
    );
    gateway.apply(&decision).unwrap();
    gateway.apply(&decision).unwrap();
    let rejected = cli(&socket, "execution", &args).await;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("validation evidence missing"));
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn peer_attribution_validators_and_default_deny_survive_real_cli_calls() {
    let root = tempfile::tempdir().unwrap();
    let execution = execution(root.path());
    let gateway = open(root.path());
    save(&gateway, &execution);
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let denied = cli(
        &socket,
        "sibling",
        &["propose", "done", "--outcome", "delivered", "--summary", "wrong run"],
    )
    .await;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("worker_attribution_mismatch"));
    let denied = cli(&socket, "execution", &["pr", "status"]).await;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("hub_unavailable"));
    let invalid = cli(
        &socket,
        "execution",
        &["propose", "pr-created", "--url", "https://example.com/invalid"],
    )
    .await;
    assert!(!invalid.status.success());
    assert!(
        !records(&gateway)
            .iter()
            .any(|r| matches!(r.body, Some(record::Body::Proposal(_))))
    );
    let denied = cli(&socket, "execution", &["product", "list"]).await;
    assert!(!denied.status.success());
    // Direct protocol client exercises the default deny even if CLI spelling evolves.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let mut bytes = serde_json::to_vec(&boss_protocol::FrontendRequestEnvelope {
        request_id: "deny".into(),
        payload: boss_protocol::FrontendRequest::ListProducts,
    })
    .unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).await.unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.unwrap();
    assert!(reply.contains("worker_method_denied"));
    // A matching environment variable cannot replace ancestry and incarnation.
    let mut recycled = execution.clone();
    recycled.observation.as_mut().unwrap().pid_start = Some(0);
    save(&gateway, &recycled);
    let denied = cli(&socket, "execution", &["pr", "body"]).await;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("worker_attribution_denied"));
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn context_snapshot_and_hub_reply_keep_revision_and_observation_time() {
    let root = tempfile::tempdir().unwrap();
    let execution = execution(root.path());
    let gateway = open(root.path());
    save(&gateway, &execution);
    let bundle = WorkerContextBundle {
        task: Task::builder()
            .id("task")
            .product_id("product")
            .created_at("1")
            .description("Build gateway")
            .kind(TaskKind::Task)
            .name("Gateway")
            .status(TaskStatus::Todo)
            .updated_at("1")
            .build(),
        product: Product::builder()
            .id("product")
            .created_at("1")
            .description("Workers")
            .name("Boss")
            .slug("boss")
            .status("active")
            .updated_at("1")
            .build(),
        project: None,
        sibling_tasks: vec![],
        attention_groups: vec![],
        proposals: vec![],
        own_dependencies: WorkItemDependencyDetail {
            work_item_id: "task".into(),
            dependents: vec![],
            prerequisites: vec![],
        },
    };
    gateway
        .provision(
            &command(
                &identity(&execution),
                command::Body::Spawn(v1::Spawn {
                    run_context_json: serde_json::to_vec(&bundle).unwrap(),
                    context_revision: "snapshot-7".into(),
                    ..Default::default()
                }),
            ),
            &stamp().unwrap(),
        )
        .unwrap();
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let cached = cli(&socket, "execution", &["context"]).await;
    let freshness = String::from_utf8_lossy(&cached.stderr);
    assert!(freshness.contains("cached") && freshness.contains("snapshot-7") && freshness.contains("1234"));
    assert_eq!(success(cached)["task"]["name"], "Gateway");
    let fetch = tokio::spawn({
        let socket = socket.clone();
        async move { cli(&socket, "execution", &["pr", "body"]).await }
    });
    let request = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(request) = records(&gateway).into_iter().find_map(|r| match r.body {
                Some(record::Body::Request(r)) => Some(r),
                _ => None,
            }) {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let reply = command(
        &identity(&execution),
        command::Body::WorkerReply(v1::WorkerReply {
            request_id: request.request_id,
            code: v1::ResultCode::Ok as i32,
            result_json: serde_json::to_vec(&FrontendEvent::PrBodyResult {
                body: PrBodyView {
                    pr_url: None,
                    title: Some("Gateway".into()),
                    body: Some("Evidence".into()),
                },
            })
            .unwrap(),
            revision: "hub-8".into(),
            observed_unix_ms: 4567,
            reason: String::new(),
        }),
    );
    let mut foreign = reply.clone();
    foreign.run.as_mut().unwrap().spawn_token = "foreign".into();
    assert!(gateway.apply(&foreign).is_err());
    foreign = reply.clone();
    foreign.run.as_mut().unwrap().assignment_generation += 1;
    assert!(gateway.apply(&foreign).is_err());
    gateway.apply(&reply).unwrap();
    let live = fetch.await.unwrap();
    assert!(String::from_utf8_lossy(&live.stderr).contains("hub-8"));
    assert_eq!(success(live)["body"], "Evidence");
    let cached = cli(&socket, "execution", &["pr", "body"]).await;
    let freshness = String::from_utf8_lossy(&cached.stderr);
    assert!(freshness.contains("cached") && freshness.contains("4567"));
    assert_eq!(success(cached)["title"], "Gateway");
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn artifacts_are_validated_durable_and_chunked_without_losing_metadata() {
    let root = tempfile::tempdir().unwrap();
    let execution = execution(root.path());
    let gateway = open(root.path());
    save(&gateway, &execution);
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let path = root.path().join("image.png");
    // The existing image validator reads the PNG signature and IHDR dimensions.
    let mut bytes = vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0,
    ];
    bytes.resize(150_000, 0);
    std::fs::write(&path, &bytes).unwrap();
    let args = ["attach", path.to_str().unwrap(), "--caption", "gateway evidence"];
    let attached = success(cli(&socket, "execution", &args).await);
    assert_eq!(attached["status"], "queued");
    assert_eq!(
        success(cli(&socket, "execution", &args).await)["value"]["already_registered"],
        true
    );
    assert!(
        records(&gateway).is_empty(),
        "registration does not preallocate bulk sequences"
    );
    std::fs::remove_file(&path).unwrap();
    for _ in 0..3 {
        gateway.pump(&identity(&execution), &stamp().unwrap()).unwrap();
    }
    let chunks: Vec<_> = records(&gateway)
        .into_iter()
        .filter_map(|r| match r.body {
            Some(record::Body::Artifact(a)) => Some(a),
            _ => None,
        })
        .collect();
    assert_eq!(chunks.len(), 3);
    assert!(chunks.last().unwrap().eof);
    assert_eq!(chunks.iter().flat_map(|c| c.data.clone()).collect::<Vec<_>>(), bytes);
    assert_eq!(
        serde_json::from_slice::<Value>(&chunks[0].metadata_json).unwrap()["caption"],
        "gateway evidence"
    );
    for suffix in 1..boss_protocol::ATTACHMENT_CAP_PER_EXECUTION {
        bytes[100] = suffix as u8;
        std::fs::write(&path, &bytes).unwrap();
        success(cli(&socket, "execution", &args).await);
    }
    bytes[100] = 255;
    std::fs::write(&path, &bytes).unwrap();
    assert!(!cli(&socket, "execution", &args).await.status.success());
    // Filling the attachment allowance must not prevent capturing a PR.
    let output = root.path().join("pr-url.json");
    std::fs::write(&output, br#"{"pr_url":"https://github.com/spinyfin/mono/pull/123"}"#).unwrap();
    let mut execution = execution.clone();
    execution
        .worker_outputs
        .insert("BOSS_PR_URL_OUTPUT".into(), output.display().to_string());
    gateway.capture_outputs(&execution, &stamp().unwrap()).unwrap();
    let malformed = root.path().join("invalid.png");
    std::fs::write(&malformed, b"not an image").unwrap();
    assert!(
        !cli(&socket, "execution", &["attach", malformed.to_str().unwrap()])
            .await
            .status
            .success()
    );
    assert!(
        !cli(
            &socket,
            "execution",
            &["attach", malformed.to_str().unwrap(), "--caption", &"x".repeat(241)]
        )
        .await
        .status
        .success()
    );
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn failed_durable_commit_never_returns_a_queued_receipt() {
    let root = tempfile::tempdir().unwrap();
    let gateway = open_with(
        root.path(),
        Limits {
            spool_bytes: 4096,
            lifecycle_reserve: 1024,
        },
    );
    let execution = execution(root.path());
    save(&gateway, &execution);
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let output = cli(
        &socket,
        "execution",
        &["propose", "wait", "--reason", &"r".repeat(4000), "--duration", "1m"],
    )
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("spool full"));
    assert!(records(&gateway).is_empty());
    assert!(
        gateway
            .journal
            .lock()
            .unwrap()
            .source(&identity(&execution), crate::STATE)
            .unwrap()
            .is_none()
    );
    task.abort();
    let _ = task.await;
}

fn proposals(gateway: &Gateway) -> Vec<v1::Proposal> {
    records(gateway)
        .into_iter()
        .filter_map(|r| match r.body {
            Some(record::Body::Proposal(p)) => Some(p),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn keyless_wait_renews_while_keyed_and_other_proposals_replay() {
    let root = tempfile::tempdir().unwrap();
    let gateway = open(root.path());
    save(&gateway, &execution(root.path()));
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let wait = ["propose", "wait", "--reason", "CI is running", "--duration", "1m"];
    let first = success(cli(&socket, "execution", &wait).await);
    let second = success(cli(&socket, "execution", &wait).await);
    assert_eq!(first["value"]["already_submitted"], false);
    assert_eq!(second["value"]["already_submitted"], false);
    assert_ne!(first["value"]["receipt"]["id"], second["value"]["receipt"]["id"]);
    assert_eq!(proposals(&gateway).len(), 2);
    let keyed = [
        "propose",
        "wait",
        "--reason",
        "CI is running",
        "--duration",
        "1m",
        "--idempotency-key",
        "wait-1",
    ];
    success(cli(&socket, "execution", &keyed).await);
    let again = success(cli(&socket, "execution", &keyed).await);
    assert_eq!(again["value"]["already_submitted"], true);
    assert_eq!(proposals(&gateway).len(), 3);
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn guide_outcome_is_a_queued_run_scoped_mutation() {
    let root = tempfile::tempdir().unwrap();
    let gateway = open(root.path());
    save(&gateway, &execution(root.path()));
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let args = [
        "comment",
        "guide-outcome",
        "--comment-id",
        "cmt_1",
        "--disposition",
        "answered",
        "--body",
        "The guide already covers this",
    ];
    let queued = success(cli(&socket, "execution", &args).await);
    assert_eq!(queued["status"], "queued");
    assert_eq!(queued["value"]["receipt"]["kind"], "guide_outcome");
    let repeat = success(cli(&socket, "execution", &args).await);
    assert_eq!(repeat["value"]["already_submitted"], true);
    let stored = proposals(&gateway);
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].payload_contract, "guide-outcome/1");
    let payload: Value = serde_json::from_slice(&stored[0].canonical_json).unwrap();
    assert_eq!(payload["run_id"], "execution");
    assert_eq!(payload["comment_id"], "cmt_1");
    assert_eq!(payload["disposition"], "answered");
    // Bound to the caller's own run, and body is required.
    let foreign = cli(&socket, "sibling", &args).await;
    assert!(!foreign.status.success());
    assert!(String::from_utf8_lossy(&foreign.stderr).contains("worker_attribution_mismatch"));
    let empty = cli(
        &socket,
        "execution",
        &[
            "comment",
            "guide-outcome",
            "--comment-id",
            "cmt_1",
            "--disposition",
            "no_change",
            "--body",
            "   ",
        ],
    )
    .await;
    assert!(!empty.status.success());
    assert_eq!(proposals(&gateway).len(), 1);
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn done_watermark_is_the_observed_source_not_the_ingested_cursor() {
    let root = tempfile::tempdir().unwrap();
    let execution = execution(root.path());
    let gateway = open(root.path());
    save(&gateway, &execution);
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let done = ["propose", "done", "--outcome", "delivered", "--summary", "Done"];
    // The ingestion cursor stands at 100 bytes while the file has 5000.
    gateway
        .journal
        .lock()
        .unwrap()
        .commit_source(
            &identity(&execution),
            "transcript",
            None,
            br#"{"incarnation":1,"cursor":{"generation":0,"offset":100}}"#,
            vec![],
        )
        .unwrap();
    std::fs::write(root.path().join("transcript.log"), vec![b'x'; 5000]).unwrap();
    success(cli(&socket, "execution", &done).await);
    let stored = proposals(&gateway);
    assert_eq!(stored[0].transcript_through, 5000);
    assert_eq!(stored[0].transcript_generation, "1-0");
    // An unresolvable source is explicit evidence, never a silent zero.
    std::fs::remove_file(root.path().join("transcript.log")).unwrap();
    success(
        cli(
            &socket,
            "execution",
            &["propose", "wait", "--reason", "x", "--duration", "1m"],
        )
        .await,
    );
    assert!(records(&gateway).iter().any(|r| matches!(
        &r.body,
        Some(record::Body::Gap(g)) if g.resource == "transcript" && g.reason.contains("unavailable")
    )));
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn done_is_queued_even_when_output_capture_fails() {
    let root = tempfile::tempdir().unwrap();
    let mut first = execution(root.path());
    let structured = root.path().join("structured.json");
    std::fs::write(&structured, b"this is not json").unwrap();
    first
        .worker_outputs
        .insert("BOSS_STRUCTURED_OUTPUT".into(), structured.display().to_string());
    let gateway = open(root.path());
    save(&gateway, &first);
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let done = ["propose", "done", "--outcome", "delivered", "--summary", "Done"];
    let queued = success(cli(&socket, "execution", &done).await);
    assert_eq!(queued["status"], "queued");
    assert!(records(&gateway).iter().any(|r| matches!(
        &r.body,
        Some(record::Body::Gap(g)) if g.resource == "output:BOSS_STRUCTURED_OUTPUT" && g.reason.contains("not JSON")
    )));
    task.abort();
    let _ = task.await;

    // A nearly full bulk allowance cannot stop done either.
    let root = tempfile::tempdir().unwrap();
    let mut execution = execution(root.path());
    let output = root.path().join("pr-url.json");
    std::fs::write(&output, br#"{"pr_url":"https://github.com/spinyfin/mono/pull/123"}"#).unwrap();
    execution
        .worker_outputs
        .insert("BOSS_PR_URL_OUTPUT".into(), output.display().to_string());
    let gateway = open_with(
        root.path(),
        Limits {
            spool_bytes: 6000,
            lifecycle_reserve: 2500,
        },
    );
    save(&gateway, &execution);
    gateway
        .journal
        .lock()
        .unwrap()
        .commit_source(
            &identity(&execution),
            "worker-blob:filler:0",
            None,
            &[7u8; 3000],
            vec![],
        )
        .unwrap();
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let queued = success(cli(&socket, "execution", &done).await);
    assert_eq!(queued["status"], "queued");
    assert_eq!(proposals(&gateway).len(), 1);
    task.abort();
    let _ = task.await;
}

fn png(size: usize) -> Vec<u8> {
    let mut bytes = vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0,
    ];
    bytes.resize(size, 0);
    bytes
}

fn chunks(gateway: &Gateway) -> Vec<v1::ArtifactChunk> {
    records(gateway)
        .into_iter()
        .filter_map(|r| match r.body {
            Some(record::Body::Artifact(a)) => Some(a),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn staged_artifact_bytes_move_into_records_and_are_released_with_the_run() {
    let root = tempfile::tempdir().unwrap();
    let mut execution = execution(root.path());
    let gateway = open(root.path());
    save(&gateway, &execution);
    let baseline = gateway.journal.lock().unwrap().bytes().unwrap();
    let path = root.path().join("image.png");
    let bytes = png(150_000);
    std::fs::write(&path, &bytes).unwrap();
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let attached = success(cli(&socket, "execution", &["attach", path.to_str().unwrap()]).await);
    let artifact = attached["value"]["artifact"]["id"].as_str().unwrap().to_owned();
    task.abort();
    let _ = task.await;
    let staged = gateway.journal.lock().unwrap().bytes().unwrap();
    assert!(staged >= baseline + 150_000);
    // Not streamed yet, so a reclaim must keep the staged bytes.
    execution.reclaimed = true;
    gateway.release_if_done(&identity(&execution)).unwrap();
    assert!(
        gateway
            .journal
            .lock()
            .unwrap()
            .source(&identity(&execution), crate::STATE)
            .unwrap()
            .is_some()
    );
    let staged_keys: Vec<_> = (0..3)
        .map(|index| format!("worker-blob:{artifact}:{index}"))
        .chain([format!("worker-blob:{artifact}:reserve")])
        .collect();
    {
        let journal = gateway.journal.lock().unwrap();
        for source in &staged_keys {
            assert!(
                journal.source(&identity(&execution), source).unwrap().is_some(),
                "{source} should be staged before streaming"
            );
        }
    }
    gateway.pump(&identity(&execution), &stamp().unwrap()).unwrap();
    let streamed = gateway.journal.lock().unwrap().bytes().unwrap();
    // Records carry the bytes now; staging no longer counts them a second time.
    assert!(
        streamed < staged + 16 * 1024,
        "streaming must not double the footprint: {staged} -> {streamed}"
    );
    assert_eq!(
        chunks(&gateway).iter().flat_map(|c| c.data.clone()).collect::<Vec<_>>(),
        bytes
    );
    gateway.release_if_done(&identity(&execution)).unwrap();
    let journal = gateway.journal.lock().unwrap();
    // The staged keys derive from the registered artifact, so each of these
    // names a source that really existed before release.
    let staged_keys = (0..3)
        .map(|index| format!("worker-blob:{artifact}:{index}"))
        .chain([format!("worker-blob:{artifact}:reserve"), crate::STATE.to_owned()]);
    for source in staged_keys {
        assert!(
            journal.source(&identity(&execution), &source).unwrap().is_none(),
            "{source} was not released"
        );
    }
    let released = journal.bytes().unwrap();
    assert!(
        released < streamed,
        "gateway rows are released: {streamed} -> {released}"
    );
    assert!(released > baseline, "numbered records remain until receipted");
}

#[tokio::test]
async fn an_accepted_artifact_reaches_eof_on_a_small_spool() {
    let root = tempfile::tempdir().unwrap();
    let execution = execution(root.path());
    let gateway = open_with(
        root.path(),
        Limits {
            spool_bytes: 500_000,
            lifecycle_reserve: 50_000,
        },
    );
    save(&gateway, &execution);
    let path = root.path().join("image.png");
    let bytes = png(220_000);
    std::fs::write(&path, &bytes).unwrap();
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    success(cli(&socket, "execution", &["attach", path.to_str().unwrap()]).await);
    task.abort();
    let _ = task.await;
    for _ in 0..8 {
        gateway.pump(&identity(&execution), &stamp().unwrap()).unwrap();
    }
    let chunks = chunks(&gateway);
    assert!(chunks.last().unwrap().eof);
    assert_eq!(chunks.iter().flat_map(|c| c.data.clone()).collect::<Vec<_>>(), bytes);
}

#[tokio::test]
async fn a_failed_registration_releases_what_it_staged() {
    let root = tempfile::tempdir().unwrap();
    let execution = execution(root.path());
    let gateway = open_with(
        root.path(),
        Limits {
            spool_bytes: 200_000,
            lifecycle_reserve: 20_000,
        },
    );
    save(&gateway, &execution);
    let baseline = gateway.journal.lock().unwrap().bytes().unwrap();
    let path = root.path().join("image.png");
    std::fs::write(&path, png(400_000)).unwrap();
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    assert!(
        !cli(&socket, "execution", &["attach", path.to_str().unwrap()])
            .await
            .status
            .success()
    );
    task.abort();
    let _ = task.await;
    assert_eq!(gateway.journal.lock().unwrap().bytes().unwrap(), baseline);
}

#[tokio::test]
async fn one_stalled_client_does_not_block_other_workers() {
    let root = tempfile::tempdir().unwrap();
    let gateway = open(root.path());
    save(&gateway, &execution(root.path()));
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    // Connects and says nothing for the whole exchange deadline.
    let _idle = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let started = std::time::Instant::now();
    success(
        cli(
            &socket,
            "execution",
            &["propose", "wait", "--reason", "x", "--duration", "1m"],
        )
        .await,
    );
    assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn a_late_hub_reply_fills_the_cache_and_leaves_no_rows_behind() {
    let root = tempfile::tempdir().unwrap();
    let execution = execution(root.path());
    let gateway = open(root.path());
    save(&gateway, &execution);
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    // No hub reply: the wait times out and its request row is deleted.
    let timed_out = cli(&socket, "execution", &["pr", "body"]).await;
    assert!(String::from_utf8_lossy(&timed_out.stderr).contains("hub_unavailable"));
    for _ in 0..3 {
        let repeated = cli(&socket, "execution", &["pr", "body"]).await;
        assert!(String::from_utf8_lossy(&repeated.stderr).contains("hub_unavailable"));
    }
    assert_eq!(
        records(&gateway)
            .iter()
            .filter(|r| matches!(r.body, Some(record::Body::Request(_))))
            .count(),
        1
    );
    // The numbered request record remains; the request row is replaced by a tombstone.
    let request_id = records(&gateway)
        .into_iter()
        .find_map(|r| match r.body {
            Some(record::Body::Request(request)) => Some(request.request_id),
            _ => None,
        })
        .unwrap();
    assert!(
        gateway
            .journal
            .lock()
            .unwrap()
            .source(&identity(&execution), &format!("worker-request:{request_id}"))
            .unwrap()
            .is_none()
    );
    // The timed-out wait left a tombstone, so a late reply still fills the
    // cache instead of being dropped.
    let late = |request_id: String| {
        command(
            &identity(&execution),
            command::Body::WorkerReply(v1::WorkerReply {
                request_id,
                code: v1::ResultCode::Ok as i32,
                result_json: serde_json::to_vec(&FrontendEvent::PrBodyResult {
                    body: PrBodyView {
                        pr_url: None,
                        title: Some("Late".into()),
                        body: Some("Arrived after the wait".into()),
                    },
                })
                .unwrap(),
                revision: "hub-9".into(),
                observed_unix_ms: 99,
                reason: String::new(),
            }),
        )
    };
    gateway.apply(&late(request_id.clone())).unwrap();
    // Replaying it, or answering an unknown request, is ignored rather than refused.
    gateway.apply(&late(request_id.clone())).unwrap();
    gateway.apply(&late("never-asked".into())).unwrap();
    {
        let journal = gateway.journal.lock().unwrap();
        let run = identity(&execution);
        let method = v1::WorkerMethod::PrBody as i32;
        assert!(
            journal
                .source(&run, &format!("worker-cache:{method}"))
                .unwrap()
                .is_some()
        );
        for leftover in [
            format!("worker-request:{request_id}"),
            format!("worker-reply:{request_id}"),
            format!("worker-late:{method}"),
        ] {
            assert!(
                journal.source(&run, &leftover).unwrap().is_none(),
                "{leftover} left behind"
            );
        }
    }
    let cached = cli(&socket, "execution", &["pr", "body"]).await;
    assert!(String::from_utf8_lossy(&cached.stderr).contains("cached"));
    assert_eq!(success(cached)["title"], "Late");
    task.abort();
    let _ = task.await;
}

fn pr_url_output(root: &Path, execution: &mut ExecutionRecord) {
    let output = root.join("pr-url.json");
    std::fs::write(&output, br#"{"pr_url":"https://github.com/spinyfin/mono/pull/123"}"#).unwrap();
    execution
        .worker_outputs
        .insert("BOSS_PR_URL_OUTPUT".into(), output.display().to_string());
}

fn pr_chunks(gateway: &Gateway) -> Vec<v1::ArtifactChunk> {
    chunks(gateway)
        .into_iter()
        .filter(|c| c.kind == v1::ArtifactKind::PrUrl as i32)
        .collect()
}

fn ack_all(gateway: &Gateway) {
    let mut journal = gateway.journal.lock().unwrap();
    let (_, journal_id) = journal.identity().unwrap();
    let (_, through) = journal.record_bounds().unwrap();
    journal.receipt(&v1::Cursor { journal_id, through }).unwrap();
}

#[tokio::test]
async fn run_scoped_writes_from_real_prompts_queue_as_records_and_the_rest_stay_denied() {
    let root = tempfile::tempdir().unwrap();
    let gateway = open(root.path());
    save(&gateway, &execution(root.path()));
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let producer = [
        "engine",
        "conflicts",
        "record-producer",
        "--execution-id",
        "execution",
        "--head-branch",
        "boss/exec_1",
        "--base-branch",
        "main",
        "--files",
        "a.rs,b.rs",
    ];
    let queued = success(cli(&socket, "execution", &producer).await);
    assert_eq!(queued["status"], "queued");
    assert_eq!(queued["value"]["receipt"]["kind"], "conflict_producer");
    assert_eq!(
        success(cli(&socket, "execution", &producer).await)["value"]["already_submitted"],
        true
    );
    let human = cli_format(&socket, "execution", &producer, false).await;
    assert!(human.status.success());
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("keep working") && !text.contains("finish this turn"));
    let mut foreign = producer;
    foreign[4] = "sibling";
    let wrong_run = cli(&socket, "execution", &foreign).await;
    assert!(String::from_utf8_lossy(&wrong_run.stderr).contains("worker_attribution_mismatch"));
    let conflict = success(
        cli(
            &socket,
            "execution",
            &[
                "engine",
                "conflicts",
                "mark-failed",
                "att_1",
                "--reason",
                "product_decision_required",
            ],
        )
        .await,
    );
    assert_eq!(conflict["value"]["receipt"]["kind"], "conflict_mark_failed");
    let unknown_reason = cli(
        &socket,
        "execution",
        &["engine", "conflicts", "mark-failed", "att_1", "--reason", "because"],
    )
    .await;
    assert!(!unknown_reason.status.success());
    let ci = success(
        cli(
            &socket,
            "execution",
            &[
                "engine",
                "ci",
                "mark-failed",
                "--attempt-id",
                "cir_1",
                "--reason",
                "unfixable",
            ],
        )
        .await,
    );
    assert_eq!(ci["value"]["receipt"]["kind"], "ci_mark_failed");
    let reply = success(
        cli(
            &socket,
            "execution",
            &["comment", "reply", "--body", "Here is the answer"],
        )
        .await,
    );
    assert_eq!(reply["value"]["receipt"]["kind"], "answer_reply");
    let second = cli(&socket, "execution", &["comment", "reply", "--body", "A second answer"]).await;
    assert!(!second.status.success(), "an answer agent posts one reply");
    let by_contract = |contract: &str| {
        proposals(&gateway)
            .into_iter()
            .filter(|p| p.payload_contract == contract)
            .count()
    };
    for contract in [
        "conflict-producer/1",
        "conflict-mark-failed/1",
        "ci-mark-failed/1",
        "answer-reply/1",
    ] {
        assert_eq!(by_contract(contract), 1, "{contract}");
    }
    // Verbs that need a synchronous hub verdict stay denied.
    let denied = cli(
        &socket,
        "execution",
        &["engine", "ci", "mark-noop", "--attempt-id", "cir_1"],
    )
    .await;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("worker_method_denied"));
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn guide_outcomes_are_bounded_per_comment_and_never_spend_the_proposal_budget() {
    let root = tempfile::tempdir().unwrap();
    let gateway = open(root.path());
    save(&gateway, &execution(root.path()));
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let outcome = |comment: usize, body: &str| {
        [
            "comment".to_owned(),
            "guide-outcome".to_owned(),
            "--comment-id".to_owned(),
            format!("cmt_{comment}"),
            "--disposition".to_owned(),
            "answered".to_owned(),
            "--body".to_owned(),
            body.to_owned(),
        ]
    };
    for comment in 0..40 {
        let args = outcome(comment, "grounded");
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        success(cli(&socket, "execution", &args).await);
    }
    // Re-recording a comment replaces its outcome instead of adding to the total.
    let args = outcome(3, "revised after the regeneration");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    success(cli(&socket, "execution", &args).await);
    let (_, state) = crate::state(&gateway.journal.lock().unwrap(), &identity(&execution(root.path()))).unwrap();
    assert_eq!(state.proposals.len(), 40);
    assert_eq!(state.superseded.len(), 1);
    let old_id = state.superseded[0].id.clone();
    gateway
        .apply(&command(
            &identity(&execution(root.path())),
            command::Body::ProposalDecision(v1::ProposalDecision {
                proposal_id: old_id.clone(),
                code: v1::ResultCode::Ok as i32,
                reason: String::new(),
                result_json: b"{}".to_vec(),
            }),
        ))
        .unwrap();
    let args = outcome(3, "grounded");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let retry = success(cli(&socket, "execution", &args).await);
    assert_eq!(retry["value"]["receipt"]["id"], old_id);
    assert_eq!(retry["value"]["already_submitted"], true);
    // Forty outcomes leave every proposal kind its full budget, done included.
    let done = ["propose", "done", "--outcome", "delivered", "--summary", "Done"];
    assert_eq!(success(cli(&socket, "execution", &done).await)["status"], "queued");
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn an_undecodable_execution_row_does_not_deny_other_workers() {
    let root = tempfile::tempdir().unwrap();
    let gateway = open(root.path());
    save(&gateway, &execution(root.path()));
    gateway
        .journal
        .lock()
        .unwrap()
        .save_execution("corrupt", "token", b"{not an execution")
        .unwrap();
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let wait = ["propose", "wait", "--reason", "CI is running", "--duration", "1m"];
    assert_eq!(success(cli(&socket, "execution", &wait).await)["status"], "queued");
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn a_turn_stop_captures_outputs_while_the_worker_stays_alive() {
    let root = tempfile::tempdir().unwrap();
    let mut execution = execution(root.path());
    pr_url_output(root.path(), &mut execution);
    let gateway = open(root.path());
    save(&gateway, &execution);
    let run = identity(&execution);
    // No boundary has been observed: an Alive worker may still rewrite the file.
    gateway.upkeep(&execution, &stamp().unwrap());
    assert!(pr_chunks(&gateway).is_empty());
    crate::note_turn_stop(&gateway.journal, &run).unwrap();
    assert!(!crate::captures_concluded(&gateway.journal, &execution).unwrap());
    gateway.upkeep(&execution, &stamp().unwrap());
    let streamed = pr_chunks(&gateway);
    assert!(streamed.last().is_some_and(|c| c.eof));
    assert!(String::from_utf8_lossy(&streamed[0].data).contains("/pull/123"));
    assert!(crate::captures_concluded(&gateway.journal, &execution).unwrap());
}

#[tokio::test]
async fn a_transient_failure_at_done_is_retried_once_capacity_returns() {
    let root = tempfile::tempdir().unwrap();
    let mut execution = execution(root.path());
    pr_url_output(root.path(), &mut execution);
    let gateway = open_with(
        root.path(),
        Limits {
            spool_bytes: 12_000,
            lifecycle_reserve: 3_000,
        },
    );
    save(&gateway, &execution);
    let run = identity(&execution);
    gateway
        .journal
        .lock()
        .unwrap()
        .commit_source(&run, "worker-blob:filler:0", None, &[7u8; 7_000], vec![])
        .unwrap();
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let done = ["propose", "done", "--outcome", "delivered", "--summary", "Done"];
    assert_eq!(success(cli(&socket, "execution", &done).await)["status"], "queued");
    task.abort();
    let _ = task.await;
    assert!(pr_chunks(&gateway).is_empty());
    // The worker is parked and Alive; the owed capture is still pending.
    assert!(!crate::captures_concluded(&gateway.journal, &execution).unwrap());
    gateway.upkeep(&execution, &stamp().unwrap());
    assert!(pr_chunks(&gateway).is_empty(), "still no room");
    gateway
        .journal
        .lock()
        .unwrap()
        .delete_sources(&run, &["worker-blob:filler:0".to_owned()])
        .unwrap();
    gateway.upkeep(&execution, &stamp().unwrap());
    assert!(pr_chunks(&gateway).last().is_some_and(|c| c.eof));
    assert!(crate::captures_concluded(&gateway.journal, &execution).unwrap());
}

#[tokio::test]
async fn exit_capture_gates_reclaim_when_drain_finishes_between_ticks() {
    let root = tempfile::tempdir().unwrap();
    let mut execution = execution(root.path());
    pr_url_output(root.path(), &mut execution);
    execution.phase = Phase::Terminal;
    let gateway = open(root.path());
    save(&gateway, &execution);
    // Drain is complete but the gateway has not yet read the output file.
    assert!(!crate::captures_concluded(&gateway.journal, &execution).unwrap());
    gateway.upkeep(&execution, &stamp().unwrap());
    assert!(crate::captures_concluded(&gateway.journal, &execution).unwrap());
    assert!(pr_chunks(&gateway).last().is_some_and(|c| c.eof));
    // A run with nothing to capture concludes on its first tick too.
    let mut idle = execution.clone();
    idle.worker_outputs.clear();
    idle.run_id = "idle".into();
    idle.spawn_token = "idle-token".into();
    save(&gateway, &idle);
    assert!(!crate::captures_concluded(&gateway.journal, &idle).unwrap());
    gateway.upkeep(&idle, &stamp().unwrap());
    assert!(crate::captures_concluded(&gateway.journal, &idle).unwrap());
}

#[tokio::test]
async fn a_rejected_done_reopens_capture_so_the_corrected_output_streams() {
    let root = tempfile::tempdir().unwrap();
    let mut execution = execution(root.path());
    let structured = root.path().join("structured.json");
    std::fs::write(&structured, b"this is not json").unwrap();
    execution
        .worker_outputs
        .insert("BOSS_STRUCTURED_OUTPUT".into(), structured.display().to_string());
    let gateway = open(root.path());
    save(&gateway, &execution);
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let done = ["propose", "done", "--outcome", "delivered", "--summary", "Done"];
    let queued = success(cli(&socket, "execution", &done).await);
    let id = queued["value"]["receipt"]["id"].as_str().unwrap().to_owned();
    assert!(records(&gateway).iter().any(|r| matches!(
        &r.body,
        Some(record::Body::Gap(g)) if g.resource == "output:BOSS_STRUCTURED_OUTPUT"
    )));
    gateway
        .apply(&command(
            &identity(&execution),
            command::Body::ProposalDecision(v1::ProposalDecision {
                proposal_id: id,
                code: v1::ResultCode::Rejected as i32,
                reason: "structured output is invalid".into(),
                result_json: b"{}".to_vec(),
            }),
        ))
        .unwrap();
    crate::note_turn_stop(&gateway.journal, &identity(&execution)).unwrap();
    gateway.upkeep(&execution, &stamp().unwrap());
    std::fs::write(&structured, br#"{"verdict":"corrected"}"#).unwrap();
    // The worker finishes again; the replay is refused but the capture runs.
    let _ = cli(&socket, "execution", &done).await;
    for _ in 0..2 {
        gateway.pump(&identity(&execution), &stamp().unwrap()).unwrap();
    }
    let replacement: Vec<_> = chunks(&gateway)
        .into_iter()
        .filter(|c| c.kind == v1::ArtifactKind::ReviewResult as i32)
        .collect();
    assert!(replacement.last().is_some_and(|c| c.eof));
    assert!(String::from_utf8_lossy(&replacement[0].data).contains("corrected"));
    task.abort();
    let _ = task.await;
}

fn pin_small_credit(gateway: &Gateway, execution: &ExecutionRecord) {
    gateway
        .journal
        .lock()
        .unwrap()
        .append(
            v1::Record {
                run: Some(identity(execution)),
                observed: Some(stamp().unwrap()),
                body: Some(record::Body::Gap(v1::Gap {
                    resource: "credit".into(),
                    reason: "x".repeat(245_000),
                    ..Default::default()
                })),
                ..Default::default()
            },
            boss_spoke_journal::Admission::Lifecycle,
        )
        .unwrap();
    let credit = gateway.journal.lock().unwrap().bulk_credit().unwrap();
    assert!((8_000..16_000).contains(&credit), "{credit}");
}

fn accepts_artifact(limits: Limits, size: usize) -> bool {
    let root = tempfile::tempdir().unwrap();
    let execution = execution(root.path());
    let gateway = open_with(root.path(), limits);
    save(&gateway, &execution);
    pin_small_credit(&gateway, &execution);
    let path = root.path().join("image.png");
    std::fs::write(&path, png(size)).unwrap();
    gateway
        .attach(&execution, path.to_str().unwrap(), None, &stamp().unwrap())
        .is_ok()
}

#[tokio::test]
async fn an_artifact_accepted_at_the_bulk_boundary_streams_to_eof_without_new_capacity() {
    let limits = Limits {
        spool_bytes: 500_000,
        lifecycle_reserve: 50_000,
    };
    let (mut accepted, mut refused) = (100_000usize, 420_000usize);
    assert!(accepts_artifact(limits, accepted) && !accepts_artifact(limits, refused));
    while refused - accepted > 1 {
        let middle = (accepted + refused) / 2;
        if accepts_artifact(limits, middle) {
            accepted = middle;
        } else {
            refused = middle;
        }
    }
    let root = tempfile::tempdir().unwrap();
    let execution = execution(root.path());
    let gateway = open_with(root.path(), limits);
    save(&gateway, &execution);
    pin_small_credit(&gateway, &execution);
    let path = root.path().join("image.png");
    let bytes = png(accepted);
    std::fs::write(&path, &bytes).unwrap();
    gateway
        .attach(&execution, path.to_str().unwrap(), None, &stamp().unwrap())
        .unwrap();
    for _ in 0..8 {
        gateway.pump(&identity(&execution), &stamp().unwrap()).unwrap();
        assert!(
            chunks(&gateway).is_empty(),
            "small credit must not spend the overhead reserve"
        );
    }
    ack_all(&gateway);
    for _ in 0..64 {
        match gateway.pump(&identity(&execution), &stamp().unwrap()) {
            Ok(()) => {}
            Err(error) if boss_spoke_journal::Fault::is_full(&error) => ack_all(&gateway),
            Err(error) => panic!("{error:#}"),
        }
        ack_all(&gateway);
    }
    let chunks = chunks(&gateway);
    assert!(chunks.last().unwrap().eof, "an accepted artifact must reach EOF");
    assert_eq!(chunks.iter().flat_map(|c| c.data.clone()).collect::<Vec<_>>(), bytes);
}

#[tokio::test]
async fn partial_credit_below_one_chunk_never_counts_an_emitted_prefix_twice() {
    let root = tempfile::tempdir().unwrap();
    let execution = execution(root.path());
    let gateway = open(root.path());
    save(&gateway, &execution);
    let run = identity(&execution);
    // Unreceipted records pin most of the result window, leaving little credit.
    for _ in 0..11 {
        gateway
            .journal
            .lock()
            .unwrap()
            .append(
                v1::Record {
                    run: Some(run.clone()),
                    observed: Some(stamp().unwrap()),
                    body: Some(record::Body::Gap(v1::Gap {
                        resource: "pad".into(),
                        reason: "x".repeat(20_000),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                boss_spoke_journal::Admission::Lifecycle,
            )
            .unwrap();
    }
    assert!(gateway.journal.lock().unwrap().bulk_credit().unwrap() < 64 * 1024);
    let path = root.path().join("image.png");
    let bytes = png(150_000);
    std::fs::write(&path, &bytes).unwrap();
    let before = gateway.journal.lock().unwrap().bytes().unwrap();
    gateway
        .attach(&execution, path.to_str().unwrap(), None, &stamp().unwrap())
        .unwrap();
    for _ in 0..64 {
        match gateway.pump(&run, &stamp().unwrap()) {
            Ok(()) => {}
            Err(error) if boss_spoke_journal::Fault::is_full(&error) => {}
            Err(error) => panic!("{error:#}"),
        }
        ack_all(&gateway);
    }
    let chunks = chunks(&gateway);
    assert!(chunks.last().unwrap().eof);
    assert!(chunks.len() > 3, "credit below a chunk splits chunks");
    assert_eq!(chunks.iter().flat_map(|c| c.data.clone()).collect::<Vec<_>>(), bytes);
    // The bytes live in records now; staging holds nothing for them.
    let after = gateway.journal.lock().unwrap().bytes().unwrap();
    assert!(after < before + 150_000 + 24 * 1024, "{before} -> {after}");
}

#[tokio::test]
async fn architectural_mismatch_requires_valid_evidence_from_the_cli() {
    let root = tempfile::tempdir().unwrap();
    let gateway = open(root.path());
    save(&gateway, &execution(root.path()));
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    let base = [
        "engine",
        "conflicts",
        "mark-failed",
        "attempt",
        "--reason",
        "architectural_mismatch",
    ];
    assert!(!cli(&socket, "execution", &base).await.status.success());
    let mut args = base.to_vec();
    args.extend([
        "--basis",
        "subsystem_deleted",
        "--conflicting-commits",
        "abc123",
        "--main-changes",
        "The queue was deleted",
        "--attempted-port",
        "Tried using the dispatcher",
        "--recommendation",
        "Rewrite around the dispatcher",
        "--contradiction",
        "too short",
    ]);
    let invalid = cli(&socket, "execution", &args).await;
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("evidence"));
    *args.last_mut().unwrap() = "Main deleted the review queue entirely, so queue priority has nowhere to live.";
    assert_eq!(success(cli(&socket, "execution", &args).await)["status"], "queued");
    let missing = gateway
        .submit_attempt_failure(
            &execution(root.path()),
            crate::RunWriteKind::ConflictMarkFailed,
            "attempt".into(),
            "architectural_mismatch".into(),
            serde_json::json!({"evidence": null}),
            &stamp().unwrap(),
        )
        .unwrap();
    assert!(matches!(missing, FrontendEvent::ProposalRejected { .. }));
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn continuation_and_locally_rejected_done_do_not_freeze_output_bytes() {
    let root = tempfile::tempdir().unwrap();
    let mut execution = execution(root.path());
    let path = root.path().join("output.json");
    execution
        .worker_outputs
        .insert("BOSS_STRUCTURED_OUTPUT".into(), path.display().to_string());
    let gateway = open(root.path());
    save(&gateway, &execution);
    std::fs::write(&path, br#"{"result":"first"}"#).unwrap();
    crate::note_turn_stop(&gateway.journal, &identity(&execution)).unwrap();
    gateway.upkeep(&execution, &stamp().unwrap());
    assert_eq!(chunks(&gateway).len(), 1);
    let socket = root.path().join("worker.sock");
    let task = server(&gateway, &socket);
    std::fs::write(&path, br#"{"result":"invalid-done"}"#).unwrap();
    let invalid = cli(
        &socket,
        "execution",
        &["propose", "done", "--outcome", "delivered", "--summary", ""],
    )
    .await;
    assert!(!invalid.status.success());
    gateway.upkeep(&execution, &stamp().unwrap());
    assert_eq!(chunks(&gateway).len(), 1, "refused done must not capture");
    std::fs::write(&path, br#"{"result":"corrected"}"#).unwrap();
    success(
        cli(
            &socket,
            "execution",
            &["propose", "done", "--outcome", "delivered", "--summary", "Done"],
        )
        .await,
    );
    gateway.upkeep(&execution, &stamp().unwrap());
    assert_eq!(chunks(&gateway).len(), 2);
    assert!(String::from_utf8_lossy(&chunks(&gateway)[1].data).contains("corrected"));
    execution.phase = Phase::Terminal;
    std::fs::write(&path, br#"{"result":"exit"}"#).unwrap();
    gateway.upkeep(&execution, &stamp().unwrap());
    assert!(String::from_utf8_lossy(&chunks(&gateway).last().unwrap().data).contains("exit"));
    task.abort();
    let _ = task.await;
}
