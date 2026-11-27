//! Conversation reducer, turn queue, intent log, replay snapshot, credential
//! redaction, and the live service for the native coordinator chat backend.
//!
//! This crate reduces the frozen wire journal (`boss_agent_host::journal::WireRecord`)
//! joined with the engine's append-only intent log ([`intent::IntentLog`]) into
//! Boss's own chat vocabulary (`boss_protocol::types::CoordinatorChat*`):
//! connection state, ordered chat items, an in-progress streaming snapshot,
//! incremental offset-carrying deltas, and the single-flight [`queue::TurnQueue`].
//! A versioned [`snapshot::ReducerSnapshot`] caches reducer state across engine
//! restarts and falls back transparently to full replay on any cache fault.
//! Every text-bearing field — including streamed text with a held-back tail —
//! is scrubbed of the coordinator generation's write credential before it
//! leaves the reducer.
//!
//! [`service::ChatService`] runs all of that live against an agent host's
//! socket: it verifies the host, drives the queue, writes the intent log
//! before acknowledging a send, publishes items and streaming deltas, serves
//! fetches, and reconnects with replay. [`retention`] creates and prunes the
//! per-generation directories the files live in.

pub mod intent;
pub mod queue;
pub mod redact;
pub mod reducer;
pub mod retention;
pub mod service;
pub mod snapshot;

#[cfg(test)]
mod intent_tests;
#[cfg(test)]
mod queue_tests;
#[cfg(test)]
mod redact_tests;
#[cfg(test)]
mod reducer_tests;
#[cfg(test)]
mod retention_tests;

pub use boss_agent_host::journal::{Direction, WireRecord, read_records};
pub use intent::{
    IntentLog, IntentRecord, IntentRecordKind, REPLAY_READ_BUFFER_BYTES, WithdrawReason, read_intent_file,
    read_intent_file_if_exists, read_intent_records,
};
pub use queue::{InFlightPrompt, PromptContent, PromptImageRef, PromptOrigin, QueuedPrompt, StopOutcome, TurnQueue};
pub use redact::{CredentialRedactor, REDACTED_PLACEHOLDER, StreamingRedactor};
pub use reducer::{
    ConversationReducer, DEFAULT_TOOL_TEXT_LIMIT, ReduceDelta, replay_reader, replay_records, replay_with_intent_log,
};
pub use retention::{
    DEFAULT_MAX_AGE, DEFAULT_MAX_TOTAL_BYTES, Generation, GenerationStore, PruneReason, PruneReport, PrunedGeneration,
    RetentionPolicy,
};
pub use service::{
    CancelOutcome, ChatBackend, ChatError, ChatPush, ChatService, ConnectionStatus, DELTA_PUBLISH_INTERVAL,
    HostEndpoint, ReturnedDraft, ServiceConfig, ServiceTuning,
};
pub use snapshot::{
    DEFAULT_SNAPSHOT_INTERVAL_RECORDS, RecoveryResult, ReducerSnapshot, SNAPSHOT_SCHEMA_VERSION, SnapshotCursor,
    SnapshotLoad, SnapshotLoadStatus, SnapshotProjection, UnsentDispatchPolicy, load_snapshot, load_snapshot_bytes,
    read_wire_file_if_exists, recover_from_files, recover_from_slices, recover_from_snapshot_and_tail, save_snapshot,
};
