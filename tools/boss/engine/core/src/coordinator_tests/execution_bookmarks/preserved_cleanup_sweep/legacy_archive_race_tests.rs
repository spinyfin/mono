use super::*;
use boss_engine_recovery::execution_bookmark::{PreservedScope, cleanup_legacy_store, legacy_archive_name};

#[tokio::test]
async fn stale_closed_chain_cleanup_keeps_archives_during_migration_and_inspection() {
    for completed in [false, true] {
        let mut f = fixture();
        let chore = f.chore("Old chain with recovered legacy work");
        let id = f.preserved(&chore, "legacy.txt").await;
        let tip = f.db.preserved_tip(&id).unwrap().unwrap();
        // Existing restore archives make archive cleanup pending before the
        // migration creates another archive in its own namespace.
        let restored = preserved_archive_name(&id, &tip.commit_id);
        JjRepo::run(&f.repo.repo, &["bookmark", "create", &restored, "-r", &tip.commit_id]);
        f.db.upsert_execution_attention(
            &id,
            PRESERVED_WORK_ARCHIVED_ATTENTION_KIND,
            "Archived variant",
            &restored,
        )
        .unwrap();
        f.close_chain(&chore, 30 * DAY);
        // Capture the sweep's request before the migration starts.
        let cleanup = f.db.list_closed_chain_preserved(14 * DAY, 50, &[]).unwrap().remove(0);
        assert_eq!(cleanup.scope, PreservedScope::TipAndArchives);
        // Use the recorded canonical store path, exactly as the migration's
        // seed query does (macOS temp paths can have a /private alias).
        let repo = f.db.preservation_source(&id).unwrap().unwrap().repo_path;
        f.sql(
            "INSERT INTO legacy_store_cleanups (host_id, repo_path) VALUES ('local', ?1)",
            [repo.to_str().unwrap()],
        );
        assert!(f.db.preserved_archives_protected(&id).unwrap());
        JjRepo::run(
            &f.repo.repo,
            &[
                "bookmark",
                "create",
                &format!("boss-recovery/{id}"),
                "-r",
                &tip.commit_id,
            ],
        );
        let report = cleanup_legacy_store(&LocalJj, &f.repo.repo, &[]).await.unwrap();
        let archive = legacy_archive_name(&id, &tip.commit_id);
        assert!(f.exists(&archive));
        if completed {
            // The other interleaving: completion files an open inspection item
            // after the sweep read but before it forgets.
            f.db.finish_legacy_store_cleanup("local", &repo, &report).unwrap();
        }
        let forgotten = crate::preserved_cleanup_sweep::forget_one(&f.db, &f.coordinator, &cleanup)
            .await
            .unwrap();
        assert_eq!(forgotten, 1, "only Tip was sent to the real coordinator");
        assert!(!f.exists(&preserved_name(&id)));
        assert!(f.exists(&archive));
        let pending: bool =
            f.db.connect()
                .unwrap()
                .query_row(
                    "SELECT archive_cleanup_pending FROM execution_bookmarks WHERE execution_id = ?1",
                    [&id],
                    |r| r.get(0),
                )
                .unwrap();
        assert!(pending);
        if !completed {
            f.db.finish_legacy_store_cleanup("local", &repo, &report).unwrap();
        }
        assert_eq!(f.pass().await, CleanupReport::default());
        assert_eq!(
            JjRepo::run(&f.repo.repo, &["log", "--no-graph", "-r", &archive, "-T", "commit_id"]),
            tip.commit_id
        );
    }
}
