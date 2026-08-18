//! Version pinning for the unversioned Codex `--json` stream.
//!
//! Concrete regressions observed across codex-cli 0.137.0 → 0.145.0 (see the
//! Codex driver design doc and the progress-channel investigation):
//!
//! - removed CLI flag (`-a` / `--ask-for-approval`)
//! - added `usage` counter (`cache_write_input_tokens`)
//! - item IDs changing form / base
//! - `error` items gaining a second meaning (operational warnings)
//! - four new `TurnItem` variants
//! - one new hook event
//!
//! Tolerance: additive fields and unknown enum variants must not crash the
//! reader. Fail loudly on removals and on semantic changes to existing fields.
//!
//! ## Live CLI check
//!
//! [`installed_codex_matches_pinned_version_when_present`] is a soft skip when
//! `codex` is not on PATH (CI images without the CLI). Set
//! `BOSS_REQUIRE_CODEX_CLI=1` to require the binary and fail instead of
//! skipping — use this on hosts that claim the live pin (dev machines that
//! re-capture fixtures).

use std::process::Command;

use crate::conformance::fixtures::{
    CODEX_STDOUT_SESSION_JSONL, PINNED_CODEX_CLI_VERSION, PINNED_CODEX_ITEM_ID_BASE, assert_codex_spawn_contract,
    codex_shaped_driver, decode_jsonl,
};
use crate::conformance::{require_codex_cli, require_grok_cli, which};
use crate::driver::{AgentDriver, GrokDriver};

/// Parse `codex-cli X.Y.Z` (or `codex X.Y.Z`) stdout from `codex --version`.
fn parse_codex_version(stdout: &str) -> Option<String> {
    let line = stdout.lines().next()?.trim();
    // Observed forms: "codex-cli 0.145.0", "codex 0.145.0".
    let version = line.split_whitespace().last()?;
    if version.split('.').count() >= 2 {
        Some(version.to_owned())
    } else {
        None
    }
}

#[test]
fn pinned_version_constant_is_semver_shaped() {
    let parts: Vec<_> = PINNED_CODEX_CLI_VERSION.split('.').collect();
    assert!(
        parts.len() >= 3 && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())),
        "PINNED_CODEX_CLI_VERSION must be X.Y.Z, got {PINNED_CODEX_CLI_VERSION}",
    );
}

#[test]
fn installed_codex_matches_pinned_version_when_present() {
    // Soft skip when `codex` is absent (CI images without the CLI); fixture-side
    // pins below still defend the stream contract. Set BOSS_REQUIRE_CODEX_CLI=1
    // to require the binary and fail instead of skipping.
    let require = require_codex_cli();
    let output = match Command::new("codex").arg("--version").output() {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            if require {
                panic!(
                    "BOSS_REQUIRE_CODEX_CLI is set but `codex --version` failed (status {})",
                    o.status
                );
            }
            eprintln!(
                "codex --version failed (status {}); skipping live version check \
                 (set BOSS_REQUIRE_CODEX_CLI=1 to require it)",
                o.status
            );
            return;
        }
        Err(err) => {
            if require {
                panic!("BOSS_REQUIRE_CODEX_CLI is set but codex is not on PATH: {err}");
            }
            eprintln!(
                "codex not on PATH ({err}); skipping live version check \
                 (set BOSS_REQUIRE_CODEX_CLI=1 to require it)"
            );
            return;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let version = parse_codex_version(&stdout)
        .or_else(|| parse_codex_version(&stderr))
        .unwrap_or_else(|| panic!("could not parse codex --version from stdout={stdout:?} stderr={stderr:?}"));
    assert_eq!(
        version, PINNED_CODEX_CLI_VERSION,
        "installed codex-cli is {version}, but the conformance harness is pinned to \
         {PINNED_CODEX_CLI_VERSION}. Re-run the investigation, update fixtures, and \
         bump PINNED_CODEX_CLI_VERSION deliberately — do not silently absorb drift.",
    );
}

#[test]
fn fixture_stream_declares_pinned_usage_fields() {
    // `cache_write_input_tokens` was added between 0.137.0 and 0.145.0. Its
    // presence in the fixture is the pin; a future CLI that renames or removes
    // it must break this assertion when fixtures are re-captured.
    let turn_completed = CODEX_STDOUT_SESSION_JSONL
        .lines()
        .find(|l| l.contains("turn.completed"))
        .expect("fixture must include turn.completed");
    let value: serde_json::Value = serde_json::from_str(turn_completed).unwrap();
    let usage = value.get("usage").expect("turn.completed must carry usage");
    for field in [
        "input_tokens",
        "cached_input_tokens",
        "cache_write_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
    ] {
        assert!(
            usage.get(field).is_some(),
            "pinned usage must include `{field}`; got {usage}",
        );
    }
}

#[test]
fn fixture_item_ids_use_pinned_prefix_form() {
    // Fail loudly if item ids stop using the `item_<n>` form the pin documents.
    let mut saw_item_id = false;
    for line in CODEX_STDOUT_SESSION_JSONL.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(id) = value.pointer("/item/id").and_then(|v| v.as_str()) {
            saw_item_id = true;
            assert!(
                id.starts_with(PINNED_CODEX_ITEM_ID_BASE),
                "item id {id:?} must use the pinned prefix {PINNED_CODEX_ITEM_ID_BASE:?} \
                 (an id-base change is a semantic break, not additive drift)",
            );
        }
    }
    assert!(saw_item_id, "fixture must carry at least one item id");
}

