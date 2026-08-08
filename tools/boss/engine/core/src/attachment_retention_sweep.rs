//! Periodic reclaim of stored screenshot evidence past retention policy.
//!
//! Mirrors `codex_home_retention_sweep` / `grok_home_retention_sweep`: same
//! cadence, same outcome shape, same "candidates come from recorded rows,
//! liveness comes from execution status" rule. Two things differ, both
//! forced by content addressing:
//!
//! - A blob can be shared by several rows, so
//!   [`boss_engine_attachments::plan_reclaim`] deletes it only once every row
//!   pointing at it is being reclaimed.
//! - Reclaiming stamps `reclaimed_at` rather than deleting the row, so a
//!   gallery link in a merged PR body can explain itself instead of 404ing.
//!
//! ## The orphan pass
//!
//! The home sweeps deliberately never scan a directory — they would risk
//! deleting an operator's interactive `~/.codex`. This store has no such
//! hazard: `<state_root>/attachments` is created by, written by, and owned
//! by the engine alone. Scanning it is therefore both safe and *necessary*,
//! because a crash between "blob written" and "row inserted" leaves bytes no
//! row will ever reference and so no row-driven sweep would ever reclaim. A
//! grace period keeps the pass off blobs that are merely mid-ingest.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use boss_engine_attachments::store::AttachmentStore;
use boss_engine_attachments::{AttachmentRetentionPolicy, plan_reclaim};

use crate::work::WorkDb;

/// How often the retention pass runs. Same cadence as the home sweeps —
/// reclaim is not time-critical once the policy window has elapsed.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// How long an unreferenced blob must sit before the orphan pass takes it.
/// Comfortably longer than the window between the store write and the row
/// insert, which is one SQLite transaction.
pub const ORPHAN_GRACE: Duration = Duration::from_secs(60 * 60);

/// Counts from one pass; logged when anything was reclaimed or errored.
#[derive(Debug, Default, PartialEq, Eq, bon::Builder)]
pub struct AttachmentRetentionSweepOutcome {
    #[builder(default)]
    pub scanned: u64,
    /// Rows stamped `reclaimed_at`.
    pub reclaimed_rows: u64,
    /// Blobs deleted from the store (row-driven plus orphans).
    #[builder(default)]
    pub deleted_blobs: u64,
    #[builder(default)]
    pub reclaimed_bytes: u64,
    #[builder(default)]
    pub skipped_live: u64,
    #[builder(default)]
    pub kept_in_policy: u64,
    /// Blobs with no referencing row, removed by the orphan pass.
    #[builder(default)]
    pub orphans_deleted: u64,
    #[builder(default)]
    pub errors: u64,
}

impl crate::sweep_loop::SweepOutcome for AttachmentRetentionSweepOutcome {
    fn has_activity(&self) -> bool {
        self.reclaimed_rows > 0 || self.orphans_deleted > 0 || self.errors > 0
    }

    fn log(&self) {
        tracing::info!(
            scanned = self.scanned,
            reclaimed_rows = self.reclaimed_rows,
            deleted_blobs = self.deleted_blobs,
            reclaimed_bytes = self.reclaimed_bytes,
            skipped_live = self.skipped_live,
            kept_in_policy = self.kept_in_policy,
            orphans_deleted = self.orphans_deleted,
            errors = self.errors,
            "attachment retention sweep: reclaimed evidence past policy",
        );
    }
}

/// Spawn a tokio task running [`run_one_pass`] forever at `interval`.
pub fn spawn_loop(work_db: Arc<WorkDb>, store: AttachmentStore, interval: Duration) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let store = store.clone();
        async move { run_one_pass(work_db.as_ref(), &store, false).await }
    })
}

/// Run a single pass with the default policy.
pub async fn run_one_pass(work_db: &WorkDb, store: &AttachmentStore, dry_run: bool) -> AttachmentRetentionSweepOutcome {
    run_one_pass_with_policy(work_db, store, &AttachmentRetentionPolicy::default(), dry_run).await
}

