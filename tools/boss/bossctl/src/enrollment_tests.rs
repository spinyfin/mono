use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use boss_host_enrollment::{
    Denial, EnrollInput, EnrollmentToken, HostAuthorizer as _, HostCredentials, MANUAL_JOB_ID, Policy, SystemClock,
};
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::pki_types::CertificateDer;

use super::*;

fn hub() -> (tempfile::TempDir, HostCredentials) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let credentials = HostCredentials::open(dir.path(), Policy::default(), Arc::new(SystemClock)).expect("open");
    (dir, credentials)
}

fn text(out: Vec<u8>) -> String {
    String::from_utf8(out).expect("utf8")
}

/// The token printed on the line after the heading.
fn printed_token(output: &str) -> EnrollmentToken {
    let line = output.lines().nth(1).expect("a token line");
    EnrollmentToken::decode(line).expect("a decodable token")
}

fn enroll(credentials: &HostCredentials, host: &str, token: &EnrollmentToken) -> Vec<CertificateDer<'static>> {
    static REQUESTS: AtomicU32 = AtomicU32::new(0);
    let request_id = format!("request-{}", REQUESTS.fetch_add(1, Ordering::Relaxed));
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key");
    let csr = CertificateParams::default().serialize_request(&key).expect("csr");
    let issued = credentials
        .enroll(&EnrollInput {
            host_id: host,
            job_id: MANUAL_JOB_ID,
            request_id: &request_id,
            token: token.as_bytes(),
            csr_der: csr.der(),
        })
        .expect("enroll");
    vec![CertificateDer::from(issued.leaf_der)]
}

#[test]
fn a_minted_token_is_shown_once_and_enrolls_the_named_host() {
    let (_dir, credentials) = hub();
    let mut out = Vec::new();
    token(&credentials, "edge-1", None, false, false, &mut out).expect("mint");
    let output = text(out);
    assert!(output.contains("single use"), "{output}");
    assert!(output.contains("not stored"), "{output}");

    enroll(&credentials, "edge-1", &printed_token(&output));
    let mut listing = Vec::new();
    list(&credentials, false, &mut listing).expect("list");
    assert!(
        text(listing).contains("edge-1  enrolled"),
        "enrolled after the token is used"
    );
}

#[test]
fn a_token_file_is_owner_only_and_the_token_is_not_printed() {
    let (dir, credentials) = hub();
    let path = dir.path().join("edge-1.token");
    let mut out = Vec::new();
    token(&credentials, "edge-1", Some(&path), false, false, &mut out).expect("mint");
    let output = text(out);
    let stored = std::fs::read_to_string(&path).expect("token file");
    assert!(!output.contains(stored.trim()), "the secret stays out of the terminal");
    assert_eq!(
        std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
        0o600
    );
    assert!(EnrollmentToken::decode(&stored).is_some());

    let again = token(&credentials, "edge-1", Some(&path), false, false, &mut Vec::new());
    assert!(again.is_err(), "an existing file is never overwritten");
    assert_eq!(
        std::fs::read_to_string(&path).expect("token file"),
        stored,
        "the refused command leaves the first token in place"
    );
    // The refused command did not mint, so the first token still enrolls.
    enroll(
        &credentials,
        "edge-1",
        &EnrollmentToken::decode(&stored).expect("decodable"),
    );
}

#[test]
fn a_failed_mint_leaves_no_token_file_behind() {
    let (dir, credentials) = hub();
    token(&credentials, "edge-1", None, false, false, &mut Vec::new()).expect("mint");
    revoke(&credentials, "edge-1", "retired", false, &mut Vec::new()).expect("revoke");
    let path = dir.path().join("edge-1.token");
    token(&credentials, "edge-1", Some(&path), false, false, &mut Vec::new()).expect_err("revoked host");
    assert!(!path.exists());
}

#[test]
fn json_output_carries_the_token_or_the_file_but_not_both() {
    let (dir, credentials) = hub();
    let mut out = Vec::new();
    token(&credentials, "edge-1", None, false, true, &mut out).expect("mint");
    let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
    assert!(EnrollmentToken::decode(value["token"].as_str().expect("token")).is_some());
    assert!(value.get("token_file").is_none());

    let path = dir.path().join("t");
    let mut out = Vec::new();
    token(&credentials, "edge-2", Some(&path), false, true, &mut out).expect("mint");
    let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
    assert!(value.get("token").is_none());
    assert_eq!(value["token_file"], path.display().to_string());
}

#[test]
fn minting_again_invalidates_the_previous_token() {
    let (_dir, credentials) = hub();
    let (mut first, mut second) = (Vec::new(), Vec::new());
    token(&credentials, "edge-1", None, false, false, &mut first).expect("mint");
    token(&credentials, "edge-1", None, false, false, &mut second).expect("mint");
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key");
    let csr = CertificateParams::default().serialize_request(&key).expect("csr");
    let stale = printed_token(&text(first));
    let refused = credentials.enroll(&EnrollInput {
        host_id: "edge-1",
        job_id: MANUAL_JOB_ID,
        request_id: "request-1",
        token: stale.as_bytes(),
        csr_der: csr.der(),
    });
    assert!(refused.is_err());
    enroll(&credentials, "edge-1", &printed_token(&text(second)));
}

