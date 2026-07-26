//! Pre-spawn log-excerpt path: injectable `CiLogReaderFactory` must never
//! shell out to real `bk`/`gh` under unit tests, and a custom factory must
//! be able to store a concrete excerpt.

use super::helpers::*;
use crate::ci_log_reader::{CiLogReader, FixedContentLogReader};
use crate::ci_watch::ci_log_reader_hooks;

fn canned_reader_factory(_provider: crate::ci_log_reader::CiProvider, _target_url: &str) -> Box<dyn CiLogReader> {
    Box::new(FixedContentLogReader::new("ERROR: canned fixture failure at line 42\n"))
}

#[tokio::test]
async fn default_factory_is_noop_so_excerpt_stays_absent() {
    // Default under cfg(test) is noop_reader_for: even with a real-looking
    // Buildkite provider_job_id fixture, the pre-spawn fetch must not spawn
    // `bk` and must leave log_excerpt unset (best-effort failure is silent).
    ci_log_reader_hooks::reset();

    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/9001";
    let (product, chore) = make_in_review(&db, "C-log-noop", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-log-1"),
        &one_failure(),
    )
    .await;
    assert!(flipped);

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("pending attempt");
    assert!(
        attempt.revision_task_id.is_some(),
        "revision must still spawn when log fetch is a no-op"
    );
    assert_eq!(
        attempt.log_excerpt, None,
        "default test factory must not store an excerpt (and must not shell out)"
    );
}

#[tokio::test]
async fn injected_factory_stores_log_excerpt_on_attempt() {
    // A recording/canned factory proves the injection seam: production
    // still uses reader_for, but tests can supply a FixedContentLogReader
    // and observe the stored excerpt without any real CLI.
    ci_log_reader_hooks::set_factory(Some(canned_reader_factory));

    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/9002";
    let (product, chore) = make_in_review(&db, "C-log-inject", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-log-2"),
        &one_failure(),
    )
    .await;
    assert!(flipped);

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("pending attempt");
    assert_eq!(
        attempt.log_excerpt.as_deref(),
        Some("ERROR: canned fixture failure at line 42"),
        "injected factory content must be stored on the remediation row"
    );

    ci_log_reader_hooks::reset();
}
