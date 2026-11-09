//! Production hub stream service: a WorkDb-backed observation journal and
//! command outbox, composed with host enrollment on one optional-client-auth
//! mTLS listener.
//!
//! This crate does not install a host adapter and does not start a listener
//! from engine configuration. New work stays on SSH (`HostTransport::LegacySsh`)
//! until a later composition proves host readiness and wires
//! [`HubStream::manager`] into dispatch.

mod serve;
mod store;

use std::sync::Arc;

use boss_host_enrollment::HostCredentials;
use boss_spoke_stream::{Config, StreamManager};
use boss_work_db::WorkDb;

pub use serve::{HubListener, server_tls};
pub use store::{WorkDbOutbox, WorkDbSink};

/// Handle through which engine adapter composition uses the same
/// [`StreamManager`] that the listener serves.
#[derive(Clone)]
pub struct HubStream {
    manager: StreamManager,
    credentials: Arc<HostCredentials>,
}

impl HubStream {
    /// Compose the durable sink/outbox, credential authorizer, and stream
    /// manager. The caller supplies the hub identity in `config`; nothing here
    /// binds a socket.
    pub fn new(config: Config, credentials: Arc<HostCredentials>, db: WorkDb) -> Self {
        let sink = Arc::new(WorkDbSink::new(db.clone()));
        let outbox = Arc::new(WorkDbOutbox::new(db));
        let manager = StreamManager::new(config, credentials.clone(), sink, outbox);
        Self { manager, credentials }
    }

    /// The stream manager later adapter composition submits commands through.
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
