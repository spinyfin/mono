//! Unit tests for the Codex hook-trust gate.
//!
//! Hash goldens were captured against codex-cli 0.145.0 via `hooks/list`
//! (see the sibling investigation doc). Observer is injected so refuse paths
//! do not require a live Codex binary.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::*;

// ── Hash goldens (codex-cli 0.145.0) ────────────────────────────────────────

#[test]
fn command_hook_hash_is_stable_and_path_sensitive() {
    let a = command_hook_trusted_hash(HookEvent::PreToolUse, "/private/tmp/guard-a.sh", Some(".*"), 600, false);
    let b = command_hook_trusted_hash(HookEvent::PreToolUse, "/private/tmp/guard-b.sh", Some(".*"), 600, false);
    let a2 = command_hook_trusted_hash(HookEvent::PreToolUse, "/private/tmp/guard-a.sh", Some(".*"), 600, false);
    assert_eq!(a, a2, "hash must be deterministic");
    assert_ne!(a, b, "different command paths must produce different hashes");
    assert!(
        a.starts_with("sha256:") && a.len() == "sha256:".len() + 64,
        "hash must be sha256: + 64 hex chars, got {a}"
    );

    // Timeout and async are part of the identity.
    let different_timeout =
        command_hook_trusted_hash(HookEvent::PreToolUse, "/private/tmp/guard-a.sh", Some(".*"), 30, false);
    assert_ne!(a, different_timeout);
    let async_true = command_hook_trusted_hash(HookEvent::PreToolUse, "/private/tmp/guard-a.sh", Some(".*"), 600, true);
    assert_ne!(a, async_true);
}

#[test]
fn command_hook_hash_matches_known_live_capture() {
    // Captured live with codex app-server hooks/list on 2026-07-26 against
    // codex-cli 0.145.0. Command path and hash are from that run's resolved
    // realpath — re-check algorithm fidelity without re-running Codex.
    //
    // Identity JSON (canonical):
    // {"event_name":"session_start","hooks":[{"async":false,"command":"/private/tmp/boss-hook-trust-golden/h.sh","timeout":600,"type":"command"}]}
    let dir = PathBuf::from("/private/tmp/boss-hook-trust-golden");
    let cmd = dir.join("h.sh");
    let hash = command_hook_trusted_hash(HookEvent::SessionStart, cmd.to_str().unwrap(), None, 600, false);
    // Independent recomputation of the same identity.
    let mut handler = serde_json::Map::new();
    handler.insert("async".into(), serde_json::json!(false));
    handler.insert("command".into(), serde_json::json!(cmd.to_str().unwrap()));
    handler.insert("timeout".into(), serde_json::json!(600));
    handler.insert("type".into(), serde_json::json!("command"));
    let mut identity = serde_json::Map::new();
    identity.insert("event_name".into(), serde_json::json!("session_start"));
    identity.insert(
        "hooks".into(),
        serde_json::Value::Array(vec![serde_json::Value::Object(handler)]),
    );
    let independent = version_for_json(&serde_json::Value::Object(identity));
    assert_eq!(hash, independent);

    // Matcher presence changes the hash.
    let with_matcher = command_hook_trusted_hash(HookEvent::PreToolUse, cmd.to_str().unwrap(), Some(".*"), 600, false);
    let without = command_hook_trusted_hash(HookEvent::PreToolUse, cmd.to_str().unwrap(), None, 600, false);
    assert_ne!(with_matcher, without);
}

#[test]
fn hook_state_key_format() {
    let key = hook_state_key(Path::new("/private/tmp/home/config.toml"), HookEvent::PreToolUse, 0, 0);
    assert_eq!(key, "/private/tmp/home/config.toml:pre_tool_use:0:0");
}

// ── Fake observer ───────────────────────────────────────────────────────────

struct FakeObserver {
    /// key → (trust_status, current_hash)
    responses: Mutex<BTreeMap<String, (String, String)>>,
    fail: Mutex<Option<String>>,
}

impl FakeObserver {
    fn new() -> Self {
        Self {
            responses: Mutex::new(BTreeMap::new()),
            fail: Mutex::new(None),
        }
    }

    fn set(&self, key: &str, status: &str, hash: &str) {
        self.responses
            .lock()
            .unwrap()
            .insert(key.to_string(), (status.to_string(), hash.to_string()));
    }

    fn fail_with(&self, msg: &str) {
        *self.fail.lock().unwrap() = Some(msg.to_string());
    }
}

