//! buildifier-wasm — a PROTOTYPE checkleft `sandbox-v1` external check.
//!
//! This guest compiles to `wasm32-unknown-unknown` and runs under checkleft's
//! wasmtime runtime. It reimplements the built-in buildifier check on top of the
//! (newly added, prototype-only) host-mediated command primitive:
//!
//!   1. The host writes an `ExternalCheckRuntimeInput` JSON ({changeset, config,
//!      capabilities}) at guest offset 0 and calls `checkleft_run(ptr, len)`.
//!   2. For each changed Starlark file, the guest builds a `run_command` request
//!      ({program, args}) and calls the host import `("checkleft","run_command")`
//!      to run buildifier (format pass + lint pass), receiving the captured
//!      stdout/stderr/exit back as JSON.
//!   3. The guest parses buildifier's `--format=json` output into checkleft
//!      `Finding`s (see `parser`, copied from the built-in check) and returns
//!      `{ "findings": [...] }`, packed as (ptr<<32|len).
//!
//! See PROTOTYPE-NOTES.md for the discovered ABI, the policy relaxations, and what
//! a production version would need.
//!
//! The wasm ABI (`checkleft_run`, `checkleft_alloc`, and the `run_command` import)
//! is gated to `target_arch = "wasm32"` so the pure `parser` module — and its
//! parity tests — still build and run on the host via `cargo test`.

pub mod parser;

#[cfg(target_arch = "wasm32")]
mod guest {
    use crate::parser::{self, Finding, Location, Severity};
    use serde::Deserialize;

    // The host-mediated command primitive. See runtime.rs ABI doc.
    #[link(wasm_import_module = "checkleft")]
    extern "C" {
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

    fn config_string(config: &serde_json::Value, key: &str) -> Option<String> {
        config.get(key).and_then(|v| v.as_str()).map(str::to_owned)
    }

    fn config_bool(config: &serde_json::Value, key: &str, default: bool) -> bool {
        config.get(key).and_then(serde_json::Value::as_bool).unwrap_or(default)
    }

    /// Calls the host command import and decodes the JSON response. `request` bytes
    /// stay alive for the duration of the synchronous host call.
    fn invoke(binary: &str, args: &[String]) -> Result<HostResponse, String> {
        let request = serde_json::json!({ "program": binary, "args": args });
        let bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
        let encoded = unsafe { run_command(bytes.as_ptr() as i32, bytes.len() as i32) };
        let encoded = encoded as u64;
        let resp_ptr = (encoded >> 32) as usize;
        let resp_len = (encoded & 0xffff_ffff) as usize;
        let response = unsafe { core::slice::from_raw_parts(resp_ptr as *const u8, resp_len) };
        serde_json::from_slice::<HostResponse>(response).map_err(|e| e.to_string())
    }

    fn error_finding(path: &str, message: String) -> Finding {
        Finding {
            severity: Severity::Warning,
            message,
            location: Some(Location {
                path: path.to_owned(),
                line: None,
                column: None,
            }),
            remediations: Vec::new(),
            suggested_fix: None,
        }
    }

    /// One buildifier pass (format or lint) over `path`, appending findings.
    fn pass(
        binary: &str,
        path: &str,
        args: Vec<String>,
        parse: fn(&[u8], &str) -> Result<Vec<Finding>, String>,
        findings: &mut Vec<Finding>,
    ) {
        match invoke(binary, &args) {
            Ok(response) if response.error.is_none() => match parse(response.stdout.as_bytes(), path)
            {
                Ok(parsed) => findings.extend(parsed),
                Err(err) => findings.push(error_finding(
                    path,
                    format!("could not run buildifier on `{path}`: {err}"),
                )),
            },
            Ok(response) => findings.push(error_finding(
                path,
                format!(
                    "could not run buildifier on `{path}`: {}",
                    response.error.unwrap_or_else(|| "command failed".to_owned())
                ),
            )),
            Err(err) => findings.push(error_finding(
                path,
                format!("could not run buildifier on `{path}`: {err}"),
            )),
        }
    }

    fn run_inner(input_bytes: &[u8]) -> Result<Vec<Finding>, String> {
        let input: Input = serde_json::from_slice(input_bytes).map_err(|e| e.to_string())?;
        // PROTOTYPE: default to a direct `buildifier` on PATH (the simpler route).
        // The built-in additionally supports bazel-target resolution; see notes.
        let binary =
            config_string(&input.config, "buildifier_path").unwrap_or_else(|| "buildifier".to_owned());
        let check_format = config_bool(&input.config, "check_format", true);
        let check_lint = config_bool(&input.config, "check_lint", true);

        let mut findings = Vec::new();
        for file in &input.changeset.changed_files {
            if file.kind == "deleted" || !parser::is_buildifier_file(&file.path) {
                continue;
            }
            if check_format {
                let args = vec![
                    "--mode=check".to_owned(),
                    "--format=json".to_owned(),
                    file.path.clone(),
                ];
                pass(&binary, &file.path, args, parser::parse_format_output, &mut findings);
            }
            if check_lint {
                let args = vec![
                    "--mode=check".to_owned(),
                    "--lint=warn".to_owned(),
                    "--format=json".to_owned(),
                    file.path.clone(),
                ];
                pass(&binary, &file.path, args, parser::parse_lint_output, &mut findings);
            }
        }
        Ok(findings)
    }

    /// Entry point shared with the exported `checkleft_run`. Returns the JSON
    /// `{ "findings": [...] }` payload bytes.
    pub fn run(input_bytes: &[u8]) -> Vec<u8> {
        let findings = run_inner(input_bytes).unwrap_or_else(|err| {
            vec![Finding {
                severity: Severity::Warning,
                message: format!("buildifier-wasm guest error: {err}"),
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
#[no_mangle]
pub extern "C" fn checkleft_alloc(size: i32) -> i32 {
    let size = size.max(0) as usize;
    let buffer = vec![0_u8; size].into_boxed_slice();
    Box::leak(buffer).as_mut_ptr() as i32
}

/// The runtime entry point. Reads the input JSON at `(input_ptr, input_len)`,
/// runs the check, leaks the output JSON, and returns the packed (ptr<<32|len).
#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn checkleft_run(input_ptr: i32, input_len: i32) -> i64 {
    let input =
        unsafe { core::slice::from_raw_parts(input_ptr as *const u8, input_len.max(0) as usize) };
    let output = guest::run(input);
    let leaked: &'static mut [u8] = output.leak();
    let ptr = leaked.as_ptr() as u64;
    let len = leaked.len() as u64;
    ((ptr << 32) | len) as i64
}
