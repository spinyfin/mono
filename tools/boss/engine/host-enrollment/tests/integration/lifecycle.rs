//! The credential lifecycle against a real CA and database: enrollment,
//! renewal, expiry and revocation, with time under the test's control.

use std::time::Duration;

use boss_host_enrollment::{
    CredentialError, CredentialEvent, Denial, EnrollInput, HostAuthorizer as _, HostState, Policy, RenewInput,
};
use x509_parser::extensions::{GeneralName, ParsedExtension};

use crate::support::{Fixture, Spoke, presented, refusal};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const HOUR: Duration = Duration::from_secs(60 * 60);
const MINUTE: Duration = Duration::from_secs(60);

fn renew(
    fx: &Fixture,
    caller: &boss_host_enrollment::AuthorizedHost,
    spoke: &Spoke,
    request_id: &str,
) -> Result<boss_host_enrollment::IssuedCertificate, CredentialError> {
    fx.credentials.renew(
        caller,
        &RenewInput {
            request_id,
            csr_der: &spoke.csr(),
        },
    )
}

// ------------------------------------------------------------ enrollment

#[test]
fn enrollment_issues_a_client_only_certificate_for_exactly_that_host() {
    let fx = Fixture::new();
    let (_spoke, cert) = fx.enrolled("mac-mini-1");

    assert_eq!(cert.host_id, "mac-mini-1");
    assert_eq!(cert.chain_der, fx.credentials.trust_bundle_der());

    let (_, parsed) = x509_parser::parse_x509_certificate(&cert.leaf_der).expect("leaf parses");
    let san = parsed
        .subject_alternative_name()
        .expect("san ext")
        .expect("san present");
    let names: Vec<_> = san.value.general_names.iter().collect();
    assert!(
        matches!(names.as_slice(), [GeneralName::URI("urn:boss:host:mac-mini-1")]),
        "{names:?}"
    );
    let usage = parsed
        .extensions()
        .iter()
        .find_map(|ext| match ext.parsed_extension() {
            ParsedExtension::ExtendedKeyUsage(eku) => Some(eku.clone()),
            _ => None,
        })
        .expect("extended key usage");
    assert!(
        usage.client_auth && !usage.server_auth && !usage.any,
        "clientAuth only: {usage:?}"
    );
    assert!(!parsed.is_ca(), "a host certificate must not be able to sign");
    let lifetime = parsed.validity().not_after.timestamp() - parsed.validity().not_before.timestamp();
    let expected = (30 * DAY + 5 * MINUTE).as_secs() as i64;
    assert!(
        (lifetime - expected).abs() <= 2,
        "30 days plus the skew allowance, got {lifetime}s"
    );

    let status = fx
        .credentials
        .status("mac-mini-1")
        .expect("status")
        .expect("known host");
    assert_eq!(status.state, HostState::Enrolled);
    assert_eq!(status.current.expect("current").serial, cert.serial);
}

#[test]
fn a_pending_host_is_not_authorized_until_it_enrolls() {
    let fx = Fixture::new();
    fx.credentials.authorize_host("h1", "test").expect("authorize");
    let status = fx.credentials.status("h1").expect("status").expect("known");
    assert_eq!(status.state, HostState::Pending);
    assert!(status.current.is_none());
    assert!(matches!(
        fx.credentials.issue_enrollment_token("unknown", "job").err(),
        Some(CredentialError::UnknownHost)
    ));
}

#[test]
fn a_replayed_token_is_refused() {
    let fx = Fixture::new();
    let grant = fx.grant("h1", "job-1");
    let first = Spoke::new();
    fx.enroll(&first, ("h1", "job-1", "req-1"), &grant.token)
        .expect("first enrollment");

    // The same secret from another request, even with a fresh key.
    let thief = Spoke::new();
    assert_eq!(
        refusal(fx.enroll(&thief, ("h1", "job-1", "req-2"), &grant.token)),
        Denial::TokenReplayed
    );
    assert_eq!(
        refusal(fx.enroll(&thief, ("h1", "job-1", "req-1"), &grant.token)),
        Denial::KeyChanged
    );
    // And bound to its host and job: neither can be swapped.
    assert_eq!(
        refusal(fx.enroll(&first, ("h2", "job-1", "req-1"), &grant.token)),
        Denial::HostMismatch
    );
    assert_eq!(
        refusal(fx.enroll(&first, ("h1", "job-9", "req-1"), &grant.token)),
        Denial::JobMismatch
    );
}