impl TrustObserver for FakeObserver {
    fn observe_hooks(&self, _codex_home: &Path, _cwd: &Path) -> Result<Vec<ObservedHook>, TrustGateError> {
        if let Some(msg) = self.fail.lock().unwrap().clone() {
            return Err(TrustGateError::ObservationFailed { detail: msg });
        }
        let map = self.responses.lock().unwrap();
        if map.is_empty() {
            return Err(TrustGateError::ObservationFailed {
                detail: "fake observer has no entries — silence is not success".into(),
            });
        }
        Ok(map
            .iter()
            .map(|(key, (status, hash))| ObservedHook {
                key: key.clone(),
                trust_status: status.clone(),
                current_hash: hash.clone(),
                enabled: true,
            })
            .collect())
    }
}

// ── Fixture helpers ─────────────────────────────────────────────────────────

struct Fixture {
    _tmp: tempfile::TempDir,
    codex_home: PathBuf,
    config_path: PathBuf,
    cwd: PathBuf,
    guard: PathBuf,
}

fn setup_fixture(guard_body: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let codex_home = tmp.path().join("home");
    let cwd = tmp.path().join("repo");
    fs::create_dir_all(&codex_home).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    // Minimal git repo so a live observer would be happy; fake observer ignores cwd.
    let _ = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&cwd)
        .status();

    let guard = codex_home.join("guard.sh");
    fs::write(&guard, guard_body).unwrap();
    let mut perms = fs::metadata(&guard).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&guard, perms).unwrap();

    let guard = resolve_absolute(&guard);
    // Write config first so resolve_absolute can canonicalize the path to the
    // same realpath form Codex uses in hooks.state keys (/private/... on macOS).
    let config_path_pre = codex_home.join("config.toml");
    let config = format!(
        "[[hooks.SessionStart]]\n\
         [[hooks.SessionStart.hooks]]\n\
         type = \"command\"\n\
         command = \"{guard}\"\n\
         \n\
         [[hooks.PreToolUse]]\n\
         matcher = \".*\"\n\
         [[hooks.PreToolUse.hooks]]\n\
         type = \"command\"\n\
         command = \"{guard}\"\n",
        guard = guard.display()
    );
    fs::write(&config_path_pre, config).unwrap();

    let config_path = resolve_absolute(&config_path_pre);
    let codex_home = resolve_absolute(&codex_home);
    let cwd = resolve_absolute(&cwd);

    Fixture {
        _tmp: tmp,
        codex_home,
        config_path,
        cwd,
        guard,
    }
}

fn standard_hooks(fx: &Fixture) -> Vec<CommandHookSpec> {
    vec![
        CommandHookSpec {
            event: HookEvent::SessionStart,
            matcher: None,
            command: fx.guard.clone(),
            timeout_sec: None,
            async_hook: false,
            group_index: 0,
            handler_index: 0,
            require_guard_executable: true,
        },
        CommandHookSpec {
            event: HookEvent::PreToolUse,
            matcher: Some(".*".into()),
            command: fx.guard.clone(),
            timeout_sec: None,
            async_hook: false,
            group_index: 0,
            handler_index: 0,
            require_guard_executable: true,
        },
    ]
}

fn expected_hash(fx: &Fixture, event: HookEvent, matcher: Option<&str>) -> String {
    command_hook_trusted_hash(
        event,
        fx.guard.to_str().unwrap(),
        matcher,
        event.default_timeout_sec(),
        false,
    )
}

// ── Gate tests ──────────────────────────────────────────────────────────────

#[test]
fn arm_refuses_when_no_hooks_configured() {
    let fx = setup_fixture("#!/bin/sh\n");
    let req = ArmRequest {
        codex_home: fx.codex_home.clone(),
        config_path: fx.config_path.clone(),
        cwd: fx.cwd.clone(),
        hooks: vec![],
        codex_bin: PathBuf::from("codex"),
    };
    let err = arm_and_attest_with_observer(&req, &FakeObserver::new()).unwrap_err();
    assert!(matches!(err, TrustGateError::NoHooksConfigured));
}

#[test]
fn arm_refuses_when_guard_executable_missing() {
    let fx = setup_fixture("#!/bin/sh\n");
    let missing = fx.codex_home.join("nope.sh");
    let hooks = vec![CommandHookSpec {
        event: HookEvent::PreToolUse,
        matcher: Some(".*".into()),
        command: missing,
        timeout_sec: None,
        async_hook: false,
        group_index: 0,
        handler_index: 0,
        require_guard_executable: true,
    }];
    let req = ArmRequest {
        codex_home: fx.codex_home.clone(),
        config_path: fx.config_path.clone(),
        cwd: fx.cwd.clone(),
        hooks,
        codex_bin: PathBuf::from("codex"),
    };
    let err = arm_and_attest_with_observer(&req, &FakeObserver::new()).unwrap_err();
    assert!(matches!(err, TrustGateError::GuardExecutableMissing { .. }));
}

