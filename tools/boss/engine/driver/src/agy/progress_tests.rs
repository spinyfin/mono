use super::*;
use std::collections::HashMap;
use std::sync::Mutex;

use boss_protocol::{SessionStartSource, StopReason};
use serde_json::json;

use crate::AgentDriver;

struct MemoryIdentityStore {
    inner: Mutex<HashMap<String, String>>,
    fail_read: std::sync::atomic::AtomicBool,
    fail_write: std::sync::atomic::AtomicBool,
    claims: std::sync::atomic::AtomicUsize,
    non_terminal: std::sync::atomic::AtomicBool,
}

impl MemoryIdentityStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HashMap::new()),
            fail_read: false.into(),
            fail_write: false.into(),
            claims: 0.into(),
            non_terminal: true.into(),
        })
    }
}

impl ProgressIdentityStore for MemoryIdentityStore {
    fn claim_progress_identity_if_absent(&self, run_id: &str, session_id: &str) -> Result<(String, bool), String> {
        self.claims.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_write.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("database is locked".into());
        }
        let mut map = self.inner.lock().unwrap();
        let existed = map.contains_key(run_id);
        let stored = map
            .entry(run_id.to_owned())
            .or_insert_with(|| session_id.to_owned())
            .clone();
        Ok((stored, existed))
    }

    fn claim_progress_identity(&self, run_id: &str, session_id: &str) -> Result<bool, String> {
        let mut map = self.inner.lock().expect("identity store lock");
        match map.get(run_id) {
            Some(existing) if existing == session_id => Ok(true),
            Some(_) => {
                map.insert(run_id.to_owned(), session_id.to_owned());
                Ok(false)
            }
            None => {
                map.insert(run_id.to_owned(), session_id.to_owned());
                Ok(false)
            }
        }
    }

    fn claimed_progress_identity(&self, run_id: &str) -> Result<Option<String>, String> {
        if self.fail_read.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("database is locked".into());
        }
        Ok(self.inner.lock().expect("identity store lock").get(run_id).cloned())
    }

    fn progress_run_is_non_terminal(&self, _run_id: &str) -> Result<bool, String> {
        Ok(self.non_terminal.load(std::sync::atomic::Ordering::SeqCst))
    }
}

const PARENT: &str = "42227c9d-cdd1-4aa2-b4cf-2233178cd0d1";
const CHILD: &str = "74b3e2a3-2e36-4cfd-982a-bc499d905419";
const RUN: &str = "run-agy-progress";

fn stored_session() -> (AgyProgressSession, Arc<MemoryIdentityStore>) {
    let store = MemoryIdentityStore::new();
    (
        AgyProgressSession::new(Some(RUN.to_owned()), Some(store.clone())),
        store,
    )
}

fn stateless() -> AgyProgressSession {
    AgyProgressSession::new(None, None)
}

fn session_start(cid: &str) -> Value {
    json!({
        "hookEventName": "SessionStart",
        "conversationId": cid,
        "modelName": "gemini-4-hydrogen-xhigh",
        "transcriptPath": format!("/tmp/brain/{cid}/transcript_full.jsonl"),
        "artifactDirectoryPath": format!("/tmp/brain/{cid}"),
        "workspacePaths": ["/tmp/ws"],
        "cwd": "/tmp/ws",
        "lastUserInput": "the whole 50KB brief must not be logged",
    })
}

fn pre_invocation(cid: &str, invocation_num: u64) -> Value {
    json!({
        "hookEventName": "PreInvocation",
        "conversationId": cid,
        "modelName": "gemini-4-hydrogen-xhigh",
        "invocationNum": invocation_num,
        "initialNumSteps": 1,
        "lastUserInput": "the whole 50KB brief must not be logged",
    })
}

fn post_invocation(cid: &str) -> Value {
    json!({
        "hookEventName": "PostInvocation",
        "conversationId": cid,
        "invocationNum": 0,
        "lastUserInput": "the whole 50KB brief must not be logged",
    })
}