#[test]
fn fixture_carries_required_envelope_types_for_pinned_version() {
    // Removals of these types would break the Codex progress channel entirely.
    let mut types = std::collections::HashSet::new();
    for line in CODEX_STDOUT_SESSION_JSONL.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(t) = value.get("type").and_then(|v| v.as_str()) {
            types.insert(t.to_owned());
        }
    }
    for required in [
        "thread.started",
        "turn.started",
        "item.started",
        "item.completed",
        "turn.completed",
    ] {
        assert!(
            types.contains(required),
            "pinned stream must include envelope type `{required}`; got {types:?}",
        );
    }
}

#[test]
fn codex_spawn_satisfies_shared_flag_contract() {
    // Pin against the shared spawn contract, not only the test double's
    // hardcoded command string.
    let plan = codex_shaped_driver().spawn_invocation(crate::driver::SpawnRequest {
        model: "gpt-5.5",
        effort: None,
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: None,
        run_id: None,
    });
    assert_codex_spawn_contract(&plan);

    // Production CodexDriver must satisfy the same required/forbidden flags.
    let production = crate::driver::CodexDriver::default().spawn_invocation(crate::driver::SpawnRequest {
        model: "gpt-5.6-terra",
        effort: Some("high"),
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: None,
        run_id: Some("version-pin-codex"),
    });
    assert_codex_spawn_contract(&production);
}

