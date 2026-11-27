//! Conversation reducer, turn queue, intent log, replay snapshot, and
//! credential redaction for the native coordinator chat backend.
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

pub mod intent;
pub mod queue;
pub mod redact;
pub mod reducer;
pub mod snapshot;

#[cfg(test)]
mod intent_tests;
#[cfg(test)]
mod queue_tests;
#[cfg(test)]
mod redact_tests;
#[cfg(test)]
mod reducer_tests;

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
pub use snapshot::{
    DEFAULT_SNAPSHOT_INTERVAL_RECORDS, RecoveryResult, ReducerSnapshot, SNAPSHOT_SCHEMA_VERSION, SnapshotCursor,
    SnapshotLoad, SnapshotLoadStatus, SnapshotProjection, UnsentDispatchPolicy, load_snapshot, load_snapshot_bytes,
    read_wire_file_if_exists, recover_from_files, recover_from_slices, recover_from_snapshot_and_tail, save_snapshot,
};
