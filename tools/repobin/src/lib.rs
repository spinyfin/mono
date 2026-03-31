pub mod app;
mod bazel;
mod cli;
mod config;
mod dispatch;
mod install;
mod shell;

pub use app::{RepobinError, run_from_env};