/// Config-schema conformance: the exact `config.toml` production writes
/// (`render_base_config_toml` + `append_hooks_toml`) must actually load
/// under `--strict-config` on the pinned codex binary.
///
/// This is the check that would have caught the `external_config_migration_prompts`
/// top-level-field bug: every other test in this module pins the `--json`
/// stream *contract*, not the config *schema*, so a hand-written config
/// literal with an invalid key sailed through untested. Soft-skip when
/// `codex` is absent, same convention as
/// [`installed_codex_matches_pinned_version_when_present`]; set
/// `BOSS_REQUIRE_CODEX_CLI=1` to require it.
///
/// Network-free: points the model provider at a closed local port so the
/// process fails fast on a connection error rather than requiring real
/// credentials. Config parsing happens strictly before any network attempt
/// (confirmed live: a config-schema error exits in well under 100ms with no
/// `thread.started`/`turn.started` ever printed; a valid config reaches both
/// within about a second), so within the bounded wait below, "we saw
/// `thread.started`" and "we saw the fatal config-load error" are mutually
/// exclusive, distinguishable outcomes — never a race.
#[test]
fn generated_config_toml_loads_under_strict_config_on_pinned_codex() {
    use std::io::Read;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let require = require_codex_cli();
    let Some(codex_bin) = which("codex") else {
        if require {
            panic!("BOSS_REQUIRE_CODEX_CLI is set but codex is not on PATH");
        }
        eprintln!(
            "codex not on PATH; skipping config-schema conformance check \
             (set BOSS_REQUIRE_CODEX_CLI=1 to require it)"
        );
        return;
    };

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let workspace = tmp.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("create workspace dir");
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir_all(&codex_home).expect("create codex home");

    // Exact production document: base config + one synthetic hook, the same
    // shape `write_hooks_and_attest` builds from `materialize_guards`.
    let base = crate::driver::codex::render_base_config_toml(&workspace);
    let guards = vec![crate::driver::codex::MaterializedGuard {
        command_path: PathBuf::from("/bin/true"),
        matcher: Some(".*"),
    }];
    let full = crate::driver::codex::append_hooks_toml(&base, &guards);
    std::fs::write(codex_home.join("config.toml"), &full).expect("write config.toml");

    let mut child = Command::new(&codex_bin)
        .current_dir(&workspace)
        .env("CODEX_HOME", &codex_home)
        .args([
            "exec",
            "--json",
            "--strict-config",
            "--skip-git-repo-check",
            "-c",
            "model_provider=boss_conformance_probe",
            "-c",
            r#"model_providers.boss_conformance_probe={name="probe",base_url="http://127.0.0.1:1/v1",wire_api="responses"}"#,
            "hi",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codex exec");

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => panic!("waiting on codex exec: {err}"),
        }
    }
    // Best-effort: process may have already exited (bad-config case).
    let _ = child.kill();
    let _ = child.wait();

    let stdout = String::from_utf8_lossy(&stdout_reader.join().expect("join stdout reader")).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_reader.join().expect("join stderr reader")).into_owned();
    let combined = format!("{stdout}\n{stderr}");

    assert!(
        !combined.contains("Error loading config.toml") && !combined.contains("unknown configuration field"),
        "generated config.toml must load under --strict-config on pinned codex \
         {PINNED_CODEX_CLI_VERSION}; got stdout={stdout:?} stderr={stderr:?}",
    );
    assert!(
        stdout.contains("\"thread.started\""),
        "expected codex to get past config load and start a turn (thread.started); \
         got stdout={stdout:?} stderr={stderr:?}",
    );
}

#[test]
fn unknown_turn_item_variants_are_tolerated_not_fatal() {
    // Four new TurnItem variants shipped unannounced (DynamicToolCall,
    // CollabAgentToolCall, Extension, EnteredReviewMode). A forward-compatible
    // normaliser must skip them, not crash the stream.
    let stream = concat!(
        r#"{"type":"thread.started","thread_id":"t1"}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_9","type":"DynamicToolCall","name":"x"}}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_10","type":"Extension","payload":{}}}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0}}"#,
        "\n",
    );
    let events = decode_jsonl(&codex_shaped_driver(), stream);
    assert_eq!(events.len(), 2, "unknown items skipped; start + completed remain");
    assert!(matches!(&events[0], boss_protocol::WorkerEvent::SessionStart { .. }));
    assert!(matches!(&events[1], boss_protocol::WorkerEvent::Stop { .. }));
    // Sticky: turn.completed inherited thread_id from thread.started.
    match &events[1] {
        boss_protocol::WorkerEvent::Stop { session_id, .. } => assert_eq!(session_id, "t1"),
        other => panic!("expected Stop, got {other:?}"),
    }
}

#[test]
fn additive_usage_fields_do_not_break_turn_completed_normalisation() {
    // A future CLI adding another usage counter must not break decoding.
    // Seed sticky session via thread.started (real streams always start that way).
    let stream = concat!(
        r#"{"type":"thread.started","thread_id":"t-add"}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"future_counter":99}}"#,
        "\n",
    );
    let events = decode_jsonl(&codex_shaped_driver(), stream);
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[1], boss_protocol::WorkerEvent::Stop { .. }));
}

