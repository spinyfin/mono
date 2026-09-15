//! File validation, streaming, and checkpoint resume regression tests.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use boss_protocol::{StopReason, WorkerEvent};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

use super::*;
use crate::agent_jsonl_discovery::DISCOVERY_POLL;
use crate::events_socket::IncomingHookEvent;

#[derive(Clone, Default)]
struct CaptureSink {
    events: Arc<Mutex<Vec<IncomingHookEvent>>>,
    attached_runs: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait::async_trait]
impl WorkerEventSink for CaptureSink {
    fn record_driver_attach(&self, run_id: &str) {
        self.attached_runs.lock().unwrap().push(run_id.to_owned());
        self.notify.notify_waiters();
    }

    async fn dispatch_worker_event(&self, incoming: IncomingHookEvent) {
        self.events.lock().unwrap().push(incoming);
        self.notify.notify_waiters();
    }
}

/// In-memory stand-in for the `work_runs` column, so a test can read back
/// the resume point the ingress actually wrote.
#[derive(Clone, Default)]
struct MemoryCheckpointStore {
    stored: Arc<Mutex<HashMap<String, IngressCheckpoint>>>,
}

impl MemoryCheckpointStore {
    fn get(&self, run_id: &str) -> Option<IngressCheckpoint> {
        self.stored.lock().unwrap().get(run_id).cloned()
    }
}

impl IngressCheckpointStore for MemoryCheckpointStore {
    fn store_ingress_checkpoint(&self, run_id: &str, checkpoint: &IngressCheckpoint) -> Result<(), String> {
        self.stored
            .lock()
            .unwrap()
            .insert(run_id.to_owned(), checkpoint.clone());
        Ok(())
    }

    fn load_ingress_checkpoint(&self, run_id: &str) -> Result<Option<IngressCheckpoint>, String> {
        Ok(self.get(run_id))
    }
}

fn test_store() -> Arc<dyn IngressCheckpointStore> {
    Arc::new(MemoryCheckpointStore::default())
}

/// The device/inode a checkpoint would have recorded for `path` as it is
/// on disk right now.
fn identity_of(path: &Path) -> FileIdentity {
    file_identity(&fs::metadata(path).unwrap())
}

/// Replace `path` with a file that has a different inode.
///
/// Unlink-and-recreate is not enough: linux-sandbox mounts `/tmp` as
/// tmpfs, which reuses the freed inode, so the "different file" check
/// would not see a rotation. Creating the replacement first (while the
/// original inode is still live) and renaming over keeps the new inode.
fn replace_with_new_inode(path: &Path, contents: impl AsRef<[u8]>) {
    let tmp = path.with_extension("replacement");
    fs::write(&tmp, contents).unwrap();
    fs::rename(&tmp, path).unwrap();
}

/// The tail as every pre-existing test drives it: from byte zero, with an
/// offset map nobody reads back. Shadows the production name so those
/// tests keep asserting on exactly the code path they always did.
async fn stream_file_bytes(
    prepared: PreparedSource,
    candidate: Candidate,
    writer: tokio::io::DuplexStream,
    halt: watch::Receiver<StreamHalt>,
) -> Result<(), String> {
    super::stream_file_bytes(
        prepared,
        candidate,
        0,
        Arc::new(Mutex::new(StreamFileMap::default())),
        writer,
        halt,
    )
    .await
}

async fn stream_file_bytes_with_test_hooks<F, G>(
    prepared: PreparedSource,
    candidate: Candidate,
    writer: tokio::io::DuplexStream,
    halt: watch::Receiver<StreamHalt>,
    before_descriptor_read: G,
    after_rotation_validation: F,
) -> Result<(), String>
where
    F: FnMut(&Candidate) -> Result<(), String> + Send,
    G: FnMut(&Path) -> Result<(), String> + Send,
{
    super::stream_file_bytes_with_test_hooks(
        prepared,
        candidate,
        0,
        Arc::new(Mutex::new(StreamFileMap::default())),
        writer,
        halt,
        before_descriptor_read,
        after_rotation_validation,
    )
    .await
}

fn rollout_text(workspace: &Path, thread_id: &str) -> String {
    [
        serde_json::json!({
            "type":"session_meta",
            "payload":{"id":thread_id,"cwd":workspace}
        }),
        serde_json::json!({
            "type":"event_msg",
            "payload":{"type":"task_started","turn_id":"turn-live"}
        }),
        serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"function_call",
                "name":"exec_command",
                "call_id":"call-live",
                "arguments":r#"{"cmd":"printf live"}"#
            }
        }),
        serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"function_call_output",
                "call_id":"call-live",
                "output":"live\n"
            }
        }),
        serde_json::json!({
            "type":"event_msg",
            "payload":{
                "type":"task_complete",
                "turn_id":"turn-live",
                "last_agent_message":"done"
            }
        }),
    ]
    .into_iter()
    .map(|record| serde_json::to_string(&record).unwrap())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n"
}

fn rollout_with_marker(workspace: &Path, thread_id: &str, marker: &str) -> String {
    let meta = serde_json::json!({
        "type":"session_meta",
        "payload":{"id":thread_id,"cwd":workspace}
    });
    let marker = serde_json::json!({"marker": marker});
    format!(
        "{}\n{}\n",
        serde_json::to_string(&meta).unwrap(),
        serde_json::to_string(&marker).unwrap()
    )
}

