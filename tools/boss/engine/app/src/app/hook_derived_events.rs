//! Publishes the event-bus transitions derived from a hook event.

use super::*;

impl ServerState {
    /// Publish the bus events a hook-ingress event implies. The execution's
    /// driver is resolved from the work DB so a transcript in a native
    /// dialect is normalised before the trailing-error check.
    pub(super) async fn publish_hook_derived_events(
        &self,
        incoming: &boss_engine_runtime::events_socket::IncomingHookEvent,
    ) {
        boss_engine_runtime::events_socket::publish_hook_derived_events(&self.event_bus, &self.work_db, incoming).await;
    }
}
