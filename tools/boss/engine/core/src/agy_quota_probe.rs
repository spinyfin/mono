//! Composition glue: the driver-quota panel's Antigravity (`agy`) probe.
//!
//! `boss-engine-driver-quota` parses `/usage` JSON but does not know how to
//! run `agy` in a scoped HOME. The driver crate knows the scoped runner but
//! knows nothing about the quota panel. This module is the one place the two
//! meet.

use async_trait::async_trait;
use boss_engine_driver_quota::DriverQuotaProbe;
use boss_protocol::{DriverQuotaFailureKind, DriverQuotaOutcome};

/// [`DriverQuotaProbe`] that runs the driver's scoped `agy --print /usage`.
pub(crate) struct AgyQuotaProbe;

#[async_trait]
impl DriverQuotaProbe for AgyQuotaProbe {
    fn driver(&self) -> &'static str {
        boss_protocol::DRIVER_SLUG_AGY
    }

    /// `agy --print /usage` is a local command; captured fixtures show
    /// `num_turns: 0`.
    fn spends_model_tokens(&self) -> bool {
        false
    }

    async fn probe(&self) -> DriverQuotaOutcome {
        outcome_from_usage_result(tokio::task::spawn_blocking(boss_engine_driver::agy::quota::usage_json).await)
    }
}

fn outcome_from_usage_result(joined: Result<anyhow::Result<String>, tokio::task::JoinError>) -> DriverQuotaOutcome {
    match joined {
        Ok(Ok(output)) => boss_engine_driver_quota::agy::parse_usage_json(&output),
        Ok(Err(err)) => map_runner_error(&err),
        Err(err) => unavailable(
            DriverQuotaFailureKind::ProbeFailed,
            format!("agy quota probe task failed: {err}"),
        ),
    }
}

fn map_runner_error(err: &anyhow::Error) -> DriverQuotaOutcome {
    if err.chain().any(|cause| {
        cause
            .downcast_ref::<boss_engine_driver::agy::AgyNotInstalled>()
            .is_some()
    }) {
        return unavailable(
            DriverQuotaFailureKind::NotInstalled,
            "agy is not installed or not on PATH",
        );
    }
    unavailable(
        DriverQuotaFailureKind::ProbeFailed,
        user_facing_reason(&format!("{err:#}")),
    )
}

fn user_facing_reason(detail: &str) -> String {
    detail
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .unwrap_or("Could not read agy quota")
        .to_owned()
}

fn unavailable(kind: DriverQuotaFailureKind, reason: impl Into<String>) -> DriverQuotaOutcome {
    DriverQuotaOutcome::Unavailable {
        kind,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_resolution_failure_maps_to_not_installed() {
        let err = anyhow::Error::new(boss_engine_driver::agy::AgyNotInstalled(
            "`which agy` failed; install Antigravity CLI or set BOSS_AGY_BIN".into(),
        ));
        match map_runner_error(&err) {
            DriverQuotaOutcome::Unavailable {
                kind: DriverQuotaFailureKind::NotInstalled,
                ..
            } => {}
            other => panic!("expected NotInstalled, got {other:?}"),
        }
    }

    #[test]
    fn free_text_which_agy_message_does_not_map_to_not_installed() {
        let err = anyhow::anyhow!("`which agy` failed; install Antigravity CLI or set BOSS_AGY_BIN");
        match map_runner_error(&err) {
            DriverQuotaOutcome::Unavailable {
                kind: DriverQuotaFailureKind::ProbeFailed,
                ..
            } => {}
            other => panic!("expected ProbeFailed without a typed error, got {other:?}"),
        }
    }

    #[test]
    fn lock_contention_and_command_failures_map_to_probe_failed() {
        for message in [
            "agy quota check already running",
            "agy quota command failed or timed out",
            "agy quota output was not UTF-8",
            "Could not provision isolated agy quota home",
        ] {
            match map_runner_error(&anyhow::anyhow!("{message}")) {
                DriverQuotaOutcome::Unavailable {
                    kind: DriverQuotaFailureKind::ProbeFailed,
                    ..
                } => {}
                other => panic!("{message}: expected ProbeFailed, got {other:?}"),
            }
        }
    }

    #[test]
    fn successful_output_is_handed_to_the_parser() {
        match outcome_from_usage_result(Ok(Ok("{".to_owned()))) {
            DriverQuotaOutcome::Unavailable {
                kind: DriverQuotaFailureKind::Unparseable,
                ..
            } => {}
            other => panic!("expected Unparseable, got {other:?}"),
        }
    }
}