fn long_rollout_with_marker(workspace: &Path, thread_id: &str, marker: &str) -> String {
    let mut rollout = rollout_with_marker(workspace, thread_id, marker);
    rollout.push_str(
        &serde_json::to_string(&serde_json::json!({
            "padding": "x".repeat(2048)
        }))
        .unwrap(),
    );
    rollout.push('\n');
    rollout
}

#[derive(bon::Builder)]
#[builder(on(String, into))]
struct TailFixture {
    _temp: TempDir,
    prepared: PreparedSource,
    path: PathBuf,
    workspace: PathBuf,
    wrong_workspace: PathBuf,
    candidate: Candidate,
}

fn tail_fixture(thread_id: &str) -> TailFixture {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    let wrong_workspace = temp.path().join("wrong-workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&wrong_workspace).unwrap();
    let prepared = PreparedSource::new(AgentJsonlFileIngress {
        directory: sessions.clone(),
        filename_prefix: "rollout-".into(),
        filename_suffix: ".jsonl".into(),
        workspace_path: workspace.clone(),
    })
    .unwrap();
    let path = sessions.join(format!("rollout-test-{thread_id}.jsonl"));
    fs::write(&path, long_rollout_with_marker(&workspace, thread_id, "initial-stream")).unwrap();
    let candidate = validate_candidate(&prepared, &path).unwrap().unwrap();
    TailFixture {
        _temp: temp,
        prepared,
        path,
        workspace,
        wrong_workspace,
        candidate,
    }
}

#[tokio::test]
async fn prepared_rollout_uses_shared_reader_and_exact_run_correlation() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    let wrong_workspace = temp.path().join("other-workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&wrong_workspace).unwrap();

    // A same-workspace rollout that predates preparation belongs to an
    // earlier process and must remain excluded.
    fs::write(
        sessions.join("rollout-old-thread-old.jsonl"),
        rollout_text(&workspace, "thread-old"),
    )
    .unwrap();

    let manager = AgentJsonlProgressManager::new();
    let sink = CaptureSink::default();
    manager
        .prepare_run(
            "run-live",
            Arc::new(crate::driver::CodexDriver::default()),
            AgentJsonlFileIngress {
                directory: sessions.clone(),
                filename_prefix: "rollout-".into(),
                filename_suffix: ".jsonl".into(),
                workspace_path: workspace.clone(),
            },
            sink.clone(),
            test_store(),
        )
        .unwrap();

    // A new file in the exact run-private root but with the wrong cwd is
    // not enough correlation and must be ignored.
    fs::write(
        sessions.join("rollout-new-thread-wrong.jsonl"),
        rollout_text(&wrong_workspace, "thread-wrong"),
    )
    .unwrap();
    let accepted = sessions.join("rollout-new-thread-live.jsonl");
    fs::write(&accepted, rollout_text(&workspace, "thread-live")).unwrap();
    manager.activate_run("run-live");

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if sink.events.lock().unwrap().len() >= 6 {
                break;
            }
            sink.notify.notified().await;
        }
    })
    .await
    .expect("rollout events should reach the shared fanout");

    let events = sink.events.lock().unwrap();
    let canonical_accepted = fs::canonicalize(&accepted).unwrap();
    // Six, not five: this synthetic rollout has no armed CODEX_HOME behind
    // its run id — no arming attestation, no guard trace — which is exactly
    // the condition `GUARDS_SILENT_MARKER` reports (see the guard-trace
    // notification asserted below). A real dispatch always arms and attests
    // in `write_permission_config`, so the marker there means the hooks
    // genuinely are not being enforced.
    assert_eq!(events.len(), 6);
    assert!(events.iter().all(|event| event.run_id.as_deref() == Some("run-live")));
    assert!(
        events
            .iter()
            .all(|event| event.transcript_path.as_deref() == Some(canonical_accepted.to_string_lossy().as_ref()))
    );
    assert!(matches!(
        &events[0].event,
        WorkerEvent::SessionStart { session_id, .. } if session_id == "thread-live"
    ));
    assert!(matches!(&events[1].event, WorkerEvent::UserPromptSubmit { .. }));
    assert!(matches!(&events[2].event, WorkerEvent::PreToolUse { .. }));
    assert!(matches!(&events[3].event, WorkerEvent::PostToolUse { .. }));
    assert!(matches!(
        &events[4].event,
        WorkerEvent::Notification { message, .. }
            if message.starts_with(crate::driver::codex::GUARDS_SILENT_MARKER)
    ));
    assert!(matches!(
        &events[5].event,
        WorkerEvent::Stop {
            stop_reason: StopReason::Completed,
            ..
        }
    ));
    drop(events);

    // Proof of life is recorded twice here: once when discovery sees
    // the new file grow, and again at attach. The production sink is
    // idempotent (`record_driver_signal` keeps the first timestamp);
    // this test sink logs both calls so a missing producer is visible.
    assert_eq!(
        &*sink.attached_runs.lock().unwrap(),
        &["run-live".to_owned(), "run-live".to_owned()],
    );

    manager.stop_run("run-live");
}

