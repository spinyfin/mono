//! The host credential authority, end to end.
//!
//! One test crate, so the fixtures in `support` are shared by both suites:
//!
//! - `lifecycle`: the credential lifecycle against a real CA and database,
//!   with time under the test's control (enrollment, renewal overlap, expiry,
//!   revocation, restart).
//! - `tls_boundary`: the authority behind a real mutual-TLS listener
//!   (enrollment and renewal over gRPC, a stand-in for the stream manager's
//!   `Connect` handler, revocation closing open streams and refusing
//!   reconnects).

mod lifecycle;
mod support;
mod tls_boundary;
