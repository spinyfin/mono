//! giant-struct-wasm — a PROTOTYPE checkleft `sandbox-v1` external check.
//!
//! This guest compiles to `wasm32-unknown-unknown` and runs under checkleft's
//! wasmtime runtime. It reimplements the built-in
//! `rust-giant-structs-use-builder` check (`tools/checkleft/src/checks/
//! rust_giant_structs_use_builder.rs`) as a wasm external check, to prove the
//! custom **programmatic** external-check path end-to-end. Unlike the buildifier
//! spikes (which can go declarative), this check parses Rust with `syn` and counts
//! named struct fields — logic that is not expressible as config/pattern-matching,
//! so it genuinely exercises the programmatic wasm path.
//!
//! ## Control flow
//!
//!   1. The host writes an `ExternalCheckRuntimeInput` JSON ({changeset, config,
//!      capabilities}) at guest offset 0 and calls `checkleft_run(ptr, len)`.
//!   2. For each changed `.rs` file (skipping deleted), the guest reads the file
//!      **contents** by calling the host import `("checkleft","run_command")` with
//!      `cat <path>` (cwd = repo root). `cat` is in checkleft's production global
//!      command ceiling, so this needs no policy relaxation.
//!   3. The guest parses the bytes with `syn`, runs the giant-struct analysis
//!      (`analyzer`, copied from the built-in), and turns each violation into a
//!      checkleft `Finding` with the SAME message, location, severity, and
//!      remediations as the built-in.
//!   4. It returns `{ "findings": [...] }`, packed as (ptr<<32|len).
//!
//! ## The discovered gap
//!
//! The sandbox-v1 ABI has **no file-read primitive** — the changeset carries only
//! paths, not contents. A check that needs bytes (this one does, to feed `syn`)
//! must shell out to `cat` through `run_command`. That works, but couples file
//! access to the command allow-list and the stdout cap (so files larger than the
//! cap are truncated and fail to parse, where the built-in — reading from the
//! source tree — has no such limit). See PROTOTYPE-NOTES.md.
//!
//! The wasm ABI (`checkleft_run`, `checkleft_alloc`, the `run_command` import) is
//! gated to `target_arch = "wasm32"` so the pure `analyzer` and the finding
//! construction below — and their parity tests — still build and run on the host
//! via `cargo test`.

pub mod analyzer;

use std::collections::HashSet;

use serde::Serialize;

use analyzer::{BuilderKind, DEFAULT_MAX_FIELDS, collect_violations, find_struct_line};

// ── finding shapes (serialize-compatible with checkleft::output) ──────────────
//
// These serialize to exactly the JSON that `checkleft::output::Finding`
// deserializes, so the host runtime reads them back unchanged. `suggested_fix`
// is `Option<()>` so it serializes to `null` (the built-in always emits `null`
// for this check).

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    #[allow(dead_code)]
    Warning,
    #[allow(dead_code)]
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Location {
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
    pub location: Option<Location>,
    pub remediations: Vec<String>,
    pub suggested_fix: Option<()>,
}

/// The resolved subset of the built-in check's config the guest honors:
/// `max_fields`, `builder`, and simple-name `exclude_structs`. The built-in's
/// `config_dir`-scoped / qualified exclusions, `exclude_files` globs, and
/// stale-exclusion auditing are intentionally not ported (see PROTOTYPE-NOTES.md).
pub struct Config {
    pub max_fields: usize,
    pub builder: BuilderKind,
    pub exclude_structs: HashSet<String>,
}

impl Config {
    /// Parse the check config (the TOML config, serialized to JSON by the host).
    /// Mirrors the built-in's `parse_config` for the ported knobs.
    pub fn from_json(config: &serde_json::Value) -> Result<Self, String> {
        let max_fields = match config.get("max_fields") {
            None | Some(serde_json::Value::Null) => DEFAULT_MAX_FIELDS,
            Some(value) => {
                let raw = value
                    .as_i64()
                    .ok_or_else(|| "`max_fields` must be an integer".to_owned())?;
                usize::try_from(raw)
                    .map_err(|_| "`max_fields` must be a non-negative integer".to_owned())?
            }
        };

        let builder = match config.get("builder").and_then(serde_json::Value::as_str) {
            Some("bon") | None => BuilderKind::Bon,
            Some("derive_builder") => BuilderKind::DeriveBuilder,
            Some(other) => {
                return Err(format!(
                    "unknown `builder` value: {other:?}; expected \"bon\" or \"derive_builder\""
                ));
            }
        };

        let exclude_structs = config
            .get("exclude_structs")
            .and_then(serde_json::Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    // Qualified `path::Struct` entries are not honored by the guest
                    // (no config_dir scope); keep only simple names, matching the
                    // built-in's simple-name behavior under an empty config_dir.
                    .filter(|entry| !entry.contains("::"))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            max_fields,
            builder,
            exclude_structs,
        })
    }
}

