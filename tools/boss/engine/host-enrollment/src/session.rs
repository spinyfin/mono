//! The per-stream side of authorization: admitting a connection and knowing
//! when to close it.
//!
//! [`admit`] does what [`HostAuthorizer`]'s contract requires of a stream
//! manager, in the order it requires: subscribe first, then authorize off the
//! async runtime. The returned [`StreamGuard`] then owns the stream's closing
//! rules (revocation, supersession, the certificate's own deadline, missed
//! events), so a stream manager only has to `select!` on
//! [`StreamGuard::closed`].

use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::CertificateDer;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::authority::{AuthorizedHost, CredentialEvent, HostAuthorizer};
use crate::error::Denial;

/// The longest the guard sleeps before looking at the authorizer's clock
/// again, so a deadline that moves (or a clock that jumps) is noticed promptly.
const MAX_WAIT: Duration = Duration::from_secs(1);

/// Why a stream must close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// The host's identity was revoked.
    Revoked,
    /// The stream's certificate reached its deadline: expiry, or the end of
    /// a renewal overlap.
    DeadlineElapsed,
    /// A recheck against the records refused the certificate.
    Denied(Denial),
    /// The authority went away; nothing can vouch for the stream any more.
    AuthorityGone,
}

/// An admitted stream's authorization state.
pub struct StreamGuard {
    authorizer: Arc<dyn HostAuthorizer>,
    events: broadcast::Receiver<CredentialEvent>,
    host: AuthorizedHost,
    /// A recheck against the records is owed: set when events were missed or
    /// the host's certificate was superseded, cleared only once `revalidate`
    /// has answered. It lives on the guard so dropping a `closed()` future
    /// mid-recheck does not forget it.
    needs_revalidate: bool,
}

/// Authorizes the certificate chain of a new stream.
///
/// Subscribes to credential events **before** authorizing, so a revocation
/// committed in between still reaches the guard, and runs the blocking check
/// on the blocking pool.
pub async fn admit(
    authorizer: Arc<dyn HostAuthorizer>,
    peer_chain: Vec<CertificateDer<'static>>,
) -> Result<StreamGuard, Denial> {
    let events = authorizer.subscribe();
    let checking = Arc::clone(&authorizer);
    let host = tokio::task::spawn_blocking(move || checking.authorize(&peer_chain))
        .await
        .map_err(|_| Denial::StoreUnavailable)??;
    Ok(StreamGuard {
        authorizer,
        events,
        host,
        needs_revalidate: false,
    })
}

impl StreamGuard {
    /// The host the certificate speaks for.
    pub fn host(&self) -> &AuthorizedHost {
        &self.host
    }

    /// Checks the host a spoke claims in its first frame against the one its
    /// certificate proved. A mismatch is [`Denial::WrongIdentity`].
    pub fn check_hello(&self, claimed_host_id: &str) -> Result<(), Denial> {
        if claimed_host_id == self.host.host_id {
            Ok(())
        } else {
            Err(Denial::WrongIdentity)
        }
    }

    /// Resolves when the stream must close. Cancel-safe: dropping the future
    /// and calling again loses no event.
    ///
    /// The deadline only moves earlier: from a matching `Superseded`, and from
    /// the host `revalidate` returns after a `Lagged` receiver. Events about
    /// other hosts do not postpone it.
    pub async fn closed(&mut self) -> CloseReason {
        loop {
            if self.needs_revalidate
                && let Err(reason) = self.refresh().await
            {
                return reason;
            }
            let now = self.authorizer.now();
            let Ok(remaining) = self.host.valid_until.duration_since(now) else {
                return CloseReason::DeadlineElapsed;
            };
            if remaining.is_zero() {
                return CloseReason::DeadlineElapsed;
            }
            tokio::select! {
                () = tokio::time::sleep(remaining.min(MAX_WAIT)) => {}
                event = self.events.recv() => match event {
                    Ok(CredentialEvent::Revoked { host_id }) if host_id == self.host.host_id => {
                        return CloseReason::Revoked;
                    }
                    Ok(CredentialEvent::Superseded { host_id, serial, valid_until })
                        if host_id == self.host.host_id && serial == self.host.serial =>
                    {
                        self.host.valid_until = self.host.valid_until.min(valid_until);
                        self.needs_revalidate = true;
                    }
                    Ok(_) => {}
                    // Missed events: the only safe answer is to look again.
                    Err(RecvError::Lagged(_)) => self.needs_revalidate = true,
                    Err(RecvError::Closed) => return CloseReason::AuthorityGone,
                },
            }
        }
    }

    /// Re-checks the host against the records and adopts its current deadline.
    async fn refresh(&mut self) -> Result<(), CloseReason> {
        let (authorizer, current) = (Arc::clone(&self.authorizer), self.host.clone());
        match tokio::task::spawn_blocking(move || authorizer.revalidate(&current)).await {
            Ok(Ok(updated)) => {
                self.host = updated;
                self.needs_revalidate = false;
                Ok(())
            }
            Ok(Err(Denial::Revoked)) => Err(CloseReason::Revoked),
            Ok(Err(denial)) => Err(CloseReason::Denied(denial)),
            Err(_) => Err(CloseReason::Denied(Denial::StoreUnavailable)),
        }
    }
}
