//! The JavaScript **cell harness** dialect.
//!
//! `gpt-5.6-terra` (codex-cli 0.145.0) does not call a shell tool with a
//! command string. It writes a JavaScript cell that calls into a `tools.*`
//! namespace, and the cell — not the command — is the unit the harness
//! reports on:
//!
//! ```text
//! custom_tool_call        name="exec"  call_id=A
//!   input: const r = await tools.exec_command({"cmd":"…","yield_time_ms":30000,…});
//!          text(JSON.stringify(r));
//! custom_tool_call_output              call_id=A
//!   output: "Script running with cell ID 1\nWall time 11.1 seconds\nOutput:\n"
//! function_call           name="wait"  call_id=W
//!   arguments: {"cell_id":"1","yield_time_ms":30000,"max_tokens":2000}
//! function_call_output                 call_id=W
//!   output: ["Script completed\nWall time 17.7 seconds\nOutput:\n",
//!           "{\"chunk_id\":…,\"exit_code\":4,\"output\":\"…\"}"]
//! ```
//!
//! Two facts drive everything here:
//!
//! - When a command outlives the cell's model-chosen `yield_time_ms`, the
//!   call's own output is a **yield placeholder** naming a cell id. The
//!   command's real stdout arrives later, on the output of a separate
//!   `wait` call that targets that cell id. A consumer that treats the
//!   placeholder as the command's result sees a still-running command as
//!   observed, and never sees its output at all.
//! - `Script completed` refers to the **JavaScript cell** finishing, not to
//!   the shell command. It is not an exit-status claim.
//!
//! Both are documented with captured transcripts in
//! `tools/boss/docs/investigations/codex-exit-code-surfacing.md`
//! (findings 3 and 4).
//!
//! This module is pure dialect parsing. Correlating a yielded cell with its
//! `wait` continuation is the consumer's job — see
//! `boss_engine_driver::codex::rollout_calls`.

use serde_json::Value;

use crate::coerce_command_to_string;

/// The tool Codex calls to poll a cell that yielded while still running.
pub const CELL_WAIT_TOOL: &str = "wait";

/// `tools.*` helpers inside a cell script that run a shell command, in the
/// order they are searched. `write_stdin` is deliberately absent: it
/// continues an existing shell session whose id only ever appears in a
/// prior cell's *output*, so it carries no command of its own and cannot be
/// attributed to one from the script text alone.
const COMMAND_BEARING_CELL_CALLS: &[&str] = &["exec_command", "shell"];

/// Header line prefix of a yield placeholder, followed by the cell id.
const YIELD_HEADER_PREFIX: &str = "Script running with cell ID ";

/// Header line of a terminal cell result. Says the *cell* finished — never
/// that the command it ran succeeded.
const COMPLETED_HEADER: &str = "Script completed";

/// Marker line separating the harness's own header from the cell's output.
const OUTPUT_MARKER: &str = "\nOutput:";

/// Whether `tool_name` is the cell harness's continuation poll.
pub fn is_cell_wait_tool(tool_name: &str) -> bool {
    tool_name == CELL_WAIT_TOOL
}

