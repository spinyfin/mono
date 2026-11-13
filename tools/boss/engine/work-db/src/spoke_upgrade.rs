//! Durable spoke inventory and hub upgrade-rollout state.
//!
//! Inventory is what a connected spoke reports on Capacity. The rollout row is
//! the hub's decision for the release it itself is running: whether packages
//! exist, which host is in flight, and whether that release has halted.
use super::*;
use rusqlite::OptionalExtension;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const MIGRATION_ID: &str = "20261112_1200_spoke_upgrade_rollout";

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS host_upgrade_status (
            host_id TEXT PRIMARY KEY NOT NULL,
            running_release TEXT NOT NULL DEFAULT '',
            os TEXT NOT NULL DEFAULT '',
            arch TEXT NOT NULL DEFAULT '',
            last_upgrade_command_id TEXT NOT NULL DEFAULT '',
            last_upgrade_result TEXT NOT NULL DEFAULT '',
            last_upgrade_error TEXT NOT NULL DEFAULT '',
            last_upgrade_at TEXT,
            observed_at TEXT
         );
         CREATE TABLE IF NOT EXISTS spoke_upgrade_rollout (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            desired_release TEXT NOT NULL,
            packages_published INTEGER NOT NULL DEFAULT 0,
            halted INTEGER NOT NULL DEFAULT 0,
            halt_host_id TEXT,
            halt_reason TEXT,
            in_flight_host_id TEXT,
            in_flight_command_id TEXT,
            updated_at TEXT NOT NULL
         );",
    )?;
    Ok(())
}

fn now_epoch_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}

/// What a spoke last reported about its install and helper.
#[derive(bon::Builder, Debug, Clone, PartialEq, Eq)]
#[builder(on(String, into))]
pub struct HostUpgradeStatus {
    pub host_id: String,
    pub running_release: String,
    pub os: String,
    pub arch: String,
    pub last_upgrade_command_id: String,
    pub last_upgrade_result: String,
    pub last_upgrade_error: String,
    pub last_upgrade_at: Option<String>,
    pub observed_at: Option<String>,
}

/// The hub's current desired spoke release and sequential rollout cursor.
#[derive(bon::Builder, Debug, Clone, PartialEq, Eq)]
#[builder(on(String, into))]
pub struct SpokeUpgradeRollout {
    pub desired_release: String,
    pub packages_published: bool,
    pub halted: bool,
    pub halt_host_id: Option<String>,
    pub halt_reason: Option<String>,
    pub in_flight_host_id: Option<String>,
    pub in_flight_command_id: Option<String>,
    pub updated_at: String,
}

/// Operator-visible upgrade state for one host. Never silent for behind/failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostUpgradeState {
    Current,
    Behind,
    Upgrading,
    Failed,
    Unpublished,
}

impl HostUpgradeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Behind => "behind",
            Self::Upgrading => "upgrading",
            Self::Failed => "failed",
            Self::Unpublished => "unpublished",
        }
    }
}

/// Host listing overlay: running/desired release and last attempt.
#[derive(bon::Builder, Debug, Clone, PartialEq, Eq)]
#[builder(on(String, into))]
pub struct HostUpgradeView {
    pub running_release: Option<String>,
    pub desired_release: Option<String>,
    pub state: HostUpgradeState,
    pub last_upgrade_at: Option<String>,
    pub last_upgrade_result: Option<String>,
    pub last_upgrade_error: Option<String>,
}

fn nonempty(value: &str) -> Option<String> {
    if value.is_empty() { None } else { Some(value.to_owned()) }
}

fn helper_failed(result: &str) -> bool {
    !result.is_empty() && result != "ok"
}

