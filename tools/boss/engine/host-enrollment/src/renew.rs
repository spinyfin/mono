//! `Renew`: replacing a host's certificate before it expires.

use crate::authority::{AuthorizedHost, CredentialEvent, check_serial};
use crate::clock::{from_unix_secs, unix_secs};
use crate::credentials::{HostCredentials, IssuedCertificate, certificate_response, check_opaque_id};
use crate::csr;
use crate::error::{CredentialError, Denial};
use crate::store::{self, CertKind, CertRow, HostState};

/// The fields of `RenewRequest`.
#[derive(Debug, Clone, Copy)]
pub struct RenewInput<'a> {
    pub request_id: &'a str,
    pub csr_der: &'a [u8],
}

impl HostCredentials {
    /// Issues `caller` a replacement certificate over a new key.
    ///
    /// `caller` is the host the transport authenticated for this request; it
    /// is re-checked inside the transaction because a revocation or another
    /// renewal may have landed since. The replacement becomes current and the
    /// certificate it replaces keeps authorizing for
    /// [`Policy::renewal_overlap`](crate::Policy), capped by its own expiry,
    /// so a spoke that has not yet installed the replacement can reconnect.
    ///
    /// A repeat of an earlier `request_id` with the same key returns the
    /// certificate that request produced, from either the current or the
    /// overlapping certificate, so a lost reply is recoverable. A new request
    /// must come from the current certificate and use a key the host is not
    /// already using. An expired certificate cannot renew: that host must be
    /// provisioned again.
    pub fn renew(
        &self,
        caller: &AuthorizedHost,
        request: &RenewInput<'_>,
    ) -> Result<IssuedCertificate, CredentialError> {
        check_opaque_id("request_id", request.request_id)?;
        let csr = csr::parse(request.csr_der)?;
        let now = self.clock.now();
        let now_secs = unix_secs(now);
        let host_id = caller.host_id.as_str();
        let ids = ("", request.request_id);
        let mut events = Vec::new();
        let outcome = self.store.write(|tx| {
            let host = store::host(tx, host_id)?.ok_or(Denial::UnknownHost)?;
            check_serial(tx, &host, &caller.serial, now_secs)?;
            if let Some(stored) = store::certificate_for_request(tx, host_id, CertKind::Renew, ids)? {
                if stored.key_digest != csr.key_digest {
                    return Err(Denial::RequestReused.into());
                }
                check_serial(tx, &host, &stored.serial, now_secs)?;
                store::audit(tx, now_secs, host_id, "renew_retried", "")?;
                return Ok(stored);
            }
            if host.current_serial.as_deref() != Some(caller.serial.as_str()) {
                return Err(Denial::NotCurrentCertificate.into());
            }
            if store::key_in_use(tx, &host, &csr.key_digest)? {
                return Err(Denial::KeyReused.into());
            }
            let leaf = self
                .trust
                .ca
                .issue_host_certificate(host_id, &csr.public_key, now, &self.policy)?;
            let row = CertRow {
                serial: leaf.serial,
                key_digest: csr.key_digest.to_vec(),
                leaf_der: leaf.der,
                not_after: unix_secs(leaf.not_after),
            };
            let replaced = store::certificate(tx, &caller.serial)?.ok_or(Denial::Superseded)?;
            let overlap_end = (now_secs + self.policy.renewal_overlap.as_secs() as i64).min(replaced.not_after);
            store::insert_certificate(tx, host_id, CertKind::Renew, ids, &row)?;
            store::set_host(
                tx,
                host_id,
                HostState::Enrolled,
                (Some(&row.serial), Some((&caller.serial, overlap_end))),
            )?;
            // The certificate that was already "previous" loses its overlap
            // now; the one just replaced gets the overlap instead.
            if let Some(dropped) = &host.previous_serial {
                events.push(CredentialEvent::Superseded {
                    host_id: host_id.to_owned(),
                    serial: dropped.clone(),
                    valid_until: now,
                });
            }
            events.push(CredentialEvent::Superseded {
                host_id: host_id.to_owned(),
                serial: caller.serial.clone(),
                valid_until: from_unix_secs(overlap_end),
            });
            store::audit(tx, now_secs, host_id, "renewed", &format!("replaced={}", caller.serial))?;
            Ok(row)
        });
        let stored = match outcome {
            Ok(stored) => stored,
            Err(CredentialError::Denied(denial)) => {
                tracing::warn!(host_id, reason = denial.code(), "renewal refused");
                return Err(denial.into());
            }
            Err(other) => return Err(other),
        };
        self.publish(events);
        Ok(certificate_response(self, host_id, &stored))
    }
}