/// The cell id a `wait` call targets, from its canonicalised tool input
/// (`{"cell_id":"1",…}`). Accepts the string and number forms — observed as
/// a string, but the id is rendered as a bare integer in the placeholder it
/// comes from, so both are normalised to the same key.
pub fn wait_target_cell_id(tool_input: &Value) -> Option<String> {
    match tool_input.get("cell_id")? {
        Value::String(text) => Some(text.trim().to_owned()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Whether the cell is still running, and what the harness forwarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellOutcome {
    /// The cell yielded: the command it started is still running and its
    /// real output will arrive on a `wait` targeting `cell_id`.
    Yielded { cell_id: String },
    /// The cell itself finished. Terminal for the cell — **not** a claim
    /// about the command's exit status.
    Completed,
}

/// One parsed cell-harness output envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellOutput {
    pub outcome: CellOutcome,
    /// Everything the cell forwarded after the harness's `Output:` marker.
    /// Empty for a yield placeholder, and for a cell that projected nothing.
    pub payload: String,
}

/// Parse a flattened tool-output body as a cell-harness envelope.
///
/// Returns `None` when the body is not one — the legacy prose-wrapped
/// `exec_command` form (`Process exited with code N\nOutput:\n…`), a bare
/// structured chunk, or ordinary text. Callers keep their pre-cell handling
/// for those rather than guessing.
pub fn parse_cell_output(text: &str) -> Option<CellOutput> {
    let header = text.lines().next()?.trim_end();
    let outcome = if header == COMPLETED_HEADER {
        CellOutcome::Completed
    } else {
        let cell_id = header.strip_prefix(YIELD_HEADER_PREFIX)?.trim();
        if cell_id.is_empty() {
            return None;
        }
        CellOutcome::Yielded {
            cell_id: cell_id.to_owned(),
        }
    };
    // The harness emits `Output:` on its own line; a content-block array
    // joined with `\n` leaves an extra blank line before the payload.
    let payload = text
        .split_once(OUTPUT_MARKER)
        .map(|(_, rest)| rest.trim_start_matches('\n'))
        .unwrap_or("")
        .to_owned();
    Some(CellOutput { outcome, payload })
}

/// The shell command(s) a cell script issues, in source order.
///
/// The cell's tool input is JavaScript source, so it never parses as JSON
/// and the command lives inside a `tools.exec_command({…})` argument. That
/// argument object is itself emitted as JSON in every captured transcript,
/// so it is extracted by brace balancing and parsed as JSON — a script that
/// uses JavaScript object shorthand instead yields nothing here rather than
/// a guess.
///
/// Empty means "no command-bearing call in this script": a pure
/// continuation cell (`tools.write_stdin`), a `store`/`load` cell, or a
/// shape this parser does not recognise. Callers must not read that as
/// "the cell ran no command".
pub fn commands_from_cell_script(source: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut rest = source;
    while let Some((at, needle_len)) = next_command_bearing_call(rest) {
        let after = &rest[at + needle_len..];
        if let Some(object) = balanced_object_literal(after)
            && let Ok(parsed) = serde_json::from_str::<Value>(object)
            && let Some(command) = parsed.get("cmd").or_else(|| parsed.get("command"))
        {
            commands.push(coerce_command_to_string(command));
        }
        rest = after;
    }
    commands
}

/// A single display/gate string for the commands a cell script issues, or
/// `None` when it issues none.
///
/// One command — the shape in every captured transcript — is returned
/// verbatim, so it feeds command gates (`gh pr` / `cube pr` classification,
/// editorial audit) exactly as a plain shell driver's command would. The
/// multi-command form is one command per line: an honest list of what the
/// cell ran, never a fabricated compound command.
pub fn cell_script_command(source: &str) -> Option<String> {
    let commands = commands_from_cell_script(source);
    (!commands.is_empty()).then(|| commands.join("\n"))
}

/// Offset and needle length of the earliest `tools.<fn>(` call in `source`
/// that runs a command, so a script mixing helpers is read in source order.
fn next_command_bearing_call(source: &str) -> Option<(usize, usize)> {
    COMMAND_BEARING_CELL_CALLS
        .iter()
        .filter_map(|name| {
            let needle = format!("tools.{name}(");
            source.find(&needle).map(|at| (at, needle.len()))
        })
        .min_by_key(|(at, _)| *at)
}

/// Take the balanced `{…}` object literal at the start of `source` (after
/// any leading whitespace), respecting JSON string literals and escapes.
fn balanced_object_literal(source: &str) -> Option<&str> {
    let trimmed = source.trim_start();
    let offset = source.len() - trimmed.len();
    if !trimmed.starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in trimmed.char_indices() {
        if in_string {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = offset + index + ch.len_utf8();
                    return Some(&source[offset..end]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Verbatim `custom_tool_call.input` from probe 6 of the exit-code
    /// investigation (`p6_hidden_exit`).
    const P6_CELL_SCRIPT: &str = concat!(
        r#"const r = await tools.exec_command({"cmd":"sh -c 'for i in $(seq 1 12); do echo tick-$i; sleep 4; done; "#,
        r#"echo FINAL-LINE; exit 4'","workdir":"/tmp/work","yield_time_ms":30000,"max_output_tokens":2000});"#,
        "\ntext(JSON.stringify(r));"
    );

    #[test]
    fn cell_script_yields_the_shell_command_not_the_script() {
        assert_eq!(
            cell_script_command(P6_CELL_SCRIPT).as_deref(),
            Some("sh -c 'for i in $(seq 1 12); do echo tick-$i; sleep 4; done; echo FINAL-LINE; exit 4'")
        );
    }

    #[test]
    fn cell_script_command_survives_braces_inside_the_command_string() {
        let source = r#"const r = await tools.exec_command({"cmd":"awk '{print $1}' f","workdir":"/w"});"#;
        assert_eq!(cell_script_command(source).as_deref(), Some("awk '{print $1}' f"));
    }

    #[test]
    fn cell_script_command_survives_escaped_quotes_inside_the_command_string() {
        let source = r#"const r = await tools.exec_command({"cmd":"sh -c 'touch x; echo \"exit:$?\"'"});"#;
        assert_eq!(
            cell_script_command(source).as_deref(),
            Some(r#"sh -c 'touch x; echo "exit:$?"'"#)
        );
    }

    #[test]
    fn multiple_command_bearing_calls_are_listed_in_source_order() {
        let source = concat!(
            r#"await tools.exec_command({"cmd":"first"});"#,
            r#"await tools.shell({"command":"second"});"#
        );
        assert_eq!(commands_from_cell_script(source), vec!["first", "second"]);
        assert_eq!(cell_script_command(source).as_deref(), Some("first\nsecond"));
    }

    #[test]
    fn continuation_only_cell_script_has_no_command() {
        // p4_longrun's second cell: a poll of an existing shell session,
        // whose id came from the *previous* cell's output. There is no
        // command in this script to recover.
        let source = concat!(
            "const r = load(\"run\");\n",
            r#"const p = await tools.write_stdin({"session_id":r.session_id,"chars":"","yield_time_ms":30000});"#,
            "\ntext(JSON.stringify(p));"
        );
        assert!(commands_from_cell_script(source).is_empty());
        assert_eq!(cell_script_command(source), None);
    }

    #[test]
    fn javascript_object_shorthand_is_not_guessed_at() {
        let source = r#"await tools.exec_command({cmd: "echo hi"});"#;
        assert_eq!(cell_script_command(source), None);
    }

    #[test]
    fn plain_command_string_is_not_a_cell_script() {
        assert_eq!(cell_script_command("echo hi"), None);
    }

    #[test]
    fn yield_placeholder_parses_to_its_cell_id_with_no_payload() {
        let parsed =
            parse_cell_output("Script running with cell ID 1\nWall time 11.1 seconds\nOutput:\n").expect("cell");
        assert_eq!(
            parsed,
            CellOutput {
                outcome: CellOutcome::Yielded { cell_id: "1".into() },
                payload: String::new(),
            }
        );
    }

    #[test]
    fn completed_cell_parses_to_its_forwarded_payload() {
        // Content-block arrays are joined with `\n`, which leaves a blank
        // line between the harness header and the payload.
        let chunk = r#"{"chunk_id":"5ec81c","exit_code":4,"output":"tick-9\n"}"#;
        let parsed =
            parse_cell_output(&format!("Script completed\nWall time 1.9 seconds\nOutput:\n\n{chunk}")).expect("cell");
        assert_eq!(parsed.outcome, CellOutcome::Completed);
        assert_eq!(parsed.payload, chunk);
    }

    #[test]
    fn output_marker_inside_the_payload_does_not_re_split_it() {
        let parsed = parse_cell_output("Script completed\nOutput:\nfirst\nOutput:\nsecond").expect("cell");
        assert_eq!(parsed.payload, "first\nOutput:\nsecond");
    }

    #[test]
    fn legacy_prose_wrapped_exec_output_is_not_a_cell_envelope() {
        assert_eq!(
            parse_cell_output("Process exited with code 0\nOutput:\ntouch: denied.txt: Operation not permitted\n"),
            None
        );
        assert_eq!(parse_cell_output(r#"{"output":"boom\n","exit_code":1}"#), None);
        assert_eq!(parse_cell_output(""), None);
        assert_eq!(parse_cell_output("Script running with cell ID \nOutput:\n"), None);
    }

    #[test]
    fn wait_cell_id_accepts_the_string_and_number_forms() {
        assert_eq!(
            wait_target_cell_id(&json!({"cell_id":"1","yield_time_ms":30000})).as_deref(),
            Some("1")
        );
        assert_eq!(wait_target_cell_id(&json!({"cell_id":2})).as_deref(), Some("2"));
        assert_eq!(wait_target_cell_id(&json!({"yield_time_ms":30000})), None);
        assert_eq!(wait_target_cell_id(&json!({"cell_id":["1"]})), None);
    }

    #[test]
    fn wait_is_the_only_recognised_continuation_tool() {
        assert!(is_cell_wait_tool(CELL_WAIT_TOOL));
        assert!(!is_cell_wait_tool("Bash"));
        assert!(!is_cell_wait_tool("wait_agent"));
    }
}