/// Combine inventory and the current rollout into listing state.
pub fn host_upgrade_view(
    status: Option<&HostUpgradeStatus>,
    rollout: Option<&SpokeUpgradeRollout>,
) -> Option<HostUpgradeView> {
    let host_id = status.map(|row| row.host_id.as_str());
    let mentioned = rollout.is_some_and(|row| {
        host_id
            .is_some_and(|id| row.in_flight_host_id.as_deref() == Some(id) || row.halt_host_id.as_deref() == Some(id))
    });
    if status.is_none() && !mentioned {
        return None;
    }
    let running = status.map(|row| row.running_release.as_str()).unwrap_or("");
    let desired = rollout.map(|row| row.desired_release.as_str()).unwrap_or("");
    let in_flight = rollout.is_some_and(|row| host_id.is_some_and(|id| row.in_flight_host_id.as_deref() == Some(id)));
    let halted_here =
        rollout.is_some_and(|row| row.halted && host_id.is_some_and(|id| row.halt_host_id.as_deref() == Some(id)));
    let failed = status.is_some_and(|row| helper_failed(&row.last_upgrade_result)) || halted_here;
    let state = if rollout.is_none_or(|row| !row.packages_published) {
        HostUpgradeState::Unpublished
    } else if failed {
        HostUpgradeState::Failed
    } else if in_flight {
        HostUpgradeState::Upgrading
    } else if !desired.is_empty() && running == desired {
        HostUpgradeState::Current
    } else {
        HostUpgradeState::Behind
    };
    Some(HostUpgradeView {
        running_release: nonempty(running),
        desired_release: nonempty(desired),
        state,
        last_upgrade_at: status.and_then(|row| row.last_upgrade_at.clone()),
        last_upgrade_result: status.and_then(|row| nonempty(&row.last_upgrade_result)),
        last_upgrade_error: status.and_then(|row| nonempty(&row.last_upgrade_error)),
    })
}

fn map_status(row: &Row<'_>) -> rusqlite::Result<HostUpgradeStatus> {
    Ok(HostUpgradeStatus {
        host_id: row.get(0)?,
        running_release: row.get(1)?,
        os: row.get(2)?,
        arch: row.get(3)?,
        last_upgrade_command_id: row.get(4)?,
        last_upgrade_result: row.get(5)?,
        last_upgrade_error: row.get(6)?,
        last_upgrade_at: row.get(7)?,
        observed_at: row.get(8)?,
    })
}