fn pre_tool_use(cid: &str, command: &str) -> Value {
    json!({
        "hookEventName": "PreToolUse",
        "conversationId": cid,
        "modelName": "gemini-4-hydrogen-xhigh",
        "lastUserInput": "the whole 50KB brief must not be logged",
        "toolCall": {
            "id": "call_1",
            "name": "run_command",
            "args": {
                "CommandLine": command,
                "Cwd": "/tmp/ws",
                "WaitMsBeforeAsync": 5000,
            }
        }
    })
}

fn post_tool_use(cid: &str) -> Value {
    json!({
        "hookEventName": "PostToolUse",
        "conversationId": cid,
        "error": "",
        "lastUserInput": "the whole 50KB brief must not be logged",
        "toolCall": {
            "id": "call_1",
            "name": "run_command",
            "args": {
                "CommandLine": "printf PARENT",
                "Cwd": "/tmp/ws",
            }
        }
    })
}

fn stop(cid: &str, fully_idle: bool) -> Value {
    json!({
        "hookEventName": "Stop",
        "conversationId": cid,
        "executionNum": 0,
        "terminationReason": "NO_TOOL_CALL",
        "fullyIdle": fully_idle,
        "error": "",
        "lastUserInput": "the whole 50KB brief must not be logged",
        "transcriptPath": format!("/tmp/brain/{cid}/transcript_full.jsonl"),
    })
}

#[test]
fn session_start_maps_conversation_id_and_model() {
    let event = stateless().normalize_progress_event(&session_start(PARENT)).unwrap();
    assert_eq!(
        event,
        WorkerEvent::SessionStart {
            session_id: PARENT.into(),
            source: SessionStartSource::Startup,
            model: Some("gemini-4-hydrogen-xhigh".into()),
        }
    );
}

#[test]
fn last_user_input_is_stripped_from_the_canonical_payload() {
    let canonical = canonicalize(&session_start(PARENT)).unwrap();
    assert!(canonical.get("lastUserInput").is_none());
    assert!(canonical.get("last_user_input").is_none());
    assert!(canonical.get("prompt").is_none());
}

#[test]
fn pre_tool_use_maps_run_command_to_bash() {
    let WorkerEvent::PreToolUse {
        session_id,
        tool_name,
        tool_input,
    } = stateless()
        .normalize_progress_event(&pre_tool_use(PARENT, "printf PARENT"))
        .unwrap()
    else {
        panic!("expected PreToolUse");
    };
    assert_eq!(session_id, PARENT);
    assert_eq!(tool_name, "Bash");
    assert_eq!(tool_input["command"], "printf PARENT");
    assert_eq!(tool_input["cwd"], "/tmp/ws");
}

#[test]
fn post_tool_use_maps_empty_error_to_empty_response() {
    let WorkerEvent::PostToolUse {
        tool_name,
        tool_response,
        ..
    } = stateless().normalize_progress_event(&post_tool_use(PARENT)).unwrap()
    else {
        panic!("expected PostToolUse");
    };
    assert_eq!(tool_name, "Bash");
    assert_eq!(tool_response, json!({}));
}

#[test]
fn stop_maps_to_worker_event_stop_after_a_claim() {
    let mut session = stateless();
    session.normalize_progress_event(&session_start(PARENT)).unwrap();
    let WorkerEvent::Stop {
        session_id,
        stop_hook_active,
        stop_reason,
    } = session.normalize_progress_event(&stop(PARENT, true)).unwrap()
    else {
        panic!("expected Stop");
    };
    assert_eq!(session_id, PARENT);
    assert!(!stop_hook_active);
    assert_eq!(stop_reason, StopReason::Completed);
}

#[test]
fn first_pre_invocation_is_session_start_fallback_when_nothing_is_claimed() {
    let (mut session, store) = stored_session();
    assert_eq!(
        session.normalize_progress_event(&pre_invocation(PARENT, 0)).unwrap(),
        WorkerEvent::SessionStart {
            session_id: PARENT.into(),
            source: SessionStartSource::Startup,
            model: Some("gemini-4-hydrogen-xhigh".into()),
        }
    );
    assert_eq!(store.claimed_progress_identity(RUN).unwrap().as_deref(), Some(PARENT));
}