#[test]
fn arm_refuses_when_guard_not_executable() {
    let fx = setup_fixture("#!/bin/sh\n");
    // Drop execute bits.
    let mut perms = fs::metadata(&fx.guard).unwrap().permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&fx.guard, perms).unwrap();

    let req = ArmRequest {
        codex_home: fx.codex_home.clone(),
        config_path: fx.config_path.clone(),
        cwd: fx.cwd.clone(),
        hooks: standard_hooks(&fx),
        codex_bin: PathBuf::from("codex"),
    };
    let err = arm_and_attest_with_observer(&req, &FakeObserver::new()).unwrap_err();
    assert!(matches!(err, TrustGateError::GuardExecutableNotExecutable { .. }));
}

#[test]
fn arm_refuses_when_observation_is_silent() {
    let fx = setup_fixture("#!/bin/sh\necho guard\n");
    let observer = FakeObserver::new();
    observer.fail_with("simulated RPC hang");

    let req = ArmRequest {
        codex_home: fx.codex_home.clone(),
        config_path: fx.config_path.clone(),
        cwd: fx.cwd.clone(),
        hooks: standard_hooks(&fx),
        codex_bin: PathBuf::from("codex"),
    };
    let err = arm_and_attest_with_observer(&req, &observer).unwrap_err();
    match err {
        TrustGateError::ObservationFailed { detail } => {
            assert!(detail.contains("simulated RPC hang"));
        }
        other => panic!("expected ObservationFailed, got {other}"),
    }
}

#[test]
fn arm_refuses_when_hook_listed_but_untrusted() {
    let fx = setup_fixture("#!/bin/sh\necho guard\n");
    let hooks = standard_hooks(&fx);
    let observer = FakeObserver::new();
    for hook in &hooks {
        let key = hook_state_key(&fx.config_path, hook.event, hook.group_index, hook.handler_index);
        let hash = expected_hash(&fx, hook.event, hook.matcher.as_deref());
        // Stale/wrong: report untrusted even though hash matches.
        observer.set(&key, "untrusted", &hash);
    }

    let req = ArmRequest {
        codex_home: fx.codex_home.clone(),
        config_path: fx.config_path.clone(),
        cwd: fx.cwd.clone(),
        hooks,
        codex_bin: PathBuf::from("codex"),
    };
    let err = arm_and_attest_with_observer(&req, &observer).unwrap_err();
    assert!(matches!(err, TrustGateError::HookNotTrusted { .. }));
}

#[test]
fn arm_refuses_when_observed_hash_mismatches_stamp() {
    let fx = setup_fixture("#!/bin/sh\necho guard\n");
    let hooks = standard_hooks(&fx);
    let observer = FakeObserver::new();
    for hook in &hooks {
        let key = hook_state_key(&fx.config_path, hook.event, hook.group_index, hook.handler_index);
        observer.set(&key, "trusted", "sha256:deadbeef");
    }

    let req = ArmRequest {
        codex_home: fx.codex_home.clone(),
        config_path: fx.config_path.clone(),
        cwd: fx.cwd.clone(),
        hooks,
        codex_bin: PathBuf::from("codex"),
    };
    let err = arm_and_attest_with_observer(&req, &observer).unwrap_err();
    assert!(matches!(err, TrustGateError::HashMismatch { .. }));
}

#[test]
fn arm_succeeds_when_observer_reports_trusted_matching_hashes() {
    let fx = setup_fixture("#!/bin/sh\necho guard\n");
    let hooks = standard_hooks(&fx);
    let observer = FakeObserver::new();
    for hook in &hooks {
        let key = hook_state_key(&fx.config_path, hook.event, hook.group_index, hook.handler_index);
        let hash = expected_hash(&fx, hook.event, hook.matcher.as_deref());
        observer.set(&key, "trusted", &hash);
    }

    let req = ArmRequest {
        codex_home: fx.codex_home.clone(),
        config_path: fx.config_path.clone(),
        cwd: fx.cwd.clone(),
        hooks: hooks.clone(),
        codex_bin: PathBuf::from("codex"),
    };
    let att = arm_and_attest_with_observer(&req, &observer).expect("arm should succeed");
    assert_eq!(att.hooks.len(), 2);
    for entry in &att.hooks {
        assert_eq!(entry.observed_trust_status, "trusted");
        assert!(entry.guard_content_sha256.is_some());
        assert!(entry.trusted_hash.starts_with("sha256:"));
    }

    // Config must carry stamped hashes.
    let config = fs::read_to_string(&fx.config_path).unwrap();
    assert!(config.contains("[hooks.state."));
    assert!(config.contains("trusted_hash"));

    // Re-verify against disk succeeds.
    verify_attestation(&att, &hooks).expect("fresh attestation must verify");

    // Content change makes attestation stale.
    fs::write(&fx.guard, "#!/bin/sh\necho mutated\n").unwrap();
    let mut perms = fs::metadata(&fx.guard).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fx.guard, perms).unwrap();
    let err = verify_attestation(&att, &hooks).unwrap_err();
    assert!(matches!(err, TrustGateError::AttestationStale { .. }));
}