fn map_rollout(row: &Row<'_>) -> rusqlite::Result<SpokeUpgradeRollout> {
    Ok(SpokeUpgradeRollout {
        desired_release: row.get(0)?,
        packages_published: row.get::<_, i64>(1)? != 0,
        halted: row.get::<_, i64>(2)? != 0,
        halt_host_id: row.get(3)?,
        halt_reason: row.get(4)?,
        in_flight_host_id: row.get(5)?,
        in_flight_command_id: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Upsert the inventory a spoke reported on Capacity.
    pub fn record_host_inventory(&self, host_id: &str, inventory: &boss_protocol_spoke::v1::Inventory) -> Result<()> {
        let conn = self.connect()?;
        let observed_at = now_epoch_string();
        let previous: Option<String> = conn
            .query_row(
                "SELECT last_upgrade_command_id FROM host_upgrade_status WHERE host_id = ?1",
                [host_id],
                |row| row.get(0),
            )
            .optional()?;
        let command_changed = previous.as_deref() != Some(inventory.last_upgrade_command_id.as_str())
            && !inventory.last_upgrade_command_id.is_empty();
        let last_upgrade_at = if command_changed {
            Some(observed_at.clone())
        } else {
            None
        };
        conn.execute(
            "INSERT INTO host_upgrade_status (
                host_id, running_release, os, arch,
                last_upgrade_command_id, last_upgrade_result, last_upgrade_error,
                last_upgrade_at, observed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(host_id) DO UPDATE SET
                running_release = excluded.running_release,
                os = excluded.os,
                arch = excluded.arch,
                last_upgrade_command_id = excluded.last_upgrade_command_id,
                last_upgrade_result = excluded.last_upgrade_result,
                last_upgrade_error = excluded.last_upgrade_error,
                last_upgrade_at = COALESCE(excluded.last_upgrade_at, host_upgrade_status.last_upgrade_at),
                observed_at = excluded.observed_at",
            params![
                host_id,
                inventory.release_id,
                inventory.os,
                inventory.arch,
                inventory.last_upgrade_command_id,
                inventory.last_upgrade_result,
                inventory.last_upgrade_error,
                last_upgrade_at,
                observed_at,
            ],
        )?;
        Ok(())
    }

    pub fn host_upgrade_status(&self, host_id: &str) -> Result<Option<HostUpgradeStatus>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT host_id, running_release, os, arch,
                    last_upgrade_command_id, last_upgrade_result, last_upgrade_error,
                    last_upgrade_at, observed_at
               FROM host_upgrade_status WHERE host_id = ?1",
            [host_id],
            map_status,
        )
        .optional()
        .context("host_upgrade_status")
    }

    pub fn spoke_upgrade_rollout(&self) -> Result<Option<SpokeUpgradeRollout>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT desired_release, packages_published, halted, halt_host_id, halt_reason,
                    in_flight_host_id, in_flight_command_id, updated_at
               FROM spoke_upgrade_rollout WHERE id = 1",
            [],
            map_rollout,
        )
        .optional()
        .context("spoke_upgrade_rollout")
    }

    pub fn save_spoke_upgrade_rollout(&self, rollout: &SpokeUpgradeRollout) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO spoke_upgrade_rollout (
                id, desired_release, packages_published, halted, halt_host_id, halt_reason,
                in_flight_host_id, in_flight_command_id, updated_at
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                desired_release = excluded.desired_release,
                packages_published = excluded.packages_published,
                halted = excluded.halted,
                halt_host_id = excluded.halt_host_id,
                halt_reason = excluded.halt_reason,
                in_flight_host_id = excluded.in_flight_host_id,
                in_flight_command_id = excluded.in_flight_command_id,
                updated_at = excluded.updated_at",
            params![
                rollout.desired_release,
                rollout.packages_published as i64,
                rollout.halted as i64,
                rollout.halt_host_id,
                rollout.halt_reason,
                rollout.in_flight_host_id,
                rollout.in_flight_command_id,
                rollout.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Listing overlay for one host. `None` when the host has never spoken
    /// inventory and is not the in-flight or halted host of the current rollout.
    pub fn host_upgrade_view(&self, host_id: &str) -> Result<Option<HostUpgradeView>> {
        let status = self.host_upgrade_status(host_id)?;
        let rollout = self.spoke_upgrade_rollout()?;
        Ok(host_upgrade_view(status.as_ref(), rollout.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> WorkDb {
        WorkDb::open_in_memory().expect("memory db")
    }

    fn inventory(
        release_id: &str,
        os: &str,
        arch: &str,
        command_id: &str,
        result: &str,
        error: &str,
    ) -> boss_protocol_spoke::v1::Inventory {
        boss_protocol_spoke::v1::Inventory {
            release_id: release_id.into(),
            os: os.into(),
            arch: arch.into(),
            last_upgrade_command_id: command_id.into(),
            last_upgrade_result: result.into(),
            last_upgrade_error: error.into(),
            ..Default::default()
        }
    }

    #[test]
    fn inventory_round_trips_and_stamps_attempt_time_when_command_changes() {
        let db = db();
        db.record_host_inventory("edge", &inventory("1.0.1", "linux", "x86_64", "up-1", "ok", ""))
            .unwrap();
        let first = db.host_upgrade_status("edge").unwrap().unwrap();
        assert_eq!(first.running_release, "1.0.1");
        assert_eq!(first.os, "linux");
        assert_eq!(first.arch, "x86_64");
        assert_eq!(first.last_upgrade_command_id, "up-1");
        assert_eq!(first.last_upgrade_result, "ok");
        assert!(first.last_upgrade_at.is_some());
        let stamped = first.last_upgrade_at.clone();
        db.record_host_inventory("edge", &inventory("1.0.1", "linux", "x86_64", "up-1", "ok", ""))
            .unwrap();
        let again = db.host_upgrade_status("edge").unwrap().unwrap();
        assert_eq!(again.last_upgrade_at, stamped);
        db.record_host_inventory(
            "edge",
            &inventory("1.0.2", "linux", "x86_64", "up-2", "manifest_sig", "bad"),
        )
        .unwrap();
        let next = db.host_upgrade_status("edge").unwrap().unwrap();
        assert_eq!(next.running_release, "1.0.2");
        assert_eq!(next.last_upgrade_command_id, "up-2");
        assert_eq!(next.last_upgrade_result, "manifest_sig");
        assert_eq!(next.last_upgrade_error, "bad");
        // Stamped again (unix-second clock may equal the previous stamp).
        assert!(next.last_upgrade_at.is_some());
    }

    #[test]
    fn view_is_behind_until_running_matches_published_desired() {
        let status = HostUpgradeStatus {
            host_id: "edge".into(),
            running_release: "1.0.1".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            last_upgrade_command_id: String::new(),
            last_upgrade_result: String::new(),
            last_upgrade_error: String::new(),
            last_upgrade_at: None,
            observed_at: None,
        };
        let rollout = SpokeUpgradeRollout {
            desired_release: "1.0.2".into(),
            packages_published: true,
            halted: false,
            halt_host_id: None,
            halt_reason: None,
            in_flight_host_id: None,
            in_flight_command_id: None,
            updated_at: "1".into(),
        };
        let view = host_upgrade_view(Some(&status), Some(&rollout)).unwrap();
        assert_eq!(view.state, HostUpgradeState::Behind);
        assert_eq!(view.running_release.as_deref(), Some("1.0.1"));
        assert_eq!(view.desired_release.as_deref(), Some("1.0.2"));
    }

    #[test]
    fn unpublished_packages_and_failure_are_visible() {
        let status = HostUpgradeStatus {
            host_id: "edge".into(),
            running_release: "1.0.1".into(),
            os: String::new(),
            arch: String::new(),
            last_upgrade_command_id: "up-1".into(),
            last_upgrade_result: "manifest_sig".into(),
            last_upgrade_error: "bad sig".into(),
            last_upgrade_at: Some("9".into()),
            observed_at: None,
        };
        let unpublished = SpokeUpgradeRollout {
            desired_release: "1.0.2".into(),
            packages_published: false,
            halted: false,
            halt_host_id: None,
            halt_reason: None,
            in_flight_host_id: None,
            in_flight_command_id: None,
            updated_at: "1".into(),
        };
        assert_eq!(
            host_upgrade_view(Some(&status), Some(&unpublished)).unwrap().state,
            HostUpgradeState::Unpublished
        );
        let halted = SpokeUpgradeRollout {
            packages_published: true,
            halted: true,
            halt_host_id: Some("edge".into()),
            halt_reason: Some("manifest_sig".into()),
            ..unpublished.clone()
        };
        let failed = host_upgrade_view(Some(&status), Some(&halted)).unwrap();
        assert_eq!(failed.state, HostUpgradeState::Failed);
        assert_eq!(failed.last_upgrade_result.as_deref(), Some("manifest_sig"));
        assert_eq!(failed.last_upgrade_error.as_deref(), Some("bad sig"));
    }

    #[test]
    fn hosts_without_inventory_stay_blank() {
        assert!(host_upgrade_view(None, None).is_none());
        let rollout = SpokeUpgradeRollout {
            desired_release: "1.0.2".into(),
            packages_published: true,
            halted: false,
            halt_host_id: None,
            halt_reason: None,
            in_flight_host_id: Some("other".into()),
            in_flight_command_id: Some("up".into()),
            updated_at: "1".into(),
        };
        assert!(host_upgrade_view(None, Some(&rollout)).is_none());
    }
}
