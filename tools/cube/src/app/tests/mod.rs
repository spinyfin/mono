//! Unit tests for `crate::app`, split to mirror the production module layout.

mod bazel_output_base_tests;
mod change_tests;
mod checkleft_tests;
mod cleanup_tests;
mod dispatch_tests;
mod excludes_tests;
mod gc_tests;
mod gh_pr_tests;
mod goto_tests;
mod health_recovery_tests;
mod jj_tests;
mod lease_reclaim_tests;
mod lease_tests;
mod list_status_tests;
mod pr_bookmarks_tests;
mod pr_push_tests;
mod pr_rebase_tests;
mod pr_tests;
mod push_tests;
mod quarantine_tests;
mod rebase_tests;
mod reclaim_bazel_bases_tests;
mod reclaim_tests;
mod reconcile_tests;
mod release_tests;
mod remove_tests;
mod repo_tests;
mod setup_tests;
mod store_gc_tests;
mod support;

pub(super) fn with_checkleft_fixture<T>(run: impl FnOnce() -> T) -> T {
    let _lock = support::ENV_MUTEX.lock().unwrap();
    let _env = checkleft_tests::CheckleftEnvGuard::with_gate_disabled();
    run()
}

mod cleanup_source_tests;
