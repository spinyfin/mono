//! Stamps the current `workers.tmux_hosting` pool state onto every emitted
//! dispatch event.
//!
//! Per the tmux-hosting migration design
//! (`tools/boss/docs/designs/run-agents-and-the-coordinator-in-tmux-so-work-survives-app-and-engine-restarts.md`,
//! §"Migration and rollback"): an operator-set, logged, UI-visible hosting
//! mode is *not* the silent fallback the design forbids, but only if it is
//! actually visible — stamped on every dispatch event, badged on the
//! Workers grid, and reported by `bossctl doctor`. This module is the first
//! of those three surfaces.
//!
//! [`HostingModeStampingSink`] wraps the production event sink at its one
//! construction site (`ServerState::new`) rather than touching every one of
//! the dozens of call sites that construct a [`DispatchEvent`] — the stamp
//! is therefore best-effort and coarse (the three-pool snapshot at emit
//! time, not the specific pool the event's execution happens to belong to,
//! which would need a DB lookup on every event). That trade-off is
//! deliberate: it is cheap (three in-memory `Mutex` reads, no I/O), safe
//! (never blocks or fails an emit), and correct in the common case where a
//! pool's hosting mode isn't changing mid-flight. For the one place the
//! precise per-execution pool is already known for free — the spawn
//! decision itself — see `runner::pane_spawn`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::dispatch_events::{DispatchEvent, DispatchEventSink};
use crate::settings::SettingsStore;

/// Key under `DispatchEvent::details` carrying the per-pool snapshot.
const DETAILS_KEY: &str = "tmux_hosting";

/// Wraps another [`DispatchEventSink`] and stamps the current tmux-hosting
/// pool snapshot into every event's `details` before delegating. See the
/// module doc for why this is a wrapper around the one production sink
/// rather than a change to every emit call site.
pub struct HostingModeStampingSink {
    inner: Arc<dyn DispatchEventSink>,
    settings: Arc<SettingsStore>,
}

impl HostingModeStampingSink {
    pub fn new(inner: Arc<dyn DispatchEventSink>, settings: Arc<SettingsStore>) -> Self {
        Self { inner, settings }
    }
}

#[async_trait]
impl DispatchEventSink for HostingModeStampingSink {
    async fn emit(&self, mut event: DispatchEvent) {
        stamp(&mut event, &self.settings);
        self.inner.emit(event).await;
    }
}

/// Inserts the per-pool tmux-hosting snapshot into `event.details`.
///
/// `details` is documented as a per-stage "open object" — every existing
/// call site either leaves it as the `serde_json::Value` default (`null`)
/// or builds a JSON object. This handles both: `null` becomes a fresh
/// object holding just the stamp; an existing object gains the
/// [`DETAILS_KEY`] sibling alongside whatever the stage already put there.
/// A `details` value that is neither (never seen in this codebase today)
/// is left untouched rather than risking corruption of stage-specific data.
fn stamp(event: &mut DispatchEvent, settings: &SettingsStore) {
    let pools = settings.tmux_hosting_pool_snapshot();
    let stamp = serde_json::json!({
        "review": pools.review,
        "automation": pools.automation,
        "interactive": pools.interactive,
    });
    if event.details.is_null() {
        event.details = serde_json::json!({ DETAILS_KEY: stamp });
    } else if let Some(details) = event.details.as_object_mut() {
        details.insert(DETAILS_KEY.to_owned(), stamp);
    } else {
        tracing::debug!(
            stage = %event.stage,
            execution_id = %event.execution_id,
            "dispatch_hosting_stamp: details is neither null nor an object; leaving unstamped",
        );
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::dispatch_events::{Outcome, RecordingDispatchEventSink, Stage};

    fn make_settings(tmp: &TempDir) -> Arc<SettingsStore> {
        let store = Arc::new(SettingsStore::new(tmp.path().join("settings.toml")));
        store.load().unwrap();
        store
    }

    #[tokio::test]
    async fn stamps_null_details_with_the_current_pool_snapshot() {
        let tmp = TempDir::new().unwrap();
        let settings = make_settings(&tmp);
        settings.set_tmux_hosting_enabled(true).unwrap();
        let recording = Arc::new(RecordingDispatchEventSink::new());
        let sink = HostingModeStampingSink::new(recording.clone(), settings);

        sink.emit(DispatchEvent::new(Stage::TmuxAdopt, Outcome::Ok, "exec-1"))
            .await;

        let events = recording.events().await;
        assert_eq!(events.len(), 1);
        let stamped = &events[0].details[DETAILS_KEY];
        assert_eq!(stamped["review"], true);
        assert_eq!(stamped["automation"], true);
        assert_eq!(stamped["interactive"], true);
    }

    #[tokio::test]
    async fn preserves_existing_details_keys_when_stamping() {
        let tmp = TempDir::new().unwrap();
        let settings = make_settings(&tmp);
        let recording = Arc::new(RecordingDispatchEventSink::new());
        let sink = HostingModeStampingSink::new(recording.clone(), settings);

        let event = DispatchEvent::new(Stage::TmuxAdopt, Outcome::Ok, "exec-1")
            .with_details(serde_json::json!({ "session_name": "boss-6-abc" }));
        sink.emit(event).await;

        let events = recording.events().await;
        let details = &events[0].details;
        assert_eq!(details["session_name"], "boss-6-abc");
        assert_eq!(details[DETAILS_KEY]["review"], true);
        assert_eq!(details[DETAILS_KEY]["automation"], true);
        assert_eq!(details[DETAILS_KEY]["interactive"], true);
    }

    #[tokio::test]
    async fn reflects_a_partial_pool_set_precisely() {
        let tmp = TempDir::new().unwrap();
        let settings = make_settings(&tmp);
        // Enable only review, bypassing the all-or-nothing UI switch, the
        // way a staged acceptance sweep would.
        settings
            .set_tmux_hosting_pools(crate::settings::TmuxHostingPools::from_pools([
                crate::settings::TmuxHostingPool::Review,
            ]))
            .unwrap();
        let recording = Arc::new(RecordingDispatchEventSink::new());
        let sink = HostingModeStampingSink::new(recording.clone(), settings);

        sink.emit(DispatchEvent::new(Stage::TmuxAdopt, Outcome::Ok, "exec-1"))
            .await;

        let events = recording.events().await;
        let stamped = &events[0].details[DETAILS_KEY];
        assert_eq!(stamped["review"], true);
        assert_eq!(stamped["automation"], false);
        assert_eq!(stamped["interactive"], false);
    }
}