/// A rollout whose first `session_meta` line is still being written
/// (no terminating newline) must still count as driver-originated
/// evidence. Attach waits on a parseable record; liveness must not.
#[tokio::test]
async fn growing_incomplete_session_meta_records_attach_before_parse() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();

    let manager = AgentJsonlProgressManager::new();
    let sink = CaptureSink::default();
    manager
        .prepare_run(
            "run-incomplete",
            Arc::new(crate::driver::CodexDriver::default()),
            AgentJsonlFileIngress {
                directory: sessions.clone(),
                filename_prefix: "rollout-".into(),
                filename_suffix: ".jsonl".into(),
                workspace_path: workspace.clone(),
            },
            sink.clone(),
            test_store(),
        )
        .unwrap();

    let path = sessions.join("rollout-thread-incomplete.jsonl");
    let partial = format!(
        r#"{{"type":"session_meta","payload":{{"id":"thread-incomplete","cwd":"{}""#,
        workspace.display()
    );
    fs::write(&path, &partial).unwrap();
    manager.activate_run("run-incomplete");

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !sink.attached_runs.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(DISCOVERY_POLL).await;
        }
    })
    .await
    .expect("file growth during discovery must record attach before session_meta parses");

    assert_eq!(
        &*sink.attached_runs.lock().unwrap(),
        &["run-incomplete".to_owned()],
        "attach evidence must fire from file progress, not from a parsed record",
    );
    assert!(
        sink.events.lock().unwrap().is_empty(),
        "an incomplete first line is not a dispatchable record",
    );

    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, r#","extra":"still-incomplete""#).unwrap();
        file.flush().unwrap();
    }
    tokio::time::sleep(DISCOVERY_POLL * 2).await;
    assert!(
        sink.events.lock().unwrap().is_empty(),
        "further growth without a newline must still not parse",
    );
    assert_eq!(
        &*sink.attached_runs.lock().unwrap(),
        &["run-incomplete".to_owned()],
        "discovery records attach once, even as the incomplete line keeps growing",
    );

    manager.stop_run("run-incomplete");
}

async fn read_until_contains(reader: &mut tokio::io::DuplexStream, observed: &mut Vec<u8>, needle: &[u8]) {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut chunk = [0u8; 4096];
        while !observed.windows(needle.len()).any(|window| window == needle) {
            let count = reader.read(&mut chunk).await.unwrap();
            assert!(count > 0, "tail closed before expected bytes arrived");
            observed.extend_from_slice(&chunk[..count]);
        }
    })
    .await
    .expect("tail should expose appended file bytes");
}

#[tokio::test]
async fn raw_tail_handles_truncation_rotation_and_incomplete_final_line() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let ingress = AgentJsonlFileIngress {
        directory: sessions.clone(),
        filename_prefix: "rollout-".into(),
        filename_suffix: ".jsonl".into(),
        workspace_path: workspace.clone(),
    };
    let prepared = PreparedSource::new(ingress).unwrap();
    let path = sessions.join("rollout-test-thread-tail.jsonl");
    let meta = serde_json::to_string(&serde_json::json!({
        "type":"session_meta",
        "payload":{"id":"thread-tail","cwd":workspace}
    }))
    .unwrap();
    let long_partial = format!("{meta}\n{{\"partial\":\"{}\"", "x".repeat(512));
    fs::write(&path, &long_partial).unwrap();
    let candidate = validate_candidate(&prepared, &path).unwrap().unwrap();

    let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
    let (cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
    let tail = tokio::spawn(stream_file_bytes(prepared.clone(), candidate, writer, cancel_rx));
    let mut observed = Vec::new();
    read_until_contains(&mut reader, &mut observed, b"partial").await;

    // Shorter in-place rewrite forces offset reset and a separating
    // newline before the new JSONL stream.
    let truncated = format!("{meta}\n{{\"phase\":\"truncated\"}}\n");
    fs::write(&path, truncated).unwrap();
    read_until_contains(&mut reader, &mut observed, b"truncated").await;

    // Atomic same-path replacement changes inode. The replacement keeps
    // the same direct session metadata, so rotation is accepted.
    let replacement = sessions.join("replacement.jsonl");
    let rotated = format!("{meta}\n{{\"phase\":\"rotated\"}}\n");
    fs::write(&replacement, rotated).unwrap();
    fs::rename(&replacement, &path).unwrap();
    read_until_contains(&mut reader, &mut observed, b"rotated").await;

    // Cancellation is the logical worker EOF. The raw tail closes without
    // inventing a newline; the generic reader owns final-fragment parsing.
    {
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write;
        write!(file, "{{\"final\":true").unwrap();
    }
    read_until_contains(&mut reader, &mut observed, b"final").await;
    cancel_tx.send(StreamHalt::Cancel).unwrap();
    reader.read_to_end(&mut observed).await.unwrap();
    tail.await.unwrap().unwrap();
    assert!(observed.ends_with(b"{\"final\":true"));
}

