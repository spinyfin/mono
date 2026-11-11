//! Daily, durable calibration. Readers consume the published values rather
//! than scanning the ledger on each sweep. Overrides survive recalibration.
use super::*;
use boss_engine_storage_values::progression::{CALIBRATED_KEYS, ProgressionSettings};
use boss_protocol::{ProgressionCalibration, ProgressionLimit};
use std::collections::BTreeMap;

pub(crate) const REPORT_KEY: &str = "progression.calibration";
const CONFIG_KEY: &str = "progression.calibration_config";
pub(super) const DAY: i64 = 86400;
pub(super) const WINDOW_DAYS: i64 = 28;
const MINIMUM_DAYS: usize = 7;

pub(super) fn read_report(conn: &Connection) -> Result<Option<ProgressionCalibration>> {
    let json: Option<String> = conn
        .query_row("SELECT value FROM metadata WHERE key = ?1", [REPORT_KEY], |r| r.get(0))
        .optional()?;
    Ok(json.and_then(|json| match serde_json::from_str(&json) {
        Ok(report) => Some(report),
        Err(error) => {
            tracing::warn!(?error, "invalid progression calibration report; ignoring");
            None
        }
    }))
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Publish configuration and fallbacks without measuring history.
    /// Unchanged configuration preserves the calibrated values.
    pub fn configure_progression_calibration(&self, settings: &ProgressionSettings) -> Result<()> {
        let json = serde_json::to_string(settings)?;
        if self.get_metadata(CONFIG_KEY)?.as_deref() != Some(&json) {
            let mut conn = self.connect()?;
            let conn = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            conn.execute("DELETE FROM metadata WHERE key = ?1", [REPORT_KEY])?;
            conn.execute(
                "INSERT INTO metadata(key,value) VALUES (?1,?2)
                ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![CONFIG_KEY, json],
            )?;
            for (key, value) in CALIBRATED_KEYS.iter().zip(settings.calibration_values()) {
                if let Some(value) = value {
                    conn.execute(
                        "INSERT INTO metadata(key,value) VALUES (?1,?2)
                        ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                        params![format!("progression.{key}"), value.to_string()],
                    )?;
                } else {
                    conn.execute("DELETE FROM metadata WHERE key=?1", [format!("progression.{key}")])?;
                }
            }
            conn.execute(
                "INSERT INTO metadata(key,value) VALUES ('progression.max_creates_window_secs',?1)
                ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [settings.max_creates_window_secs.to_string()],
            )?;
            conn.commit()?;
        }
        Ok(())
    }

    pub fn calibrate_progression(&self, now: i64) -> Result<ProgressionCalibration> {
        let reader = self.reader()?;
        let mut conn = reader.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        if let Some(report) = read_report(&tx)?
            && report.computed_at.div_euclid(DAY) == now.div_euclid(DAY)
        {
            return Ok(report);
        }
        let config: Option<String> = tx
            .query_row("SELECT value FROM metadata WHERE key = ?1", [CONFIG_KEY], |r| r.get(0))
            .optional()?;
        let settings: ProgressionSettings = config
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default();
        let mut report = super::progression_calibration_data::measure(&tx, now, settings.max_creates_window_secs)?;
        let enough = report.snapshot_days >= MINIMUM_DAYS;
        let defaults = ProgressionSettings::default().calibration_values();
        let configured = settings.calibration_values();
        let m = &report.inputs;
        let measurement = |key: &str| m.get(key).copied();
        let triple = |key: &str| measurement(key).map(|v| v.saturating_mul(3));
        let computed = [
            measurement("peak_candidates_per_day")
                .zip(measurement("mean_run_seconds"))
                .map(|(count, seconds)| {
                    count
                        .saturating_mul(seconds)
                        .div_ceil(DAY as u64)
                        .saturating_mul(3)
                        .max(1)
                }),
            measurement("ledger_interval_p99_secs").map(|v| v.saturating_mul(10)),
            triple("automatic_pause_p99_secs"),
            triple("revision_to_review_max_secs"),
            triple("remediation_attempts_p99"),
            triple("run_duration_p99_secs"),
            None,
            measurement("progression_questions")
                .zip(measurement("engine_ladder_questions"))
                .filter(|(p, l)| p + l > 0)
                .map(|(p, l)| (p + l).saturating_mul(7).div_ceil(WINDOW_DAYS as u64)),
        ];
        let bases = [
            "ceil(peak daily candidates * mean run seconds / 86400) * 3",
            "10 * p99 ledger interval",
            "3 * p99 automatic remediation scheduling pause",
            "3 * longest progression revision time to first review",
            "3 * p99 per-item remediation attempts in a create-budget window",
            "3 * p99 completed progression run duration",
            "fixed reminder cadence; answer latency is diagnostic only",
            "ceil((progression + ladder questions) * 7 / 28); alarm only",
        ];
        for (index, key) in CALIBRATED_KEYS.iter().enumerate() {
            let overridden = settings.explicit_overrides & (1 << index) != 0 || configured[index] != defaults[index];
            let (value, source, basis) = if overridden {
                (configured[index], "overridden", "explicit configuration".to_owned())
            } else if enough && computed[index].is_some() {
                (
                    computed[index].map(|v| v.max(defaults[index].unwrap_or(0))),
                    "computed",
                    bases[index].to_owned(),
                )
            } else {
                (
                    defaults[index],
                    "fallback",
                    if index == 6 {
                        bases[index].to_owned()
                    } else if enough {
                        "no eligible measurement samples".to_owned()
                    } else {
                        format!("{} daily snapshots; need {MINIMUM_DAYS}", report.snapshot_days)
                    },
                )
            };
            // Diagnostic bounds expose extreme computed values without hiding
            // the measurement or overriding explicit configuration.
            let sanity_bounds = [
                64,
                3600,
                DAY as u64,
                7 * DAY as u64,
                100,
                DAY as u64,
                3 * DAY as u64,
                100,
            ];
            let basis = if source == "computed" && value.is_some_and(|v| v > sanity_bounds[index]) {
                format!(
                    "{basis}; WARNING: computed value exceeds sanity bound {}",
                    sanity_bounds[index]
                )
            } else {
                basis
            };
            let ceiling = match *key {
                "pool_size" => boss_engine_storage_values::pool_policy::MAX_PROGRESSION_POOL_SIZE as u64,
                "max_creates_per_item_window" | "expected_questions_per_week" => u32::MAX as u64,
                _ => i64::MAX as u64,
            };
            let limited = value.map(|v| v.min(ceiling));
            report.limits.insert(
                (*key).into(),
                ProgressionLimit {
                    value: limited,
                    source: source.into(),
                    basis: if limited != value {
                        format!("{basis}; clamped to representable ceiling {ceiling}")
                    } else {
                        basis
                    },
                },
            );
        }
        tx.commit()?;
        drop(conn);
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_config: Option<String> = tx
            .query_row("SELECT value FROM metadata WHERE key=?1", [CONFIG_KEY], |r| r.get(0))
            .optional()?;
        anyhow::ensure!(
            current_config == config,
            "progression configuration changed during measurement"
        );
        for (key, limit) in &report.limits {
            if let Some(value) = limit.value {
                tx.execute(
                    "INSERT INTO metadata(key,value) VALUES (?1,?2)
                    ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    params![format!("progression.{key}"), value.to_string()],
                )?;
            } else {
                tx.execute("DELETE FROM metadata WHERE key=?1", [format!("progression.{key}")])?;
            }
        }
        tx.execute(
            "INSERT INTO metadata(key,value) VALUES ('progression.max_creates_window_secs',?1)
            ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [settings.max_creates_window_secs.to_string()],
        )?;
        tx.execute(
            "INSERT INTO metadata(key,value) VALUES (?1,?2)
            ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![REPORT_KEY, serde_json::to_string(&report)?],
        )?;
        tx.commit()?;
        tracing::info!(calibration = %serde_json::to_string(&report)?, "progression limits recalibrated");
        Ok(report)
    }

    pub fn progression_limit(&self, key: &str, fallback: u64) -> Result<u64> {
        Ok(self
            .get_metadata(&format!("progression.{key}"))?
            .and_then(|value| value.parse().ok())
            .unwrap_or(fallback))
    }
}

pub(super) fn empty_report(now: i64) -> ProgressionCalibration {
    ProgressionCalibration::builder()
        .computed_at(now)
        .window_start((now.div_euclid(DAY) - WINDOW_DAYS) * DAY)
        .snapshot_days(0)
        .minimum_snapshot_days(MINIMUM_DAYS)
        .inputs(BTreeMap::new())
        .limits(BTreeMap::new())
        .fingerprint_changes_per_item(BTreeMap::new())
        .fingerprint_risk("insufficient fingerprint observations")
        .build()
}

#[cfg(test)]
#[path = "progression_calibration_tests.rs"]
mod tests;