/// Run a single pass with an explicit policy (`bossctl` / tests).
pub async fn run_one_pass_with_policy(
    work_db: &WorkDb,
    store: &AttachmentStore,
    policy: &AttachmentRetentionPolicy,
    dry_run: bool,
) -> AttachmentRetentionSweepOutcome {
    let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();

    let candidates = match work_db.list_retained_attachments() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "attachment retention sweep: failed to list attachments; skipping"
            );
            return AttachmentRetentionSweepOutcome::default();
        }
    };

    let plan = plan_reclaim(&candidates, now_epoch, policy);
    let mut outcome = AttachmentRetentionSweepOutcome {
        scanned: plan.scanned as u64,
        skipped_live: plan.skipped_live as u64,
        kept_in_policy: plan.kept_in_policy as u64,
        reclaimed_bytes: plan.reclaimed_bytes,
        ..AttachmentRetentionSweepOutcome::default()
    };

    if !dry_run {
        // Rows first. A stamped row with its blob still present is a
        // harmless transient (the orphan pass collects the blob next time);
        // a deleted blob with an unstamped row would serve a 404 that reads
        // like a bug for a whole interval.
        match work_db.mark_attachments_reclaimed(&plan.rows_to_reclaim) {
            Ok(updated) => outcome.reclaimed_rows = updated as u64,
            Err(err) => {
                tracing::warn!(
                    error = %format!("{err:#}"),
                    "attachment retention sweep: failed to stamp reclaimed rows; skipping blob deletion"
                );
                outcome.errors += 1;
                return outcome;
            }
        }
        for (digest, media_type) in &plan.blobs_to_delete {
            match store.remove_blob(digest, *media_type) {
                Ok(()) => outcome.deleted_blobs += 1,
                Err(err) => {
                    tracing::warn!(digest, error = %format!("{err:#}"), "attachment retention sweep: blob delete failed");
                    outcome.errors += 1;
                }
            }
        }
    } else {
        outcome.reclaimed_rows = plan.rows_to_reclaim.len() as u64;
        outcome.deleted_blobs = plan.blobs_to_delete.len() as u64;
    }

    match sweep_orphans(work_db, store, now_epoch, dry_run) {
        Ok((deleted, errors)) => {
            outcome.orphans_deleted = deleted;
            outcome.errors += errors;
        }
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "attachment retention sweep: orphan pass failed");
            outcome.errors += 1;
        }
    }

    outcome
}