#[tokio::test]
async fn initial_path_replacement_streams_only_the_validated_descriptor() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    let wrong_workspace = temp.path().join("wrong-workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&wrong_workspace).unwrap();
    let prepared = PreparedSource::new(AgentJsonlFileIngress {
        directory: sessions.clone(),
        filename_prefix: "rollout-".into(),
        filename_suffix: ".jsonl".into(),
        workspace_path: workspace.clone(),
    })
    .unwrap();
    let path = sessions.join("rollout-test-thread-attach.jsonl");
    fs::write(
        &path,
        rollout_with_marker(&workspace, "thread-attach", "validated-initial"),
    )
    .unwrap();
    let candidate = validate_candidate(&prepared, &path).unwrap().unwrap();

    // Replace the pathname after validation but before tail attachment.
    // The replacement has the right filename/session but the wrong cwd,
    // so a path reopen would expose attacker bytes before correlation.
    let attacker = sessions.join("attacker-initial.jsonl");
    fs::write(
        &attacker,
        rollout_with_marker(&wrong_workspace, "thread-attach", "unvalidated-replacement"),
    )
    .unwrap();
    fs::rename(&attacker, &path).unwrap();

    let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
    let (_cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
    let result = stream_file_bytes(prepared, candidate, writer, cancel_rx).await;
    let mut observed = Vec::new();
    reader.read_to_end(&mut observed).await.unwrap();

    assert!(result.unwrap_err().contains("lost run correlation"));
    let observed = String::from_utf8(observed).unwrap();
    assert!(observed.contains("validated-initial"));
    assert!(!observed.contains("unvalidated-replacement"));
}

#[tokio::test]
async fn rotation_path_replacement_streams_only_the_validated_rotation_descriptor() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    let wrong_workspace = temp.path().join("wrong-workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&wrong_workspace).unwrap();
    let prepared = PreparedSource::new(AgentJsonlFileIngress {
        directory: sessions.clone(),
        filename_prefix: "rollout-".into(),
        filename_suffix: ".jsonl".into(),
        workspace_path: workspace.clone(),
    })
    .unwrap();
    let path = sessions.join("rollout-test-thread-rotate.jsonl");
    fs::write(
        &path,
        rollout_with_marker(&workspace, "thread-rotate", "validated-initial"),
    )
    .unwrap();
    let candidate = validate_candidate(&prepared, &path).unwrap().unwrap();

    // Install a valid rotation so the tailer validates and opens it.
    let valid_rotation = sessions.join("valid-rotation.jsonl");
    fs::write(
        &valid_rotation,
        rollout_with_marker(&workspace, "thread-rotate", "validated-rotation"),
    )
    .unwrap();
    fs::rename(&valid_rotation, &path).unwrap();

    // The hook atomically replaces the path after rotation validation
    // but before the validated descriptor is installed in the tailer.
    let attacker = sessions.join("attacker-rotation.jsonl");
    fs::write(
        &attacker,
        rollout_with_marker(&wrong_workspace, "thread-rotate", "unvalidated-rotation"),
    )
    .unwrap();
    let hook_ran = Arc::new(AtomicBool::new(false));
    let hook_ran_for_tail = hook_ran.clone();
    let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
    let (_cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
    let result = stream_file_bytes_with_test_hooks(
        prepared,
        candidate,
        writer,
        cancel_rx,
        |_| Ok(()),
        move |validated_rotation| {
            assert_eq!(validated_rotation.session_id, "thread-rotate");
            fs::rename(&attacker, &validated_rotation.path)
                .map_err(|err| format!("replace path after rotation validation: {err}"))?;
            hook_ran_for_tail.store(true, Ordering::SeqCst);
            Ok(())
        },
    )
    .await;
    let mut observed = Vec::new();
    reader.read_to_end(&mut observed).await.unwrap();

    assert!(hook_ran.load(Ordering::SeqCst));
    assert!(result.unwrap_err().contains("lost run correlation"));
    let observed = String::from_utf8(observed).unwrap();
    assert!(observed.contains("validated-initial"));
    assert!(observed.contains("validated-rotation"));
    assert!(!observed.contains("unvalidated-rotation"));
}

#[tokio::test]
async fn surviving_hard_link_alias_cannot_inject_bytes_before_publication() {
    let fixture = tail_fixture("thread-alias");
    let alias = fixture.path.with_file_name("rollout-alias.jsonl");
    let replacement = fixture.path.with_file_name("rollout-replacement.jsonl");
    fs::write(
        &replacement,
        rollout_with_marker(&fixture.workspace, "thread-alias", "tracked-replacement"),
    )
    .unwrap();
    let attack_ran = Arc::new(AtomicBool::new(false));
    let attack_ran_in_loop = attack_ran.clone();
    let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
    let (_cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
    let result = stream_file_bytes_with_test_hooks(
        fixture.prepared,
        fixture.candidate,
        writer,
        cancel_rx,
        move |tracked_path| {
            assert!(!attack_ran_in_loop.swap(true, Ordering::SeqCst));
            fs::hard_link(tracked_path, &alias).map_err(|err| format!("create surviving rollout alias: {err}"))?;
            fs::rename(&replacement, tracked_path).map_err(|err| format!("replace tracked rollout path: {err}"))?;
            let mut attacker = fs::OpenOptions::new()
                .append(true)
                .open(&alias)
                .map_err(|err| format!("open surviving rollout alias: {err}"))?;
            std::io::Write::write_all(&mut attacker, b"{\"marker\":\"alias-attacker\"}\n")
                .map_err(|err| format!("append through surviving rollout alias: {err}"))?;
            Ok(())
        },
        |_| Ok(()),
    )
    .await;
    let mut observed = Vec::new();
    reader.read_to_end(&mut observed).await.unwrap();

    assert!(attack_ran.load(Ordering::SeqCst));
    assert!(result.unwrap_err().contains("no longer exclusively named"));
    assert!(
        observed.is_empty(),
        "no bytes read before the failed post-read check may emerge"
    );
}

#[tokio::test]
async fn wrong_workspace_truncation_is_rejected_before_prefix_publication() {
    let fixture = tail_fixture("thread-truncate-cwd");
    let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
    let (_cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
    let tail = tokio::spawn(stream_file_bytes(
        fixture.prepared,
        fixture.candidate,
        writer,
        cancel_rx,
    ));
    let mut observed = Vec::new();
    read_until_contains(&mut reader, &mut observed, b"initial-stream").await;

    fs::write(
        &fixture.path,
        rollout_with_marker(&fixture.wrong_workspace, "thread-truncate-cwd", "wrong-cwd-prefix"),
    )
    .unwrap();
    let result = tail.await.unwrap();
    reader.read_to_end(&mut observed).await.unwrap();

    assert!(result.unwrap_err().contains("lost run correlation"));
    let observed = String::from_utf8(observed).unwrap();
    assert!(!observed.contains("wrong-cwd-prefix"));
    assert!(!observed.contains(fixture.wrong_workspace.to_string_lossy().as_ref()));
}

#[tokio::test]
async fn wrong_session_truncation_is_rejected_before_prefix_publication() {
    let fixture = tail_fixture("thread-truncate-session");
    let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
    let (_cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
    let tail = tokio::spawn(stream_file_bytes(
        fixture.prepared,
        fixture.candidate,
        writer,
        cancel_rx,
    ));
    let mut observed = Vec::new();
    read_until_contains(&mut reader, &mut observed, b"initial-stream").await;

    fs::write(
        &fixture.path,
        rollout_with_marker(&fixture.workspace, "thread-attacker", "wrong-session-prefix"),
    )
    .unwrap();
    let result = tail.await.unwrap();
    reader.read_to_end(&mut observed).await.unwrap();

    assert!(result.unwrap_err().contains("lost run correlation"));
    let observed = String::from_utf8(observed).unwrap();
    assert!(!observed.contains("wrong-session-prefix"));
    assert!(!observed.contains("thread-attacker"));
}

#[tokio::test]
async fn valid_same_session_truncation_restarts_after_revalidation() {
    let fixture = tail_fixture("thread-truncate-valid");
    let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
    let (cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
    let tail = tokio::spawn(stream_file_bytes(
        fixture.prepared,
        fixture.candidate,
        writer,
        cancel_rx,
    ));
    let mut observed = Vec::new();
    read_until_contains(&mut reader, &mut observed, b"initial-stream").await;

    fs::write(
        &fixture.path,
        rollout_with_marker(&fixture.workspace, "thread-truncate-valid", "valid-truncated-stream"),
    )
    .unwrap();
    read_until_contains(&mut reader, &mut observed, b"valid-truncated-stream").await;
    cancel_tx.send(StreamHalt::Cancel).unwrap();
    reader.read_to_end(&mut observed).await.unwrap();
    tail.await.unwrap().unwrap();

    let observed = String::from_utf8(observed).unwrap();
    assert!(observed.contains("valid-truncated-stream"));
}

#[tokio::test]
async fn duplicate_prepare_keeps_exactly_one_run_handle() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let ingress = AgentJsonlFileIngress {
        directory: sessions,
        filename_prefix: "rollout-".into(),
        filename_suffix: ".jsonl".into(),
        workspace_path: workspace,
    };
    let manager = AgentJsonlProgressManager::new();
    let sink = CaptureSink::default();
    for _ in 0..2 {
        manager
            .prepare_run(
                "run-duplicate",
                Arc::new(crate::driver::CodexDriver::default()),
                ingress.clone(),
                sink.clone(),
                test_store(),
            )
            .unwrap();
    }
    assert_eq!(manager.runs.lock().unwrap().len(), 1);
    manager.stop_run("run-duplicate");
    assert!(manager.runs.lock().unwrap().is_empty());
}

/// A second turn on an already-attached rollout, appended after the
/// engine went away. Carries no `session_meta` of its own — the thread id
/// was announced once, at the head of the file — which is what makes the
/// restored session state load-bearing rather than decorative.
fn second_turn_text() -> String {
    [
        serde_json::json!({
            "type":"event_msg",
            "payload":{"type":"task_started","turn_id":"turn-two"}
        }),
        serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"function_call",
                "name":"exec_command",
                "call_id":"call-two",
                "arguments":r#"{"cmd":"printf two"}"#
            }
        }),
        serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"function_call_output",
                "call_id":"call-two",
                "output":"two\n"
            }
        }),
        serde_json::json!({
            "type":"event_msg",
            "payload":{
                "type":"task_complete",
                "turn_id":"turn-two",
                "last_agent_message":"done two"
            }
        }),
    ]
    .into_iter()
    .map(|record| serde_json::to_string(&record).unwrap())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n"
}