#[test]
fn later_pre_invocation_zero_is_not_another_session_start() {
    let (mut session, _) = stored_session();
    session.normalize_progress_event(&session_start(PARENT)).unwrap();
    assert!(
        session
            .normalize_progress_events(&pre_invocation(PARENT, 0))
            .unwrap()
            .is_empty(),
        "invocationNum==0 recurs on every later execution and must not re-claim"
    );
    assert!(
        session
            .normalize_progress_events(&pre_invocation(PARENT, 1))
            .unwrap()
            .is_empty()
    );
    assert!(
        session
            .normalize_progress_events(&post_invocation(PARENT))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn child_session_start_does_not_replace_the_parent_claim() {
    let (mut session, store) = stored_session();
    session.normalize_progress_event(&session_start(PARENT)).unwrap();
    assert!(
        session
            .normalize_progress_events(&session_start(CHILD))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.claimed_progress_identity(RUN).unwrap().as_deref(),
        Some(PARENT),
        "a child SessionStart must not replace the parent claim"
    );
    // A fresh session object (the next hook connection) still sees the parent.
    let mut next = AgyProgressSession::new(Some(RUN.to_owned()), Some(store.clone()));
    assert!(
        next.normalize_progress_events(&session_start(CHILD))
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.claimed_progress_identity(RUN).unwrap().as_deref(), Some(PARENT));
}

#[test]
fn child_idle_stop_is_dropped_including_the_older_missing_parent_boundary() {
    // Hook surface study, "Ordering caveat": on 1.1.12 the parent can
    // stop with fullyIdle=false and emit no later parent idle Stop while
    // a child finishes with fullyIdle=true. The child's boundary must
    // not become the parent's.
    let (mut session, store) = stored_session();
    session.normalize_progress_event(&session_start(PARENT)).unwrap();
    assert!(
        session
            .normalize_progress_events(&stop(PARENT, false))
            .unwrap()
            .is_empty()
    );
    assert!(
        session
            .normalize_progress_events(&session_start(CHILD))
            .unwrap()
            .is_empty()
    );
    assert!(
        session
            .normalize_progress_events(&stop(CHILD, true))
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.claimed_progress_identity(RUN).unwrap().as_deref(), Some(PARENT));
    let mut later = AgyProgressSession::new(Some(RUN.to_owned()), Some(store));
    assert!(later.normalize_progress_events(&stop(CHILD, true)).unwrap().is_empty());
}

#[test]
fn parent_idle_stop_is_kept() {
    let (mut session, _) = stored_session();
    session.normalize_progress_event(&session_start(PARENT)).unwrap();
    let WorkerEvent::Stop { session_id, .. } = session.normalize_progress_event(&stop(PARENT, true)).unwrap() else {
        panic!("expected Stop");
    };
    assert_eq!(session_id, PARENT);
}

#[test]
fn all_termination_reasons_require_parent_idleness_and_never_claude_continuation() {
    let driver = crate::AgyDriver::default();
    for (reason, expected) in [
        ("NO_TOOL_CALL", StopReason::Completed),
        ("TERMINAL_CUSTOM_HOOK", StopReason::Completed),
        ("USER_CANCELED", StopReason::Interrupted),
        ("UNSPECIFIED", StopReason::Other),
        ("ERROR", StopReason::Other),
        ("MAX_INVOCATIONS", StopReason::Other),
        ("MAX_FORCED_INVOCATIONS", StopReason::Other),
        ("EARLY_CONTINUE", StopReason::Other),
        ("TERMINAL_STEP_TYPE", StopReason::Other),
        ("INJECTED_RESPONSE", StopReason::Other),
        ("MAX_TOKEN_BUDGET_EXCEEDED", StopReason::Other),
        ("HALTED_STEP", StopReason::Other),
        ("CHAINED", StopReason::Other),
        ("FUTURE_REASON", StopReason::Other),
        ("model_stop", StopReason::Other),
    ] {
        for idle in [false, true] {
            let mut session = stateless();
            session.normalize_progress_event(&session_start(PARENT)).unwrap();
            for cid in [CHILD, PARENT] {
                let mut raw = stop(cid, idle);
                raw["terminationReason"] = json!(reason);
                raw["stop_hook_active"] = json!(true);
                let events = session.normalize_progress_events(&raw).unwrap();
                if !idle || cid == CHILD {
                    assert!(events.is_empty(), "{raw}");
                } else {
                    assert_eq!(events.len(), 1);
                    let boundary = driver.turn_boundary(&events[0]).unwrap();
                    assert_eq!(boundary.reason, expected, "{reason}");
                    assert_eq!(boundary.session_id, PARENT);
                    assert!(!boundary.continuation);
                }
            }
        }
    }
}

#[test]
fn malformed_idleness_never_establishes_a_boundary() {
    let mut session = stateless();
    session.normalize_progress_event(&session_start(PARENT)).unwrap();
    for idle in [Value::Null, json!("true"), json!(1)] {
        let mut raw = stop(PARENT, true);
        raw["fullyIdle"] = idle;
        assert!(matches!(
            session.normalize_progress_events(&raw),
            Err(NormalizeError::Malformed(_))
        ));
    }
    let mut raw = stop(PARENT, true);
    raw.as_object_mut().unwrap().remove("fullyIdle");
    assert!(session.normalize_progress_events(&raw).is_err());
    raw["fullyIdle"] = json!(true);
    raw.as_object_mut().unwrap().remove("terminationReason");
    let event = session.normalize_progress_event(&raw).unwrap();
    assert_eq!(
        crate::AgyDriver::default().turn_boundary(&event).unwrap().reason,
        StopReason::Other
    );
}

#[test]
fn background_tool_reinvocation_only_ends_at_parent_idle_stop() {
    // Measured 1.5.5 order: non-idle Stop, delayed PostToolUse, automatic
    // invocationNum=0, idle Stop. PostInvocation can precede PostToolUse.
    let mut session = stateless();
    let driver = crate::AgyDriver::default();
    session.normalize_progress_event(&session_start(PARENT)).unwrap();
    for raw in [
        post_invocation(PARENT),
        stop(PARENT, false),
        post_tool_use(PARENT),
        pre_invocation(PARENT, 0),
    ] {
        for event in session.normalize_progress_events(&raw).unwrap() {
            assert!(driver.turn_boundary(&event).is_none());
        }
    }
    let event = session.normalize_progress_event(&stop(PARENT, true)).unwrap();
    assert!(driver.turn_boundary(&event).is_some());
}

#[test]
fn stop_without_a_claim_fails_loudly_instead_of_claiming() {
    let (mut session, store) = stored_session();
    let err = session
        .normalize_progress_event(&stop(PARENT, true))
        .expect_err("Stop must not invent a claim");
    assert!(
        matches!(err, NormalizeError::BoundaryBeforeClaim(ref message) if message.contains("before a claimed conversation identity")),
        "got {err:?}"
    );
    assert!(store.claimed_progress_identity(RUN).unwrap().is_none());
}

#[test]
fn stop_for_terminal_run_without_a_claim_is_rejected_permanently() {
    let (mut session, store) = stored_session();
    store.non_terminal.store(false, std::sync::atomic::Ordering::SeqCst);
    let err = session
        .normalize_progress_event(&stop(PARENT, true))
        .expect_err("Stop for a terminal run must not retry forever");
    assert!(
        matches!(err, NormalizeError::Malformed(ref message) if message.contains("terminal run")),
        "got {err:?}"
    );
    assert!(store.claimed_progress_identity(RUN).unwrap().is_none());
}

#[test]
fn child_tool_events_are_kept() {
    let (mut session, _) = stored_session();
    session.normalize_progress_event(&session_start(PARENT)).unwrap();
    let WorkerEvent::PreToolUse {
        session_id, tool_name, ..
    } = session
        .normalize_progress_event(&pre_tool_use(CHILD, "printf CHILD"))
        .unwrap()
    else {
        panic!("expected PreToolUse");
    };
    assert_eq!(session_id, CHILD);
    assert_eq!(tool_name, "Bash");
}

#[test]
fn claimed_identity_survives_a_new_session_object() {
    let store = MemoryIdentityStore::new();
    let mut first = AgyProgressSession::new(Some(RUN.to_owned()), Some(store.clone()));
    first.normalize_progress_event(&session_start(PARENT)).unwrap();
    let mut continued = AgyProgressSession::new(Some(RUN.to_owned()), Some(store.clone()));
    let WorkerEvent::SessionStart { source, .. } = continued.normalize_progress_event(&session_start(PARENT)).unwrap()
    else {
        panic!("expected SessionStart");
    };
    assert_eq!(source, SessionStartSource::Resume);
    let WorkerEvent::Stop { session_id, .. } = continued.normalize_progress_event(&stop(PARENT, true)).unwrap() else {
        panic!("expected Stop");
    };
    assert_eq!(session_id, PARENT);
    assert_eq!(store.claimed_progress_identity(RUN).unwrap().as_deref(), Some(PARENT));
}

#[test]
fn missing_hook_event_name_is_a_real_error() {
    // 1.1.12 payloads omit hookEventName. The conformance pin is 1.5.7+,
    // which stamps it; guessing the event from adjacent fields is not
    // this normaliser's job.
    let raw = json!({ "conversationId": PARENT });
    assert!(matches!(
        stateless().normalize_progress_event(&raw),
        Err(NormalizeError::MissingField("hook_event_name"))
    ));
}

#[test]
fn missing_conversation_id_is_a_real_error() {
    let raw = json!({ "hookEventName": "Stop", "fullyIdle": true });
    assert!(matches!(
        stateless().normalize_progress_event(&raw),
        Err(NormalizeError::MissingField("session_id"))
    ));
}

#[test]
fn unknown_event_is_ignored_with_logging_not_a_silent_ok() {
    let raw = json!({
        "hookEventName": "Notification",
        "conversationId": PARENT,
    });
    assert!(matches!(
        stateless().normalize_progress_event(&raw),
        Err(NormalizeError::UnknownEvent(name)) if name == "Notification"
    ));
}

#[test]
fn non_object_payload_is_malformed() {
    assert!(matches!(
        stateless().normalize_progress_event(&json!("not an object")),
        Err(NormalizeError::Malformed(_))
    ));
}

#[test]
fn resume_state_round_trips_the_claimed_id() {
    let (mut session, _) = stored_session();
    session.normalize_progress_event(&session_start(PARENT)).unwrap();
    let state = session.resume_state().expect("claimed id is resumable");
    let mut restored = AgyProgressSession::new(None, None);
    restored.restore_resume_state(&state).unwrap();
    assert!(
        restored
            .normalize_progress_events(&session_start(CHILD))
            .unwrap()
            .is_empty()
    );
    let WorkerEvent::Stop { session_id, .. } = restored.normalize_progress_event(&stop(PARENT, true)).unwrap() else {
        panic!("expected Stop");
    };
    assert_eq!(session_id, PARENT);
}

#[test]
fn stateless_driver_requires_session_for_boundaries() {
    for raw in [session_start(PARENT), pre_invocation(PARENT, 0), stop(PARENT, true)] {
        for raw in [canonicalize(&raw).unwrap(), raw] {
            assert!(matches!(crate::AgyDriver::default().normalize_progress_event(&raw),
            Err(NormalizeError::Malformed(message)) if message.contains("per-run progress_session")));
        }
    }
}

#[test]
fn tool_map_matches_authorization_adapter() {
    let adapter = include_str!("adapter.py");
    let body = adapter
        .split_once("TOOL_NAME_MAP = {")
        .unwrap()
        .1
        .split_once("}")
        .unwrap()
        .0;
    let pairs: std::collections::BTreeMap<_, _> = body
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (native, canonical) = line.trim().trim_end_matches(",").split_once(":").unwrap();
            (
                native.trim().trim_matches(char::from(34)),
                canonical.trim().trim_matches(char::from(34)),
            )
        })
        .collect();
    assert_eq!(pairs, TOOL_NAME_MAP.iter().copied().collect());
}

