use super::*;

struct ReleaseCube(std::sync::Mutex<Vec<bool>>);
crate::stub_cube_client! { ReleaseCube {
    async fn release_workspace(&self, _: &str) -> Result<()> {
        self.0.lock().unwrap().push(false);
        Ok(())
    }
    async fn release_workspace_with_reset(&self, _: &str) -> Result<()> {
        self.0.lock().unwrap().push(true);
        Ok(())
    }
} }

#[tokio::test]
async fn local_teardown_forces_reset_only_after_preservation_succeeds() {
    for preserved in [false, true] {
        let workspace = tempdir().unwrap();
        let (_dir, db, _, chore, execution) = fixture(workspace.path());
        let TestHarness { mut handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
        let cube = Arc::new(ReleaseCube(Default::default()));
        handler.cube_client = cube.clone();
        let missing_repository = tempdir().unwrap();
        if !preserved {
            // Occupancy is known, but the recorded path cannot be preserved
            // because it is not a jj workspace.
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE work_executions SET workspace_path = ?2 WHERE id = ?1",
                    rusqlite::params![&execution, missing_repository.path().to_str().unwrap()],
                )
                .unwrap();
        }
        db.complete_pane_parked_execution(&execution, "completed", None)
            .unwrap();
        handler
            .finish_worker_teardown(
                &execution,
                &chore,
                Some("lease-1"),
                Some(workspace.path()),
                "test",
                handler.begin_teardown(&execution),
            )
            .await;
        assert_eq!(*cube.0.lock().unwrap(), vec![preserved]);
        assert!(db.get_execution(&execution).unwrap().cube_lease_id.is_none());
    }
}