/// Core parity surface: turn one file's source into findings, identical in
/// message / location / severity / remediations to the built-in's per-struct
/// `Finding`. Host-testable (no wasm); the guest calls this after `cat`-ing each
/// file. Returns an empty vec for unparseable sources (mirrors the built-in's
/// `let Ok(parsed_file) = syn::parse_file(source) else { continue };`).
pub fn findings_for_file(path: &str, source: &str, config: &Config) -> Vec<Finding> {
    let Ok(parsed) = syn::parse_file(source) else {
        return Vec::new();
    };

    let violations = collect_violations(
        &parsed.items,
        false,
        &config.builder,
        config.max_fields,
        &config.exclude_structs,
    );

    violations
        .into_iter()
        .map(|struct_name| {
            let line = find_struct_line(source, &struct_name);
            Finding {
                severity: Severity::Error,
                message: format!(
                    "struct `{struct_name}` has more than {} named fields but lacks `#[derive({})]`",
                    config.max_fields,
                    config.builder.derive_display(),
                ),
                location: Some(Location {
                    path: path.to_owned(),
                    line: Some(line),
                    column: Some(1),
                }),
                remediations: vec![
                    format!(
                        "Add `#[derive({}::Builder)]` (and `#[builder(on(String, into))]` per the project convention) above the struct.",
                        config.builder.crate_name(),
                    ),
                    "Permanently exempt a file by adding it to `exclude_files` in the `CHECKS` file.".to_owned(),
                ],
                suggested_fix: None,
            }
        })
        .collect()
}

#[cfg(target_arch = "wasm32")]
mod guest {
    use super::{Config, Finding, Severity, findings_for_file};
    use serde::Deserialize;

    // The host-mediated command primitive. See runtime.rs ABI doc.
    #[link(wasm_import_module = "checkleft")]
    unsafe extern "C" {
        fn run_command(req_ptr: i32, req_len: i32) -> i64;
    }

    #[derive(Default, Deserialize)]
    struct Input {
        #[serde(default)]
        changeset: ChangeSet,
        #[serde(default)]
        config: serde_json::Value,
    }

    #[derive(Default, Deserialize)]
    struct ChangeSet {
        #[serde(default)]
        changed_files: Vec<ChangedFile>,
    }

    #[derive(Deserialize)]
    struct ChangedFile {
        path: String,
        #[serde(default)]
        kind: String,
    }

    #[derive(Deserialize)]
    #[allow(dead_code)] // exit_code/stderr/timed_out are part of the ABI contract.
    struct HostResponse {
        exit_code: Option<i32>,
        #[serde(default)]
        stdout: String,
        #[serde(default)]
        stderr: String,
        #[serde(default)]
        timed_out: bool,
        #[serde(default)]
        error: Option<String>,
    }

    /// Calls the host command import and decodes the JSON response. `request`
    /// bytes stay alive for the duration of the synchronous host call.
    fn invoke(program: &str, args: &[String]) -> Result<HostResponse, String> {
        let request = serde_json::json!({ "program": program, "args": args });
        let bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
        let encoded = unsafe { run_command(bytes.as_ptr() as i32, bytes.len() as i32) };
        let encoded = encoded as u64;
        let resp_ptr = (encoded >> 32) as usize;
        let resp_len = (encoded & 0xffff_ffff) as usize;
        let response = unsafe { core::slice::from_raw_parts(resp_ptr as *const u8, resp_len) };
        serde_json::from_slice::<HostResponse>(response).map_err(|e| e.to_string())
    }

    /// Reads a file's contents via `cat <path>` through the host command import.
    /// Returns `None` (skip the file, no finding) on any read failure — mirroring
    /// the built-in's `let Ok(contents) = tree.read_file(..) else { continue };`.
    fn read_file(path: &str) -> Option<String> {
        match invoke("cat", &[path.to_owned()]) {
            Ok(resp) if resp.error.is_none() && resp.exit_code == Some(0) => Some(resp.stdout),
            _ => None,
        }
    }

    fn run_inner(input_bytes: &[u8]) -> Result<Vec<Finding>, String> {
        let input: Input = serde_json::from_slice(input_bytes).map_err(|e| e.to_string())?;
        let config = Config::from_json(&input.config)?;

        let mut findings = Vec::new();
        for file in &input.changeset.changed_files {
            if file.kind == "deleted" || !file.path.ends_with(".rs") {
                continue;
            }
            let Some(source) = read_file(&file.path) else {
                continue;
            };
            findings.extend(findings_for_file(&file.path, &source, &config));
        }
        Ok(findings)
    }

    /// Entry point shared with the exported `checkleft_run`. Returns the JSON
    /// `{ "findings": [...] }` payload bytes.
    pub fn run(input_bytes: &[u8]) -> Vec<u8> {
        let findings = run_inner(input_bytes).unwrap_or_else(|err| {
            vec![Finding {
                severity: Severity::Warning,
                message: format!("giant-struct-wasm guest error: {err}"),
                location: None,
                remediations: Vec::new(),
                suggested_fix: None,
            }]
        });
        let output = serde_json::json!({ "findings": findings });
        serde_json::to_vec(&output).unwrap_or_else(|_| br#"{"findings":[]}"#.to_vec())
    }
}

// ── wasm ABI exports ─────────────────────────────────────────────────────────

/// Allocates `size` writable bytes and returns a pointer. The host calls this to
/// obtain a landing buffer for command-response payloads (see runtime.rs). The
/// buffer is intentionally leaked: a guest instance is single-shot.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn checkleft_alloc(size: i32) -> i32 {
    let size = size.max(0) as usize;
    let buffer = vec![0_u8; size].into_boxed_slice();
    Box::leak(buffer).as_mut_ptr() as i32
}