#[test]
fn every_refusal_looks_the_same_to_the_caller() {
    let fx = Fixture::new();
    let grant = fx.grant("h1", "job-1");
    let spoke = Spoke::new();
    let unknown = boss_host_enrollment::EnrollmentToken::decode(&"A".repeat(43)).expect("a token");
    let errors = [
        fx.enroll(&spoke, ("h1", "job-1", "req-1"), &unknown).unwrap_err(),
        fx.enroll(&spoke, ("h2", "job-1", "req-1"), &grant.token).unwrap_err(),
        fx.enroll(&spoke, ("h1", "job-2", "req-1"), &grant.token).unwrap_err(),
    ];
    let shown: Vec<_> = errors.iter().map(ToString::to_string).collect();
    assert!(shown.iter().all(|text| text == "not permitted"), "{shown:?}");
}

#[test]
fn an_exact_retry_after_a_lost_reply_returns_the_same_certificate() {
    let fx = Fixture::new();
    let grant = fx.grant("h1", "job-1");
    let spoke = Spoke::new();
    let first = fx
        .enroll(&spoke, ("h1", "job-1", "req-1"), &grant.token)
        .expect("enroll");

    // The reply was lost; the spoke rebuilds its request (a new signature over
    // the same key) and tries again.
    fx.clock.advance(10 * MINUTE);
    let retry = fx
        .enroll(&spoke, ("h1", "job-1", "req-1"), &grant.token)
        .expect("retry");
    assert_eq!(retry, first);

    // The retry is bounded: after the window the token is simply spent.
    fx.clock.advance(HOUR);
    assert_eq!(
        refusal(fx.enroll(&spoke, ("h1", "job-1", "req-1"), &grant.token)),
        Denial::RetryWindowElapsed
    );
}

#[test]
fn a_retry_with_a_different_key_is_refused_and_the_first_key_still_recovers() {
    let fx = Fixture::new();
    let grant = fx.grant("h1", "job-1");
    let owner = Spoke::new();
    let first = fx
        .enroll(&owner, ("h1", "job-1", "req-1"), &grant.token)
        .expect("enroll");

    let other_key = Spoke::new();
    assert_eq!(
        refusal(fx.enroll(&other_key, ("h1", "job-1", "req-1"), &grant.token)),
        Denial::KeyChanged
    );
    assert_eq!(
        fx.enroll(&owner, ("h1", "job-1", "req-1"), &grant.token)
            .expect("recover"),
        first
    );
}

#[test]
fn two_requests_racing_for_one_token_yield_one_certificate() {
    let fx = Fixture::new();
    let grant = fx.grant("h1", "job-1");
    let barrier = std::sync::Barrier::new(2);
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let attempts: Vec<_> = (0..2)
            .map(|n| {
                let (fx, grant, barrier) = (&fx, &grant, &barrier);
                scope.spawn(move || {
                    let spoke = Spoke::new();
                    barrier.wait();
                    fx.enroll(&spoke, ("h1", "job-1", "req-1"), &grant.token)
                        .map(|cert| (n, cert.serial))
                })
            })
            .collect();
        attempts
            .into_iter()
            .map(|attempt| attempt.join().expect("thread"))
            .collect()
    });
    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
        1,
        "{outcomes:?}"
    );
    let loser = outcomes.into_iter().find_map(Result::err).expect("one loser");
    assert!(
        matches!(loser, CredentialError::Denied(Denial::KeyChanged)),
        "{loser:?}"
    );
}

