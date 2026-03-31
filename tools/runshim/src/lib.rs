pub mod app;
mod bazel;
mod cli;
mod config;
mod dispatch;
mod install;
mod shell;

pub use app::{RunshimError, run_from_env};
