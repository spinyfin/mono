//! Antigravity hook dialect → [`WorkerEvent`].
//!
//! `agy` payloads are camelCase (`conversationId`, `hookEventName`,
//! `toolCall`) and, from 1.5.5, name the event on every envelope. This
//! module rewrites them into the canonical snake_case shape
//! [`boss_protocol::normalize_hook_event`] expects, claims the parent
//! `conversationId` through [`crate::ProgressIdentityStore`], and drops
//! boundary-shaped events that carry any other id.
//!
//! Subagents cannot be disabled. Characterisation found every child has
//! its own `conversationId` including at `Stop` (hook surface study,
//! "Subagent identity: distinct, including at Stop"), so filtering on
//! that id is the only available mitigation. A child must never replace
//! the claim. `PreToolUse` / `PostToolUse` still pass so a child's
//! ordinary progress remains visible; the authorization adapter is a
//! separate process and is not gated by this filter.
//!
//! Native `SessionStart` is the identity carrier. A first `PreInvocation`
//! may claim only when nothing is persisted yet — `invocationNum == 0`
//! recurs on every later execution, including automatic re-invocation.
//! A `Stop` (or any other boundary) with no claim fails rather than
//! inventing one.
//!
//! Live hooks are synchronous: `agy` waits for the shim to exit before
//! emitting the next event, so the parent's `SessionStart` is decoded
//! before a child can start. The engine's accept loop then preserves
//! that order as `ingress_seq`. Replay/reordering was not characterised.
//!
//! `lastUserInput` repeats the whole prompt on every 1.5.5 hook. It is
//! stripped during canonicalisation and must not be logged.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use boss_protocol::{NormalizeError, WorkerEvent, normalize_hook_event};

use crate::{ProgressIdentityStore, ProgressSessionNormalizer};

/// Empirically confirmed on the wire (adapter + pane/hook studies).
/// Duplicated from `adapter.py` rather than shared: the adapter is a
/// separate process with its own trust boundary.
const TOOL_NAME_MAP: &[(&str, &str)] = &[
    ("run_command", "Bash"),
    ("write_file", "Write"),
    ("edit_file", "Edit"),
    ("read_file", "Read"),
    ("grep", "Grep"),
    ("glob", "Glob"),
    ("list_dir", "Glob"),
];

const RENAME_MAP: &[(&str, &str)] = &[
    ("conversationId", "session_id"),
    ("modelName", "model"),
    ("transcriptPath", "transcript_path"),
    ("artifactDirectoryPath", "artifact_directory_path"),
    ("workspacePaths", "workspace_paths"),
    ("hookEventName", "hook_event_name"),
    ("terminationReason", "termination_reason"),
    ("fullyIdle", "fully_idle"),
    ("executionNum", "execution_num"),
    ("invocationNum", "invocation_num"),
];

/// Keys that must never reach WorkerEvent construction or logs.
const STRIP_KEYS: &[&str] = &["lastUserInput", "last_user_input"];

fn mapped<'a>(table: &[(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    table.iter().find(|(from, _)| *from == key).map(|(_, to)| *to)
}

fn tool_name_mapped(raw: &str) -> String {
    mapped(TOOL_NAME_MAP, raw).unwrap_or(raw).to_owned()
}

