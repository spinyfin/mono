//! A small Agent Client Protocol (ACP) v1 client.
//!
//! ACP's one defined transport is newline-delimited JSON-RPC over a byte
//! stream. This crate owns the three things a client needs on top of the
//! typed messages:
//!
//! - [`framing`]: splitting the byte stream into lines.
//! - [`journal`]: the record every line is written to, in both directions.
//! - [`connection`]: a connection that allocates request ids, correlates
//!   responses, and can be told to expect the response to a request that an
//!   earlier connection on the same stream published.
//!
//! The message types are not defined here. They come from the official
//! `agent-client-protocol` crate, re-exported as [`schema`], and requests are
//! sent through that crate's own `JsonRpcRequest` trait so a request type
//! fixes its method name and its response type.
//!
//! The crate knows nothing about Boss, the coordinator, or which agent is on
//! the other end of the stream.

pub mod connection;
pub mod framing;
pub mod journal;

/// Version-neutral schema items, such as `ProtocolVersion`.
pub use agent_client_protocol::schema as common_schema;
/// The protocol v1 message types from the official crate.
pub use agent_client_protocol::v1::schema;
/// The traits that tie a typed message to its method name and response type.
pub use agent_client_protocol::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, UntypedMessage};
