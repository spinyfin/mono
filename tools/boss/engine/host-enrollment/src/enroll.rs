//! `Enroll`: turning a single-use token and a CSR into the host's first
//! certificate.

use std::time::SystemTime;

use crate::authority::CredentialEvent;
use crate::clock::{from_unix_secs, unix_secs};
use crate::credentials::{HostCredentials, IssuedCertificate, certificate_response, check_host_id, check_opaque_id};
use crate::csr;
use crate::error::{CredentialError, Denial};
use crate::store::{self, CertKind, CertRow, GrantState, HostRow, HostState};
use crate::token::EnrollmentToken;

/// The fields of `EnrollRequest`.
#[derive(Debug, Clone, Copy)]
pub struct EnrollInput<'a> {
    pub host_id: &'a str,
    pub job_id: &'a str,
    pub request_id: &'a str,
    pub token: &'a [u8],
    pub csr_der: &'a [u8],
}

impl HostCredentials {
    /// Consumes the token and issues the certificate in one transaction.
    ///
    /// The token binds `(host_id, job_id)`; the first request to present it
    /// also binds `request_id` and the CSR's public key to the certificate it
    /// gets. An exact repeat (same token, ids and key) inside
    /// [`Policy::enroll_retry_window`](crate::Policy) returns that same
    /// certificate, so a lost reply is recoverable. Anything else with a used
    /// token (another request id, another key, another job) is refused, as is
    /// a token that expired unused, was replaced or cancelled, or belongs to
    /// a revoked host.
    ///
    /// Success means the host holds a certificate, not that it is ready: it
    /// still has to connect, report its preflight and be enabled.
    pub fn enroll(&self, request: &EnrollInput<'_>) -> Result<IssuedCertificate, CredentialError> {
        check_host_id(request.host_id)?;
        check_opaque_id("job_id", request.job_id)?;
        check_opaque_id("request_id", request.request_id)?;
        let token = EnrollmentToken::from_bytes(request.token)
            .ok_or_else(|| CredentialError::Invalid("token must be 32 bytes".to_owned()))?;
        let csr = csr::parse(request.csr_der)?;
        let now = self.clock.now();
        let mut events = Vec::new();
        let outcome = self.store.write(|tx| {
            let hash = token.hash();
            // Every refusal below is the same answer to the caller, whichever
            // check fired, so a token guesser learns nothing from the reason.
            let grant = store::grant(tx, &hash)?.ok_or(Denial::UnknownToken)?;
            if grant.host_id != request.host_id {
                return Err(Denial::HostMismatch.into());
            }
            if grant.job_id != request.job_id {
                return Err(Denial::JobMismatch.into());
            }
            let host = store::host(tx, request.host_id)?.ok_or(Denial::HostMismatch)?;
            if host.state == HostState::Revoked {
                return Err(Denial::Revoked.into());
            }
            if grant.generation != host.generation {
                return Err(Denial::TokenReplaced.into());
            }
            let ids = (request.job_id, request.request_id);
            match grant.state {
                GrantState::Replaced => Err(Denial::TokenReplaced.into()),
                GrantState::Cancelled => Err(Denial::JobCancelled.into()),
                GrantState::Consumed(consumed_at, bound_serial) => {
                    let replay = Replay {
                        host_id: request.host_id,
                        ids,
                        key_digest: &csr.key_digest,
                        consumed_at,
                        bound_serial: &bound_serial,
                    };
                    let stored = self.recover_enrollment(tx, &host, &replay, now)?;
                    store::audit(
                        tx,
                        unix_secs(now),
                        request.host_id,
                        "enroll_retried",
                        &note_job(request.job_id),
                    )?;
                    Ok(stored)
                }
                GrantState::Issued => {
                    if unix_secs(now) >= grant.expires_at {
                        return Err(Denial::TokenExpired.into());
                    }
                    // Request ids are scoped to the host and job for good; a
                    // fresh token cannot reuse one that already produced a
                    // certificate. Refuse it by name rather than let the
                    // uniqueness constraint surface as an internal error.
                    if store::certificate_for_request(tx, request.host_id, CertKind::Enroll, ids)?.is_some() {
                        return Err(Denial::RequestReused.into());
                    }
                    let leaf =
                        self.trust
                            .ca
                            .issue_host_certificate(request.host_id, &csr.public_key, now, &self.policy)?;
                    let row = CertRow {
                        serial: leaf.serial,
                        key_digest: csr.key_digest.to_vec(),
                        leaf_der: leaf.der,
                        not_after: unix_secs(leaf.not_after),
                    };
                    store::consume_grant(tx, &hash, unix_secs(now), &row.serial)?;
                    store::insert_certificate(tx, request.host_id, CertKind::Enroll, ids, &row)?;
                    // A new enrollment is a new identity: whatever the host
                    // held before stops authorizing at once.
                    for old in [&host.current_serial, &host.previous_serial].into_iter().flatten() {
                        events.push(CredentialEvent::Superseded {
                            host_id: request.host_id.to_owned(),
                            serial: old.clone(),
                            valid_until: now,
                        });
                    }
                    store::set_host(tx, request.host_id, HostState::Enrolled, (Some(&row.serial), None))?;
                    store::audit(
                        tx,
                        unix_secs(now),
                        request.host_id,
                        "enrolled",
                        &note_job(request.job_id),
                    )?;
                    Ok(row)
                }
            }
        });
        let stored = match outcome {
            Ok(stored) => stored,
            Err(CredentialError::Denied(denial)) => {
                tracing::warn!(host_id = request.host_id, reason = denial.code(), "enrollment refused");
                return Err(denial.into());
            }
            Err(other) => return Err(other),
        };
        self.publish(events);
        Ok(certificate_response(self, request.host_id, &stored))
    }

    /// The certificate for an exact repeat of a request whose token is spent.
    fn recover_enrollment(
        &self,
        conn: &rusqlite::Connection,
        host: &HostRow,
        replay: &Replay<'_>,
        now: SystemTime,
    ) -> Result<CertRow, CredentialError> {
        let stored = store::certificate_for_request(conn, replay.host_id, CertKind::Enroll, replay.ids)?
            .ok_or(Denial::TokenReplayed)?;
        // The token is bound to the one certificate it produced. Another
        // request's certificate for the same host and job is not its to hand
        // back, even if every id and key in this request matches that one.
        if replay.bound_serial != stored.serial {
            return Err(Denial::TokenReplayed.into());
        }
        if stored.key_digest != replay.key_digest {
            return Err(Denial::KeyChanged.into());
        }
        if now >= from_unix_secs(replay.consumed_at) + self.policy.enroll_retry_window {
            return Err(Denial::RetryWindowElapsed.into());
        }
        // Only a certificate that is still the host's current one is worth
        // handing back; after a renewal or re-enrollment it is stale.
        if host.current_serial.as_deref() != Some(stored.serial.as_str()) {
            return Err(Denial::Superseded.into());
        }
        if unix_secs(now) >= stored.not_after {
            return Err(Denial::CertificateExpired.into());
        }
        Ok(stored)
    }
}

/// What identifies a repeat of an enrollment that already succeeded.
struct Replay<'a> {
    host_id: &'a str,
    ids: (&'a str, &'a str),
    key_digest: &'a [u8; 32],
    consumed_at: i64,
    /// The serial recorded on the consumed grant.
    bound_serial: &'a str,
}

fn note_job(job_id: &str) -> String {
    format!("job={job_id}")
}