#[test]
fn error_item_as_operational_warning_is_not_silently_a_turn_failure() {
    // On 0.145.0 `error` items gained a second meaning (operational warnings,
    // not just turn failures). Mapping every error item to a terminal Stop
    // would be a semantic absorption of that change. The harness requires the
    // normaliser to *reject* bare error items (caller decides), not promote
    // them to turn failure.
    let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"error","message":"rate limit soft warning"}}"#;
    let raw: serde_json::Value = serde_json::from_str(line).unwrap();
    let err = codex_shaped_driver()
        .normalize_progress_event(&raw)
        .expect_err("error items must not silently become turn failures");
    assert!(
        matches!(err, boss_protocol::NormalizeError::UnknownEvent(_)),
        "got {err:?}",
    );
}

// ─── Grok live-CLI posture pin ──────────────────────────────────────────────
//
// No version pin: `grokVersion` is observed (and, on drift from
// `LAST_CHARACTERISED_GROK_VERSION`, logged) by
// `grok::home::assert_inspect_json_posture`, but it never gates — Grok
// auto-updates itself, and a hard pin turned every automatic bump into a
// fail-closed provisioning outage. What remains pinned here is two
// independent live-CLI surfaces, mirroring the Codex convention above but
// with two distinct tiers rather than one blanket skip:
//   - Soft-skip when `grok` is not on PATH, or (for auth-gated assertions)
//     when the CLI is present but not logged in — `grok models` and `grok
//     inspect --json` both require auth per the pane-viability spike's
//     Q1/Q9, and a bare CI image has neither the binary nor a login.
//   - Hard-fail once availability (and, where required, auth) is
//     established: a removed flag or subcommand on an otherwise-working
//     installation must panic, not degrade to a silent skip.
// Set `BOSS_REQUIRE_GROK_CLI=1` to require the binary and a successful
// invocation, failing instead of skipping — use this on hosts re-running the
// pane-viability spike / re-capturing fixtures.
//
// The hidden `--trust` flag (design D-3: absent from `--help`) and the
// `grok models` menu are asserted here rather than folded into a `--help`
// text diff: a `--help` diff would never notice `--trust`'s removal (it was
// never listed there), and losing it silently hangs every worker on the
// folder-trust dialog (spike Q3). The model list may retain legacy entries,
// but its default must fail this pin loudly if it drifts from the catalog
// selection in `GROK_DESCRIPTOR` / `model_menu.rs`.

/// Probe grok availability with a flag guaranteed to be present (`--help`).
/// Soft-skip (return `false`, meaning "test body should return early") when
/// the binary is absent or `--help` itself fails, unless `BOSS_REQUIRE_GROK_CLI`
/// is set, in which case that failure panics. Once this returns `true`, the
/// binary is established present and working, so callers must invoke their
/// real assertion directly (not through [`run_live_grok`]'s soft-skip path)
/// so a removed flag/subcommand fails loudly instead of degrading to a skip.
fn grok_cli_available() -> bool {
    run_live_grok(&["--help"]).is_some()
}

/// Run `grok` with `args`; soft-skip (return `None`) on spawn failure or a
/// non-zero exit unless `BOSS_REQUIRE_GROK_CLI` is set — same convention as
/// [`installed_codex_matches_pinned_version_when_present`]'s
/// `BOSS_REQUIRE_CODEX_CLI`.
fn run_live_grok(args: &[&str]) -> Option<std::process::Output> {
    let require = require_grok_cli();
    let joined = args.join(" ");
    match Command::new("grok").args(args).output() {
        Ok(o) if o.status.success() => Some(o),
        Ok(o) => {
            if require {
                panic!(
                    "BOSS_REQUIRE_GROK_CLI is set but `grok {joined}` failed (status {}): \
                     stdout={} stderr={}",
                    o.status,
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr),
                );
            }
            eprintln!(
                "`grok {joined}` failed (status {}); skipping live check \
                 (set BOSS_REQUIRE_GROK_CLI=1 to require it)",
                o.status,
            );
            None
        }
        Err(err) => {
            if require {
                panic!("BOSS_REQUIRE_GROK_CLI is set but grok is not on PATH: {err}");
            }
            eprintln!(
                "grok not on PATH ({err}); skipping live `grok {joined}` check \
                 (set BOSS_REQUIRE_GROK_CLI=1 to require it)"
            );
            None
        }
    }
}

