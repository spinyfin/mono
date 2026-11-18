//! Shared receipt/checkpoint writer, retaining WorkDb connection diagnostics.
use crate::*;

struct IngressConnection {
    connection: Arc<Mutex<Connection>>,
    metrics: Arc<boss_metrics::Registry>,
    /// The owning database's contention registry, so a lock wait under a
    /// batched write is reported against it like any other connection use.
    health: Arc<contention::Health>,
}

impl boss_write_batch::ConnectionSource for IngressConnection {
    fn with_connection(&self, write: boss_write_batch::ConnectionWork<'_>) -> Result<()> {
        let mut conn = crate::connection_wait::acquire(&self.connection, &self.metrics, &self.health)?;
        write(&mut conn)
    }
}

#[work_db_boundary::instrument]
impl WorkDb {
    pub(crate) fn ingress_writer(&self) -> Result<&boss_write_batch::Writer> {
        if self.ingress_writer.get().is_none() {
            let connection = Arc::new(IngressConnection {
                connection: self.conn.clone(),
                metrics: self.persona_metrics.clone(),
                health: self.contention_health.clone(),
            });
            let writer = boss_write_batch::Writer::new(connection)?;
            let _ = self.ingress_writer.set(writer);
        }
        Ok(self.ingress_writer.get().unwrap())
    }
}
