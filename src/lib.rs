//! Robinhood broker client implementation.

pub mod auth;
pub mod client;
pub mod error;

pub use crate::auth::{AuthChallenge, VerificationWorkflow};
pub use crate::client::RobinhoodClient;
pub use crate::error::RobinhoodClientError;