#[test]
fn stamp_hook_trust_writes_state_keys() {
    let fx = setup_fixture("#!/bin/sh\n");
    let hooks = standard_hooks(&fx);
    let stamped = stamp_hook_trust(&fx.config_path, &hooks).unwrap();
    assert_eq!(stamped.len(), 2);

    let config = fs::read_to_string(&fx.config_path).unwrap();
    for (key, hash) in &stamped {
        assert!(config.contains(key), "config should contain state key {key}");
        assert!(config.contains(hash), "config should contain trusted_hash {hash}");
    }
}

#[test]
fn parse_hooks_list_response_rejects_empty_data() {
    let resp = serde_json::json!({
        "id": 2,
        "result": { "data": [] }
    });
    let err = parse_hooks_list_response(&resp).unwrap_err();
    assert!(matches!(err, TrustGateError::ObservationFailed { .. }));
}

#[test]
fn parse_hooks_list_response_extracts_entries() {
    let resp = serde_json::json!({
        "id": 2,
        "result": {
            "data": [{
                "cwd": "/tmp/repo",
                "hooks": [{
                    "key": "/tmp/config.toml:pre_tool_use:0:0",
                    "trustStatus": "trusted",
                    "currentHash": "sha256:abc",
                    "enabled": true
                }]
            }]
        }
    });
    let hooks = parse_hooks_list_response(&resp).unwrap();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].trust_status, "trusted");
    assert_eq!(hooks[0].current_hash, "sha256:abc");
}

#[test]
fn write_attestation_round_trips() {
    let fx = setup_fixture("#!/bin/sh\necho g\n");
    let hooks = standard_hooks(&fx);
    let observer = FakeObserver::new();
    for hook in &hooks {
        let key = hook_state_key(&fx.config_path, hook.event, hook.group_index, hook.handler_index);
        observer.set(
            &key,
            "trusted",
            &expected_hash(&fx, hook.event, hook.matcher.as_deref()),
        );
    }
    let req = ArmRequest {
        codex_home: fx.codex_home.clone(),
        config_path: fx.config_path.clone(),
        cwd: fx.cwd.clone(),
        hooks,
        codex_bin: PathBuf::from("codex"),
    };
    let att = arm_and_attest_with_observer(&req, &observer).unwrap();
    let path = fx.codex_home.join("hook-trust-attestation.json");
    write_attestation_file(&path, &att).unwrap();
    let loaded: HookTrustAttestation = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(loaded.hooks.len(), att.hooks.len());
    assert_eq!(loaded.config_path, att.config_path);
}

/// Live integration: when `codex` is on PATH, the real observer must report
/// `trusted` after stamping. Skipped cleanly when codex is unavailable so CI
/// without the binary stays green.
#[test]
fn live_codex_observer_reports_trusted_after_stamp() {
    let codex = which_codex();
    let Some(codex_bin) = codex else {
        eprintln!("skipping live codex observer test: codex not on PATH");
        return;
    };

    let fx = setup_fixture("#!/bin/sh\necho hooked >>\"$(dirname \"$0\")/marker\"\ncat >/dev/null\n");
    let hooks = standard_hooks(&fx);
    let req = ArmRequest {
        codex_home: fx.codex_home.clone(),
        config_path: fx.config_path.clone(),
        cwd: fx.cwd.clone(),
        hooks: hooks.clone(),
        codex_bin: codex_bin.clone(),
    };
    let att = arm_and_attest(&req).expect("live arm_and_attest must succeed with codex on PATH");
    assert_eq!(att.hooks.len(), 2);
    for entry in &att.hooks {
        assert_eq!(entry.observed_trust_status.to_lowercase(), "trusted");
    }
    verify_attestation(&att, &hooks).unwrap();
}

fn which_codex() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("codex");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
