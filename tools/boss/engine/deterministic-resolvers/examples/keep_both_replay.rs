//! Runs the keep-both resolver on one file with conflict markers and
//! reports what it decided, without touching a workspace.
//!
//! This is the entry point the offline conflict replay
//! (`tools/boss/scripts/conflict-replay`) uses to measure the resolver
//! against recorded conflicts: it calls the same function the engine
//! does, so the replay measures the shipped rules rather than a copy of
//! them. Formatting is left to the caller.
//!
//! ```text
//! keep_both_replay --path <repo path> --scope items|lines --input <file> [--output <file>]
//! keep_both_replay --path <repo path> --format-command <workspace root>
//! ```
//!
//! The first form prints one JSON object:
//! `{"resolved":true,"insertions":["items",…]}` or
//! `{"resolved":false,"reason":"…"}`. The second prints, as a JSON array,
//! the formatter invocation the engine would run on that file at that
//! workspace root, for the caller to execute there. Exits 0 either way; a
//! non-zero exit means the arguments or files were unusable.

use std::path::Path;
use std::process::ExitCode;

use boss_deterministic_resolvers::{KeepBothScope, keep_both_insertions, rustfmt_command};

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("keep_both_replay: {message}");
            ExitCode::from(2)
        }
    }
}

fn run(args: Vec<String>) -> Result<String, String> {
    let mut path = None;
    let mut scope = KeepBothScope::ItemsAndImports;
    let mut input = None;
    let mut output = None;
    let mut format_at = None;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--path" => path = Some(value),
            "--input" => input = Some(value),
            "--output" => output = Some(value),
            "--format-command" => format_at = Some(value),
            "--scope" => {
                scope = match value.as_str() {
                    "items" => KeepBothScope::ItemsAndImports,
                    "lines" => KeepBothScope::WholeLines,
                    other => return Err(format!("unknown scope {other:?} (expected items or lines)")),
                }
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let path = path.ok_or("--path is required")?;
    if let Some(workspace) = format_at {
        let command: Vec<String> = rustfmt_command(Path::new(&workspace), &path)
            .iter()
            .map(|arg| json_string(arg))
            .collect();
        return Ok(format!("[{}]", command.join(",")));
    }
    let input = input.ok_or("--input is required")?;
    let content = std::fs::read_to_string(&input).map_err(|e| format!("cannot read {input}: {e}"))?;

    match keep_both_insertions(&path, &content, scope) {
        Ok(resolution) => {
            if let Some(output) = output {
                std::fs::write(&output, &resolution.content).map_err(|e| format!("cannot write {output}: {e}"))?;
            }
            let kinds: Vec<String> = resolution
                .insertions
                .iter()
                .map(|kind| format!("\"{}\"", kind.as_str()))
                .collect();
            Ok(format!("{{\"resolved\":true,\"insertions\":[{}]}}", kinds.join(",")))
        }
        Err(reason) => Ok(format!("{{\"resolved\":false,\"reason\":{}}}", json_string(&reason))),
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}