fn canonical_tool_input(args: &Value) -> Value {
    let Some(obj) = args.as_object() else {
        return args.clone();
    };
    let mut out = obj.clone();
    if let Some(command) = obj
        .get("CommandLine")
        .or_else(|| obj.get("command"))
        .or_else(|| obj.get("commandLine"))
        .cloned()
    {
        out.insert("command".to_owned(), command);
    }
    if let Some(cwd) = obj
        .get("Cwd")
        .or_else(|| obj.get("cwd"))
        .or_else(|| obj.get("workingDirectory"))
        .cloned()
    {
        out.insert("cwd".to_owned(), cwd);
    }
    if let Some(path) = obj
        .get("Path")
        .or_else(|| obj.get("path"))
        .or_else(|| obj.get("TargetFile"))
        .or_else(|| obj.get("targetFile"))
        .or_else(|| obj.get("target_file"))
        .or_else(|| obj.get("file_path"))
        .or_else(|| obj.get("AbsolutePath"))
        .or_else(|| obj.get("absolutePath"))
        .cloned()
    {
        out.insert("file_path".to_owned(), path.clone());
        out.entry("path".to_owned()).or_insert(path);
    }
    if let Some(contents) = obj
        .get("Contents")
        .or_else(|| obj.get("contents"))
        .or_else(|| obj.get("Content"))
        .or_else(|| obj.get("content"))
        .cloned()
    {
        out.insert("content".to_owned(), contents);
    }
    Value::Object(out)
}