async fn wait_for_stop(sink: &CaptureSink, at_least: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if sink
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event.event, WorkerEvent::Stop { .. }))
                .count()
                >= at_least
            {
                break;
            }
            sink.notify.notified().await;
        }
    })
    .await
    .expect("a turn boundary should reach the fanout");
}

/// The whole point of the readoption path, end to end.
///
/// One engine attaches, reads a turn to its boundary, and disappears. A
/// second engine — a fresh manager and a fresh sink, sharing only the
/// durable checkpoint — resumes the same live rollout, and a turn that
/// completes afterwards produces exactly the events it would have
/// produced without the restart: no second `SessionStart`, no replay of
/// the first turn's tool calls, no second `Stop` for a turn that already
/// ended.
#[tokio::test]
async fn a_turn_completing_after_a_restart_produces_its_own_events_and_only_those() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let ingress = AgentJsonlFileIngress {
        directory: sessions.clone(),
        filename_prefix: "rollout-".into(),
        filename_suffix: ".jsonl".into(),
        workspace_path: workspace.clone(),
    };
    let store: Arc<dyn IngressCheckpointStore> = Arc::new(MemoryCheckpointStore::default());

    // Engine one.
    let before = AgentJsonlProgressManager::new();
    let before_sink = CaptureSink::default();
    before
        .prepare_run(
            "run-restart",
            Arc::new(crate::driver::CodexDriver::default()),
            ingress.clone(),
            before_sink.clone(),
            store.clone(),
        )
        .unwrap();
    let path = sessions.join("rollout-new-thread-restart.jsonl");
    fs::write(&path, rollout_text(&workspace, "thread-restart")).unwrap();
    before.activate_run("run-restart");
    wait_for_stop(&before_sink, 1).await;
    assert!(
        matches!(
            before_sink.events.lock().unwrap()[0].event,
            WorkerEvent::SessionStart { .. }
        ),
        "precondition: the first engine saw the session start",
    );
    // The engine process dies. Its ingress goes with it; the worker and
    // its rollout do not.
    before.stop_run("run-restart");

    let checkpoint = store
        .load_ingress_checkpoint("run-restart")
        .unwrap()
        .expect("the ingress records where it got to");
    let consumed = match &checkpoint {
        IngressCheckpoint::Attached {
            path: recorded,
            session_id,
            consumed_bytes,
            session_state,
            ..
        } => {
            assert_eq!(recorded, &fs::canonicalize(&path).unwrap());
            assert_eq!(session_id, "thread-restart");
            assert!(
                session_state.is_some(),
                "the driver session state belongs to the same checkpoint as the offset",
            );
            *consumed_bytes
        }
        other => panic!("expected an attached checkpoint, got {other:?}"),
    };
    assert_eq!(
        consumed,
        fs::metadata(&path).unwrap().len(),
        "the first engine consumed the whole first turn",
    );

    // The worker keeps working across the restart.
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(second_turn_text().as_bytes()).unwrap();
    }

    // Engine two: nothing in common with engine one but the checkpoint.
    let after = AgentJsonlProgressManager::new();
    let after_sink = CaptureSink::default();
    let outcome = after
        .resume_run(
            "run-restart",
            Arc::new(crate::driver::CodexDriver::default()),
            checkpoint,
            after_sink.clone(),
            store.clone(),
        )
        .expect("the recorded rollout is still attachable");
    assert_eq!(outcome, ResumeOutcome::Reestablished);

    wait_for_stop(&after_sink, 1).await;
    let events = after_sink.events.lock().unwrap();
    let kinds: Vec<&WorkerEvent> = events.iter().map(|event| &event.event).collect();
    assert!(
        !kinds
            .iter()
            .any(|event| matches!(event, WorkerEvent::SessionStart { .. })),
        "a resumed tail must not re-read the session_meta it already published: {kinds:?}"
    );
    assert!(
        matches!(kinds.first(), Some(WorkerEvent::UserPromptSubmit { session_id, .. }) if session_id == "thread-restart"),
        "the second turn opens with its own prompt, correlated to the thread the restored \
         session remembered: {kinds:?}"
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|event| matches!(event, WorkerEvent::Stop { .. }))
            .count(),
        1,
        "exactly one turn ended after the restart: {kinds:?}"
    );
    assert!(
        matches!(
            kinds.last(),
            Some(WorkerEvent::Stop {
                stop_reason: StopReason::Completed,
                ..
            })
        ),
        "the post-restart turn reaches a real boundary: {kinds:?}"
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|event| matches!(event, WorkerEvent::PreToolUse { .. }))
            .count(),
        1,
        "only the second turn's tool call, never the first turn's again: {kinds:?}"
    );
    drop(events);
    after.stop_run("run-restart");
}

