use super::*;
use crate::database_backup::*;
use crate::work::WorkDb;
use boss_engine_offsite_backup::BACKUP_FILE_PREFIX;
use std::time::Duration;
use tempfile::TempDir;
fn open_file_db(dir: &Path) -> WorkDb {
    WorkDb::open(dir.join("state.db")).unwrap()
}
// ── off-machine copies ────────────────────────────────────────

fn offsite_runtime(dest: &Path, keep_hourly: usize, keep_daily: usize) -> (Arc<OffsiteRuntime>, Arc<Registry>) {
    let registry = Arc::new(Registry::new());
    register_metrics(&registry);
    let config = OffsiteConfig {
        enabled: true,
        destination: Some(dest.to_path_buf()),
        keep_hourly,
        keep_daily,
    };
    let rt = OffsiteRuntime::from_config(config, registry.clone(), dest.join("success-record")).expect("enabled");
    (rt, registry)
}

fn host_dir(dest: &Path) -> PathBuf {
    dest.join(boss_engine_offsite_backup::sanitize_host_component(
        &boss_engine_offsite_backup::host_name(),
    ))
}

#[test]
fn offsite_disabled_by_default() {
    let tmp = TempDir::new().unwrap();
    let registry = Arc::new(Registry::new());
    assert!(OffsiteRuntime::from_settings(&tmp.path().join("settings.toml"), registry.clone()).is_none());
    std::fs::write(tmp.path().join("settings.toml"), "default_pr_draft_mode = true\n").unwrap();
    assert!(OffsiteRuntime::from_settings(&tmp.path().join("settings.toml"), registry).is_none());
}

#[test]
fn offsite_enabled_without_destination_fails_loudly() {
    let tmp = TempDir::new().unwrap();
    let settings = tmp.path().join("settings.toml");
    std::fs::write(&settings, "[backup.offsite]\nenabled = true\n").unwrap();
    let registry = Arc::new(Registry::new());
    register_metrics(&registry);
    let rt = OffsiteRuntime::from_settings(&settings, registry.clone()).expect("runtime kept for retries");
    assert_eq!(rt.age_secs_at(123), -1);
    // Reported at startup, before any snapshot exists.
    rt.validate_at_startup();
    assert_eq!(
        registry.counter_value("database_backup.offsite.config_invalid"),
        Some(1)
    );
    // Copies fail (and are counted) rather than guessing a destination.
    let snap = tmp.path().join("state.db.bak-20260101-000000");
    std::fs::write(&snap, b"x").unwrap();
    rt.copy_and_prune(rt.open_snapshot(&snap).unwrap());
    assert_eq!(
        registry.counter_value("database_backup.offsite.config_invalid"),
        Some(2)
    );
    assert_eq!(registry.counter_value("database_backup.offsite.copies_failed"), Some(1));
    assert_eq!(
        registry.counter_value("database_backup.offsite.copies_succeeded"),
        Some(0)
    );
}

#[test]
fn offsite_unparseable_section_is_counted() {
    let tmp = TempDir::new().unwrap();
    let settings = tmp.path().join("settings.toml");
    std::fs::write(&settings, "[backup.offsite]\nenabled = \"yes\"\n").unwrap();
    let registry = Arc::new(Registry::new());
    register_metrics(&registry);
    assert!(OffsiteRuntime::from_settings(&settings, registry.clone()).is_none());
    assert_eq!(
        registry.counter_value("database_backup.offsite.config_invalid"),
        Some(1)
    );
    assert_eq!(
        registry.gauge_value("database_backup.offsite.last_success_age_secs"),
        Some(-1)
    );
}

#[test]
fn startup_reports_invalid_destination_when_local_snapshot_fails() {
    let tmp = TempDir::new().unwrap();
    let db = open_file_db(tmp.path());
    // A regular file as the backup dir makes every local snapshot fail.
    let backup_dir = tmp.path().join("not-a-dir");
    std::fs::write(&backup_dir, b"x").unwrap();
    let (rt, registry) = offsite_runtime(&tmp.path().join("missing-mount"), 1, 1);
    rt.validate_at_startup();
    wait_for_worker(&rt);
    run_backup_with_offsite(&db, &backup_dir, 24, Some(&rt));
    wait_for_worker(&rt);
    assert_eq!(
        registry.counter_value("database_backup.offsite.config_invalid"),
        Some(1)
    );
    assert_eq!(registry.counter_value("database_backup.offsite.copies_failed"), Some(0));
}

#[test]
fn zero_local_retention_still_copies_off_machine() {
    let tmp = TempDir::new().unwrap();
    let db = open_file_db(tmp.path());
    let backup_dir = tmp.path().join("backups");
    let dest = tmp.path().join("sync");
    std::fs::create_dir(&dest).unwrap();
    let (rt, registry) = offsite_runtime(&dest, 24, 14);
    run_backup_with_offsite(&db, &backup_dir, 0, Some(&rt));
    wait_for_worker(&rt);
    assert_eq!(std::fs::read_dir(&backup_dir).unwrap().count(), 0, "local pruned");
    assert_eq!(std::fs::read_dir(host_dir(&dest)).unwrap().count(), 1);
    assert_eq!(
        registry.counter_value("database_backup.offsite.copies_succeeded"),
        Some(1)
    );
}

