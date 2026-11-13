use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use boss_protocol_spoke::{
    negotiate::feature,
    v1::{Command, CommandResult, ResultCode, Upgrade, command},
};
use boss_spoke_stream::{CommandOutcome, StreamManager, store::CommandAdmission};
use boss_work_db::{SpokeUpgradeRollout, WorkDb, helper_failed, unix_epoch_secs_string};
use prost::Message;

use crate::{
    CatalogLookup, CatalogSource, SpokePackageCatalog, decode_sha256, package_platform, release_has_published_packages,
    upgrade_command_id,
};

const COMMAND_DEADLINE: Duration = Duration::from_secs(15);
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(600);
const NOT_AFTER_UNIX_MS: i64 = 4_102_444_800_000;
/// Tick-to-tick gaps larger than this are hub sleep or a missed interval, not
/// connected upgrade time.
const MAX_CONNECTED_SAMPLE_GAP_SECS: i64 = 15;

struct UpgradeBounds {
    command_deadline: Duration,
    upgrade_timeout: Duration,
    /// Last connected sample in this controller session. Not persisted: a
    /// restart or reconnect starts a new baseline so hub downtime is not
    /// charged as connected upgrade time.
    progress_sample: Mutex<Option<(String, i64)>>,
}

pub struct UpgradeController {
    db: WorkDb,
    streams: StreamManager,
    desired_release: String,
    catalog: Arc<dyn CatalogSource>,
    bounds: UpgradeBounds,
}

impl UpgradeController {
    pub fn new(
        db: WorkDb,
        streams: StreamManager,
        desired_release: impl Into<String>,
        catalog: Arc<dyn CatalogSource>,
    ) -> Self {
        Self {
            db,
            streams,
            desired_release: desired_release.into(),
            catalog,
            bounds: UpgradeBounds {
                command_deadline: COMMAND_DEADLINE,
                upgrade_timeout: UPGRADE_TIMEOUT,
                progress_sample: Mutex::new(None),
            },
        }
    }

    pub fn with_command_deadline(mut self, deadline: Duration) -> Self {
        self.bounds.command_deadline = deadline;
        self
    }

    pub fn with_upgrade_timeout(mut self, timeout: Duration) -> Self {
        self.bounds.upgrade_timeout = timeout;
        self
    }

    pub async fn tick(&self) -> Result<()> {
        let published = release_has_published_packages(&self.desired_release);
        let lookup = if published {
            self.catalog.lookup(&self.desired_release).await
        } else {
            CatalogLookup::Unpublished
        };
        let mut rollout = match &lookup {
            CatalogLookup::Found(catalog) => {
                self.align_rollout(PackagesPresence::Present(catalog.packages.keys().cloned().collect()))?
            }
            CatalogLookup::Unpublished => self.align_rollout(PackagesPresence::Absent)?,
            CatalogLookup::Unavailable => self.align_rollout(PackagesPresence::Unknown)?,
        };
        let catalog = match lookup {
            CatalogLookup::Found(catalog) => catalog,
            CatalogLookup::Unpublished => return Ok(()),
            CatalogLookup::Unavailable => {
                if !rollout.packages_published {
                    return Ok(());
                }
                SpokePackageCatalog {
                    release_id: self.desired_release.clone(),
                    packages: Default::default(),
                }
            }
        };
        if rollout.halted && rollout.desired_release == self.desired_release {
            return Ok(());
        }
        if let Some(host_id) = rollout.in_flight_host_id.clone() {
            match self.reconcile_in_flight(&mut rollout, &host_id, &catalog).await? {
                InFlight::Wait | InFlight::Unresolved => return Ok(()),
                InFlight::Healthy => {
                    self.clear_in_flight(&mut rollout)?;
                }
                InFlight::Failed(reason) => {
                    let in_flight_release = rollout.in_flight_release.clone();
                    if in_flight_release.as_deref() == Some(self.desired_release.as_str())
                        || in_flight_release.is_none()
                    {
                        self.halt(&mut rollout, &host_id, &reason)?;
                        return Ok(());
                    }
                    // Terminal evidence that the previous command cannot still
                    // run: the fleet gate may move to the new desired release.
                    self.clear_in_flight(&mut rollout)?;
                }
            }
        }
        if rollout.halted {
            return Ok(());
        }
        if rollout.in_flight_host_id.is_some() {
            return Ok(());
        }
        let Some(host_id) = self.next_behind_host(&catalog).await? else {
            return Ok(());
        };
        self.offer(&mut rollout, &host_id, &catalog).await
    }