/// The forbidden fallbacks, refused. A recorded rollout that is gone has
/// no offset that means what the checkpoint meant, so the resume fails and
/// the caller — which files an operator attention item — hears about it.
#[tokio::test]
async fn resuming_a_vanished_rollout_fails_loudly_rather_than_starting_over() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let manager = AgentJsonlProgressManager::new();
    let err = manager
        .resume_run(
            "run-gone",
            Arc::new(crate::driver::CodexDriver::default()),
            IngressCheckpoint::Attached {
                ingress: AgentJsonlFileIngress {
                    directory: sessions.clone(),
                    filename_prefix: "rollout-".into(),
                    filename_suffix: ".jsonl".into(),
                    workspace_path: workspace.clone(),
                },
                path: sessions.join("rollout-new-thread-gone.jsonl"),
                session_id: "thread-gone".into(),
                consumed_bytes: 128,
                identity: FileIdentity { device: 1, inode: 1 },
                session_state: None,
            },
            CaptureSink::default(),
            test_store(),
        )
        .expect_err("a rollout that is not there cannot be resumed");
    assert!(err.contains("rollout-new-thread-gone.jsonl"), "got {err}");
    assert!(
        manager.runs.lock().unwrap().is_empty(),
        "a failed resume must leave no half-armed ingress behind",
    );
}

