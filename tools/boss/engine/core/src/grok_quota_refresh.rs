//! Composition glue: the driver-quota panel's Grok credential refresh, backed
//! by the Grok driver's own `grok models` credential pass.
//!
//! `boss-engine-driver-quota` knows *when* a stored Grok token needs the CLI
//! to refresh it but deliberately holds no knowledge of how to run `grok`;
//! `boss-engine-driver` knows how to run `grok` against the shared credential
//! but knows nothing about quota. This module is the one place the two meet,
//! so the quota panel and the worker preflight exercise the credential through
//! the identical command, environment belt, and lock protocol.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use boss_engine_driver::grok::{GrokOauthLiveness, refresh_shared_oauth_credential_blocking, resolve_grok_auth_source};
use boss_engine_driver_quota::{GrokCredentialRefresher, GrokRefreshOutcome, QuotaProbeSet};

/// Wall-clock budget for one CLI credential pass (lock wait plus `grok
/// models`). `grok models` answers in one to two seconds when healthy; the
/// budget must stay comfortably inside the quota cache's per-probe deadline
/// ([`boss_engine_driver_quota::DEFAULT_PROBE_TIMEOUT`], 25 s) so a slow
/// refresh is reported as a refresh failure rather than as a probe timeout.
const REFRESH_BUDGET: Duration = Duration::from_secs(15);

/// [`GrokCredentialRefresher`] that runs the driver's `grok models` pass
/// against the shared credential on the blocking pool.
pub struct GrokCliCredentialRefresher {
    auth_path: PathBuf,
}

impl GrokCliCredentialRefresher {
    pub fn new(auth_path: PathBuf) -> Self {
        Self { auth_path }
    }
}

#[async_trait]
impl GrokCredentialRefresher for GrokCliCredentialRefresher {
    async fn refresh(&self) -> GrokRefreshOutcome {
        let auth_path = self.auth_path.clone();
        let joined =
            tokio::task::spawn_blocking(move || refresh_shared_oauth_credential_blocking(&auth_path, REFRESH_BUDGET))
                .await;
        match joined {
            Ok(Ok(GrokOauthLiveness::LoggedIn)) => GrokRefreshOutcome::LoggedIn,
            Ok(Ok(GrokOauthLiveness::NotLoggedIn { detail })) => GrokRefreshOutcome::NotLoggedIn { detail },
            // anyhow's Display is the outermost context only; `:#` carries the
            // chain so the panel can say *why* the CLI pass did not run.
            Ok(Err(err)) => GrokRefreshOutcome::Failed {
                detail: format!("{err:#}"),
            },
            Err(err) => GrokRefreshOutcome::Failed {
                detail: format!("grok credential refresh task failed: {err}"),
            },
        }
    }
}

/// The production probe set: every driver, with the Grok probe reading the
/// same `auth.json` the Grok driver reads and refreshing it through the same
/// CLI pass the Grok worker preflight uses.
pub fn production_probes() -> QuotaProbeSet {
    let auth_path = resolve_grok_auth_source();
    boss_engine_driver_quota::default_probes(auth_path.clone(), Arc::new(GrokCliCredentialRefresher::new(auth_path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_budget_fits_inside_the_probe_deadline_with_room_for_the_http_read() {
        assert!(
            REFRESH_BUDGET + Duration::from_secs(5) <= boss_engine_driver_quota::DEFAULT_PROBE_TIMEOUT,
            "a refresh that uses its whole budget must still leave time for the billing request"
        );
    }
}
