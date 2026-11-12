//! Production hub stream service: a WorkDb-backed observation journal and
//! command outbox, composed with host enrollment on one optional-client-auth
//! mTLS listener, and the projection that applies what the journal received.
//!
//! Engine startup binds this listener from configuration when listen,
//! credentials and TLS identity are all set. This crate does not install a
//! host adapter; new work stays on SSH (`HostTransport::LegacySsh`) until a
//! host is proven ready and [`HubStream::manager`] is wired into dispatch.
//!
//! Receipt and application are separate: the sink stores frames and advances
//! the receipt, and [`projection::Projector`] applies them to work state on its
//! own cursor. See that module for what makes an effect happen once.

pub mod projection;
mod serve;
mod store;

use std::sync::Arc;

use boss_host_enrollment::HostCredentials;
use boss_spoke_stream::{Config, StreamManager};
use boss_work_db::WorkDb;
use tokio::sync::Notify;

pub use serve::{HubListener, server_tls};
pub use store::{WorkDbOutbox, WorkDbSink};

/// Handle through which engine adapter composition uses the same
/// [`StreamManager`] that the listener serves.
#[derive(Clone)]
pub struct HubStream {
    manager: StreamManager,
    credentials: Arc<HostCredentials>,
    /// Notified after each committed record; the projector's prompt to apply it.
    projection_wake: Arc<Notify>,
}

impl HubStream {
    /// Compose the durable sink/outbox, credential authorizer, and stream
    /// manager. The caller supplies the hub identity in `config`; nothing here
    /// binds a socket.
    pub fn new(config: Config, credentials: Arc<HostCredentials>, db: WorkDb) -> Self {
        let projection_wake = Arc::new(Notify::new());
        let sink = Arc::new(WorkDbSink::with_wake(db.clone(), Arc::clone(&projection_wake)));
        let outbox = Arc::new(WorkDbOutbox::new(db));
        let manager = StreamManager::new(config, credentials.clone(), sink, outbox);
        Self {
            manager,
            credentials,
            projection_wake,
        }
    }

    /// A projector over `db` that applies what this stream's sink receives
    /// through `applier` and delivers its decisions over this stream's manager.
    /// The caller runs it ([`projection::Projector::run`]) on its task scope.
    pub fn projector(
        &self,
        db: WorkDb,
        applier: Arc<dyn projection::ObservationApplier>,
        config: projection::ProjectorConfig,
    ) -> projection::Projector {
        projection::Projector::new(
            db,
            applier,
            Arc::new(self.manager.clone()),
            Arc::clone(&self.projection_wake),
            config,
        )
    }

    /// Compose with the hub identity strings engine configuration carries.
    /// Stream limits keep the crate defaults.
    pub fn with_identity(
        hub_id: impl Into<String>,
        authority_generation: impl Into<String>,
        build_fingerprint: impl Into<String>,
        credentials: Arc<HostCredentials>,
        db: WorkDb,
    ) -> Self {
        Self::new(
            Config::builder()
                .hub_id(hub_id)
                .authority_generation(authority_generation)
                .build_fingerprint(build_fingerprint)
                .build(),
            credentials,
            db,
        )
    }

    /// The stream manager serving connected spokes.
    pub fn manager(&self) -> &StreamManager {
        &self.manager
    }

    /// The credential authority sharing this listener.
    pub fn credentials(&self) -> &Arc<HostCredentials> {
        &self.credentials
    }

    /// Serve `Enroll`/`Renew` and `Connect` on `incoming`. The server
    /// certificate is separate from the host CA: pass it as `identity`.
    pub async fn serve(
        self,
        incoming: tokio::net::TcpListener,
        identity: tonic::transport::Identity,
    ) -> Result<(), tonic::transport::Error> {
        HubListener::from_stream(self).serve(incoming, identity).await
    }
}