#[test]
fn an_expired_token_is_refused_and_a_replacement_invalidates_the_old_one() {
    let fx = Fixture::new();
    let grant = fx.grant("h1", "job-1");
    let spoke = Spoke::new();
    fx.clock.advance(16 * MINUTE);
    assert_eq!(
        refusal(fx.enroll(&spoke, ("h1", "job-1", "req-1"), &grant.token)),
        Denial::TokenExpired
    );

    let replacement = fx
        .credentials
        .issue_enrollment_token("h1", "job-2")
        .expect("replacement");
    let newer = fx.credentials.issue_enrollment_token("h1", "job-2").expect("newer");
    assert_eq!(
        refusal(fx.enroll(&spoke, ("h1", "job-2", "req-1"), &replacement.token)),
        Denial::TokenReplaced
    );
    fx.enroll(&spoke, ("h1", "job-2", "req-1"), &newer.token)
        .expect("the latest token works");
}

#[test]
fn cancelling_a_job_closes_its_retry_window() {
    let fx = Fixture::new();
    let grant = fx.grant("h1", "job-1");
    let spoke = Spoke::new();
    fx.enroll(&spoke, ("h1", "job-1", "req-1"), &grant.token)
        .expect("enroll");
    fx.credentials.cancel_job("h1", "job-1", "test").expect("cancel");
    assert_eq!(
        refusal(fx.enroll(&spoke, ("h1", "job-1", "req-1"), &grant.token)),
        Denial::JobCancelled
    );
}

#[test]
fn malformed_requests_are_rejected_before_they_touch_a_token() {
    let fx = Fixture::new();
    let grant = fx.grant("h1", "job-1");
    let spoke = Spoke::new();
    let bad = |input: EnrollInput<'_>| matches!(fx.credentials.enroll(&input), Err(CredentialError::Invalid(_)));
    let csr = spoke.csr();
    let base = EnrollInput {
        host_id: "h1",
        job_id: "job-1",
        request_id: "req-1",
        token: grant.token.as_bytes(),
        csr_der: &csr,
    };
    assert!(bad(EnrollInput {
        host_id: "bad host",
        ..base
    }));
    assert!(bad(EnrollInput { job_id: "", ..base }));
    assert!(bad(EnrollInput {
        request_id: "has space",
        ..base
    }));
    assert!(bad(EnrollInput {
        token: &[1; 31],
        ..base
    }));
    assert!(bad(EnrollInput {
        csr_der: b"nope",
        ..base
    }));
    // None of that spent the token.
    fx.enroll(&spoke, ("h1", "job-1", "req-1"), &grant.token)
        .expect("token is still good");
}

