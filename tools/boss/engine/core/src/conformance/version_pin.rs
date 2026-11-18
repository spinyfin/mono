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
    AGY_HOOK_SESSION_JSONL, CODEX_STDOUT_SESSION_JSONL, PINNED_CODEX_CLI_VERSION, PINNED_CODEX_ITEM_ID_BASE,
    assert_codex_spawn_contract, codex_shaped_driver, decode_jsonl,
};
use crate::conformance::{agy_cli_binary, codex_cli_binary, require_agy_cli, require_codex_cli, require_grok_cli};
use crate::driver::agy::{
    AGY_VERSION_TIMEOUT, DISABLE_AUTO_UPDATE_ENV, DISABLE_AUTO_UPDATE_VALUE, MIN_QUALIFIED_AGY_VERSION,
    parse_agy_version, run_agy_bounded, version_at_least,
};
use crate::driver::{AgentDriver, GrokDriver};

/// Parse `codex-cli X.Y.Z` (or `codex X.Y.Z`) stdout from `codex --version`.
fn parse_codex_version(stdout: &str) -> Option<String> {
    let line = stdout.lines().next()?.trim();
    // Observed forms: "codex-cli 0.153.4", "codex 0.153.4", and the same
    // `codex-cli X.Y.Z` / `codex X.Y.Z` shape on earlier pins.
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
    // Under Bazel the checksum-pinned release binary arrives via
    // `BOSS_TEST_CODEX` and this check is required; on a dev shell without it,
    // soft skip when `codex` is absent (fixture-side pins below still defend
    // the stream contract) unless BOSS_REQUIRE_CODEX_CLI=1 asks for a failure.
    let require = require_codex_cli();
    let Some(codex_bin) = codex_cli_binary() else {
        if require {
            panic!("BOSS_REQUIRE_CODEX_CLI is set but codex is not on PATH");
        }
        eprintln!("codex not on PATH; skipping live version check (set BOSS_REQUIRE_CODEX_CLI=1 to require it)");
        return;
    };
    let output = match Command::new(&codex_bin).arg("--version").output() {
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
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let require = require_codex_cli();
    let Some(codex_bin) = codex_cli_binary() else {
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
        timeout_sec: None,
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

    // Both pipes are streamed line by line through one channel so the wait
    // below can stop the moment either verdict appears, instead of holding
    // the process for a fixed wall-clock budget and reading everything
    // afterwards. The deadline is therefore only a hang bound: under
    // `bazel test` this probe shares the host with every other shard, and a
    // cold start of a 240 MB binary on a loaded machine took well over the
    // 5 s that an idle dev shell needs, which produced an empty stdout and a
    // false "never reached thread.started" — not a config-load failure.
    enum Line {
        Stdout(String),
        Stderr(String),
    }
    let (line_tx, line_rx) = std::sync::mpsc::channel::<Line>();
    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");
    let stdout_tx = line_tx.clone();
    let stdout_reader = std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout_pipe).lines().map_while(Result::ok) {
            if stdout_tx.send(Line::Stdout(line)).is_err() {
                break;
            }
        }
    });
    let stderr_reader = std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
            if line_tx.send(Line::Stderr(line)).is_err() {
                break;
            }
        }
    });

    const CONFIG_LOAD_ERRORS: [&str; 2] = ["Error loading config.toml", "unknown configuration field"];
    let mut stdout = String::new();
    let mut stderr = String::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match line_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Line::Stdout(line)) => {
                stdout.push_str(&line);
                stdout.push('\n');
            }
            Ok(Line::Stderr(line)) => {
                stderr.push_str(&line);
                stderr.push('\n');
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let decided = stdout.contains("\"thread.started\"")
            || CONFIG_LOAD_ERRORS
                .iter()
                .any(|needle| stdout.contains(needle) || stderr.contains(needle));
        if decided || Instant::now() >= deadline {
            break;
        }
        if let Ok(Some(_status)) = child.try_wait() {
            // Exited on its own (the bad-config case): drain what is left.
            while let Ok(line) = line_rx.recv_timeout(Duration::from_millis(200)) {
                match line {
                    Line::Stdout(line) => {
                        stdout.push_str(&line);
                        stdout.push('\n');
                    }
                    Line::Stderr(line) => {
                        stderr.push_str(&line);
                        stderr.push('\n');
                    }
                }
            }
            break;
        }
    }
    // Best-effort: process may have already exited (bad-config case).
    let _ = child.kill();
    let _ = child.wait();
    drop(line_rx);
    stdout_reader.join().expect("join stdout reader");
    stderr_reader.join().expect("join stderr reader");
    let combined = format!("{stdout}\n{stderr}");

    assert!(
        !CONFIG_LOAD_ERRORS.iter().any(|needle| combined.contains(needle)),
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
// folder-trust dialog (spike Q3). The `grok models` pin asserts the live
// default still matches `GROK_DESCRIPTOR`'s `engine_default` (currently
// `grok-4.6`, with `grok-4.5` retained as a prior generation) — any change
// to the default, or to the retained-generation set, must fail this pin
// loudly rather than be silently absorbed as a stale pin.

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
    let expected_available = vec!["grok-4.6", "grok-4.5"];
    assert_eq!(
        available, expected_available,
        "`grok models` menu changed shape ({available:?}); this pin asserts the live default \
         still matches GROK_DESCRIPTOR's engine_default and that the retained-generation set is \
         exactly what's expected — a genuinely new generation or a dropped retained SKU must \
         update GROK_DESCRIPTOR's model menu (model_menu.rs) deliberately (design A-11 / T-20) \
         rather than being silently absorbed as a stale pin. got stdout={stdout:?}",
    );
}