#[test]
fn failed_read_never_claims_over_parent() {
    use std::sync::atomic::Ordering::SeqCst;
    let store = MemoryIdentityStore::new();
    store.claim_progress_identity(RUN, PARENT).unwrap();
    store.fail_read.store(true, SeqCst);
    let mut session = AgyProgressSession::new(Some(RUN.into()), Some(store.clone()));
    assert!(matches!(
        session.normalize_progress_events(&session_start(CHILD)),
        Err(NormalizeError::IdentityStoreTransient(_))
    ));
    assert_eq!(store.claims.load(SeqCst), 0);
    assert_eq!(store.inner.lock().unwrap().get(RUN).unwrap(), PARENT);
    store.fail_read.store(false, SeqCst);
    assert!(
        session
            .normalize_progress_events(&session_start(CHILD))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn failed_write_can_retry_authoritative_start() {
    use std::sync::atomic::Ordering::SeqCst;
    let (mut session, store) = stored_session();
    store.fail_write.store(true, SeqCst);
    assert!(matches!(
        session.normalize_progress_events(&session_start(PARENT)),
        Err(NormalizeError::IdentityStoreTransient(_))
    ));
    store.fail_write.store(false, SeqCst);
    assert_eq!(
        session.normalize_progress_events(&session_start(PARENT)).unwrap().len(),
        1
    );
    assert_eq!(store.claimed_progress_identity(RUN).unwrap().as_deref(), Some(PARENT));
}

#[test]
fn overlapping_sessions_cannot_replace_first_committed_claim() {
    let (mut parent, store) = stored_session();
    let mut child = AgyProgressSession::new(Some(RUN.into()), Some(store.clone()));
    parent.normalize_progress_events(&session_start(PARENT)).unwrap();
    assert!(
        child
            .normalize_progress_events(&session_start(CHILD))
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.claimed_progress_identity(RUN).unwrap().as_deref(), Some(PARENT));
}

#[test]
fn child_pre_invocation_preserves_native_parent_claim() {
    let (mut session, store) = stored_session();
    session.normalize_progress_event(&session_start(PARENT)).unwrap();
    assert!(
        session
            .normalize_progress_events(&pre_invocation(CHILD, 0))
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.claimed_progress_identity(RUN).unwrap().as_deref(), Some(PARENT));
}

#[test]
fn fallback_claim_drops_later_child_boundaries() {
    let (mut session, store) = stored_session();
    session.normalize_progress_event(&pre_invocation(PARENT, 0)).unwrap();
    for raw in [session_start(CHILD), pre_invocation(CHILD, 0)] {
        assert!(session.normalize_progress_events(&raw).unwrap().is_empty());
    }
    assert_eq!(store.claimed_progress_identity(RUN).unwrap().as_deref(), Some(PARENT));
}

#[test]
fn concurrent_sessions_commit_only_one_identity() {
    let store = MemoryIdentityStore::new();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = [PARENT, CHILD]
        .into_iter()
        .map(|id| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut session = AgyProgressSession::new(Some(RUN.into()), Some(store));
                barrier.wait(); // Both constructors observed no claim.
                (id, session.normalize_progress_events(&session_start(id)).unwrap())
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
    let stored = store.claimed_progress_identity(RUN).unwrap().unwrap();
    for (id, events) in results {
        assert_eq!(events.len(), usize::from(id == stored));
    }
}
