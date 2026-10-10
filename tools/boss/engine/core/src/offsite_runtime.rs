//! Off-machine dispatch and metrics.
use boss_engine_offsite_backup::{CONFIG_SECTION, OffsiteConfig};
use boss_metrics::Registry;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
// ── Off-machine copy metrics ──────────────────────────────────────────────

crate::register_counter!(
    OFFSITE_COPIES_SUCCEEDED,
    "database_backup.offsite.copies_succeeded",
    "Backups copied to the off-machine destination."
);
crate::register_counter!(
    OFFSITE_COPIES_FAILED,
    "database_backup.offsite.copies_failed",
    "Off-machine copies that failed (destination missing, not writable, disk full, privacy denial, ...)."
);
crate::register_counter!(
    OFFSITE_CONFIG_INVALID,
    "database_backup.offsite.config_invalid",
    "Times [backup.offsite] was enabled but unusable (unset/missing/unwritable destination, parse error)."
);
crate::register_gauge!(
    OFFSITE_LAST_SUCCESS_AGE_SECS,
    "database_backup.offsite.last_success_age_secs",
    "Seconds since the last successful off-machine copy (-1 if no durable success is known). Only updated while off-machine backups are enabled."
);
crate::register_counter!(
    OFFSITE_COPIES_SKIPPED,
    "database_backup.offsite.copies_skipped",
    "Copies skipped while the previous destination operation is running."
);
crate::register_counter!(
    OFFSITE_RETENTION_FAILED,
    "database_backup.offsite.retention_failed",
    "Off-machine retention enumeration or deletion failures."
);

/// Register the off-machine backup metrics (called from `metrics_init`).
pub fn register_metrics(registry: &Registry) {
    registry.register_counter(&OFFSITE_COPIES_SUCCEEDED);
    registry.register_counter(&OFFSITE_COPIES_FAILED);
    registry.register_counter(&OFFSITE_CONFIG_INVALID);
    registry.register_counter(&OFFSITE_COPIES_SKIPPED);
    registry.register_counter(&OFFSITE_RETENTION_FAILED);
    registry.register_gauge(&OFFSITE_LAST_SUCCESS_AGE_SECS);
}

/// A finished local backup held open so local retention cannot invalidate it.
pub struct Snapshot {
    file: File,
    name: String,
}

/// Runtime state for off-machine copies. Never fails the caller: every
/// problem is logged at ERROR/WARN naming the setting and counted.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct OffsiteRuntime {
    config: OffsiteConfig,
    host: String,
    registry: Arc<Registry>,
    success_path: PathBuf,
    in_flight: AtomicBool,
    /// Epoch seconds of the last successful copy; 0 = no durable success.
    last_success: AtomicI64,
}

impl OffsiteRuntime {
    /// Build from `<state_root>/settings.toml`. `None` when the feature is
    /// disabled (the default) or the section cannot be parsed. An enabled but
    /// unusable destination still yields a runtime. The destination worker
    /// validates it every cycle, since a sync folder may mount late.
    pub fn from_settings(settings_path: &Path, registry: Arc<Registry>) -> Option<Arc<Self>> {
        let config = match OffsiteConfig::load(settings_path) {
            Ok(config) => config,
            Err(err) => {
                tracing::error!(
                    error = %format!("{err:#}"),
                    settings = %settings_path.display(),
                    "database-backup: cannot read {CONFIG_SECTION} settings; off-machine backups are NOT running",
                );
                OFFSITE_CONFIG_INVALID.inc(&registry);
                // No runtime means no refresher; a default 0 would read as fresh.
                OFFSITE_LAST_SUCCESS_AGE_SECS.set(&registry, -1);
                return None;
            }
        };
        Self::from_config(config, registry, settings_path.with_file_name("offsite-last-success"))
    }

    fn from_config(config: OffsiteConfig, registry: Arc<Registry>, success_path: PathBuf) -> Option<Arc<Self>> {
        if !config.enabled {
            return None;
        }
        let host = boss_engine_offsite_backup::host_name();
        // Read local durable evidence only: startup must never touch the mount.
        let last_success = std::fs::read_to_string(&success_path)
            .ok()
            .and_then(|record| {
                let (timestamp, destination) = record.split_once('\n')?;
                if destination != format!("{:?}", config.destination) {
                    return None;
                }
                timestamp.parse::<i64>().ok()
            })
            .unwrap_or(0);
        Some(Arc::new(
            Self::builder()
                .config(config)
                .host(host)
                .registry(registry)
                .success_path(success_path)
                .in_flight(AtomicBool::new(false))
                .last_success(AtomicI64::new(last_success))
                .build(),
        ))
    }

    /// Fail loudly at startup, independent of local snapshot success. The
    /// settings checks need no I/O and run inline; filesystem validation runs
    /// on the single-flight worker so startup never waits on the destination.
    pub fn validate_at_startup(self: &Arc<Self>) {
        if let Err(err) = self.config.destination_setting() {
            self.report_config_invalid(&err);
            return;
        }
        self.start_job(|runtime| {
            if let Err(err) = runtime.config.validate(&runtime.host) {
                runtime.report_config_invalid(&err);
            }
            runtime.refresh_age_gauge();
        });
    }