/// Delete blobs no live row references and that are older than
/// [`ORPHAN_GRACE`]. Returns `(deleted, errors)`.
fn sweep_orphans(
    work_db: &WorkDb,
    store: &AttachmentStore,
    now_epoch: i64,
    dry_run: bool,
) -> anyhow::Result<(u64, u64)> {
    let referenced: HashSet<String> = work_db.referenced_attachment_digests()?;
    let mut deleted = 0u64;
    let mut errors = 0u64;

    let root = store.root();
    if !root.is_dir() {
        return Ok((0, 0));
    }
    for shard in std::fs::read_dir(root)? {
        let shard = match shard {
            Ok(entry) => entry,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        if !shard.path().is_dir() {
            continue;
        }
        for blob in std::fs::read_dir(shard.path())? {
            let Ok(blob) = blob else {
                errors += 1;
                continue;
            };
            let path = blob.path();
            let Some(digest) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if referenced.contains(digest) {
                continue;
            }
            let age_ok = blob
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|m| m.elapsed().ok())
                .is_none_or(|elapsed| elapsed >= ORPHAN_GRACE);
            if !age_ok {
                continue;
            }
            let _ = now_epoch;
            if dry_run {
                deleted += 1;
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => deleted += 1,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "attachment orphan sweep: delete failed");
                    errors += 1;
                }
            }
        }
    }
    Ok((deleted, errors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::work::SubmitAttachmentInput;
    use boss_engine_attachments::IngestedImage;
    use boss_protocol::AttachmentMediaType;

    const DAY: i64 = 24 * 60 * 60;

    fn store_at(dir: &std::path::Path) -> AttachmentStore {
        AttachmentStore::at(dir.join("attachments"))
    }

    fn image(digest: &str) -> IngestedImage {
        IngestedImage {
            content_digest: digest.to_owned(),
            media_type: AttachmentMediaType::Png,
            pixel_width: 10,
            pixel_height: 10,
            size_bytes: 4,
            source_name: "shot.png".to_owned(),
        }
    }

    /// Store one attachment against a terminal execution `age_days` old.
    fn seed(db: &WorkDb, store: &AttachmentStore, name: &str, digest: &str, age_days: i64) -> String {
        let product = create_test_product(db);
        let chore = create_test_chore(db, &product.id, name);
        let execution = create_ready_chore_execution(db, &chore.id);
        let stored = db
            .submit_work_attachment(SubmitAttachmentInput {
                execution_id: &execution.id,
                work_item_id: &chore.id,
                image: &image(digest),
                caption: "",
            })
            .unwrap()
            .unwrap();
        store.write_blob(digest, AttachmentMediaType::Png, b"data").unwrap();
        let created = boss_engine_utils::epoch_time::now_epoch_secs() - age_days * DAY;
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            rusqlite::params![execution.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_attachments SET created_at = ?2 WHERE id = ?1",
            rusqlite::params![stored.attachment.id, created.to_string()],
        )
        .unwrap();
        stored.attachment.id
    }

    #[tokio::test]
    async fn reclaims_past_policy_and_leaves_a_tombstone() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(tmp.path());
        let (_dir, db) = open_db();

        let old = seed(&db, &store, "old", "aaold", 200);
        let fresh = seed(&db, &store, "fresh", "bbfresh", 1);

        let policy = AttachmentRetentionPolicy::new(Duration::from_secs(90 * DAY as u64), u64::MAX);
        let outcome = run_one_pass_with_policy(&db, &store, &policy, false).await;

        assert_eq!(outcome.errors, 0, "{outcome:?}");
        assert_eq!(outcome.reclaimed_rows, 1, "{outcome:?}");
        assert_eq!(outcome.deleted_blobs, 1, "{outcome:?}");
        assert!(
            db.get_work_attachment(&old).unwrap().unwrap().is_reclaimed(),
            "the row survives as a tombstone so a stale PR link can explain itself"
        );
        assert!(!db.get_work_attachment(&fresh).unwrap().unwrap().is_reclaimed());
        assert!(!store.blob_path("aaold", AttachmentMediaType::Png).exists());
        assert!(store.blob_path("bbfresh", AttachmentMediaType::Png).exists());

        // Idempotent: a second pass has nothing left to do.
        let second = run_one_pass_with_policy(&db, &store, &policy, false).await;
        assert_eq!(second.reclaimed_rows, 0, "{second:?}");
        assert_eq!(second.errors, 0, "{second:?}");
    }

    #[tokio::test]
    async fn dry_run_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(tmp.path());
        let (_dir, db) = open_db();
        let old = seed(&db, &store, "old", "aaold", 200);

        let policy = AttachmentRetentionPolicy::new(Duration::from_secs(90 * DAY as u64), u64::MAX);
        let outcome = run_one_pass_with_policy(&db, &store, &policy, true).await;

        assert_eq!(outcome.reclaimed_rows, 1, "the plan is reported…");
        assert!(
            !db.get_work_attachment(&old).unwrap().unwrap().is_reclaimed(),
            "…but nothing is stamped"
        );
        assert!(store.blob_path("aaold", AttachmentMediaType::Png).exists());
    }

    #[tokio::test]
    async fn orphan_blobs_are_collected_once_past_the_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(tmp.path());
        let (_dir, db) = open_db();

        // A blob no row ever referenced — what a crash between the store
        // write and the row insert leaves behind.
        store.write_blob("orphan", AttachmentMediaType::Png, b"leaked").unwrap();
        let orphan_path = store.blob_path("orphan", AttachmentMediaType::Png);

        let policy = AttachmentRetentionPolicy::new(Duration::from_secs(90 * DAY as u64), u64::MAX);
        let fresh_pass = run_one_pass_with_policy(&db, &store, &policy, false).await;
        assert_eq!(
            fresh_pass.orphans_deleted, 0,
            "a just-written blob is inside the grace window — it may be mid-ingest"
        );
        assert!(orphan_path.exists());

        // Age it past the grace window.
        let old = std::time::SystemTime::now() - ORPHAN_GRACE - Duration::from_secs(60);
        std::fs::File::options()
            .append(true)
            .open(&orphan_path)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let outcome = run_one_pass_with_policy(&db, &store, &policy, false).await;
        assert_eq!(outcome.orphans_deleted, 1, "{outcome:?}");
        assert!(!orphan_path.exists());
    }

    #[tokio::test]
    async fn live_executions_are_never_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(tmp.path());
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "live");
        let execution = create_ready_chore_execution(&db, &chore.id);
        let stored = db
            .submit_work_attachment(SubmitAttachmentInput {
                execution_id: &execution.id,
                work_item_id: &chore.id,
                image: &image("live"),
                caption: "",
            })
            .unwrap()
            .unwrap();
        store.write_blob("live", AttachmentMediaType::Png, b"x").unwrap();
        {
            let conn = db.connect().unwrap();
            let ancient = (boss_engine_utils::epoch_time::now_epoch_secs() - 900 * DAY).to_string();
            conn.execute(
                "UPDATE work_attachments SET created_at = ?2 WHERE id = ?1",
                rusqlite::params![stored.attachment.id, ancient],
            )
            .unwrap();
        }

        let policy = AttachmentRetentionPolicy::new(Duration::from_secs(DAY as u64), 0);
        let outcome = run_one_pass_with_policy(&db, &store, &policy, false).await;
        assert_eq!(outcome.skipped_live, 1, "{outcome:?}");
        assert_eq!(outcome.reclaimed_rows, 0, "{outcome:?}");
        assert!(store.blob_path("live", AttachmentMediaType::Png).exists());
    }

    #[tokio::test]
    async fn empty_store_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(tmp.path());
        let (_dir, db) = open_db();
        let outcome = run_one_pass(&db, &store, false).await;
        assert_eq!(outcome, AttachmentRetentionSweepOutcome::default());
    }
}
