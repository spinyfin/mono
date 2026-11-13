use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use boss_protocol_spoke::{
    negotiate::feature,
    v1::{Command, CommandResult, ResultCode, Upgrade, command},
};
use boss_spoke_stream::{CommandOutcome, StreamManager, store::CommandAdmission};
use boss_work_db::{SpokeUpgradeRollout, WorkDb};

use crate::{
    CatalogSource, SpokePackageCatalog, decode_sha256, package_platform, release_has_published_packages,
    upgrade_command_id,
};

const COMMAND_DEADLINE: Duration = Duration::from_secs(15);
const NOT_AFTER_UNIX_MS: i64 = 4_102_444_800_000;

pub struct UpgradeController {
    db: WorkDb,
    streams: StreamManager,
    desired_release: String,
    catalog: Arc<dyn CatalogSource>,
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
        }
    }

    pub async fn tick(&self) -> Result<()> {
        let published = release_has_published_packages(&self.desired_release);
        let catalog = if published {
            self.catalog.catalog(&self.desired_release).await
        } else {
            None
        };
        let mut rollout = self.align_rollout(catalog.is_some())?;
        if catalog.is_none() {
            return Ok(());
        }
        let catalog = catalog.expect("checked");
        if rollout.halted {
            return Ok(());
        }
        if let Some(host_id) = rollout.in_flight_host_id.clone() {
            match self.in_flight_progress(&host_id, &rollout)? {
                InFlight::Wait => return Ok(()),
                InFlight::Healthy => {
                    rollout.in_flight_host_id = None;
                    rollout.in_flight_command_id = None;
                    self.persist(&rollout)?;
                }
                InFlight::Failed(reason) => {
                    self.halt(&mut rollout, &host_id, &reason)?;
                    return Ok(());
                }
            }
        }
        if rollout.in_flight_host_id.is_some() {
            return Ok(());
        }
        let Some(host_id) = self.next_behind_host().await? else {
            return Ok(());
        };
        self.offer(&mut rollout, &host_id, &catalog).await
    }

    fn align_rollout(&self, packages_published: bool) -> Result<SpokeUpgradeRollout> {
        let existing = self.db.spoke_upgrade_rollout()?;
        let mut rollout = existing.unwrap_or_else(|| SpokeUpgradeRollout {
            desired_release: self.desired_release.clone(),
            packages_published,
            halted: false,
            halt_host_id: None,
            halt_reason: None,
            in_flight_host_id: None,
            in_flight_command_id: None,
            updated_at: now_epoch_string(),
        });
        if rollout.desired_release != self.desired_release {
            rollout = SpokeUpgradeRollout {
                desired_release: self.desired_release.clone(),
                packages_published,
                halted: false,
                halt_host_id: None,
                halt_reason: None,
                in_flight_host_id: None,
                in_flight_command_id: None,
                updated_at: now_epoch_string(),
            };
        } else {
            rollout.packages_published = packages_published;
        }
        self.persist(&rollout)?;
        Ok(rollout)
    }

    fn in_flight_progress(&self, host_id: &str, rollout: &SpokeUpgradeRollout) -> Result<InFlight> {
        let Some(status) = self.db.host_upgrade_status(host_id)? else {
            return Ok(InFlight::Wait);
        };
        if helper_failed(&status.last_upgrade_result) {
            let reason = if status.last_upgrade_error.is_empty() {
                status.last_upgrade_result.clone()
            } else {
                format!("{}: {}", status.last_upgrade_result, status.last_upgrade_error)
            };
            return Ok(InFlight::Failed(reason));
        }
        if status.running_release == rollout.desired_release
            && (status.last_upgrade_result == "ok" || status.last_upgrade_result.is_empty())
        {
            return Ok(InFlight::Healthy);
        }
        Ok(InFlight::Wait)
    }

    async fn next_behind_host(&self) -> Result<Option<String>> {
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
            if package_platform(&status.os, &status.arch).is_none() {
                continue;
            }
            return Ok(Some(host_id));
        }
        Ok(None)
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
        self.persist(rollout)?;
        match self
            .streams
            .command(host_id, CommandAdmission::Recovery, command, COMMAND_DEADLINE)
            .await
        {
            Ok(result) => self.on_command_result(rollout, host_id, result),
            Err(CommandOutcome::Unavailable)
            | Err(CommandOutcome::Invalid)
            | Err(CommandOutcome::Busy)
            | Err(CommandOutcome::NotSubmitted) => {
                rollout.in_flight_host_id = None;
                rollout.in_flight_command_id = None;
                self.persist(rollout)
            }
            Err(CommandOutcome::OutcomeUncertain) | Err(CommandOutcome::ResetReleaseUnsupported) => Ok(()),
        }
    }

    fn on_command_result(&self, rollout: &mut SpokeUpgradeRollout, host_id: &str, result: CommandResult) -> Result<()> {
        if result.code == ResultCode::Ok as i32 {
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
        rollout.halted = true;
        rollout.halt_host_id = Some(host_id.to_owned());
        rollout.halt_reason = Some(reason.to_owned());
        tracing::error!(
            host_id,
            desired_release = %rollout.desired_release,
            reason,
            "spoke upgrade rollout halted"
        );
        self.persist(rollout)
    }

    fn persist(&self, rollout: &SpokeUpgradeRollout) -> Result<()> {
        let mut stored = rollout.clone();
        stored.updated_at = now_epoch_string();
        self.db.save_spoke_upgrade_rollout(&stored)
    }
}

enum InFlight {
    Wait,
    Healthy,
    Failed(String),
}

fn helper_failed(result: &str) -> bool {
    !result.is_empty() && result != "ok"
}

fn now_epoch_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}