// ─── agy live-CLI version pin ───────────────────────────────────────────────
//
// Qualified floor is [`MIN_QUALIFIED_AGY_VERSION`] (1.5.7): `auto_approve` and
// `deny_unless_prior_grant` were measured on 1.5.7 and 1.5.9, not 1.5.5
// (permission / shared-HOME studies). This is a minimum, not an exact pin —
// Boss freezes installed bytes and disables updates; a checksum-pinned copy is not in the
// Bazel graph the way Codex is.
//
// Live `agy --version` soft-skips when the binary is absent, or when the
// invocation itself fails, unless `BOSS_REQUIRE_AGY_CLI=1` asks for a
// failure. CI cannot authenticate `agy` (OAuth against a personal Google
// account, keychain-backed), so the live check is meaningful only on a
// signed-in developer machine. Fixture-side pins in this module and in
// `agy_goldens` / `ingress_equivalence` / `boundary_equivalence` carry the
// hook contract everywhere else. Authenticated probes (rules discovery)
// are the non-test `agy_rules_probe` binary, which cannot run under
// `bazel test`'s mandatory sandbox.

// Boss driver maintainers own qualification; the deployment operator owns
// replacing the installed pin. Before adoption, maintainers run the pane,
// hook-surface and permission recipes, commit sanitized captures with version
// and hash provenance, and run conformance with BOSS_REQUIRE_AGY_CLI=1.
// The operator stops Boss and all workers, archives homes-root bin/agy for
// rollback, removes that copy, and provisions with BOSS_AGY_BIN pointing at
// the qualified signed candidate. First-install copying verifies identity
// and version and protects the copy. Restart only after provisioning succeeds.
// Overrides cannot replace an existing pin; bumping the floor is not an upgrade.

#[test]
fn agy_version_examples_match_qualification() {
    assert_eq!(parse_agy_version("agy 1.5.9").unwrap(), "1.5.9");
    assert!(version_at_least("1.5.7", "1.5.7"));
    assert!(version_at_least("1.5.9", "1.5.7"));
    assert!(!version_at_least("1.5.5", "1.5.7"));
    assert!(!version_at_least("invalid", "1.5.7"));
}