/// A rollout shorter than what the engine already published was truncated
/// or replaced under the same name. Every byte offset in it now means
/// something else, so there is nothing honest to resume from — and
/// picking the nearest plausible offset would silently replay or silently
/// skip.
#[tokio::test]
async fn resuming_a_rollout_shorter_than_what_was_consumed_fails_loudly() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let path = sessions.join("rollout-new-thread-short.jsonl");
    fs::write(&path, rollout_text(&workspace, "thread-short")).unwrap();
    let size = fs::metadata(&path).unwrap().len();

    let manager = AgentJsonlProgressManager::new();
    let err = manager
        .resume_run(
            "run-short",
            Arc::new(crate::driver::CodexDriver::default()),
            IngressCheckpoint::Attached {
                ingress: AgentJsonlFileIngress {
                    directory: sessions.clone(),
                    filename_prefix: "rollout-".into(),
                    filename_suffix: ".jsonl".into(),
                    workspace_path: workspace.clone(),
                },
                path: fs::canonicalize(&path).unwrap(),
                session_id: "thread-short".into(),
                consumed_bytes: size + 1,
                identity: identity_of(&path),
                session_state: None,
            },
            CaptureSink::default(),
            test_store(),
        )
        .expect_err("an offset past the end of the file is not resumable");
    assert!(err.contains("already consumed"), "got {err}");
}

/// A rollout that is still there but now belongs to a different session is
/// a different run's file wearing the recorded name.
#[tokio::test]
async fn resuming_a_rollout_that_lost_its_correlation_fails_loudly() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let path = sessions.join("rollout-new-thread-other.jsonl");
    fs::write(&path, rollout_text(&workspace, "thread-other")).unwrap();

    let manager = AgentJsonlProgressManager::new();
    let err = manager
        .resume_run(
            "run-other",
            Arc::new(crate::driver::CodexDriver::default()),
            IngressCheckpoint::Attached {
                ingress: AgentJsonlFileIngress {
                    directory: sessions.clone(),
                    filename_prefix: "rollout-".into(),
                    filename_suffix: ".jsonl".into(),
                    workspace_path: workspace.clone(),
                },
                path: fs::canonicalize(&path).unwrap(),
                session_id: "thread-expected".into(),
                consumed_bytes: 0,
                identity: identity_of(&path),
                session_state: None,
            },
            CaptureSink::default(),
            test_store(),
        )
        .expect_err("a rollout for another session is not this run's resume point");
    assert!(err.contains("thread-expected"), "got {err}");
}

/// A rollout replaced under the same pathname while the engine was down
/// is a different file, and the recorded offset indexes the dead one.
/// Path plus length cannot see that — the replacement only has to be long
/// enough — so the incarnation the offset was measured against is
/// recorded and checked. Attaching anyway would skip the new file's first
/// `consumed_bytes` bytes and land mid-line.
#[tokio::test]
async fn resuming_a_rollout_rotated_under_the_same_path_fails_loudly() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let path = sessions.join("rollout-new-thread-rotated.jsonl");
    fs::write(&path, rollout_text(&workspace, "thread-rotated")).unwrap();
    let dead_incarnation = identity_of(&path);
    let consumed = fs::metadata(&path).unwrap().len();

    // Replaced, not appended to: a new inode behind the same name, at
    // least as long as what the previous engine had already consumed.
    let mut replacement = rollout_text(&workspace, "thread-rotated");
    replacement.push_str(&second_turn_text());
    replace_with_new_inode(&path, &replacement);
    assert_ne!(
        identity_of(&path),
        dead_incarnation,
        "precondition: the replacement must genuinely be a different file",
    );
    assert!(
        fs::metadata(&path).unwrap().len() >= consumed,
        "precondition: the replacement must be long enough to defeat the size check alone",
    );

    let manager = AgentJsonlProgressManager::new();
    let err = manager
        .resume_run(
            "run-rotated",
            Arc::new(crate::driver::CodexDriver::default()),
            IngressCheckpoint::Attached {
                ingress: AgentJsonlFileIngress {
                    directory: sessions.clone(),
                    filename_prefix: "rollout-".into(),
                    filename_suffix: ".jsonl".into(),
                    workspace_path: workspace.clone(),
                },
                path: fs::canonicalize(&path).unwrap(),
                session_id: "thread-rotated".into(),
                consumed_bytes: consumed,
                identity: dead_incarnation,
                session_state: None,
            },
            CaptureSink::default(),
            test_store(),
        )
        .expect_err("an offset into a dead incarnation is not resumable");
    assert!(err.contains("different file now"), "got {err}");
    assert!(manager.runs.lock().unwrap().is_empty());
}

/// The `Attached` record asserts that `consumed_bytes` is immediately past
/// a newline. Checked rather than trusted: a truncate-and-regrow that
/// happens to land at the same length keeps the inode and passes the size
/// check, and attaching mid-line there splices a fragment of the dead
/// incarnation onto the new one's next record.
#[tokio::test]
async fn resuming_at_an_offset_that_is_not_a_record_boundary_fails_loudly() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let path = sessions.join("rollout-new-thread-midline.jsonl");
    fs::write(&path, rollout_text(&workspace, "thread-midline")).unwrap();
    let size = fs::metadata(&path).unwrap().len();

    let manager = AgentJsonlProgressManager::new();
    let err = manager
        .resume_run(
            "run-midline",
            Arc::new(crate::driver::CodexDriver::default()),
            IngressCheckpoint::Attached {
                ingress: AgentJsonlFileIngress {
                    directory: sessions.clone(),
                    filename_prefix: "rollout-".into(),
                    filename_suffix: ".jsonl".into(),
                    workspace_path: workspace.clone(),
                },
                path: fs::canonicalize(&path).unwrap(),
                session_id: "thread-midline".into(),
                // One byte short of the final newline: inside the last
                // record rather than after it.
                consumed_bytes: size - 1,
                identity: identity_of(&path),
                session_state: None,
            },
            CaptureSink::default(),
            test_store(),
        )
        .expect_err("an offset inside a record is not resumable");
    assert!(err.contains("does not end a record"), "got {err}");
    assert!(manager.runs.lock().unwrap().is_empty());
}