    fn report_config_invalid(&self, err: &anyhow::Error) {
        OFFSITE_CONFIG_INVALID.inc(&self.registry);
        tracing::error!(
            error = %format!("{err:#}"),
            "database-backup: off-machine backups are enabled but the {CONFIG_SECTION} destination is unusable",
        );
    }

    /// Open the finished local `snapshot` so it can still be copied after
    /// local retention unlinks its path. Failures are logged and counted.
    pub fn open_snapshot(&self, snapshot: &Path) -> Option<Snapshot> {
        let opened = boss_engine_offsite_backup::backup_file_name(snapshot)
            .map(str::to_owned)
            .and_then(|name| {
                let file = File::open(snapshot).map_err(anyhow::Error::from)?;
                Ok(Snapshot { file, name })
            });
        match opened {
            Ok(snapshot) => Some(snapshot),
            Err(err) => {
                OFFSITE_COPIES_FAILED.inc(&self.registry);
                tracing::error!(
                    error = %format!("{err:#}"),
                    snapshot = %snapshot.display(),
                    "database-backup: cannot open snapshot for off-machine copy",
                );
                None
            }
        }
    }

    /// Dispatch without waiting for destination validation, copying, or pruning.
    pub fn submit(self: &Arc<Self>, snapshot: Snapshot) {
        self.start_job(move |runtime| runtime.copy_and_prune(snapshot));
    }

    fn start_job(self: &Arc<Self>, job: impl FnOnce(&Self) + Send + 'static) {
        if self.in_flight.swap(true, Ordering::AcqRel) {
            OFFSITE_COPIES_SKIPPED.inc(&self.registry);
            tracing::warn!("database-backup: previous off-machine copy still running; skipping copy");
            return;
        }
        let runtime = self.clone();
        // An OS thread also avoids holding Tokio shutdown open on stuck I/O.
        if let Err(error) = std::thread::Builder::new()
            .name("offsite-backup".into())
            .spawn(move || {
                struct Reset<'a>(&'a AtomicBool);
                impl Drop for Reset<'_> {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                let _reset = Reset(&runtime.in_flight);
                job(&runtime);
            })
        {
            self.in_flight.store(false, Ordering::Release);
            OFFSITE_COPIES_FAILED.inc(&self.registry);
            tracing::error!(%error, "database-backup: cannot start off-machine worker");
        }
    }

    /// Copy the finished local backup `snapshot` off-machine, then prune.
    pub fn copy_and_prune(&self, snapshot: Snapshot) {
        let Snapshot { file, name } = snapshot;
        let result = self
            .config
            .validate(&self.host)
            .inspect_err(|_| {
                OFFSITE_CONFIG_INVALID.inc(&self.registry);
            })
            .and_then(|dest| {
                let outcome = boss_engine_offsite_backup::copy_open_to_offsite(file, &name, &dest.host_dir)?;
                Ok((dest, outcome))
            });
        match result {
            Ok((dest, outcome)) => {
                let now = boss_engine_utils::epoch_time::now_epoch_secs();
                let record = format!("{now}\n{:?}", self.config.destination);
                if let Err(error) =
                    boss_engine_utils::atomic_blob::write_blob_atomic(&self.success_path, record.as_bytes())
                {
                    // The copy itself succeeded, so it is not counted as failed.
                    tracing::error!(%error, "database-backup: cannot persist off-machine success timestamp");
                }
                self.last_success.store(now, Ordering::Relaxed);
                OFFSITE_COPIES_SUCCEEDED.inc(&self.registry);
                tracing::info!(
                    path = %outcome.copied_path.display(),
                    bytes = outcome.bytes,
                    "database-backup: off-machine copy complete",
                );
                match boss_engine_offsite_backup::prune(&dest.host_dir, self.config.keep_hourly, self.config.keep_daily)
                {
                    Ok(outcome) => {
                        for (path, error) in outcome.failures {
                            OFFSITE_RETENTION_FAILED.inc(&self.registry);
                            tracing::error!(path = %path.display(), %error, "database-backup: off-machine retention failed");
                        }
                        tracing::info!(
                            removed = outcome.removed.len(),
                            "database-backup: pruned old off-machine copies"
                        );
                    }
                    Err(error) => {
                        OFFSITE_RETENTION_FAILED.inc(&self.registry);
                        tracing::error!(error = %format!("{error:#}"), "database-backup: off-machine retention failed");
                    }
                }
            }
            Err(err) => {
                OFFSITE_COPIES_FAILED.inc(&self.registry);
                tracing::error!(
                    error = %format!("{err:#}"),
                    snapshot = %name,
                    "database-backup: off-machine copy FAILED (local backup is unaffected); \
                     check the {CONFIG_SECTION} destination setting",
                );
            }
        }
        self.refresh_age_gauge();
    }

    /// Seconds since durable success, or -1 if none is known.
    fn age_secs_at(&self, now: i64) -> i64 {
        let last = self.last_success.load(Ordering::Relaxed);
        if last <= 0 {
            return -1;
        }
        (now - last).max(0)
    }

    pub fn refresh_age_gauge(&self) {
        self.refresh_age_gauge_at(boss_engine_utils::epoch_time::now_epoch_secs());
    }

    fn refresh_age_gauge_at(&self, now: i64) {
        OFFSITE_LAST_SUCCESS_AGE_SECS.set(&self.registry, self.age_secs_at(now));
    }
}

#[cfg(test)]
#[path = "offsite_runtime_tests.rs"]
mod tests;
