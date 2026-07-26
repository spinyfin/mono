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

use std::process::Command;

use crate::conformance::fixtures::{
    CODEX_STDOUT_SESSION_JSONL, PINNED_CODEX_CLI_VERSION, PINNED_CODEX_ITEM_ID_BASE, codex_shaped_driver, decode_jsonl,
};
use crate::driver::AgentDriver;

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
    // When `codex` is on PATH the harness must be running against the pin.
    // When it is absent (CI images without the CLI) the fixture-side pins
    // below still defend the stream contract; this test is a soft skip.
    let output = match Command::new("codex").arg("--version").output() {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            eprintln!(
                "codex --version failed (status {}); skipping live version check",
                o.status
            );
            return;
        }
        Err(err) => {
            eprintln!("codex not on PATH ({err}); skipping live version check");
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
fn codex_shaped_spawn_does_not_use_removed_ask_for_approval_flag() {
    // `-a` / `--ask-for-approval` was removed from `codex exec` on 0.145.0 and
    // produces a hard argument error. The harness's reference spawn plan must
    // never reintroduce it.
    let plan = codex_shaped_driver().spawn_invocation(crate::driver::SpawnRequest {
        model: "gpt-5.5",
        effort: None,
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: None,
    });
    assert!(
        !plan.command.contains("ask-for-approval") && !plan.command.split_whitespace().any(|t| t == "-a"),
        "spawn must not use the removed -a/--ask-for-approval flag; got {}",
        plan.command,
    );
    assert!(
        plan.command.contains("--json"),
        "spawn must request the JSONL progress stream; got {}",
        plan.command,
    );
    assert!(
        plan.command.contains("--strict-config"),
        "spawn must pass --strict-config so config-schema drift fails at startup; got {}",
        plan.command,
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
}

#[test]
fn additive_usage_fields_do_not_break_turn_completed_normalisation() {
    // A future CLI adding another usage counter must not break decoding.
    let line = r#"{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"future_counter":99}}"#;
    let raw: serde_json::Value = serde_json::from_str(line).unwrap();
    let event = codex_shaped_driver()
        .normalize_progress_event(&raw)
        .expect("additive usage fields must be tolerated");
    assert!(matches!(event, boss_protocol::WorkerEvent::Stop { .. }));
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
