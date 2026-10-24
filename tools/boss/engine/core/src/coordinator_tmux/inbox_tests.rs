use super::*;

#[test]
fn claude_attribution_is_allowed_but_merged_drafts_and_other_messages_are_not() {
    let mut record = json!({"type":"user", "uuid":"message", "origin":{"kind":"peer", "msg_id":"message"},
        "message":{"content":"Another Claude session sent a message:\nWrite the handoff.\n\nClaude attribution."}});
    assert!(input_record_matches(&record, "message", "Write the handoff."));
    assert!(!input_record_matches(&record, "other", "Write the handoff."));
    record["message"]["content"] = json!("operator draft Write the handoff.");
    assert!(!input_record_matches(&record, "message", "Write the handoff."));
}

#[tokio::test]
async fn transcript_confirmation_requires_the_exact_input_and_retains_partial_appends() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("transcript.jsonl");
    let unrelated = json!({"type":"user", "uuid":"other", "message":{"content":"prompt"}});
    std::fs::write(&path, format!("{unrelated}\n")).unwrap();
    let file = tokio::fs::File::open(&path).await.unwrap();
    let mut confirmation = Box::pin(wait_for_input_record(file, "expected", "prompt"));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut confirmation)
            .await
            .is_err()
    );
    let record = json!({"type":"user", "uuid":"expected", "message":{"content":"prompt"}});
    let record = format!("{record}\n");
    let mut writer = tokio::fs::OpenOptions::new().append(true).open(path).await.unwrap();
    writer.write_all(&record.as_bytes()[..10]).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut confirmation)
            .await
            .is_err()
    );
    writer.write_all(&record.as_bytes()[10..]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), confirmation)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn callback_addresses_preserve_separators_and_escape_path_data() {
    assert_eq!(
        receipt_address(Path::new("/tmp/space ~ café/12345678.sock")).unwrap(),
        "uds:/tmp/space%20%7E%20caf%C3%A9/12345678.sock"
    );
}

#[tokio::test]
async fn only_a_correlated_delivery_receipt_succeeds() {
    for status in ["delivered", "denied", "expired", "refused", "dropped"] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("coordinator.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let transcript = directory.path().join("transcript.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let prompt = "Write the handoff.\nDo not submit the operator's draft.";
        let (result, ()) = tokio::join!(
            tokio::time::timeout(
                Duration::from_secs(1),
                deliver(&path, std::process::id() as libc::pid_t, "session", prompt, &transcript)
            ),
            async {
                let (stream, _) = listener.accept().await.unwrap();
                let line = BufReader::new(stream).lines().next_line().await.unwrap().unwrap();
                let frame: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(frame["type"], "user");
                assert_eq!(frame["session_id"], "session");
                assert_eq!(frame["message"]["content"], prompt);
                let reply = frame["from"].as_str().unwrap().strip_prefix("uds:").unwrap();
                let mut stream = UnixStream::connect(reply).await.unwrap();
                // An unrelated receipt cannot accidentally start the grace period.
                let unrelated = json!({"action": "peer_message_status", "orig_msg_id": "other", "status": "delivered"});
                let receipt =
                    json!({"action": "peer_message_status", "orig_msg_id": frame["msg_id"], "status": status});
                stream
                    .write_all(format!("{unrelated}\n{receipt}\n").as_bytes())
                    .await
                    .unwrap();
            }
        );
        assert_eq!(result.as_ref().unwrap().is_ok(), status == "delivered", "{result:?}");
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            2,
            "receipt socket must be removed"
        );
    }
}

/// Connects to the receipt address in the delivered frame and writes `lines`.
async fn answer_with(listener: UnixListener, statuses: &[&str], junk_first: bool) {
    let (stream, _) = listener.accept().await.unwrap();
    let line = BufReader::new(stream).lines().next_line().await.unwrap().unwrap();
    let frame: Value = serde_json::from_str(&line).unwrap();
    let reply = frame["from"].as_str().unwrap().strip_prefix("uds:").unwrap().to_owned();
    if junk_first {
        // A stray connection that sends garbage then closes must not fail delivery.
        let mut stray = UnixStream::connect(&reply).await.unwrap();
        stray.write_all(b"not json\n{\"partial\":").await.unwrap();
        drop(stray);
    }
    for status in statuses {
        let mut stream = UnixStream::connect(&reply).await.unwrap();
        let receipt = json!({"action": "peer_message_status", "orig_msg_id": frame["msg_id"], "status": status});
        stream.write_all(format!("{receipt}\n").as_bytes()).await.unwrap();
        drop(stream);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn private_inbox() -> (tempfile::TempDir, PathBuf, UnixListener, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.path().join("coordinator.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let transcript = directory.path().join("transcript.jsonl");
    std::fs::write(&transcript, "").unwrap();
    (directory, path, listener, transcript)
}

#[tokio::test]
async fn held_is_pending_until_a_delivered_receipt_arrives() {
    let (_directory, path, listener, transcript) = private_inbox();
    let (result, ()) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(5),
            deliver(
                &path,
                std::process::id() as libc::pid_t,
                "session",
                "prompt",
                &transcript
            )
        ),
        answer_with(listener, &["held", "delivered"], false)
    );
    result.unwrap().unwrap();
}

#[tokio::test]
async fn held_alone_never_counts_as_failure() {
    let (_directory, path, listener, transcript) = private_inbox();
    let (result, ()) = tokio::join!(
        tokio::time::timeout(
            Duration::from_millis(700),
            deliver(
                &path,
                std::process::id() as libc::pid_t,
                "session",
                "prompt",
                &transcript
            )
        ),
        answer_with(listener, &["held"], false)
    );
    assert!(result.is_err(), "held must stay pending, got {result:?}");
}

#[tokio::test]
async fn a_malformed_receipt_line_does_not_abort_delivery() {
    let (_directory, path, listener, transcript) = private_inbox();
    let (result, ()) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(5),
            deliver(
                &path,
                std::process::id() as libc::pid_t,
                "session",
                "prompt",
                &transcript
            )
        ),
        answer_with(listener, &["delivered"], true)
    );
    result.unwrap().unwrap();
}

