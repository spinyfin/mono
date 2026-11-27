use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use boss_acp_client::schema::RequestId;

use crate::intent::{
    IntentLog, IntentRecordKind, REPLAY_READ_BUFFER_BYTES, WithdrawReason, read_intent_file,
    read_intent_file_if_exists, read_intent_records,
};
use crate::queue::{PromptImageRef, PromptOrigin};

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let id = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("boss-coord-chat-intent-{label}-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp test dir");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn creates_private_0600_file_and_round_trips_queued_sent_and_withdrawn_records() {
    let dir = TestDir::new("roundtrip");
    let path = dir.path().join("intent.jsonl");
    assert!(read_intent_file_if_exists(&path).unwrap().is_empty());

    let mut log = IntentLog::open(&path).expect("open new intent log");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "intent log must be owner-read/write only");

    let image = PromptImageRef {
        path: "images/paste-001.png".to_owned(),
        mime_type: "image/png".to_owned(),
        filename: Some("screenshot.png".to_owned()),
        byte_len: Some(4096),
    };
    let r1 = log
        .append_queued_at(
            1_795_000_000_001,
            PromptOrigin::Operator,
            "Describe this screenshot",
            Some("client-msg-1".to_owned()),
            vec![image.clone()],
        )
        .unwrap();
    let r2 = log
        .append_sent_at(1_795_000_000_002, r1.intent_id(), RequestId::Number(4))
        .unwrap();
    let r3 = log
        .append_queued_at(
            1_795_000_000_003,
            PromptOrigin::Engine,
            "[Boss reset request id=abc]",
            None,
            Vec::new(),
        )
        .unwrap();
    let r4 = log
        .append_withdrawn_at(1_795_000_000_004, r3.intent_id(), WithdrawReason::Timeout)
        .unwrap();

    assert_eq!((r1.seq, r2.seq, r3.seq, r4.seq), (1, 2, 3, 4));
    assert_eq!((r1.intent_id(), r3.intent_id()), (1, 2));
    assert_eq!(log.next_seq(), 5);
    assert_eq!(log.next_intent_id(), 3);

    let read_back = read_intent_file(&path).unwrap();
    assert_eq!(read_back, vec![r1, r2, r3, r4]);

    // Verify image ref survived serialization cleanly.
    match &read_back[0].kind {
        IntentRecordKind::Queued {
            origin,
            client_message_id,
            images,
            ..
        } => {
            assert_eq!(*origin, PromptOrigin::Operator);
            assert_eq!(client_message_id.as_deref(), Some("client-msg-1"));
            assert_eq!(images, &[image]);
        }
        other => panic!("expected Queued, got {other:?}"),
    }
}

#[test]
fn reopen_continues_sequence_and_truncates_torn_final_line() {
    let dir = TestDir::new("reopen");
    let path = dir.path().join("intent.jsonl");

    let first = {
        let mut log = IntentLog::open(&path).unwrap();
        let a = log
            .append_queued_at(10, PromptOrigin::Operator, "hello", Some("c1".to_owned()), Vec::new())
            .unwrap();
        let b = log.append_sent_at(11, a.intent_id(), RequestId::Number(3)).unwrap();
        vec![a, b]
    };

    // Simulate an engine crash mid-write leaving a torn final JSON line.
    {
        let mut raw = OpenOptions::new().append(true).open(&path).unwrap();
        raw.write_all(br#"{"seq":3,"ts_ms":12,"type":"queued","intent_id":2,"ori"#)
            .unwrap();
    }

    // Reader ignores the torn final record.
    assert_eq!(read_intent_file(&path).unwrap(), first);

    // Reopening truncates the torn tail and continues numbering cleanly.
    let mut reopened = IntentLog::open(&path).unwrap();
    assert_eq!(reopened.next_seq(), 3);
    assert_eq!(reopened.next_intent_id(), 2);

    let c = reopened
        .append_queued_at(20, PromptOrigin::Engine, "after restart", None, Vec::new())
        .unwrap();
    assert_eq!(c.seq, 3);
    assert_eq!(c.intent_id(), 2);

    let all = read_intent_file(&path).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[2], c);
}

#[test]
fn corrupt_record_before_end_is_an_error() {
    let dir = TestDir::new("corrupt");
    let path = dir.path().join("intent.jsonl");
    std::fs::write(
        &path,
        b"not-valid-json\n{\"seq\":1,\"ts_ms\":1,\"type\":\"withdrawn\",\"intent_id\":1,\"reason\":\"timeout\"}\n",
    )
    .unwrap();

    assert!(read_intent_file(&path).is_err());
    assert!(IntentLog::open(&path).is_err());
}

#[test]
fn reads_lines_larger_than_spike_maximum_and_default_bufreader() {
    let dir = TestDir::new("large");
    let path = dir.path().join("intent.jsonl");
    // Larger than both the spike's 27,587-byte max line and REPLAY_READ_BUFFER_BYTES.
    let huge_text = "x".repeat(REPLAY_READ_BUFFER_BYTES + 16_384);

    let mut log = IntentLog::open(&path).unwrap();
    let rec = log
        .append_queued_at(1, PromptOrigin::Engine, huge_text.clone(), None, Vec::new())
        .unwrap();
    let loaded = read_intent_file(&path).unwrap();
    assert_eq!(loaded, vec![rec]);
    match &loaded[0].kind {
        IntentRecordKind::Queued { text, .. } => assert_eq!(text.len(), huge_text.len()),
        other => panic!("unexpected record: {other:?}"),
    }
}

#[test]
fn blank_lines_are_skipped_and_unknown_withdraw_reason_decodes_as_other() {
    let raw =
        b"\n  \n{\"seq\":1,\"ts_ms\":1,\"type\":\"withdrawn\",\"intent_id\":1,\"reason\":\"future_reason\"}\n\t\n";
    let records = read_intent_records(raw.as_slice()).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].kind,
        IntentRecordKind::Withdrawn {
            intent_id: 1,
            reason: WithdrawReason::Other,
        }
    );
}