fn tool_call_parts(raw: &Map<String, Value>) -> (String, Value) {
    let tool_call = raw.get("toolCall").and_then(Value::as_object);
    let name = tool_call
        .and_then(|call| call.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let args = tool_call
        .and_then(|call| call.get("args"))
        .cloned()
        .unwrap_or(Value::Null);
    (tool_name_mapped(name), canonical_tool_input(&args))
}

/// Rewrite one raw `agy` hook payload into the canonical snake_case shape
/// [`normalize_hook_event`] expects. `lastUserInput` is dropped.
fn canonicalize(raw: &Value) -> Result<Value, NormalizeError> {
    let obj = raw
        .as_object()
        .ok_or_else(|| NormalizeError::Malformed("expected agy hook JSON object".into()))?;

    let mut out = Map::with_capacity(obj.len());
    for (key, value) in obj {
        if STRIP_KEYS.contains(&key.as_str()) {
            continue;
        }
        if key == "toolCall" {
            continue;
        }
        let canonical_key = mapped(RENAME_MAP, key).unwrap_or(key.as_str());
        out.insert(canonical_key.to_owned(), value.clone());
    }

    if obj.contains_key("toolCall") {
        let (tool_name, tool_input) = tool_call_parts(obj);
        out.insert("tool_name".to_owned(), Value::String(tool_name));
        out.insert("tool_input".to_owned(), tool_input);
        let error = obj.get("error").and_then(Value::as_str).unwrap_or("");
        out.insert(
            "tool_response".to_owned(),
            if error.is_empty() {
                json!({})
            } else {
                json!({ "error": error })
            },
        );
    }

    Ok(Value::Object(out))
}

fn event_name(canonical: &Value) -> Result<&str, NormalizeError> {
    canonical
        .get("hook_event_name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or(NormalizeError::MissingField("hook_event_name"))
}

fn session_id_of(canonical: &Value) -> Result<&str, NormalizeError> {
    canonical
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or(NormalizeError::MissingField("session_id"))
}

/// Per-reader normaliser for `agy`'s hook-callback progress stream.
///
/// Identity is seeded from the engine-owned store when present so a new
/// session object (one hook connection) still knows the parent
/// `conversationId` claimed by an earlier connection. Live hooks are
/// synchronous, so the first `SessionStart` is claimed before a child
/// exists; the store is what keeps that claim across automatic
/// continuations and engine restarts.
pub(super) struct AgyProgressSession {
    run_id: Option<String>,
    identity_store: Option<Arc<dyn ProgressIdentityStore>>,
    claimed_id: Option<String>,
}

impl AgyProgressSession {
    pub(super) fn new(run_id: Option<String>, identity_store: Option<Arc<dyn ProgressIdentityStore>>) -> Self {
        let claimed_id = match (run_id.as_deref(), identity_store.as_deref()) {
            (Some(run_id), Some(store)) => match store.claimed_progress_identity(run_id) {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(
                        run_id,
                        %err,
                        "agy: could not read claimed conversation identity"
                    );
                    None
                }
            },
            _ => None,
        };
        Self {
            run_id,
            identity_store,
            claimed_id,
        }
    }

    fn persist_claim(&mut self, session_id: &str) -> Result<bool, NormalizeError> {
        let (Some(run_id), Some(store)) = (self.run_id.as_deref(), self.identity_store.as_deref()) else {
            self.claimed_id = Some(session_id.to_owned());
            return Ok(false);
        };
        match store.claim_progress_identity(run_id, session_id) {
            Ok(resumed) => {
                self.claimed_id = Some(session_id.to_owned());
                Ok(resumed)
            }
            Err(err) => Err(NormalizeError::Malformed(format!(
                "agy: could not persist conversation identity: {err}"
            ))),
        }
    }

    /// `can_claim` is true only for `SessionStart` and the first-`PreInvocation`
    /// fallback. A later boundary must not invent a claim.
    fn accept_identity(&mut self, session_id: &str, can_claim: bool) -> Result<IdentityDecision, NormalizeError> {
        if let Some(claimed) = self.claimed_id.as_deref() {
            if claimed == session_id {
                return Ok(IdentityDecision::Accept { resume: true });
            }
            tracing::debug!(
                claimed,
                other = session_id,
                "agy: dropping boundary-shaped event from a conversation that is not the claimed parent"
            );
            return Ok(IdentityDecision::Drop);
        }
        if can_claim {
            let resume = self.persist_claim(session_id)?;
            return Ok(IdentityDecision::Accept { resume });
        }
        Err(NormalizeError::Malformed(
            "agy Stop arrived before a claimed conversation identity".into(),
        ))
    }

    fn decode(&mut self, raw: &Value) -> Result<Option<WorkerEvent>, NormalizeError> {
        let mut canonical = canonicalize(raw)?;
        let name = event_name(&canonical)?.to_owned();
        let session_id = session_id_of(&canonical)?.to_owned();

        match name.as_str() {
            "SessionStart" => match self.accept_identity(&session_id, true)? {
                IdentityDecision::Drop => Ok(None),
                IdentityDecision::Accept { resume } => {
                    let source = if resume { "resume" } else { "startup" };
                    canonical
                        .as_object_mut()
                        .expect("canonicalise produced an object")
                        .insert("source".to_owned(), Value::String(source.to_owned()));
                    normalize_hook_event(&canonical).map(Some)
                }
            },
            "PreInvocation" => match self.accept_identity(&session_id, true)? {
                IdentityDecision::Drop => Ok(None),
                IdentityDecision::Accept { resume: true } => Ok(None),
                IdentityDecision::Accept { resume: false } => {
                    // First PreInvocation is a SessionStart fallback only
                    // when nothing has been claimed yet. A later
                    // invocationNum==0 is an automatic continuation of
                    // the same conversation (`resume == true` after the
                    // in-memory / store seed) and must not emit another
                    // SessionStart.
                    tracing::debug!(
                        session_id,
                        "agy: mapping first PreInvocation to SessionStart; native SessionStart was missing"
                    );
                    let mut fallback = canonical;
                    let obj = fallback.as_object_mut().expect("canonicalise produced an object");
                    obj.insert("hook_event_name".to_owned(), Value::String("SessionStart".into()));
                    obj.insert("source".to_owned(), Value::String("startup".into()));
                    normalize_hook_event(&fallback).map(Some)
                }
            },
            "PostInvocation" => Ok(None),
            "Stop" => match self.accept_identity(&session_id, false)? {
                IdentityDecision::Drop => Ok(None),
                IdentityDecision::Accept { .. } => normalize_hook_event(&canonical).map(Some),
            },
            "PreToolUse" | "PostToolUse" => normalize_hook_event(&canonical).map(Some),
            other => {
                tracing::debug!(
                    event = other,
                    "agy hook: ignoring additive/undocumented-for-us event variant"
                );
                Err(NormalizeError::UnknownEvent(other.to_owned()))
            }
        }
    }
}

enum IdentityDecision {
    Accept { resume: bool },
    Drop,
}

impl ProgressSessionNormalizer for AgyProgressSession {
    fn normalize_progress_event(&mut self, raw: &Value) -> Result<WorkerEvent, NormalizeError> {
        self.normalize_progress_events(raw)?
            .into_iter()
            .next()
            .ok_or_else(|| NormalizeError::UnknownEvent("agy_filtered_progress".into()))
    }

    fn normalize_progress_events(&mut self, raw: &Value) -> Result<Vec<WorkerEvent>, NormalizeError> {
        match self.decode(raw)? {
            Some(event) => Ok(vec![event]),
            None => Ok(vec![]),
        }
    }

    fn transcript_path_for_session(&mut self, _raw: &Value) -> Option<String> {
        // `AgyDriver::transcript_path_for_session` owns the stamp rewrite
        // onto the durable store. This session does not duplicate it.
        None
    }

    fn resume_state(&self) -> Option<Value> {
        self.claimed_id
            .as_ref()
            .map(|id| json!({ "claimed_conversation_id": id }))
    }

    fn restore_resume_state(&mut self, state: &Value) -> Result<(), String> {
        match state.get("claimed_conversation_id") {
            Some(Value::String(id)) if !id.is_empty() => {
                self.claimed_id = Some(id.clone());
                Ok(())
            }
            Some(Value::Null) | None => {
                self.claimed_id = None;
                Ok(())
            }
            _ => Err("agy progress resume state missing claimed_conversation_id".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use boss_protocol::{SessionStartSource, StopReason};
    use serde_json::json;

    use crate::AgentDriver;

    struct MemoryIdentityStore {
        inner: Mutex<HashMap<String, String>>,
    }

    impl MemoryIdentityStore {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                inner: Mutex::new(HashMap::new()),
            })
        }
    }

    impl ProgressIdentityStore for MemoryIdentityStore {
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
            Ok(self.inner.lock().expect("identity store lock").get(run_id).cloned())
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
        session.normalize_progress_event(&stop(PARENT, false)).unwrap();
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
        let WorkerEvent::Stop { session_id, .. } = session.normalize_progress_event(&stop(PARENT, true)).unwrap()
        else {
            panic!("expected Stop");
        };
        assert_eq!(session_id, PARENT);
    }

    #[test]
    fn stop_without_a_claim_fails_loudly_instead_of_claiming() {
        let (mut session, store) = stored_session();
        let err = session
            .normalize_progress_event(&stop(PARENT, true))
            .expect_err("Stop must not invent a claim");
        assert!(
            matches!(err, NormalizeError::Malformed(ref message) if message.contains("before a claimed conversation identity")),
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
        let WorkerEvent::SessionStart { source, .. } =
            continued.normalize_progress_event(&session_start(PARENT)).unwrap()
        else {
            panic!("expected SessionStart");
        };
        assert_eq!(source, SessionStartSource::Resume);
        let WorkerEvent::Stop { session_id, .. } = continued.normalize_progress_event(&stop(PARENT, true)).unwrap()
        else {
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
        let WorkerEvent::Stop { session_id, .. } = restored.normalize_progress_event(&stop(PARENT, true)).unwrap()
        else {
            panic!("expected Stop");
        };
        assert_eq!(session_id, PARENT);
    }

    #[test]
    fn driver_stateless_path_maps_a_self_describing_session_start() {
        let event = crate::AgyDriver::default()
            .normalize_progress_event(&session_start(PARENT))
            .unwrap();
        assert_eq!(
            event,
            WorkerEvent::SessionStart {
                session_id: PARENT.into(),
                source: SessionStartSource::Startup,
                model: Some("gemini-4-hydrogen-xhigh".into()),
            }
        );
    }
}
