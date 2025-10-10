//! Robinhood broker client implementation.

pub mod client;
pub mod error;

pub use crate::client::RobinhoodClient;
pub use crate::error::RobinhoodClientError;