    fn align_rollout(&self, presence: PackagesPresence) -> Result<SpokeUpgradeRollout> {
        let existing = self.db.spoke_upgrade_rollout()?;
        let mut rollout = existing.unwrap_or_else(|| SpokeUpgradeRollout::for_release(&self.desired_release, false));
        if rollout.desired_release != self.desired_release {
            if rollout.in_flight_host_id.is_some() {
                rollout.desired_release = self.desired_release.clone();
                rollout.halted = false;
                rollout.halt_host_id = None;
                rollout.halt_reason = None;
                rollout.halt_at = None;
                rollout.halt_command_id = None;
            } else {
                let published_platforms = rollout.published_platforms.clone();
                rollout = SpokeUpgradeRollout::for_release(&self.desired_release, rollout.packages_published);
                rollout.published_platforms = published_platforms;
            }
        }
        match presence {
            PackagesPresence::Present(platforms) => {
                rollout.packages_published = true;
                rollout.published_platforms = platforms.join(",");
            }
            PackagesPresence::Absent => {
                rollout.packages_published = false;
                rollout.published_platforms.clear();
            }
            PackagesPresence::Unknown => {}
        }
        self.persist(&rollout)?;
        Ok(rollout)
    }

    async fn reconcile_in_flight(
        &self,
        rollout: &mut SpokeUpgradeRollout,
        host_id: &str,
        catalog: &SpokePackageCatalog,
    ) -> Result<InFlight> {
        let durable_result = match rollout.in_flight_command_id.as_deref() {
            Some(command_id) => match self.streams.command_result(host_id, command_id).await {
                Ok(result) => result,
                Err(_) => return Ok(InFlight::Wait),
            },
            None => None,
        };
        if let Some(result) = durable_result.as_ref() {
            match result.code {
                code if code == ResultCode::Ok as i32 => {}
                code if code == ResultCode::Rejected as i32 => {
                    let reason = if result.reason.is_empty() {
                        format!("upgrade refused ({code})")
                    } else {
                        result.reason.clone()
                    };
                    return Ok(InFlight::Failed(reason));
                }
                _ => return Ok(InFlight::Unresolved),
            }
        }
        let command_id = rollout.in_flight_command_id.clone();
        if durable_result.is_none()
            && let Some(command_id) = command_id
        {
            let stored = match self.streams.stored_command(host_id, &command_id).await {
                Ok(stored) => stored,
                Err(_) => return Ok(InFlight::Wait),
            };
            if stored.is_none() {
                if let Some(command) = rollout
                    .in_flight_command
                    .as_deref()
                    .and_then(|bytes| Command::decode(bytes).ok())
                {
                    match self
                        .streams
                        .command(
                            host_id,
                            CommandAdmission::Recovery,
                            command,
                            self.bounds.command_deadline,
                        )
                        .await
                    {
                        Ok(result) => {
                            if result.code == ResultCode::Rejected as i32 {
                                return Ok(InFlight::Failed(if result.reason.is_empty() {
                                    format!("upgrade refused ({})", result.code)
                                } else {
                                    result.reason
                                }));
                            }
                            if result.code != ResultCode::Ok as i32 {
                                return Ok(InFlight::Unresolved);
                            }
                        }
                        Err(CommandOutcome::Unavailable)
                        | Err(CommandOutcome::Invalid)
                        | Err(CommandOutcome::Busy)
                        | Err(CommandOutcome::NotSubmitted) => {
                            self.clear_in_flight(rollout)?;
                            return Ok(InFlight::Wait);
                        }
                        Err(CommandOutcome::OutcomeUncertain) | Err(CommandOutcome::ResetReleaseUnsupported) => {
                            return Ok(InFlight::Unresolved);
                        }
                    }
                } else if catalog.packages.is_empty() {
                    return Ok(InFlight::Wait);
                }
            }
        }
        let progress = self.in_flight_progress(host_id, rollout)?;
        if !matches!(progress, InFlight::Wait) {
            return Ok(progress);
        }
        self.note_connected_progress(rollout, host_id).await?;
        if self.timed_out(rollout) {
            let same_desired = rollout.in_flight_release.as_deref() == Some(self.desired_release.as_str())
                || rollout.in_flight_release.is_none();
            if same_desired {
                self.halt(rollout, host_id, "upgrade_timeout")?;
            }
            return Ok(InFlight::Unresolved);
        }
        Ok(InFlight::Wait)
    }

