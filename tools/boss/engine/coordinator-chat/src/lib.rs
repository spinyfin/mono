//! Conversation reducer and credential redaction for the native coordinator
//! chat backend.
//!
//! This crate reduces the frozen wire journal (`boss_agent_host::journal::WireRecord`)
//! into Boss's own chat vocabulary (`boss_protocol::types::CoordinatorChat*`):
//! connection state, ordered chat items, an in-progress streaming snapshot,
//! and incremental offset-carrying deltas. Every text-bearing field — including
//! streamed text with a held-back tail — is scrubbed of the coordinator
//! generation's write credential before it leaves the reducer.

pub mod redact;
pub mod reducer;

#[cfg(test)]
mod redact_tests;
#[cfg(test)]
mod reducer_tests;

pub use boss_agent_host::journal::{Direction, WireRecord, read_records};
pub use redact::{CredentialRedactor, REDACTED_PLACEHOLDER, StreamingRedactor};
pub use reducer::{
    ConversationReducer, DEFAULT_TOOL_TEXT_LIMIT, PromptIntent, ReduceDelta, replay_reader, replay_records,
    replay_with_intents,
};