/// The runtime entry point. Reads the input JSON at `(input_ptr, input_len)`,
/// runs the check, leaks the output JSON, and returns the packed (ptr<<32|len).
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn checkleft_run(input_ptr: i32, input_len: i32) -> i64 {
    let input =
        unsafe { core::slice::from_raw_parts(input_ptr as *const u8, input_len.max(0) as usize) };
    let output = guest::run(input);
    let leaked: &'static mut [u8] = output.leak();
    let ptr = leaked.as_ptr() as u64;
    let len = leaked.len() as u64;
    ((ptr << 32) | len) as i64
}

// ── host-side finding-construction parity tests (`cargo test`) ────────────────
#[cfg(test)]
mod tests {
    use super::*;

    fn bon_config() -> Config {
        Config {
            max_fields: DEFAULT_MAX_FIELDS,
            builder: BuilderKind::Bon,
            exclude_structs: HashSet::new(),
        }
    }

    #[test]
    fn flagged_struct_finding_matches_built_in_shape() {
        let source = "pub struct Big {\n a: String,\n b: String,\n c: String,\n d: String,\n e: String,\n f: String,\n}\n";
        let findings = findings_for_file("src/big.rs", source, &bon_config());
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(
            f.message,
            "struct `Big` has more than 5 named fields but lacks `#[derive(bon::Builder)]`"
        );
        let loc = f.location.as_ref().expect("location");
        assert_eq!(loc.path, "src/big.rs");
        assert_eq!(loc.line, Some(1));
        assert_eq!(loc.column, Some(1));
        assert_eq!(f.remediations.len(), 2);
        assert!(f.remediations[0].contains("#[derive(bon::Builder)]"));
        assert!(f.remediations[0].contains("#[builder(on(String, into))]"));
        assert!(f.remediations[1].contains("exclude_files"));
        assert!(f.suggested_fix.is_none());
    }

    #[test]
    fn flagged_finding_serializes_to_checkleft_json() {
        let source = "pub struct Big { a: u8, b: u8, c: u8, d: u8, e: u8, f: u8 }\n";
        let findings = findings_for_file("src/big.rs", source, &bon_config());
        let value = serde_json::to_value(&findings[0]).expect("serialize");
        assert_eq!(value["severity"], "error");
        assert_eq!(value["location"]["path"], "src/big.rs");
        assert_eq!(value["location"]["column"], 1);
        assert!(value["suggested_fix"].is_null());
        assert!(value["remediations"].as_array().unwrap().len() == 2);
    }

    #[test]
    fn clap_args_struct_produces_no_finding() {
        let source = "#[derive(Debug, Clone, Args)]\npub struct TaskArgs { a: String, b: String, c: String, d: String, e: String, f: String }\n";
        assert!(findings_for_file("src/cli.rs", source, &bon_config()).is_empty());
    }

    #[test]
    fn derive_builder_config_changes_message() {
        let source = "pub struct Big { a: u8, b: u8, c: u8, d: u8, e: u8, f: u8 }\n";
        let config = Config {
            max_fields: DEFAULT_MAX_FIELDS,
            builder: BuilderKind::DeriveBuilder,
            exclude_structs: HashSet::new(),
        };
        let findings = findings_for_file("src/big.rs", source, &config);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("derive_builder::Builder"));
        assert!(findings[0].remediations[0].contains("#[derive(derive_builder::Builder)]"));
    }

    #[test]
    fn config_from_json_parses_knobs() {
        let json = serde_json::json!({
            "max_fields": 2,
            "builder": "derive_builder",
            "exclude_structs": ["Grandfathered", "src/x.rs::Qualified"],
        });
        let config = Config::from_json(&json).expect("parse");
        assert_eq!(config.max_fields, 2);
        assert!(matches!(config.builder, BuilderKind::DeriveBuilder));
        // Simple name kept; qualified entry dropped (not honored by the guest).
        assert!(config.exclude_structs.contains("Grandfathered"));
        assert_eq!(config.exclude_structs.len(), 1);
    }

    #[test]
    fn exclude_structs_suppresses_named_violation() {
        let source = "pub struct Skip { a: u8, b: u8, c: u8, d: u8, e: u8, f: u8 }\n";
        let json = serde_json::json!({ "exclude_structs": ["Skip"] });
        let config = Config::from_json(&json).expect("parse");
        assert!(findings_for_file("src/x.rs", source, &config).is_empty());
    }

    #[test]
    fn unparseable_source_yields_no_findings() {
        assert!(findings_for_file("src/x.rs", "this is not rust ;;;", &bon_config()).is_empty());
    }
}