    async fn note_connected_progress(&self, rollout: &mut SpokeUpgradeRollout, host_id: &str) -> Result<()> {
        let connected = self.streams.connected_hosts().await.iter().any(|id| id == host_id);
        let now = unix_epoch_secs_string();
        let now_secs: i64 = now.parse().unwrap_or(0);
        let mut sample = self
            .bounds
            .progress_sample
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if connected {
            if let Some((sampled_host, last_secs)) = sample.as_ref()
                && sampled_host == host_id
            {
                let gap = (now_secs - last_secs).max(0);
                if gap > 0 && gap <= MAX_CONNECTED_SAMPLE_GAP_SECS {
                    rollout.in_flight_connected_secs = rollout.in_flight_connected_secs.saturating_add(gap);
                }
            }
            *sample = Some((host_id.to_owned(), now_secs));
            rollout.in_flight_last_progress_at = Some(now);
            drop(sample);
            self.persist(rollout)?;
        } else {
            *sample = None;
            rollout.in_flight_last_progress_at = None;
            drop(sample);
            self.persist(rollout)?;
        }
        Ok(())
    }

    fn timed_out(&self, rollout: &SpokeUpgradeRollout) -> bool {
        let limit = self.bounds.upgrade_timeout.as_secs() as i64;
        rollout.in_flight_connected_secs >= limit
    }

    fn in_flight_progress(&self, host_id: &str, rollout: &SpokeUpgradeRollout) -> Result<InFlight> {
        let Some(status) = self.db.host_upgrade_status(host_id)? else {
            return Ok(InFlight::Wait);
        };
        let Some(command_id) = rollout.in_flight_command_id.as_deref() else {
            return Ok(InFlight::Wait);
        };
        if status.last_upgrade_command_id != command_id {
            return Ok(InFlight::Wait);
        }
        if helper_failed(&status.last_upgrade_result) {
            let reason = if status.last_upgrade_error.is_empty() {
                status.last_upgrade_result.clone()
            } else {
                format!("{}: {}", status.last_upgrade_result, status.last_upgrade_error)
            };
            return Ok(InFlight::Failed(reason));
        }
        let expected_release = rollout
            .in_flight_release
            .as_deref()
            .unwrap_or(rollout.desired_release.as_str());
        if status.running_release == expected_release && status.last_upgrade_result == "ok" {
            return Ok(InFlight::Healthy);
        }
        Ok(InFlight::Wait)
    }

    async fn next_behind_host(&self, catalog: &SpokePackageCatalog) -> Result<Option<String>> {
        let mut connected = self.streams.connected_hosts().await;
        connected.sort();
        for host_id in connected {
            if self.streams.host_supports(&host_id, feature::UPGRADE).await != Some(true) {
                continue;
            }
            let Some(status) = self.db.host_upgrade_status(&host_id)? else {
                continue;
            };
            if status.running_release == self.desired_release {
                continue;
            }
            if !self.usable_package(catalog, &status.os, &status.arch) {
                continue;
            }
            return Ok(Some(host_id));
        }
        Ok(None)
    }

    fn usable_package(&self, catalog: &SpokePackageCatalog, os: &str, arch: &str) -> bool {
        let Some(platform) = package_platform(os, arch) else {
            return false;
        };
        catalog
            .packages
            .get(platform)
            .is_some_and(|package| decode_sha256(&package.manifest_sha256).is_some())
    }

