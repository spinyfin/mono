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
//! The socket reserves decode turns in accept order before spawning tasks,
//! so a parent that finishes writing before the read deadline still claims
//! before a later child. A stalled writer is retried rather than pinning
//! ingress. The shim waits for a concurrent drain lock instead of
//! stranding the current hook. This assumes the parent's connection
//! reaches the listener first; events lost before connecting or buffering
//! cannot be ordered.
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
/// `conversationId` claimed by an earlier connection. The store preserves
/// that claim across automatic continuations and engine restarts.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub(super) struct AgyProgressSession {
    run_id: Option<String>,
    identity_store: Option<Arc<dyn ProgressIdentityStore>>,
    claimed_id: Option<String>,
    store_unavailable: bool,
    from_subagent: bool,
    pending_tools: super::pending_tools::PendingTools,
}

impl AgyProgressSession {
    pub(super) fn new(run_id: Option<String>, identity_store: Option<Arc<dyn ProgressIdentityStore>>) -> Self {
        let (claimed_id, store_unavailable) = match (run_id.as_deref(), identity_store.as_deref()) {
            (Some(run_id), Some(store)) => match store.claimed_progress_identity(run_id) {
                Ok(id) => (id, false),
                Err(_) => (None, true),
            },
            _ => (None, false),
        };
        Self {
            run_id,
            identity_store,
            claimed_id,
            store_unavailable,
            from_subagent: false,
            pending_tools: Default::default(),
        }
    }

    fn persist_claim(&mut self, session_id: &str) -> Result<IdentityDecision, NormalizeError> {
        let (Some(run_id), Some(store)) = (self.run_id.as_deref(), self.identity_store.as_deref()) else {
            self.claimed_id = Some(session_id.to_owned());
            return Ok(IdentityDecision::Accept { resume: false });
        };
        let (stored, resume) = store
            .claim_progress_identity_if_absent(run_id, session_id)
            .map_err(NormalizeError::IdentityStoreTransient)?;
        let accepted = stored == session_id;
        self.claimed_id = Some(stored);
        Ok(if accepted {
            IdentityDecision::Accept { resume }
        } else {
            IdentityDecision::Drop
        })
    }

    fn refresh_identity(&mut self) -> Result<(), NormalizeError> {
        if self.store_unavailable {
            self.claimed_id = self
                .identity_store
                .as_deref()
                .expect("failed read has a store")
                .claimed_progress_identity(self.run_id.as_deref().expect("failed read has a run"))
                .map_err(NormalizeError::IdentityStoreTransient)?;
            self.store_unavailable = false;
        }
        Ok(())
    }

    /// `can_claim` is true only for `SessionStart` and the first-`PreInvocation`
    /// fallback. A later boundary must not invent a claim.
    fn accept_identity(&mut self, session_id: &str, can_claim: bool) -> Result<IdentityDecision, NormalizeError> {
        self.refresh_identity()?;
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
            return self.persist_claim(session_id);
        }
        let live = match (self.run_id.as_deref(), self.identity_store.as_deref()) {
            (Some(run_id), Some(store)) => store
                .progress_run_is_non_terminal(run_id)
                .map_err(NormalizeError::IdentityStoreTransient)?,
            _ => false,
        };
        if live {
            tracing::error!(
                session_id,
                "agy Stop arrived before a claimed conversation identity; retry required"
            );
            return Err(NormalizeError::BoundaryBeforeClaim(
                "agy Stop arrived before a claimed conversation identity; retry after SessionStart".into(),
            ));
        }
        tracing::error!(
            session_id,
            "agy Stop arrived before a claimed conversation identity on a terminal run; rejecting permanently"
        );
        Err(NormalizeError::Malformed(
            "agy Stop arrived before a claimed conversation identity on a terminal run".into(),
        ))
    }

    fn decode(&mut self, raw: &Value) -> Result<Option<WorkerEvent>, NormalizeError> {
        self.from_subagent = false;
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
                IdentityDecision::Accept { .. } => super::turn_boundary::normalize_stop(&canonical),
            },
            "PreToolUse" | "PostToolUse" => {
                self.refresh_identity()?;
                self.from_subagent = self.claimed_id.as_deref().is_some_and(|id| id != session_id);
                normalize_hook_event(&canonical).map(Some)
            }
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
    fn last_event_from_subagent(&self) -> bool {
        self.from_subagent
    }

    fn normalize_progress_event(&mut self, raw: &Value) -> Result<WorkerEvent, NormalizeError> {
        self.normalize_progress_events(raw)?
            .pop()
            .ok_or_else(|| NormalizeError::UnknownEvent("agy_filtered_progress".into()))
    }

    fn normalize_progress_events(&mut self, raw: &Value) -> Result<Vec<WorkerEvent>, NormalizeError> {
        let Some(event) = self.decode(raw)? else {
            return Ok(vec![]);
        };
        self.pending_tools.observe(raw, &event, self.claimed_id.as_deref());
        let mut events = match &event {
            WorkerEvent::Stop { session_id, .. } if raw.get("fullyIdle").and_then(Value::as_bool) == Some(true) => {
                self.pending_tools.drain(session_id)
            }
            _ => Vec::new(),
        };
        events.push(event);
        Ok(events)
    }

    fn transcript_path_for_session(&mut self, _raw: &Value) -> Option<String> {
        // `AgyDriver::transcript_path_for_session` owns the stamp rewrite
        // onto the durable store. This session does not duplicate it.
        None
    }

    fn resume_state(&self) -> Option<Value> {
        self.claimed_id
            .as_ref()
            .map(|id| json!({ "claimed_conversation_id": id, "pending_tools": self.pending_tools }))
    }

    fn restore_resume_state(&mut self, state: &Value) -> Result<(), String> {
        self.pending_tools = match state.get("pending_tools") {
            Some(value) => serde_json::from_value(value.clone()).map_err(|err| err.to_string())?,
            None => Default::default(),
        };
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
#[path = "progress_tests.rs"]
mod tests;