#[tokio::test]
async fn a_reused_socket_cannot_receive_a_prompt_for_another_process() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.path().join("coordinator.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let transcript = directory.path().join("transcript.jsonl");
    std::fs::write(&transcript, "").unwrap();
    let (result, ()) = tokio::join!(
        deliver(&path, -1, "session", "must not be delivered", &transcript),
        async {
            let (stream, _) = listener.accept().await.unwrap();
            assert!(BufReader::new(stream).lines().next_line().await.unwrap().is_none());
        }
    );
    assert!(result.unwrap_err().to_string().contains("peer does not match"));
}

#[tokio::test]
async fn frame_send_releases_lifecycle_lock_before_confirmation() {
    let (_directory, path, listener, transcript) = private_inbox();
    let lock = tokio::sync::Mutex::new(());
    let guard = lock.lock().await;
    let (result, ()) = tokio::join!(
        deliver_observed(
            &path,
            std::process::id() as libc::pid_t,
            "session",
            "prompt",
            &transcript,
            || drop(guard)
        ),
        async {
            let (stream, _) = listener.accept().await.unwrap();
            let line = BufReader::new(stream).lines().next_line().await.unwrap().unwrap();
            let frame: Value = serde_json::from_str(&line).unwrap();
            // Confirmation is withheld until another lifecycle operation acquires the lock.
            let _guard = tokio::time::timeout(Duration::from_secs(1), lock.lock()).await.unwrap();
            let reply = frame["from"].as_str().unwrap().strip_prefix("uds:").unwrap();
            let mut receipt = UnixStream::connect(reply).await.unwrap();
            let response = json!({"action":"peer_message_status", "orig_msg_id":frame["msg_id"], "status":"delivered"});
            receipt.write_all(format!("{response}\n").as_bytes()).await.unwrap();
        }
    );
    assert!(result.is_ok());
}

#[test]
fn descent_follows_parent_links_and_ignores_unrelated_processes() {
    let parents = std::collections::HashMap::from([(12, 11), (11, 10), (10, 1), (21, 20), (20, 1)]);
    let parent_of = |pid| parents.get(&pid).copied();
    assert!(descends_from(12, 10, parent_of));
    assert!(descends_from(10, 10, parent_of));
    assert!(!descends_from(21, 10, parent_of));
    assert!(!descends_from(99, 10, parent_of));
    assert!(!descends_from(13, 10, |pid| match pid {
        13 => Some(12),
        12 => Some(11),
        11 => Some(10),
        _ => None,
    }));
}

/// The registration lookup must work through the same relay command the
/// coordinator pane runs, where Claude is not the pane process.
#[tokio::test]
async fn registration_resolves_through_the_coordinator_launcher_command() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("input");
    let sessions = root.path().join("config/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    crate::driver::initial_prompt::provision(&input, &input, "brief", &crate::driver::ClaudeDriver).unwrap();
    let fake = root.path().join("fake-agent");
    std::fs::write(
        &fake,
        format!(
            r#"#!/usr/bin/env python3
import os, time
from pathlib import Path
sessions = Path({sessions:?})
if os.fork() == 0:
    (sessions / (str(os.getpid()) + ".json")).write_text("{{}}")
    time.sleep(60)
    os._exit(0)
while not list(sessions.iterdir()):
    time.sleep(0.01)
(sessions.parent / "expected").write_text(str(os.getpid()))
(sessions / (str(os.getpid()) + ".json")).write_text("{{}}")
time.sleep(60)
"#,
            sessions = sessions.to_str().unwrap(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut pane = std::process::Command::new("sh")
        .args(["-c", &relay_launch_command(&input, fake.to_str().unwrap(), "model")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pane_pid = pane.id() as libc::pid_t;
    // Hang guard only: the fake agent registers on its own schedule, which
    // stretches on a loaded host, so wait on the registrations themselves.
    let expected_path = sessions.parent().unwrap().join("expected");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut resolved = None;
    let mut registered = None;
    let mut registration_count = 0;
    let mut last_error = String::new();
    while tokio::time::Instant::now() < deadline {
        match resolve_claude_pid(&sessions, pane_pid).await {
            Ok(pid) => resolved = Some(pid),
            Err(error) => last_error = format!("{error:#}"),
        }
        registered = std::fs::read_to_string(&expected_path)
            .ok()
            .and_then(|pid| pid.parse::<libc::pid_t>().ok());
        registration_count = std::fs::read_dir(&sessions).unwrap().count();
        if resolved.is_some() && registered.is_some() && registration_count >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if resolved.is_some() && (registered.is_none() || registration_count < 2) {
        last_error = format!(
            "resolved {resolved:?} but the fake agent's own registration is missing (expected pid file: {registered:?}, session files: {registration_count} of 2)"
        );
        resolved = None;
    }
    pane.kill().unwrap();
    let relay = pane.wait_with_output().unwrap();
    let resolved = resolved.unwrap_or_else(|| {
        panic!(
            "registration did not resolve below the relay within 60s: {last_error}; relay stderr: {}",
            String::from_utf8_lossy(&relay.stderr)
        )
    });
    assert_ne!(resolved, pane_pid);
    assert_eq!(registration_count, 2, "nested session must be registered too");
    assert_eq!(Some(resolved), registered);
}