    async fn offer(
        &self,
        rollout: &mut SpokeUpgradeRollout,
        host_id: &str,
        catalog: &SpokePackageCatalog,
    ) -> Result<()> {
        let Some(status) = self.db.host_upgrade_status(host_id)? else {
            return Ok(());
        };
        let Some(platform) = package_platform(&status.os, &status.arch) else {
            return Ok(());
        };
        let Some(package) = catalog.packages.get(platform) else {
            return Ok(());
        };
        let Some(digest) = decode_sha256(&package.manifest_sha256) else {
            return Ok(());
        };
        let command_id = upgrade_command_id(host_id, &self.desired_release);
        let command = Command {
            command_id: command_id.clone(),
            not_after_unix_ms: NOT_AFTER_UNIX_MS,
            body: Some(command::Body::Upgrade(Upgrade {
                release_id: self.desired_release.clone(),
                manifest_url: package.manifest_url.clone(),
                manifest_sha256: digest,
            })),
            ..Default::default()
        };
        rollout.in_flight_host_id = Some(host_id.to_owned());
        rollout.in_flight_command_id = Some(command_id);
        rollout.in_flight_release = Some(self.desired_release.clone());
        rollout.in_flight_offered_at = Some(unix_epoch_secs_string());
        rollout.in_flight_connected_secs = 0;
        rollout.in_flight_last_progress_at = Some(unix_epoch_secs_string());
        rollout.in_flight_command = Some(command.encode_to_vec());
        self.persist(rollout)?;
        match self
            .streams
            .command(
                host_id,
                CommandAdmission::Recovery,
                command,
                self.bounds.command_deadline,
            )
            .await
        {
            Ok(result) => self.on_command_result(rollout, host_id, result),
            Err(CommandOutcome::Unavailable)
            | Err(CommandOutcome::Invalid)
            | Err(CommandOutcome::Busy)
            | Err(CommandOutcome::NotSubmitted) => {
                self.clear_in_flight(rollout)?;
                Ok(())
            }
            Err(CommandOutcome::OutcomeUncertain) | Err(CommandOutcome::ResetReleaseUnsupported) => Ok(()),
        }
    }

    fn on_command_result(&self, rollout: &mut SpokeUpgradeRollout, host_id: &str, result: CommandResult) -> Result<()> {
        if result.code == ResultCode::Ok as i32 {
            return Ok(());
        }
        if result.code != ResultCode::Rejected as i32 {
            return Ok(());
        }
        let reason = if result.reason.is_empty() {
            format!("upgrade refused ({})", result.code)
        } else {
            result.reason
        };
        self.halt(rollout, host_id, &reason)
    }

    fn halt(&self, rollout: &mut SpokeUpgradeRollout, host_id: &str, reason: &str) -> Result<()> {
        if rollout.halted && rollout.desired_release == self.desired_release {
            return Ok(());
        }
        if let Some(command_id) = rollout.in_flight_command_id.as_deref() {
            let helper_already_reported = self
                .db
                .host_upgrade_status(host_id)
                .ok()
                .flatten()
                .is_some_and(|status| status.last_upgrade_command_id == command_id);
            if !helper_already_reported {
                let result = if reason == "upgrade_timeout" {
                    "timeout"
                } else {
                    "refused"
                };
                let _ = self
                    .db
                    .record_upgrade_command_outcome(host_id, command_id, result, reason);
            }
        }
        rollout.halted = true;
        rollout.halt_host_id = Some(host_id.to_owned());
        rollout.halt_reason = Some(reason.to_owned());
        rollout.halt_at = Some(unix_epoch_secs_string());
        rollout.halt_command_id = rollout.in_flight_command_id.clone();
        tracing::error!(
            host_id,
            desired_release = %rollout.desired_release,
            reason,
            "spoke upgrade rollout halted"
        );
        self.persist(rollout)
    }

    fn clear_in_flight(&self, rollout: &mut SpokeUpgradeRollout) -> Result<()> {
        rollout.in_flight_host_id = None;
        rollout.in_flight_command_id = None;
        rollout.in_flight_release = None;
        rollout.in_flight_offered_at = None;
        rollout.in_flight_connected_secs = 0;
        rollout.in_flight_last_progress_at = None;
        rollout.in_flight_command = None;
        self.persist(rollout)
    }

    fn persist(&self, rollout: &SpokeUpgradeRollout) -> Result<()> {
        let mut stored = rollout.clone();
        stored.updated_at = unix_epoch_secs_string();
        self.db.save_spoke_upgrade_rollout(&stored)
    }
}

enum InFlight {
    Wait,
    Unresolved,
    Healthy,
    Failed(String),
}

enum PackagesPresence {
    Present(Vec<String>),
    Absent,
    Unknown,
}
