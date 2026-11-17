//! Capture interrupt recovery using the run's persisted parent identity.

use boss_engine_driver::{AgentDriver, InterruptRecoverySnapshot};
use boss_work_db::WorkDb;

pub(super) fn prepare(db: &WorkDb, driver: &dyn AgentDriver, run_id: &str) -> Option<InterruptRecoverySnapshot> {
    let claimed_id = db.get_run_progress_session_identity(run_id).unwrap_or_else(|error| {
        tracing::warn!(run_id, %error, "interrupt recovery: cannot read claimed identity");
        None
    });
    // Identity-dependent drivers arm no snapshot without a readable claim;
    // drivers whose recovery does not need an identity retain their behaviour.
    driver.prepare_interrupt_recovery(run_id, claimed_id.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_engine_driver::{AgyDriver, agy::AGY_HOMES_ROOT_ENV};
    use boss_engine_runtime::test_support::{create_ready_chore_execution, create_test_chore, create_test_product};

    struct RestoreRoot(Option<std::ffi::OsString>);

    impl Drop for RestoreRoot {
        fn drop(&mut self) {
            unsafe {
                match self.0.take() {
                    Some(value) => std::env::set_var(AGY_HOMES_ROOT_ENV, value),
                    None => std::env::remove_var(AGY_HOMES_ROOT_ENV),
                }
            }
        }
    }

    #[test]
    fn persisted_identity_reaches_driver_and_missing_or_unreadable_claim_arms_nothing() {
        let _lock = boss_engine_driver::agy::AGY_HOMES_ENV_TEST_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let _restore = RestoreRoot(std::env::var_os(AGY_HOMES_ROOT_ENV));
        unsafe { std::env::set_var(AGY_HOMES_ROOT_ENV, root.path()) };
        let db = WorkDb::open_in_memory().unwrap();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id, "Interrupt recovery");
        let execution = create_ready_chore_execution(&db, chore.id);
        db.start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease",
            "workspace",
            "/tmp/workspace",
        )
        .unwrap();
        let control = root.path().join(&execution.id).join("control");
        std::fs::create_dir_all(&control).unwrap();
        let driver = AgyDriver::default();
        assert!(prepare(&db, &driver, &execution.id).is_none());

        let parent = "9476903e-615e-4a03-b5b5-ea82733cc97b";
        db.claim_run_progress_session_identity(&execution.id, parent).unwrap();
        let snapshot = prepare(&db, &driver, &execution.id).expect("persisted parent arms recovery");
        assert_eq!(snapshot.session_id, parent);
        assert_eq!(snapshot.events_path, control.join("agy.log"));

        db.clear_run_progress_session_identity(&execution.id).unwrap();
        assert!(prepare(&db, &driver, &execution.id).is_none());
        // A malformed stored value forces the actual DB read to fail.
        db.connect()
            .unwrap()
            .execute("UPDATE work_runs SET progress_session_id = x'00'", [])
            .unwrap();
        assert!(db.get_run_progress_session_identity(&execution.id).is_err());
        assert!(prepare(&db, &driver, &execution.id).is_none());
    }
}