#[test]
fn hidden_trust_flag_still_parses_on_installed_grok() {
    // `--trust` is deliberately absent from `--help` output (design D-3), so
    // a `--help` text diff would never notice its removal. Combine it with
    // `--help`: argument parsing must accept every flag as known before the
    // `--help` action fires, so an unrecognised `--trust` fails the whole
    // invocation regardless of where `--help` sits on the command line —
    // this catches the removal a `--help` diff cannot.
    //
    // Availability (binary present, `--help` works) is probed separately
    // from the assertion itself. `run_live_grok` soft-skips on ANY non-zero
    // exit, and a removed `--trust` IS a non-zero exit (verified: an unknown
    // flag exits 2 with "unexpected argument" on stderr) — routing the
    // assertion through it would make the mandatory hidden-flag check a
    // silent no-op in the default (non-`BOSS_REQUIRE_GROK_CLI`) mode, which
    // is exactly the mode every dev host and CI run uses. So once the binary
    // is confirmed present and working, invoke `--trust --help` directly and
    // panic on failure — never skip.
    if !grok_cli_available() {
        return;
    }
    let output = Command::new("grok")
        .args(["--trust", "--help"])
        .output()
        .expect("grok confirmed available by grok_cli_available()");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_lowercase();
    assert!(
        output.status.success()
            && !combined.contains("unrecognized")
            && !combined.contains("unexpected argument")
            && !combined.contains("unknown flag"),
        "`grok --trust --help` must still accept the hidden --trust flag; a worker spawn that \
         loses it hangs on the folder-trust dialog (spike Q3). status={:?} got: {combined}",
        output.status,
    );
}

#[test]
fn grok_models_menu_matches_pinned_descriptor() {
    // `grok models` requires auth (spike Q1/Q9), so an availability probe
    // that only checks `--help` (which needs no login) is not sufficient
    // here: on a host with the binary present but logged out, `--help`
    // would succeed while `models` still fails, and routing that failure
    // through `grok_cli_available()`'s soft-skip convention would turn it
    // into a hard failure instead of the intended skip. Probe with `grok
    // inspect --json` instead — it also requires auth, so a logged-out host
    // (or one with no binary) skips here exactly as it would for `models`,
    // while a logged-in host with `models` itself removed still reaches the
    // strict assert below and panics.
    if run_live_grok(&["inspect", "--json"]).is_none() {
        return;
    }
    let output = Command::new("grok")
        .arg("models")
        .output()
        .expect("grok confirmed available by grok_cli_available()");
    assert!(
        output.status.success(),
        "`grok models` must still exist and succeed on the installed CLI; status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let default_model = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("Default model:"))
        .map(str::trim)
        .unwrap_or_else(|| panic!("`grok models` stdout must contain a \"Default model:\" line; got {stdout:?}"));
    let available: Vec<&str> = stdout
        .lines()
        .filter_map(|l| {
            let trimmed = l.trim();
            let rest = trimmed.strip_prefix('*').or_else(|| trimmed.strip_prefix('-'))?;
            let rest = rest.trim();
            Some(rest.strip_suffix(" (default)").unwrap_or(rest))
        })
        .collect();

    let driver = GrokDriver::default();
    let expected_default = driver.descriptor().model_menu.engine_default;
    assert_eq!(
        default_model, expected_default,
        "`grok models` default must match the pinned descriptor's engine_default; \
         got stdout={stdout:?}",
    );
    assert_eq!(
        available.first().copied(),
        Some(expected_default),
        "`grok models` must list the pinned descriptor default first; update GROK_DESCRIPTOR's \
         model menu deliberately when the provider changes the selected catalog entry. \
         got stdout={stdout:?}",
    );
}