#[test]
fn revoking_refuses_the_hosts_reconnect_and_a_new_token_needs_reauthorization() {
    let (dir, credentials) = hub();
    let mut out = Vec::new();
    token(&credentials, "edge-1", None, false, false, &mut out).expect("mint");
    let chain = enroll(&credentials, "edge-1", &printed_token(&text(out)));
    credentials.authorize(&chain).expect("connected host is admitted");

    // The operator command is another process with its own handle on the state.
    let operator = HostCredentials::open(dir.path(), Policy::default(), Arc::new(SystemClock)).expect("second handle");
    let mut out = Vec::new();
    revoke(&operator, "edge-1", "retired", false, &mut out).expect("revoke");
    assert!(
        text(out).contains("not stopped"),
        "the output says running workers are left alone"
    );

    assert_eq!(
        credentials.authorize(&chain).unwrap_err(),
        Denial::Revoked,
        "reconnect with the old certificate is refused"
    );
    let refused = token(&credentials, "edge-1", None, false, false, &mut Vec::new()).unwrap_err();
    assert!(refused.to_string().contains("--reauthorize"), "{refused}");
    assert_eq!(
        credentials.authorize(&chain).unwrap_err(),
        Denial::Revoked,
        "still refused"
    );

    let mut out = Vec::new();
    token(&credentials, "edge-1", None, true, false, &mut out).expect("explicit reauthorization");
    let fresh = enroll(&credentials, "edge-1", &printed_token(&text(out)));
    credentials.authorize(&fresh).expect("the new identity works");
    assert_eq!(credentials.authorize(&chain).unwrap_err(), Denial::Superseded);
}

#[test]
fn revoking_an_unknown_host_says_so() {
    let (_dir, credentials) = hub();
    let error = revoke(&credentials, "ghost", "x", false, &mut Vec::new()).unwrap_err();
    assert!(error.to_string().contains("never authorized"), "{error}");
}

#[test]
fn the_list_has_a_json_form_and_an_empty_message() {
    let (_dir, credentials) = hub();
    let mut out = Vec::new();
    list(&credentials, false, &mut out).expect("list");
    assert_eq!(text(out), "no hosts authorized\n");

    token(&credentials, "edge-1", None, false, false, &mut Vec::new()).expect("mint");
    let mut out = Vec::new();
    list(&credentials, true, &mut out).expect("list");
    let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
    assert_eq!(value["hosts"][0]["host_id"], "edge-1");
    assert_eq!(value["hosts"][0]["state"], "pending");
    assert!(value["hosts"][0]["token_expires_at"].is_u64());
}

#[test]
fn the_operator_command_will_not_create_a_hub() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let error = open(Some(dir.path().to_path_buf())).err().expect("refused");
    assert!(format!("{error:#}").contains("has not initialized"), "{error:#}");
    assert!(!dir.path().join("ca").exists());
}

#[test]
fn a_revoked_host_is_not_listed_with_a_valid_certificate() {
    let (_dir, credentials) = hub();
    let mut out = Vec::new();
    token(&credentials, "edge-1", None, false, false, &mut out).expect("mint");
    enroll(&credentials, "edge-1", &printed_token(&text(out)));
    revoke(&credentials, "edge-1", "retired", false, &mut Vec::new()).expect("revoke");

    let mut listing = Vec::new();
    list(&credentials, false, &mut listing).expect("list");
    let listing = text(listing);
    assert!(listing.contains("edge-1  revoked  certificate revoked"), "{listing}");
    assert!(!listing.contains("valid for"), "{listing}");

    let mut json = Vec::new();
    list(&credentials, true, &mut json).expect("list");
    let value: serde_json::Value = serde_json::from_slice(&json).expect("json");
    assert!(value["hosts"][0]["certificate_valid_until"].is_null());
}

/// Every `bossctl enrollment <subcommand> ...` the runbook shows, with the
/// `--flags` on it, checked against the real clap definitions.
#[test]
fn the_runbooks_bossctl_commands_and_flags_exist() {
    use clap::CommandFactory as _;

    let runbook = std::fs::read_to_string(std::env::var("RUNBOOK").expect("RUNBOOK")).expect("runbook readable");
    let cli = crate::Cli::command();
    let enrollment = cli
        .find_subcommand("enrollment")
        .expect("bossctl has an enrollment command");
    let mut shown = std::collections::BTreeSet::new();
    let mut flags = std::collections::BTreeSet::new();
    let mut snippets = Vec::new();
    for line in runbook.lines() {
        if let Some((_, rest)) = line.split_once("bossctl enrollment ") {
            snippets.push(rest.split('`').next().unwrap_or(rest).to_owned());
        }
    }
    for (index, span) in runbook.split('`').enumerate() {
        if index % 2 == 1
            && let Some((_, rest)) = span.split_once("bossctl enrollment ")
        {
            snippets.push(rest.to_owned());
        }
    }
    for command in snippets {
        let mut words = command.split_whitespace();
        let name = words.next().expect("a subcommand");
        let sub = enrollment
            .find_subcommand(name)
            .unwrap_or_else(|| panic!("the runbook runs unknown `bossctl enrollment {name}`"));
        shown.insert(name.to_owned());
        for flag in words.filter_map(|word| word.strip_prefix("--")) {
            assert!(
                sub.get_arguments().any(|arg| arg.get_long() == Some(flag)),
                "`bossctl enrollment {name}` has no --{flag}"
            );
            flags.insert(flag.to_owned());
        }
    }
    for want in ["token", "list", "revoke"] {
        assert!(
            shown.contains(want),
            "the runbook never shows `bossctl enrollment {want}`"
        );
    }
    assert!(
        flags.contains("credentials-dir"),
        "the runbook never shows --credentials-dir; collected {flags:?}"
    );
}
