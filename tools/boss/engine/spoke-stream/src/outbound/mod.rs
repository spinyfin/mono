//! Outbound-only journal transport. Network failures retry; identity, storage,
//! protocol and replay failures stop for reconciliation without losing evidence.
mod codec;
mod session;

use anyhow::{Result, ensure};
use boss_protocol_spoke::{
    negotiate::{Contracts, EPOCH},
    v1::{Feature, Hello, Stamp},
};
use boss_spoke_journal::Journal;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tonic::transport::Endpoint;
use uuid::Uuid;

pub type SharedJournal = Arc<Mutex<Journal>>;
pub use session::connect_once;

pub fn contracts() -> Contracts {
    Contracts {
        min_epoch: EPOCH,
        max_epoch: EPOCH,
        features: vec![Feature {
            name: "journal".into(),
            revision: 1,
        }],
        required: vec!["journal".into()],
    }
}

pub fn hello(journal: &Journal, boot_id: &str, nonce: &[u8]) -> Result<Hello> {
    let (host_id, journal_id) = journal.identity()?;
    let (first_retained, last_record) = journal.record_bounds()?;
    Ok(Hello {
        host_id,
        journal_id,
        boot_id: boot_id.into(),
        min_epoch: EPOCH,
        max_epoch: EPOCH,
        features: contracts().features,
        build: env!("CARGO_PKG_VERSION").into(),
        received_commands: journal.received_commands()?,
        first_retained,
        last_record,
        instance_nonce: nonce.to_vec(),
        inventory: None,
    })
}

#[derive(Debug, thiserror::Error)]
#[error("spoke transport unavailable: {0}")]
struct Retry(String);

pub async fn run(journal: SharedJournal, endpoint: Endpoint, stamp: impl Fn() -> Result<Stamp>) -> Result<()> {
    ensure!(
        endpoint.uri().scheme_str() == Some("https"),
        "spoke requires mTLS HTTPS"
    );
    let nonce = Uuid::new_v4().as_bytes().to_vec();
    let mut attempt = 0u32;
    loop {
        let started = std::time::Instant::now();
        if let Err(error) = connect_once(&journal, endpoint.clone(), &nonce, &stamp).await {
            if error.downcast_ref::<Retry>().is_none() {
                return Err(error);
            }
            tracing::warn!(error = %error, "spoke disconnected; retaining journal");
        }
        if started.elapsed() >= Duration::from_secs(60) {
            attempt = 0;
        }
        let delay = boss_backoff::equal_jitter(boss_backoff::exponential_delay(
            Duration::from_millis(250),
            Duration::from_secs(30),
            attempt,
        ));
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(delay).await;
    }
}

pub(crate) fn locked(journal: &SharedJournal) -> Result<std::sync::MutexGuard<'_, Journal>> {
    journal.lock().map_err(|_| anyhow::anyhow!("journal owner panicked"))
}

pub(crate) fn network(error: impl std::fmt::Display) -> anyhow::Error {
    Retry(error.to_string()).into()
}

pub(crate) fn status(error: tonic::Status) -> anyhow::Error {
    match error.code() {
        tonic::Code::Unavailable | tonic::Code::Cancelled | tonic::Code::DeadlineExceeded | tonic::Code::Unknown => {
            network(error)
        }
        _ => error.into(),
    }
}