#[test]
fn snapshot_pruned_while_destination_validation_is_blocked_still_copies() {
    let tmp = TempDir::new().unwrap();
    let dest = tmp.path().join("sync");
    std::fs::create_dir(&dest).unwrap();
    let (rt, registry) = offsite_runtime(&dest, 24, 14);
    let snap = tmp.path().join("state.db.bak-20260101-000000");
    std::fs::write(&snap, b"snapshot").unwrap();
    let pending = rt.open_snapshot(&snap).unwrap();
    // Occupy the worker, then unlink the path before the copy runs.
    let (release, blocked) = std::sync::mpsc::channel();
    let (started, ready) = std::sync::mpsc::channel();
    rt.start_job(move |_| {
        started.send(()).unwrap();
        blocked.recv().unwrap();
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    std::fs::remove_file(&snap).unwrap();
    release.send(()).unwrap();
    wait_for_worker(&rt);
    rt.copy_and_prune(pending);
    assert_eq!(
        std::fs::read(host_dir(&dest).join("state.db.bak-20260101-000000")).unwrap(),
        b"snapshot"
    );
    assert_eq!(
        registry.counter_value("database_backup.offsite.copies_succeeded"),
        Some(1)
    );
}

#[test]
fn success_record_write_failure_is_not_a_failed_copy() {
    let tmp = TempDir::new().unwrap();
    let dest = tmp.path().join("sync");
    std::fs::create_dir(&dest).unwrap();
    let (rt, registry) = offsite_runtime(&dest, 24, 14);
    // Make the success record path a directory so persisting it fails.
    std::fs::remove_file(&rt.success_path).ok();
    std::fs::create_dir(&rt.success_path).unwrap();
    let snap = tmp.path().join("state.db.bak-20260101-000000");
    std::fs::write(&snap, b"snapshot").unwrap();
    rt.copy_and_prune(rt.open_snapshot(&snap).unwrap());
    assert_eq!(registry.counter_value("database_backup.offsite.copies_failed"), Some(0));
    assert_eq!(
        registry.counter_value("database_backup.offsite.copies_succeeded"),
        Some(1)
    );
}

#[test]
fn run_backup_copies_off_machine_and_prunes() {
    let tmp = TempDir::new().unwrap();
    let db = open_file_db(tmp.path());
    let backup_dir = tmp.path().join("backups");
    let dest = tmp.path().join("sync");
    std::fs::create_dir(&dest).unwrap();
    let (rt, registry) = offsite_runtime(&dest, 1, 1);
    // An old off-machine copy that retention must remove once a newer one lands.
    let host = host_dir(&dest);
    std::fs::create_dir_all(&host).unwrap();
    std::fs::write(host.join("state.db.bak-20200101-000000"), b"old").unwrap();

    run_backup_with_offsite(&db, &backup_dir, 24, Some(&rt));
    wait_for_worker(&rt);

    let names: Vec<String> = std::fs::read_dir(&host)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names.len(), 1, "old pruned, new kept, no partials: {names:?}");
    assert!(names[0].starts_with(BACKUP_FILE_PREFIX) && names[0] != "state.db.bak-20200101-000000");
    // The copy is a valid database, not a live-file artefact.
    assert!(host.join(&names[0]).metadata().unwrap().len() > 0);
    assert_eq!(
        registry.counter_value("database_backup.offsite.copies_succeeded"),
        Some(1)
    );
    // Sample at a known instant: copying/pruning may cross a clock second.
    let success = rt.last_success.load(Ordering::Relaxed);
    assert!(success > 0);
    rt.refresh_age_gauge_at(success);
    assert_eq!(
        registry.gauge_value("database_backup.offsite.last_success_age_secs"),
        Some(0)
    );
    rt.refresh_age_gauge_at(success + 120);
    assert_eq!(
        registry.gauge_value("database_backup.offsite.last_success_age_secs"),
        Some(120)
    );
    assert_eq!(std::fs::read_dir(&backup_dir).unwrap().count(), 1, "local backup kept");
}

#[test]
fn offsite_failure_never_fails_local_backup() {
    let tmp = TempDir::new().unwrap();
    let db = open_file_db(tmp.path());
    let backup_dir = tmp.path().join("backups");
    let dest = tmp.path().join("sync");
    std::fs::create_dir(&dest).unwrap();
    let (rt, registry) = offsite_runtime(&dest, 24, 14);
    // Simulate the sync folder vanishing (unmounted) after startup.
    std::fs::remove_dir_all(&dest).unwrap();

    run_backup_with_offsite(&db, &backup_dir, 24, Some(&rt));
    wait_for_worker(&rt);

    assert_eq!(
        std::fs::read_dir(&backup_dir).unwrap().count(),
        1,
        "local backup still taken"
    );
    assert_eq!(registry.counter_value("database_backup.offsite.copies_failed"), Some(1));
    assert!(!dest.exists(), "must not recreate a missing destination");
    assert!(
        registry
            .gauge_value("database_backup.offsite.last_success_age_secs")
            .is_some()
    );
}

#[test]
fn age_gauge_uses_last_success_once_present() {
    let tmp = TempDir::new().unwrap();
    let (rt, _registry) = offsite_runtime(tmp.path(), 1, 1);
    rt.last_success.store(1000, Ordering::Relaxed);
    assert_eq!(rt.age_secs_at(1120), 120);
    assert_eq!(rt.age_secs_at(999), 0);
}

fn wait_for_worker(rt: &OffsiteRuntime) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while rt.in_flight.load(Ordering::Acquire) {
        assert!(std::time::Instant::now() < deadline, "worker did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn restart_preserves_stale_success_and_new_destination_has_no_success() {
    let tmp = TempDir::new().unwrap();
    let config = OffsiteConfig {
        enabled: true,
        destination: Some(tmp.path().join("missing-mount")),
        ..OffsiteConfig::default()
    };
    let path = tmp.path().join("success");
    let old = boss_engine_utils::epoch_time::now_epoch_secs() - 172800;
    std::fs::write(&path, format!("{old}\n{:?}", config.destination)).unwrap();
    let registry = Arc::new(Registry::new());
    register_metrics(&registry);
    let rt = OffsiteRuntime::from_config(config.clone(), registry.clone(), path.clone()).unwrap();
    rt.refresh_age_gauge();
    assert!(
        registry
            .gauge_value("database_backup.offsite.last_success_age_secs")
            .unwrap()
            >= 172800
    );
    let changed = OffsiteConfig {
        destination: Some(tmp.path().join("different")),
        ..config
    };
    let rt = OffsiteRuntime::from_config(changed, registry, path).unwrap();
    assert_eq!(rt.age_secs_at(123), -1);
}

#[test]
fn successful_copy_persists_timestamp_for_restart() {
    let tmp = TempDir::new().unwrap();
    let (rt, registry) = offsite_runtime(tmp.path(), 1, 1);
    let snapshot = tmp.path().join("state.db.bak-20260101-000000");
    std::fs::write(&snapshot, b"snapshot").unwrap();
    rt.copy_and_prune(rt.open_snapshot(&snapshot).unwrap());
    let restarted = OffsiteRuntime::from_config(rt.config.clone(), registry, rt.success_path.clone()).unwrap();
    assert_eq!(
        restarted.last_success.load(Ordering::Relaxed),
        rt.last_success.load(Ordering::Relaxed)
    );
    assert!(restarted.last_success.load(Ordering::Relaxed) > 0);
}

#[test]
fn retention_failure_is_counted_without_failing_copy() {
    let tmp = TempDir::new().unwrap();
    let (rt, registry) = offsite_runtime(tmp.path(), 1, 0);
    let host = host_dir(tmp.path());
    std::fs::create_dir_all(host.join("state.db.bak-20200101-000000")).unwrap();
    let snapshot = tmp.path().join("state.db.bak-20260101-000000");
    std::fs::write(&snapshot, b"snapshot").unwrap();
    rt.copy_and_prune(rt.open_snapshot(&snapshot).unwrap());
    assert_eq!(
        registry.counter_value("database_backup.offsite.retention_failed"),
        Some(1)
    );
    assert_eq!(
        registry.counter_value("database_backup.offsite.copies_succeeded"),
        Some(1)
    );
}

#[test]
fn blocked_destination_does_not_stop_local_snapshots_or_retention() {
    let tmp = TempDir::new().unwrap();
    let db = open_file_db(tmp.path());
    let backups = tmp.path().join("backups");
    let (rt, registry) = offsite_runtime(tmp.path(), 1, 1);
    let (release, blocked) = std::sync::mpsc::channel();
    let (started, ready) = std::sync::mpsc::channel();
    rt.start_job(move |_| {
        started.send(()).unwrap();
        blocked.recv().unwrap();
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    std::fs::create_dir(&backups).unwrap();
    std::fs::write(backups.join("state.db.bak-20200101-000000"), b"old").unwrap();
    run_backup_with_offsite(&db, &backups, 1, Some(&rt));
    let first = std::fs::read_dir(&backups).unwrap().next().unwrap().unwrap().path();
    assert!(!backups.join("state.db.bak-20200101-000000").exists());
    // Snapshot names have second precision.
    std::thread::sleep(Duration::from_millis(1100));
    run_backup_with_offsite(&db, &backups, 1, Some(&rt));
    assert!(
        !first.exists(),
        "second snapshot prunes the first while copy is blocked"
    );
    assert_eq!(std::fs::read_dir(&backups).unwrap().count(), 1);
    assert_eq!(
        registry.counter_value("database_backup.offsite.copies_skipped"),
        Some(2)
    );
    release.send(()).unwrap();
    wait_for_worker(&rt);
}