#[test]
fn installed_agy_meets_qualified_minimum_when_present() {
    // Soft-skip when `agy` is absent. CI images typically have neither the
    // binary nor a signed-in account; fixture-side pins below still defend
    // the hook contract. Set BOSS_REQUIRE_AGY_CLI=1 on a signed-in
    // developer machine that re-captures fixtures.
    let require = require_agy_cli();
    let Some(agy_bin) = agy_cli_binary() else {
        if require {
            panic!("BOSS_REQUIRE_AGY_CLI is set but agy is not on PATH (and BOSS_AGY_BIN is unset)");
        }
        eprintln!(
            "agy not on PATH; skipping live version check (set BOSS_REQUIRE_AGY_CLI=1 to require it). \
             CI cannot authenticate agy — fixture-side pins carry the contract here."
        );
        return;
    };
    let tmp = tempfile::TempDir::new().expect("tempdir for agy --version HOME");
    let mut command = Command::new(&agy_bin);
    command
        .env("HOME", tmp.path())
        .env(DISABLE_AUTO_UPDATE_ENV, DISABLE_AUTO_UPDATE_VALUE)
        .arg("--version");
    let output = match run_agy_bounded(&mut command, AGY_VERSION_TIMEOUT) {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            if require {
                panic!(
                    "BOSS_REQUIRE_AGY_CLI is set but `agy --version` failed (status {}): stdout={} stderr={}",
                    o.status,
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr),
                );
            }
            eprintln!(
                "agy --version failed (status {}); skipping live version check \
                 (set BOSS_REQUIRE_AGY_CLI=1 to require it). CI cannot authenticate agy.",
                o.status
            );
            return;
        }
        Err(err) => {
            if require {
                panic!("BOSS_REQUIRE_AGY_CLI is set but agy could not be spawned: {err}");
            }
            eprintln!(
                "agy not runnable ({err}); skipping live version check \
                 (set BOSS_REQUIRE_AGY_CLI=1 to require it)"
            );
            return;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let version = parse_agy_version(&stdout)
        .or_else(|_| parse_agy_version(&stderr))
        .unwrap_or_else(|_| panic!("could not parse agy --version from stdout={stdout:?} stderr={stderr:?}"));
    assert!(
        version_at_least(&version, MIN_QUALIFIED_AGY_VERSION),
        "installed agy is {version}, but the conformance harness requires \
         {MIN_QUALIFIED_AGY_VERSION} or later (auto_approve was measured on 1.5.7+). \
         Re-run the investigations, update fixtures, and bump MIN_QUALIFIED_AGY_VERSION \
         deliberately — do not silently absorb drift."
    );
}

#[test]
fn agy_fixture_stream_carries_required_1_5_7_envelope_fields() {
    // Removals of these fields would break the progress session: 1.1.12
    // omitted hookEventName, and fullyIdle is the idle-boundary predicate.
    let mut saw_session_start = false;
    let mut saw_idle_stop = false;
    for line in AGY_HOOK_SESSION_JSONL.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let name = value.get("hookEventName").and_then(|v| v.as_str());
        assert!(
            value.get("conversationId").and_then(|v| v.as_str()).is_some(),
            "every agy envelope must carry conversationId; got {value}"
        );
        match name {
            Some("SessionStart") => {
                saw_session_start = true;
                assert!(
                    value.get("lastUserInput").is_some(),
                    "1.5.5+ SessionStart repeats lastUserInput (stripped by the normaliser)"
                );
            }
            Some("Stop") => {
                assert!(
                    value.get("fullyIdle").and_then(|v| v.as_bool()).is_some(),
                    "Stop must carry boolean fullyIdle; got {value}"
                );
                assert!(
                    value.get("terminationReason").and_then(|v| v.as_str()).is_some(),
                    "Stop must carry terminationReason; got {value}"
                );
                if value.get("fullyIdle") == Some(&serde_json::Value::Bool(true)) {
                    saw_idle_stop = true;
                }
            }
            Some("PreToolUse" | "PostToolUse") => {
                assert!(
                    value.pointer("/toolCall/name").and_then(|v| v.as_str()).is_some(),
                    "tool envelopes must carry toolCall.name; got {value}"
                );
            }
            Some(_) => {}
            None => panic!("agy fixture envelope missing hookEventName: {value}"),
        }
    }
    assert!(saw_session_start, "fixture must include SessionStart");
    assert!(saw_idle_stop, "fixture must include a fullyIdle=true Stop");
}