#[test]
fn enrolling_again_supersedes_the_previous_identity_at_once() {
    let fx = Fixture::new();
    let (_old_spoke, old) = fx.enrolled("h1");
    let old_chain = presented(&old);
    fx.credentials.authorize(&old_chain).expect("old certificate works");
    let mut events = fx.credentials.subscribe();

    // Managed re-provisioning of the same host, e.g. after its identity lapsed.
    let grant = fx.credentials.issue_enrollment_token("h1", "job-2").expect("token");
    let fresh = Spoke::new();
    let new = fx
        .enroll(&fresh, ("h1", "job-2", "req-1"), &grant.token)
        .expect("re-enroll");

    assert_eq!(fx.credentials.authorize(&old_chain).unwrap_err(), Denial::Superseded);
    fx.credentials
        .authorize(&presented(&new))
        .expect("new certificate works");
    match events.try_recv().expect("an event") {
        CredentialEvent::Superseded { host_id, serial, .. } => {
            assert_eq!((host_id, serial), ("h1".to_owned(), old.serial))
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------- expiry

#[test]
fn a_certificate_stops_authorizing_when_it_expires_and_cannot_renew() {
    let fx = Fixture::new();
    let (_spoke, cert) = fx.enrolled("h1");
    let chain = presented(&cert);
    let host = fx.credentials.authorize(&chain).expect("fresh certificate");

    fx.clock.advance(30 * DAY - HOUR);
    fx.credentials
        .authorize(&chain)
        .expect("still valid an hour before expiry");
    fx.clock.advance(2 * HOUR);
    assert_eq!(
        fx.credentials.authorize(&chain).unwrap_err(),
        Denial::CertificateExpired
    );
    assert_eq!(
        fx.credentials.revalidate(&host).unwrap_err(),
        Denial::CertificateExpired
    );
    // An expired identity recovers through provisioning, not renewal.
    assert_eq!(
        refusal(renew(&fx, &host, &Spoke::new(), "r1")),
        Denial::CertificateExpired
    );
}

#[test]
fn a_certificate_from_another_ca_is_not_trusted() {
    let hub = Fixture::new();
    let other = Fixture::new();
    let (_spoke, foreign) = other.enrolled("h1");
    hub.grant("h1", "job-1");
    assert_eq!(
        hub.credentials.authorize(&presented(&foreign)).unwrap_err(),
        Denial::Untrusted
    );
    assert_eq!(hub.credentials.authorize(&[]).unwrap_err(), Denial::NoCertificate);
}

// --------------------------------------------------------------- renewal

#[test]
fn a_renewed_certificate_overlaps_the_one_it_replaces_for_a_day() {
    let fx = Fixture::new();
    let (_spoke, old) = fx.enrolled("h1");
    let caller = fx.credentials.authorize(&presented(&old)).expect("authorized");
    fx.clock.advance(20 * DAY);

    let successor = Spoke::new();
    let new = renew(&fx, &caller, &successor, "renew-1").expect("renew");
    assert_ne!(new.serial, old.serial);
    assert_eq!(new.host_id, "h1");

    let current = fx
        .credentials
        .authorize(&presented(&new))
        .expect("new certificate works at once");
    let overlapping = fx
        .credentials
        .authorize(&presented(&old))
        .expect("old certificate works during the overlap");
    // `valid_until` is what the stream manager closes an open stream by: the
    // end of the overlap for the old certificate, the expiry for the new one.
    let within_a_second = |actual: std::time::SystemTime, expected: Duration| {
        let left = actual.duration_since(fx.now()).expect("ends in the future");
        left <= expected && left + Duration::from_secs(1) > expected
    };
    assert!(within_a_second(overlapping.valid_until, DAY), "{overlapping:?}");
    assert!(within_a_second(current.valid_until, 30 * DAY), "{current:?}");
    let status = fx.credentials.status("h1").expect("status").expect("known");
    assert_eq!(status.current.expect("current").serial, new.serial);
    assert_eq!(status.previous.expect("previous").serial, old.serial);

    fx.clock.advance(DAY - MINUTE);
    fx.credentials
        .authorize(&presented(&old))
        .expect("still inside the overlap");
    fx.clock.advance(2 * MINUTE);
    assert_eq!(
        fx.credentials.authorize(&presented(&old)).unwrap_err(),
        Denial::OverlapElapsed
    );
    fx.credentials
        .authorize(&presented(&new))
        .expect("the replacement is unaffected");
}

/// Deadlines are whole seconds, so allow the sub-second remainder of `now`.
fn assert_one_day_left(fx: &Fixture, host: &boss_host_enrollment::AuthorizedHost) {
    let left = host.valid_until.duration_since(fx.now()).expect("ends in the future");
    assert!(left <= DAY && left + Duration::from_secs(1) > DAY, "{left:?}");
}

#[test]
fn revalidate_reports_the_overlap_deadline_when_a_renewal_landed_after_authorize() {
    let fx = Fixture::new();
    let (_spoke, old) = fx.enrolled("h1");
    let caller = fx.credentials.authorize(&presented(&old)).expect("authorized");

    // The renewal commits after the caller authorized, and before it subscribed.
    fx.clock.advance(MINUTE);
    renew(&fx, &caller, &Spoke::new(), "renew-1").expect("renew");

    let current = fx.credentials.revalidate(&caller).expect("still inside the overlap");
    assert_eq!(current.host_id, caller.host_id);
    assert_eq!(current.serial, caller.serial);
    assert!(
        current.valid_until < caller.valid_until,
        "the deadline moved from the certificate's expiry to the overlap end"
    );
    assert_one_day_left(&fx, &current);
}

#[test]
fn a_missed_superseded_event_is_recovered_by_revalidate() {
    let fx = Fixture::new();
    let (_spoke, old) = fx.enrolled("h1");
    let caller = fx.credentials.authorize(&presented(&old)).expect("authorized");
    let mut events = fx.credentials.subscribe();
    renew(&fx, &caller, &Spoke::new(), "renew-1").expect("renew");
    // The receiver drops the event, as a lagged one would.
    drop(events.try_recv());

    let current = fx
        .credentials
        .revalidate(&caller)
        .expect("old certificate still authorizes");
    assert_one_day_left(&fx, &current);

    fx.clock.advance(DAY + MINUTE);
    assert_eq!(fx.credentials.revalidate(&current).unwrap_err(), Denial::OverlapElapsed);
}

#[test]
fn the_overlap_never_outlives_the_old_certificates_own_expiry() {
    let fx = Fixture::new();
    let (_spoke, old) = fx.enrolled("h1");
    let caller = fx.credentials.authorize(&presented(&old)).expect("authorized");
    fx.clock.advance(30 * DAY - HOUR);
    let mut events = fx.credentials.subscribe();

    renew(&fx, &caller, &Spoke::new(), "renew-1").expect("renew an hour before expiry");
    let status = fx.credentials.status("h1").expect("status").expect("known");
    let previous = status.previous.expect("previous");
    let now = fx.now();
    assert!(previous.valid_until <= now + HOUR + MINUTE, "capped at expiry, not 24h");
    match events.try_recv().expect("an event") {
        CredentialEvent::Superseded {
            serial, valid_until, ..
        } => {
            assert_eq!(serial, old.serial);
            assert_eq!(valid_until, previous.valid_until);
        }
        other => panic!("{other:?}"),
    }
    fx.clock.advance(2 * HOUR);
    assert!(matches!(
        fx.credentials.authorize(&presented(&old)).unwrap_err(),
        Denial::CertificateExpired | Denial::OverlapElapsed
    ));
}

#[test]
fn a_repeated_renewal_returns_the_same_certificate_even_from_the_overlapping_one() {
    let fx = Fixture::new();
    let (_spoke, old) = fx.enrolled("h1");
    let caller = fx.credentials.authorize(&presented(&old)).expect("authorized");
    let successor = Spoke::new();
    let first = renew(&fx, &caller, &successor, "renew-1").expect("renew");

    // The spoke never saw the reply and is still on its old certificate.
    let again = renew(&fx, &caller, &successor, "renew-1").expect("lost-reply retry");
    assert_eq!(again, first);
    // A different key under the same request id is not a retry.
    assert_eq!(
        refusal(renew(&fx, &caller, &Spoke::new(), "renew-1")),
        Denial::RequestReused
    );
}

#[test]
fn renewal_demands_a_new_key_and_the_current_certificate() {
    let fx = Fixture::new();
    let (spoke, old) = fx.enrolled("h1");
    let caller = fx.credentials.authorize(&presented(&old)).expect("authorized");

    assert_eq!(
        refusal(renew(&fx, &caller, &spoke, "renew-1")),
        Denial::KeyReused,
        "the key must change"
    );

    let first_successor = Spoke::new();
    renew(&fx, &caller, &first_successor, "renew-2").expect("renew");
    // `caller` now holds the overlapping certificate, which may retry but not
    // start another renewal.
    assert_eq!(
        refusal(renew(&fx, &caller, &Spoke::new(), "renew-3")),
        Denial::NotCurrentCertificate
    );
}

#[test]
fn a_second_renewal_ends_the_first_overlap_immediately() {
    let fx = Fixture::new();
    let (_spoke, a) = fx.enrolled("h1");
    let caller_a = fx.credentials.authorize(&presented(&a)).expect("a");
    let b = renew(&fx, &caller_a, &Spoke::new(), "renew-1").expect("b");
    let caller_b = fx.credentials.authorize(&presented(&b)).expect("b authorizes");
    let c = renew(&fx, &caller_b, &Spoke::new(), "renew-2").expect("c");

    assert_eq!(
        fx.credentials.authorize(&presented(&a)).unwrap_err(),
        Denial::Superseded
    );
    fx.credentials
        .authorize(&presented(&b))
        .expect("b is now the overlapping certificate");
    fx.credentials.authorize(&presented(&c)).expect("c is current");
}

// ------------------------------------------------------------ revocation

#[test]
fn revocation_closes_reconnect_renewal_and_re_enrollment() {
    let fx = Fixture::new();
    let (_spoke, cert) = fx.enrolled("h1");
    let chain = presented(&cert);
    let host = fx.credentials.authorize(&chain).expect("authorized");
    let spare = fx
        .credentials
        .issue_enrollment_token("h1", "job-2")
        .expect("a second token");
    let mut events = fx.credentials.subscribe();

    fx.credentials.revoke("h1", "operator", "host retired").expect("revoke");

    assert_eq!(
        events.try_recv().expect("an event"),
        CredentialEvent::Revoked {
            host_id: "h1".to_owned()
        }
    );
    assert_eq!(
        fx.credentials.authorize(&chain).unwrap_err(),
        Denial::Revoked,
        "reconnect"
    );
    assert_eq!(
        fx.credentials.revalidate(&host).unwrap_err(),
        Denial::Revoked,
        "open stream"
    );
    assert_eq!(
        refusal(renew(&fx, &host, &Spoke::new(), "renew-1")),
        Denial::Revoked,
        "renewal"
    );
    assert_eq!(
        refusal(fx.enroll(&Spoke::new(), ("h1", "job-2", "req-1"), &spare.token)),
        Denial::Revoked,
        "outstanding token"
    );
    assert_eq!(
        refusal(fx.credentials.issue_enrollment_token("h1", "job-3")),
        Denial::Revoked,
        "automatic retry"
    );

    let trail: Vec<_> = fx
        .credentials
        .audit_trail("h1")
        .expect("trail")
        .into_iter()
        .map(|(event, _)| event)
        .collect();
    assert_eq!(
        trail,
        ["authorized", "token_issued", "enrolled", "token_issued", "revoked"]
    );
}

#[test]
fn only_an_explicit_new_authorization_reopens_a_revoked_host_and_old_certificates_stay_dead() {
    let fx = Fixture::new();
    let (_spoke, cert) = fx.enrolled("h1");
    fx.credentials.revoke("h1", "operator", "rotate").expect("revoke");

    fx.credentials
        .authorize_host("h1", "operator")
        .expect("explicit re-authorization");
    assert_eq!(
        fx.credentials.authorize(&presented(&cert)).unwrap_err(),
        Denial::NotEnrolled
    );
    let grant = fx.credentials.issue_enrollment_token("h1", "job-2").expect("token");
    let fresh = fx
        .enroll(&Spoke::new(), ("h1", "job-2", "req-1"), &grant.token)
        .expect("enroll again");
    assert_eq!(
        fx.credentials.authorize(&presented(&cert)).unwrap_err(),
        Denial::Superseded
    );
    fx.credentials
        .authorize(&presented(&fresh))
        .expect("new identity works");
}

#[test]
fn revoking_a_host_the_hub_never_authorized_is_an_error() {
    let fx = Fixture::new();
    assert!(matches!(
        fx.credentials.revoke("ghost", "operator", "x").err(),
        Some(CredentialError::UnknownHost)
    ));
}

// ------------------------------------------------------------ durability

#[test]
fn a_spent_token_only_recovers_the_certificate_it_produced() {
    let fx = Fixture::new();
    let first = fx.grant("h1", "job-1");
    let spoke_a = Spoke::new();
    let cert_a = fx
        .enroll(&spoke_a, ("h1", "job-1", "req-1"), &first.token)
        .expect("enroll with the first token");

    // A second token for the same host and job, used by another request and key.
    let second = fx.grant("h1", "job-1");
    let spoke_b = Spoke::new();
    let cert_b = fx
        .enroll(&spoke_b, ("h1", "job-1", "req-2"), &second.token)
        .expect("enroll with the second token");
    assert_ne!(cert_a, cert_b);

    // The first token, presented with the second one's identity, must not be
    // handed the second one's certificate.
    assert_eq!(
        refusal(fx.enroll(&spoke_b, ("h1", "job-1", "req-2"), &first.token)),
        Denial::TokenReplayed
    );
    // Each token still answers only its own request, though the first
    // certificate is no longer the current one.
    assert_eq!(
        refusal(fx.enroll(&spoke_a, ("h1", "job-1", "req-1"), &first.token)),
        Denial::Superseded
    );
    assert_eq!(
        fx.enroll(&spoke_b, ("h1", "job-1", "req-2"), &second.token)
            .expect("the second token recovers its own certificate"),
        cert_b
    );
    assert_eq!(
        refusal(fx.enroll(&spoke_a, ("h1", "job-1", "req-1"), &second.token)),
        Denial::TokenReplayed
    );
}

#[test]
fn a_fresh_token_cannot_reuse_a_request_id_that_already_enrolled() {
    let fx = Fixture::new();
    let first = fx.grant("h1", "job-1");
    let spoke = Spoke::new();
    fx.enroll(&spoke, ("h1", "job-1", "req-1"), &first.token)
        .expect("enroll with the first token");

    // The job re-delivers a token; the spoke retries with its old request id.
    let second = fx.grant("h1", "job-1");
    assert_eq!(
        refusal(fx.enroll(&Spoke::new(), ("h1", "job-1", "req-1"), &second.token)),
        Denial::RequestReused
    );
    // The refusal did not spend the token: a new request id still enrolls.
    fx.enroll(&Spoke::new(), ("h1", "job-1", "req-2"), &second.token)
        .expect("a new request id enrolls with the second token");
}

// ------------------------------------------------------- state permissions

fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

fn reopen(fx: &Fixture) -> Result<boss_host_enrollment::HostCredentials, CredentialError> {
    let clock: std::sync::Arc<dyn boss_host_enrollment::Clock> = fx.clock.clone();
    boss_host_enrollment::HostCredentials::open(fx.dir.path(), Policy::default(), clock)
}

#[test]
fn a_permissive_credential_database_is_refused_on_reopen() {
    let fx = Fixture::new();
    fx.enrolled("h1");
    let db = fx.dir.path().join("credentials.db");

    set_mode(&db, 0o660);
    assert!(matches!(reopen(&fx), Err(CredentialError::Internal(_))));
    set_mode(&db, 0o600);
    reopen(&fx).expect("owner-only database is accepted");

    let wal = fx.dir.path().join("credentials.db-wal");
    if wal.exists() {
        set_mode(&wal, 0o666);
        assert!(matches!(reopen(&fx), Err(CredentialError::Internal(_))));
        set_mode(&wal, 0o600);
    }
}

#[test]
fn a_writable_state_directory_is_refused() {
    let fx = Fixture::new();
    fx.enrolled("h1");
    set_mode(fx.dir.path(), 0o770);
    let result = reopen(&fx);
    set_mode(fx.dir.path(), 0o700);
    assert!(matches!(result, Err(CredentialError::Internal(_))));
    reopen(&fx).expect("owner-only directory is accepted");
}

#[test]
fn credentials_and_spent_tokens_survive_a_hub_restart() {
    let mut fx = Fixture::new();
    let grant = fx.grant("h1", "job-1");
    let spoke = Spoke::new();
    let cert = fx
        .enroll(&spoke, ("h1", "job-1", "req-1"), &grant.token)
        .expect("enroll");

    fx.restart();

    fx.credentials
        .authorize(&presented(&cert))
        .expect("the same CA still trusts the certificate");
    assert_eq!(
        fx.enroll(&spoke, ("h1", "job-1", "req-1"), &grant.token)
            .expect("retry works across a restart"),
        cert
    );
    assert_eq!(
        refusal(fx.enroll(&Spoke::new(), ("h1", "job-1", "req-2"), &grant.token)),
        Denial::TokenReplayed
    );
}

#[test]
fn the_default_policy_is_the_designs() {
    let policy = Policy::default();
    assert_eq!(policy.leaf_lifetime, 30 * DAY);
    assert_eq!(policy.token_lifetime, 15 * MINUTE);
    assert_eq!(policy.renewal_overlap, DAY);
}
