//! The `executions.<id>` / product-topic publication contract and the post-commit
//! review-guide notification built on it.

#[cfg(test)]
pub use boss_execution_contracts::NoopExecutionPublisher;

pub(crate) use boss_engine_completion::execution_publisher::notify_review_guide_changed;