/// A session state the driver cannot take back is a failed resume, not a
/// degraded one. Caught before anything attaches, so the caller can file
/// an attention item — inside the ingress task it would only be a log
/// line, and the run would read nothing while looking healthy.
#[tokio::test]
async fn resuming_with_a_session_state_the_driver_rejects_fails_loudly() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let path = sessions.join("rollout-new-thread-badstate.jsonl");
    fs::write(&path, rollout_text(&workspace, "thread-badstate")).unwrap();

    let manager = AgentJsonlProgressManager::new();
    let err = manager
        .resume_run(
            "run-badstate",
            Arc::new(crate::driver::CodexDriver::default()),
            IngressCheckpoint::Attached {
                ingress: AgentJsonlFileIngress {
                    directory: sessions.clone(),
                    filename_prefix: "rollout-".into(),
                    filename_suffix: ".jsonl".into(),
                    workspace_path: workspace.clone(),
                },
                path: fs::canonicalize(&path).unwrap(),
                session_id: "thread-badstate".into(),
                consumed_bytes: 0,
                identity: identity_of(&path),
                session_state: Some(serde_json::json!("not a session snapshot")),
            },
            CaptureSink::default(),
            test_store(),
        )
        .expect_err("an unreadable session snapshot is not resumable");
    assert!(err.contains("rollout resume state"), "got {err}");
    assert!(manager.runs.lock().unwrap().is_empty());
}

/// A driver that never tails a file records that fact, and readoption
/// reads it as "nothing to do" rather than having to guess from silence.
#[tokio::test]
async fn resuming_a_non_file_ingress_is_a_no_op_not_a_failure() {
    let manager = AgentJsonlProgressManager::new();
    let outcome = manager
        .resume_run(
            "run-hooks",
            Arc::new(crate::driver::ClaudeDriver),
            IngressCheckpoint::NotFileIngress,
            CaptureSink::default(),
            test_store(),
        )
        .unwrap();
    assert_eq!(outcome, ResumeOutcome::NotFileIngress);
    assert!(manager.runs.lock().unwrap().is_empty());
}

/// The reader's position and the file's are the same number right up
/// until they are not: a resumed tail starts at a non-zero file offset,
/// and the synthetic delimiter written on truncation/rotation is a stream
/// byte that corresponds to no file byte at all. Getting this wrong shifts
/// every checkpoint after the first anomaly.
#[test]
fn stream_positions_resolve_to_the_file_offsets_they_came_from() {
    let first = FileIdentity { device: 1, inode: 100 };
    let mut map = StreamFileMap::default();
    // A resumed tail: the reader's byte 0 is the file's byte 100.
    map.record_bytes(100, 50, first);
    assert_eq!(map.file_position_for(0).unwrap().offset, 100);
    assert_eq!(map.file_position_for(50).unwrap().offset, 150);

    // Contiguous growth stays one segment.
    map.record_bytes(150, 25, first);
    assert_eq!(map.segments.len(), 1);
    assert_eq!(map.file_position_for(75).unwrap().offset, 175);

    // The file is truncated: one stream byte, no file bytes, and the file
    // offset restarts. Truncation keeps the inode.
    map.record_delimiter(0, first);
    map.record_bytes(0, 10, first);
    assert_eq!(
        map.file_position_for(76).unwrap().offset,
        0,
        "the delimiter itself maps to the start of the new incarnation",
    );
    assert_eq!(map.file_position_for(80).unwrap().offset, 4);

    // Positions the reader cannot have reached yet clamp rather than
    // running off the end of the segment they land in.
    assert_eq!(map.file_position_for(999).unwrap().offset, 10);
}

/// A rotation replaces the inode mid-stream. A checkpoint taken while the
/// reader is still behind the boundary must stay paired with the
/// incarnation its own offset came from — pairing it with the incarnation
/// the *tail* is now reading is the stale-offset resume this identity
/// exists to refuse.
#[test]
fn positions_carry_the_incarnation_their_offset_belongs_to() {
    let old = FileIdentity { device: 1, inode: 7 };
    let new = FileIdentity { device: 1, inode: 8 };
    let mut map = StreamFileMap::default();
    map.record_bytes(0, 40, old);
    map.record_delimiter(0, new);
    map.record_bytes(0, 30, new);

    assert_eq!(map.file_position_for(10).unwrap().identity, old);
    assert_eq!(
        map.file_position_for(40).unwrap().identity,
        new,
        "the boundary itself belongs to the incarnation that follows it",
    );
    assert_eq!(map.file_position_for(60).unwrap().identity, new);
    assert_eq!(
        map.segments.len(),
        3,
        "bytes from two incarnations must never merge into one segment",
    );
}

/// Pruning must never move an answer the checkpointer could still ask for.
#[test]
fn pruning_consumed_segments_preserves_the_live_answer() {
    let identity = FileIdentity { device: 1, inode: 9 };
    let mut map = StreamFileMap::default();
    map.record_bytes(0, 10, identity);
    map.record_delimiter(0, identity);
    map.record_bytes(0, 10, identity);
    map.prune_through(21);
    assert_eq!(map.file_position_for(21).unwrap().offset, 10);
}
