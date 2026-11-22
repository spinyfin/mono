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
//!   the shell command. It is not an exit-status claim — the command's
//!   terminal signal is the `exit_code` on the harness chunk the cell
//!   forwarded, which is why a `Script completed` whose chunk carries none
//!   still means "running" ([`payload_is_running_chunk`]).
//!
//! Both are documented with captured transcripts in
//! `tools/boss/docs/investigations/codex-exit-code-surfacing.md`
//! (findings 3 and 4).
//!
//! This module is pure dialect parsing. Correlating a yielded cell with its
//! `wait` continuation is the consumer's job — see
//! `boss_engine_driver::codex::rollout_calls`.

use std::iter::Peekable;

use serde_json::{Map, Value};

use crate::coerce_command_to_string;

#[path = "cell_object.rs"]
mod cell_object;

/// The tool Codex calls to poll a cell that yielded while still running.
pub const CELL_WAIT_TOOL: &str = "wait";

/// `tools.*` helpers inside a cell script that run a shell command, in the
/// order they are searched. `write_stdin` is deliberately absent: it
/// continues an existing shell session and carries no command of its own.
/// Its literal session id, when the script names one, is read by
/// [`write_stdin_session_id_from_script`].
const COMMAND_BEARING_CELL_CALLS: &[&str] = &["exec_command", "shell"];

/// Header line prefix of a yield placeholder, followed by the cell id.
const YIELD_HEADER_PREFIX: &str = "Script running with cell ID ";

/// Header line of a terminal cell result. Says the *cell* finished — never
/// that the command it ran succeeded.
const COMPLETED_HEADER: &str = "Script completed";

/// Prefix and suffix of the harness's second header line
/// (`Wall time 17.7 seconds`). Present on every captured envelope, and
/// required here so ordinary stdout whose first line happens to read
/// `Script completed` is not mistaken for a harness envelope.
const WALL_TIME_PREFIX: &str = "Wall time ";
const WALL_TIME_SUFFIX: &str = " seconds";

/// Marker line separating the harness's own header from the cell's output.
const OUTPUT_MARKER: &str = "\nOutput:";

/// Strip the OpenAI `functions.` qualifier some rollouts put on tool names
/// (`functions.exec`, `functions.wait`, `functions.write_stdin`).
pub fn dialect_tool_name(tool_name: &str) -> &str {
    tool_name.strip_prefix("functions.").unwrap_or(tool_name)
}

/// Whether `tool_name` is the cell harness's continuation poll.
pub fn is_cell_wait_tool(tool_name: &str) -> bool {
    dialect_tool_name(tool_name) == CELL_WAIT_TOOL
}

/// Whether `tool_name` is a `write_stdin` poll of an existing shell session.
pub fn is_write_stdin_tool(tool_name: &str) -> bool {
    dialect_tool_name(tool_name) == "write_stdin"
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
    /// about the command's exit status. Whether the *command* finished is
    /// [`payload_is_running_chunk`]'s question, asked of
    /// [`CellOutput::payload`].
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
///
/// This runs against *every* rollout tool output, so the header line alone
/// is not enough to claim an envelope: a command whose own stdout opens with
/// `Script completed` would have its body blanked, and one opening with
/// `Script running with cell ID 3` would be read as a yield — suppressing
/// its result and later flagging it as an abandoned command. Both are ruled
/// out by also requiring the harness's `Wall time N seconds` second line,
/// which every captured envelope carries and neither of those bodies would.
pub fn parse_cell_output(text: &str) -> Option<CellOutput> {
    let mut lines = text.lines();
    let header = lines.next()?.trim_end();
    if !is_wall_time_line(lines.next()?.trim_end()) {
        return None;
    }
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

/// Whether `line` is the harness's `Wall time N seconds` header line.
fn is_wall_time_line(line: &str) -> bool {
    line.strip_prefix(WALL_TIME_PREFIX)
        .and_then(|rest| rest.strip_suffix(WALL_TIME_SUFFIX))
        .is_some_and(|seconds| seconds.parse::<f64>().is_ok())
}

/// Whether a completed cell's forwarded payload is a harness chunk that
/// carries **no exit code** — the cell finished, but the command it started
/// is still running.
///
/// `Script completed` is a claim about the JavaScript cell, never about the
/// command (see [`CellOutcome::Completed`]), so the header cannot be the
/// terminal signal for the command. The chunk's `exit_code` is: probes 1, 2,
/// 5 and 7 of the exit-code investigation all carry one, while probe 6's
/// chunk — and probe 4's *first* poll — carry `chunk_id` and `session_id`
/// with no `exit_code` while the command runs on. Probe 6 is the canonical
/// reproduction of the reported failure, and reading its `Script completed`
/// as terminal is exactly what let a still-running command look observed.
///
/// Only a recognised harness chunk (a JSON object carrying `chunk_id`) can
/// be still-running here. A bare `7` projection (`text(r.exit_code)`) and
/// truncation-warning prose are not chunks at all.
pub fn payload_is_running_chunk(payload: &str) -> bool {
    value_is_running_chunk(&parse_json_value(payload))
}

/// Whether a structured harness chunk reports the command's exit status.
pub fn payload_reports_exit_code(payload: &str) -> bool {
    value_reports_exit_code(&parse_json_value(payload))
}

/// Whether `output` is already a structured object that reports `exit_code`.
///
/// Direct `write_stdin` polls sometimes carry the chunk as a JSON object
/// rather than a JSON string. Flattening that object keeps only a `text`
/// field, so callers must inspect the raw value before flattening.
pub fn value_reports_exit_code(value: &Value) -> bool {
    value.as_object().is_some_and(object_reports_exit_code)
}

/// Whether `output` is already a structured running chunk (`chunk_id`, no
/// `exit_code`). Same object-form gap as [`value_reports_exit_code`].
pub fn value_is_running_chunk(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|chunk| chunk.contains_key("chunk_id") && !object_reports_exit_code(chunk))
}

fn parse_json_value(payload: &str) -> Value {
    serde_json::from_str(payload).unwrap_or(Value::Null)
}

fn object_reports_exit_code(chunk: &Map<String, Value>) -> bool {
    chunk.contains_key("exit_code")
        || chunk
            .get("metadata")
            .and_then(|metadata| metadata.get("exit_code"))
            .is_some()
}

/// Whether the harness truncated a completed cell's output before forwarding
/// it, in a way that still carries the command's terminal signal.
///
/// The warning prefix alone is not evidence the *command* finished — outer
/// truncation is applied to the tool-output block regardless of whether the
/// forwarded chunk reports an exit code, so a poll whose command is still
/// running but whose large partial output triggered truncation carries the
/// same prefix (see `cell.rs`'s own
/// `a_payload_that_is_not_a_harness_chunk_is_never_still_running` fixture).
/// The investigation doc records that `exit_code` survives outer truncation
/// in text form even though the payload as a whole no longer parses as
/// JSON, so require its literal occurrence too: only a truncated payload
/// that still shows `"exit_code"` is terminal. A truncated payload with no
/// exit-code evidence falls through to the unconfirmed-result case instead,
/// leaving the call open.
pub fn payload_is_truncated_output(payload: &str) -> bool {
    payload.starts_with("Warning: truncated output (original token count:") && payload.contains("\"exit_code\"")
}

/// The shell-session identifier carried by a structured harness chunk.
///
/// Terminal chunks carry it too, so a later polling cell can associate the
/// terminal result with the command that started the session.
pub fn payload_session_id(payload: &str) -> Option<String> {
    serde_json::from_str::<Value>(payload)
        .ok()?
        .get("session_id")
        .and_then(|session_id| match session_id {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
}

/// Whether `source` contains a command-bearing `exec_command` / `shell` call,
/// even when the argument object cannot be parsed far enough to recover the
/// command string. Used to distinguish a command cell from a pure
/// `write_stdin` continuation (or a compute-only cell) without requiring
/// JSON-quoted keys.
pub fn script_issues_command(source: &str) -> bool {
    next_command_bearing_call(source).is_some()
}

/// Whether `source` is a cell that only polls an existing shell session.
///
/// A wrapper that starts a command and then polls it with `write_stdin` is
/// command-bearing, not this. A plain argv/string `exec` input (`echo hi`)
/// is also not this: it never mentions `write_stdin` and still issues a
/// command.
pub fn script_is_continuation_only(source: &str) -> bool {
    find_js_call(source, "write_stdin").is_some() && !script_issues_command(source)
}

/// Cell builtins that compute, print or stash data without running a command.
const CELL_BUILTIN_CALLS: &[&str] = &["text", "load", "store", "notify"];

/// `tools.*` helpers that cannot start a shell command. This list is the
/// contract for compute-only classification: a `tools.<name>` access is
/// command-free only when `name` is listed here. Extend it when Codex grows
/// another non-shell helper; unknown names, computed `tools[...]`, and a
/// bare `tools` identifier (aliasing) stay command-capable.
const NON_COMMAND_CELL_TOOLS: &[&str] = &["write_stdin", "apply_patch", "view_image", "update_plan"];

/// Whether `source` is positively a cell script that runs no command: it
/// calls a cell builtin (`text`, `load`, `store`, `notify`) or a known
/// non-shell `tools.*` helper, and never reaches a command-bearing tool
/// (`exec_command`, `shell`, computed `tools[...]`, an unknown `tools.*`
/// name, or a `tools` alias).
///
/// Such a cell is ordinary code-mode work, not a shell command, so it must not
/// be announced or drained as one with its JavaScript as the "command". A
/// string that does not look like a cell (`echo hi`) is not this.
pub fn script_is_compute_only(source: &str) -> bool {
    if script_mentions_command_tool(source) {
        return false;
    }
    CELL_BUILTIN_CALLS
        .iter()
        .any(|name| find_js_call(source, name).is_some())
        || NON_COMMAND_CELL_TOOLS
            .iter()
            .any(|name| *name != "write_stdin" && find_js_call(source, name).is_some())
}

/// Whether `source` names a command-bearing helper anywhere in code (not only
/// as a call), or reaches `tools` in a way that could dispatch a shell:
/// computed property access, an unknown member, or a bare identifier that
/// could be aliased. `tools.<known non-command tool>` is not this.
fn script_mentions_command_tool(source: &str) -> bool {
    let mut lexer = JsLexer::new(source);
    while let Some((at, ch)) = lexer.next_code() {
        if !js_ident_part(ch) {
            continue;
        }
        let rest = &source[at..];
        let ident_len = rest.find(|c: char| !js_ident_part(c)).unwrap_or(rest.len());
        let ident = &rest[..ident_len];
        if COMMAND_BEARING_CELL_CALLS.contains(&ident) {
            return true;
        }
        if ident == "tools" {
            let after = rest[ident_len..].trim_start();
            if let Some(member_src) = after.strip_prefix('.') {
                let member_len = member_src.find(|c: char| !js_ident_part(c)).unwrap_or(member_src.len());
                let member = &member_src[..member_len];
                if !member.is_empty() && NON_COMMAND_CELL_TOOLS.contains(&member) {
                    continue;
                }
            }
            return true;
        }
    }
    false
}

/// The shell command(s) a cell script issues, in source order.
///
/// The cell's tool input is JavaScript source, so it never parses as JSON
/// and the command lives inside a `tools.exec_command({…})` argument. That
/// argument object is JSON in some transcripts and JavaScript object
/// shorthand (`{cmd:"echo hi"}`) in the 0.160.1 dialect; both are recovered
/// here by decoding static literals, including bare property names and
/// single-quoted strings. Dynamic expressions yield nothing rather than a
/// guessed command.
///
/// Empty means "no recoverable command string in this script": a pure
/// continuation cell (`tools.write_stdin`), a `store`/`load` cell, or a
/// command-bearing call whose argument this parser does not recognise.
/// [`script_issues_command`] is the "did this cell start a command" bit;
/// callers must not read an empty vec as "the cell ran no command".
pub fn commands_from_cell_script(source: &str) -> Vec<String> {
    let mut commands = Vec::new();
    // One lexer over the original source so template / brace state is
    // retained between matches. Re-slicing after a call found inside
    // `${...}` would restart with empty template_depths and treat the
    // template's closing backtick as an opener, swallowing later commands.
    for (at, needle_len) in js_call_sites(source, COMMAND_BEARING_CELL_CALLS) {
        let after = source.get(at + needle_len..).unwrap_or("");
        if let Some(object) = balanced_object_literal(after)
            && let Some(parsed) = cell_object::parse_literal_object(object)
            && let Some(command) = parsed.get("cmd").or_else(|| parsed.get("command"))
        {
            commands.push(coerce_command_to_string(command));
        }
    }
    commands
}

/// The shell-session id a `write_stdin` cell script polls, when the argument
/// object names one. Used to bind a continuation cell to the command that
/// started the session even when the cell's output dropped `session_id`
/// (`text(p.output)`).
pub fn write_stdin_session_id_from_script(source: &str) -> Option<String> {
    for (at, needle_len) in js_call_sites(source, &["write_stdin"]) {
        let after = source.get(at + needle_len..).unwrap_or("");
        if let Some(object) = balanced_object_literal(after)
            && let Some(session_id) = object_literal_string_field(object, "session_id")
        {
            return Some(session_id);
        }
    }
    None
}

/// `session_id` on a canonicalised `write_stdin` tool input (JSON args, or
/// the id recovered from a continuation cell script).
pub fn write_stdin_session_id(tool_input: &Value) -> Option<String> {
    match tool_input.get("session_id") {
        Some(Value::String(text)) => Some(text.trim().to_owned()).filter(|text| !text.is_empty()),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => tool_input
            .get("script")
            .and_then(Value::as_str)
            .and_then(write_stdin_session_id_from_script),
    }
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

/// Offset and needle length of the earliest command-bearing call in
/// `source`. Matches `tools.exec_command(` and the bare `exec_command(`
/// form some cells emit, so a script mixing helpers is read in source order.
/// Requires an identifier boundary before the name (`.` is allowed) and
/// ignores matches inside string literals, so `spawnshell(` and `"shell("`
/// in a string are not command-bearing.
fn next_command_bearing_call(source: &str) -> Option<(usize, usize)> {
    js_call_sites(source, COMMAND_BEARING_CELL_CALLS).into_iter().next()
}

/// Every `name(` call site in `source`, in source order, from one lexer
/// pass so template and brace state is retained between matches.
fn js_call_sites(source: &str, names: &[&str]) -> Vec<(usize, usize)> {
    let mut sites = Vec::new();
    let mut lexer = JsLexer::new(source);
    while let Some((at, _)) = lexer.next_code() {
        for name in names {
            if js_call_at(source, at, name) {
                sites.push((at, name.len() + 1));
                break;
            }
        }
    }
    sites
}

/// Byte offset of `name(` at an identifier boundary, skipping string
/// literals and comments. `.` may precede the name so `tools.write_stdin(`
/// matches; `spawnshell(`, a quoted `"shell("`, and a commented-out
/// `shell(` do not.
fn find_js_call(source: &str, name: &str) -> Option<usize> {
    let mut lexer = JsLexer::new(source);
    while let Some((at, _)) = lexer.next_code() {
        if js_call_at(source, at, name) {
            return Some(at);
        }
    }
    None
}

fn js_call_at(source: &str, at: usize, name: &str) -> bool {
    let Some(rest) = source.get(at..) else {
        return false;
    };
    rest.starts_with(name)
        && rest.get(name.len()..).is_some_and(|tail| tail.starts_with('('))
        && js_ident_boundary_at(source, at)
}

fn js_ident_boundary_at(source: &str, at: usize) -> bool {
    let Some(preceding) = source.get(..at).and_then(|prefix| prefix.chars().next_back()) else {
        return true;
    };
    !preceding.is_ascii_alphanumeric() && preceding != '_' && preceding != '$'
}

/// Last significant token seen by [`JsLexer`], used only to tell a regex
/// literal from division.
#[derive(Clone, Copy)]
enum LastJsToken {
    Start,
    Word { start: usize, end: usize },
    Operand,
    RegexPunct,
}

/// A single forward pass over JavaScript cell source that yields only *code*:
/// string, template-text, comment and regex-literal bodies are skipped, while
/// template `${…}` substitutions are scanned as code (tracking brace depth).
/// The previous significant token is tracked as the scan advances, so a `/`
/// is classified as a regex opener or division from state the scanner already
/// holds rather than by re-lexing the prefix.
///
/// Each item is the byte offset and first character of an identifier/number
/// token (consumed whole) or of a single punctuation character; whitespace is
/// not reported. When the previous token is unclear a slash is treated as
/// division, so a later command stays visible.
struct JsLexer<'a> {
    source: &'a str,
    chars: Peekable<std::str::CharIndices<'a>>,
    last: LastJsToken,
    depth: usize,
    /// Brace depth at which each open template `${` substitution began.
    template_depths: Vec<usize>,
}

impl<'a> JsLexer<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            chars: source.char_indices().peekable(),
            last: LastJsToken::Start,
            depth: 0,
            template_depths: Vec::new(),
        }
    }

    /// Whether a `/` here begins a regex literal rather than a division. A
    /// regex can follow the start of input, an operator or opening
    /// punctuation, or a keyword such as `return`; it cannot follow an
    /// identifier, number, string or closing bracket.
    fn regex_allowed(&self) -> bool {
        match self.last {
            LastJsToken::Start | LastJsToken::RegexPunct => true,
            LastJsToken::Word { start, end } => matches!(
                self.source.get(start..end),
                Some(
                    "return"
                        | "typeof"
                        | "case"
                        | "do"
                        | "else"
                        | "in"
                        | "of"
                        | "void"
                        | "delete"
                        | "throw"
                        | "new"
                        | "yield"
                        | "await"
                )
            ),
            LastJsToken::Operand => false,
        }
    }

    /// Skip template-literal text up to the closing backtick, or up to and
    /// including a `${` that opens a code substitution.
    fn skip_template_text(&mut self) {
        let mut escaped = false;
        while let Some((_, ch)) = self.chars.next() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '`' => {
                    self.last = LastJsToken::Operand;
                    return;
                }
                '$' if self.chars.peek().map(|&(_, next)| next) == Some('{') => {
                    self.chars.next();
                    self.template_depths.push(self.depth);
                    self.depth += 1;
                    self.last = LastJsToken::RegexPunct;
                    return;
                }
                _ => {}
            }
        }
    }

    fn next_code(&mut self) -> Option<(usize, char)> {
        while let Some((at, ch)) = self.chars.next() {
            match ch {
                '/' => match self.chars.peek().map(|&(_, next)| next) {
                    Some('/') => {
                        self.chars.next();
                        skip_js_line_comment(&mut self.chars);
                    }
                    Some('*') => {
                        self.chars.next();
                        skip_js_block_comment(&mut self.chars);
                    }
                    _ if self.regex_allowed() => {
                        skip_js_regex_literal(&mut self.chars);
                        self.last = LastJsToken::Operand;
                    }
                    _ => {
                        self.last = LastJsToken::RegexPunct;
                        return Some((at, ch));
                    }
                },
                '"' | '\'' => {
                    skip_js_quoted_string(&mut self.chars, ch, false);
                    self.last = LastJsToken::Operand;
                }
                '`' => self.skip_template_text(),
                ch if ch.is_whitespace() => {}
                '{' => {
                    self.depth += 1;
                    self.last = LastJsToken::RegexPunct;
                    return Some((at, ch));
                }
                '}' => {
                    self.depth = self.depth.saturating_sub(1);
                    if self.template_depths.last() == Some(&self.depth) {
                        self.template_depths.pop();
                        self.skip_template_text();
                    } else {
                        // Closing a block or object is an operand, so `/`
                        // after `}` is division rather than a regex that
                        // would skip later commands. Unclear slashes fail
                        // toward division so a later command stays visible.
                        self.last = LastJsToken::Operand;
                        return Some((at, ch));
                    }
                }
                ')' | ']' => {
                    self.last = LastJsToken::Operand;
                    return Some((at, ch));
                }
                ch if js_ident_part(ch) => {
                    let mut end = at + ch.len_utf8();
                    while let Some(&(next_at, next)) = self.chars.peek() {
                        if !js_ident_part(next) {
                            break;
                        }
                        end = next_at + next.len_utf8();
                        self.chars.next();
                    }
                    self.last = if ch.is_ascii_digit() {
                        LastJsToken::Operand
                    } else {
                        LastJsToken::Word { start: at, end }
                    };
                    return Some((at, ch));
                }
                ch if ch.is_ascii() => {
                    self.last = LastJsToken::RegexPunct;
                    return Some((at, ch));
                }
                // Non-ASCII punctuation is not a reliable regex opener; fail
                // toward division so a later command stays visible.
                _ => {
                    self.last = LastJsToken::Operand;
                    return Some((at, ch));
                }
            }
        }
        None
    }
}

fn js_ident_part(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

/// Skip a regex literal body (the opening `/` is already consumed): ends at
/// the first unescaped `/` outside a `[...]` class, or at a newline.
fn skip_js_regex_literal(chars: &mut Peekable<impl Iterator<Item = (usize, char)>>) {
    let mut in_class = false;
    while let Some((_, ch)) = chars.next() {
        match ch {
            '\\' => {
                chars.next();
            }
            '[' => in_class = true,
            ']' => in_class = false,
            '/' if !in_class => return,
            '\n' => return,
            _ => {}
        }
    }
}

fn skip_js_line_comment(chars: &mut Peekable<impl Iterator<Item = (usize, char)>>) {
    for (_, ch) in chars.by_ref() {
        if ch == '\n' {
            break;
        }
    }
}

fn skip_js_block_comment(chars: &mut Peekable<impl Iterator<Item = (usize, char)>>) {
    let mut prev_star = false;
    for (_, ch) in chars.by_ref() {
        if prev_star && ch == '/' {
            return;
        }
        prev_star = ch == '*';
    }
}

/// Skip a JS string whose opening quote has already been consumed.
/// Single and double quoted strings end at an unescaped newline (they are
/// a SyntaxError in JS; treating the newline as the end keeps later code
/// visible). Template literals may contain newlines.
fn skip_js_quoted_string(chars: &mut Peekable<impl Iterator<Item = (usize, char)>>, quote: char, allow_newline: bool) {
    let mut escaped = false;
    for (_, ch) in chars.by_ref() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            break;
        }
        if !allow_newline && (ch == '\n' || ch == '\r') {
            break;
        }
    }
}

/// A string/number field on a JSON object or a JavaScript object literal
/// (`{cmd:"echo hi"}`, `{session_id:8467}`). Nested objects/arrays are
/// skipped; a missing or non-scalar field yields `None`.
fn object_literal_string_field(object: &str, field: &str) -> Option<String> {
    let parsed = cell_object::parse_literal_object(object)?;
    match parsed.get(field)? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        other => Some(coerce_command_to_string(other)),
    }
    .filter(|text| !text.is_empty())
}

/// Take the balanced `{…}` object literal at the start of `source` (after
/// any leading whitespace), respecting quoted string literals, comments,
/// regex literals, and escapes.
fn balanced_object_literal(source: &str) -> Option<&str> {
    let trimmed = source.trim_start();
    let offset = source.len() - trimmed.len();
    if !trimmed.starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    let mut lexer = JsLexer::new(trimmed);
    while let Some((index, ch)) = lexer.next_code() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
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
    fn javascript_bare_property_name_preserves_the_literal_command() {
        let source = r#"await tools.exec_command({cmd: "echo hi"});"#;
        assert_eq!(cell_script_command(source).as_deref(), Some("echo hi"));
        assert!(script_issues_command(source));
        let source = r#"await tools.exec_command({cmd: 'echo }', yield_time_ms: 1000});"#;
        assert_eq!(cell_script_command(source).as_deref(), Some("echo }"));
        let compact = r#"text(await tools.exec_command({cmd:"echo hello-boss-qual"}));"#;
        assert_eq!(cell_script_command(compact).as_deref(), Some("echo hello-boss-qual"));
    }

    #[test]
    fn javascript_object_shorthand_survives_sibling_fields() {
        let source = r#"text(await tools.exec_command({cmd:"sleep 45 && echo late-output",yield_time_ms:1000}));"#;
        assert_eq!(
            cell_script_command(source).as_deref(),
            Some("sleep 45 && echo late-output")
        );
    }

    #[test]
    fn javascript_dynamic_command_is_not_guessed_at() {
        for source in [
            "await tools.exec_command({cmd});",
            "await tools.exec_command({cmd: command});",
            "await tools.exec_command({cmd: 'boss ' + verb});",
        ] {
            assert_eq!(cell_script_command(source), None);
        }
    }

    #[test]
    fn bare_exec_command_call_without_tools_prefix_is_still_command_bearing() {
        let source = r#"const r = await exec_command({"cmd":"jj status"});"#;
        assert_eq!(cell_script_command(source).as_deref(), Some("jj status"));
        assert!(script_issues_command(source));
    }

    #[test]
    fn write_stdin_session_id_reads_json_and_shorthand_object_literals() {
        assert_eq!(
            write_stdin_session_id_from_script(r#"const p = await tools.write_stdin({"session_id":8467,"chars":""});"#)
                .as_deref(),
            Some("8467")
        );
        assert_eq!(
            write_stdin_session_id_from_script(
                r#"text(await tools.write_stdin({session_id:80143,chars:"",yield_time_ms:30000}));"#
            )
            .as_deref(),
            Some("80143")
        );
        assert_eq!(
            write_stdin_session_id_from_script("const r = await tools.exec_command({cmd:\"echo hi\"});"),
            None
        );
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
        let parsed = parse_cell_output("Script completed\nWall time 0.2 seconds\nOutput:\nfirst\nOutput:\nsecond")
            .expect("cell");
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
        assert_eq!(
            parse_cell_output("Script running with cell ID \nWall time 1.0 seconds\nOutput:\n"),
            None
        );
    }

    #[test]
    fn ordinary_stdout_that_merely_starts_with_a_harness_header_is_not_an_envelope() {
        // Both bodies a command could plausibly print itself. Without the
        // `Wall time N seconds` line neither is a harness envelope: the
        // first would otherwise have its body blanked before the denial
        // scan reads it, and the second would suppress the command's result
        // and later flag it as abandoned.
        assert_eq!(
            parse_cell_output("Script completed\nall 12 checks passed\ndone\n"),
            None
        );
        assert_eq!(
            parse_cell_output("Script running with cell ID 3\nstarting worker\n"),
            None
        );
        // A plausible-looking but non-numeric wall time is not the harness's.
        assert_eq!(parse_cell_output("Script completed\nWall time later\nOutput:\nx"), None);
    }

    #[test]
    fn a_completed_cell_whose_chunk_carries_no_exit_code_is_still_running() {
        // Probe 6 verbatim: the cell finished, the command did not.
        assert!(payload_is_running_chunk(
            r#"{"chunk_id":"d0540d","wall_time_seconds":30.001035083,"session_id":8467,"original_token_count":14,"output":"tick-1\ntick-2\n"}"#
        ));
    }

    #[test]
    fn payload_session_id_reads_numeric_and_string_session_handles() {
        assert_eq!(
            payload_session_id(r#"{"chunk_id":"chunk","session_id":8467,"output":"tick"}"#).as_deref(),
            Some("8467")
        );
        assert_eq!(
            payload_session_id(r#"{"chunk_id":"chunk","session_id":"session-1","exit_code":0}"#).as_deref(),
            Some("session-1")
        );
        assert_eq!(payload_session_id(r#"{"chunk_id":"chunk","output":"tick"}"#), None);
    }

    #[test]
    fn a_chunk_that_reports_an_exit_code_is_terminal() {
        assert!(!payload_is_running_chunk(
            r#"{"chunk_id":"5ec81c","exit_code":4,"output":"tick-9\n"}"#
        ));
        assert!(!payload_is_running_chunk(
            r#"{"chunk_id":"ab","exit_code":0,"output":"ok\n"}"#
        ));
        assert!(!payload_is_running_chunk(
            r#"{"chunk_id":"ab","metadata":{"exit_code":0},"output":"ok\n"}"#
        ));
    }

    #[test]
    fn payload_reports_exit_code_checks_top_level_and_metadata_fields() {
        assert!(payload_reports_exit_code(r#"{"exit_code":0}"#));
        assert!(payload_reports_exit_code(r#"{"metadata":{"exit_code":4}}"#));
        assert!(!payload_reports_exit_code(r#"{"chunk_id":"running"}"#));
    }

    #[test]
    fn truncation_warning_with_exit_code_evidence_marks_a_completed_payload_as_terminal() {
        assert!(payload_is_truncated_output(
            "Warning: truncated output (original token count: 11827)\n\ntouch: x: denied\n\"exit_code\":1\n"
        ));
        assert!(!payload_is_truncated_output("partial output\n"));
    }

    #[test]
    fn truncation_warning_with_no_exit_code_evidence_is_not_terminal() {
        // A still-running poll whose large partial output triggered outer
        // truncation carries the same warning prefix as a genuinely
        // finished command, but with no `exit_code` surviving in the text —
        // this must not be read as terminal (probe 3's `chunk_id`-only
        // shape, also asserted by
        // `a_payload_that_is_not_a_harness_chunk_is_never_still_running`).
        assert!(!payload_is_truncated_output(
            "Warning: truncated output (original token count: 11827)\n\n{\"chunk_id\":\"5ce387\"}"
        ));
    }

    #[test]
    fn a_payload_that_is_not_a_harness_chunk_is_never_still_running() {
        // Probe 8 projects the exit code alone; probe 3's outer truncation
        // warning leaves the payload unparseable. Neither is a running chunk.
        assert!(!payload_is_running_chunk("7"));
        assert!(!payload_is_running_chunk(
            "Warning: truncated output (original token count: 11827)\n\n{\"chunk_id\":\"5ce387\"}"
        ));
        assert!(!payload_is_running_chunk(""));
        // Probe 2's chunk has no `chunk_id` but does report an exit code.
        assert!(!payload_is_running_chunk(r#"{"exit_code":9,"output":"STEP-A\n"}"#));
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
    fn wait_and_write_stdin_are_the_recognised_continuation_tools() {
        assert!(is_cell_wait_tool(CELL_WAIT_TOOL));
        assert!(is_cell_wait_tool("functions.wait"));
        assert!(!is_cell_wait_tool("Bash"));
        assert!(!is_cell_wait_tool("wait_agent"));
        assert!(is_write_stdin_tool("write_stdin"));
        assert!(is_write_stdin_tool("functions.write_stdin"));
        assert!(!is_write_stdin_tool("exec"));
    }

    #[test]
    fn continuation_only_script_does_not_issue_a_command() {
        let source = concat!(
            "const r = load(\"run\");\n",
            r#"const p = await tools.write_stdin({"session_id":r.session_id,"chars":"","yield_time_ms":30000});"#,
        );
        assert!(!script_issues_command(source));
        assert!(script_is_continuation_only(source));
        assert!(!script_is_continuation_only("echo rollout"));
        assert!(!script_is_continuation_only("sleep 999"));
        let wrapper = concat!(
            r#"let r = await tools.exec_command({cmd: "bazel test //tools/boss/engine/...", yield_time_ms: 30000});"#,
            r#"while (!("exit_code" in r)) { r = await tools.write_stdin({session_id: r.session_id, chars: ""}); }"#,
            "text(JSON.stringify(r));",
        );
        assert!(script_issues_command(wrapper));
        assert!(!script_is_continuation_only(wrapper));
        assert_eq!(
            cell_script_command(wrapper).as_deref(),
            Some("bazel test //tools/boss/engine/...")
        );
    }

    #[test]
    fn javascript_object_expressions_are_not_guessed_as_a_command() {
        assert_eq!(
            cell_script_command(r#"await tools.exec_command({cmd: "cd " + dir + " && make"});"#),
            None
        );
        assert_eq!(
            cell_script_command(r#"await tools.exec_command({cmd: `cd ${dir}`});"#),
            None
        );
        assert_eq!(
            cell_script_command(r#"await tools.exec_command({cmd: cond ? "a" : "b"});"#),
            None
        );
        assert!(script_issues_command(
            r#"await tools.exec_command({cmd: "cd " + dir + " && make"});"#
        ));
    }

    #[test]
    fn command_bearing_calls_require_an_identifier_boundary() {
        assert!(!script_issues_command(r#"await tools.spawnshell({cmd:"x"});"#));
        let poll = concat!(
            r#"const p = await tools.write_stdin({session_id:8467,chars:""});"#,
            r#"const note = "shell(";"#,
        );
        assert!(script_is_continuation_only(poll));
        assert!(!script_issues_command(poll));
    }

    #[test]
    fn an_apostrophe_in_a_comment_does_not_hide_a_later_exec_command() {
        let source = concat!(
            r#"await tools.write_stdin({session_id:8467,chars:""});"#,
            "\n// Don't wait\n",
            r#"await tools.exec_command({cmd:"sleep 999"});"#,
        );
        assert!(script_issues_command(source));
        assert!(!script_is_continuation_only(source));
        assert_eq!(cell_script_command(source).as_deref(), Some("sleep 999"));
    }

    #[test]
    fn a_dont_comment_before_a_lone_exec_command_still_recovers_the_cmd() {
        let source = concat!("// don't\n", r#"await tools.exec_command({cmd:"echo hi"});"#);
        assert!(script_issues_command(source));
        assert_eq!(cell_script_command(source).as_deref(), Some("echo hi"));
    }

    #[test]
    fn a_pure_poll_with_commented_out_shell_text_is_still_a_continuation() {
        let source = concat!(
            r#"await tools.write_stdin({session_id:8467,chars:""});"#,
            "\n// await tools.shell({cmd:\"echo hi\"});\n",
            "/* also didn't run exec_command({cmd:\"sleep 1\"}); */",
        );
        assert!(!script_issues_command(source));
        assert!(script_is_continuation_only(source));
        assert_eq!(cell_script_command(source), None);
    }

    #[test]
    fn js_quoted_cmd_decodes_unicode_and_hex_escapes() {
        assert_eq!(
            cell_script_command(r#"await tools.exec_command({cmd:"echo \u0041\u{42}\x43"});"#).as_deref(),
            Some("echo ABC")
        );
        assert_eq!(
            cell_script_command(r#"await tools.exec_command({cmd:'a\0b\tc'});"#).as_deref(),
            Some("a\0b\tc")
        );
        assert_eq!(
            cell_script_command(r#"await tools.exec_command({env:"a\.b", cmd:"echo \$HOME \q"});"#).as_deref(),
            Some("echo $HOME q")
        );
        for malformed in [
            r#"echo \x4"#,
            r#"echo \u00G1"#,
            r#"echo \u{110000}"#,
            r#"echo \7"#,
            r#"echo \01"#,
        ] {
            let script = format!(r#"await tools.exec_command({{cmd:"{malformed}"}});"#);
            assert_eq!(cell_script_command(&script), None, "{malformed}");
            assert!(script_issues_command(&script));
        }
    }

    const REGEX_SLASH_CELL: &str = r#"await tools.write_stdin({session_id:8467,chars:""}); const n = "a/b".replace(/\//g, "_"); await tools.exec_command({cmd:"sleep 999"});"#;

    #[test]
    fn a_regex_literal_containing_slashes_does_not_hide_a_later_exec_command() {
        assert!(script_issues_command(REGEX_SLASH_CELL));
        assert!(!script_is_continuation_only(REGEX_SLASH_CELL));
        assert_eq!(cell_script_command(REGEX_SLASH_CELL).as_deref(), Some("sleep 999"));
    }

    #[test]
    fn a_regex_that_looks_like_a_block_comment_does_not_hide_later_lines() {
        let script = "await tools.write_stdin({session_id:8467,chars:\"\"});\nconst r = s.replace(/\\/*/g, \"\");\nawait tools.exec_command({cmd:\"sleep 999\"});\n";
        assert!(!script_is_continuation_only(script));
        assert_eq!(cell_script_command(script).as_deref(), Some("sleep 999"));
    }

    #[test]
    fn division_after_a_block_comment_does_not_hide_a_later_exec_command() {
        let source = concat!(
            r#"await tools.write_stdin({session_id:8467,chars:""});"#,
            r#" const ratio = 4 /* units */ / 2; "#,
            r#"await tools.exec_command({cmd:"sleep 999"});"#,
        );
        assert!(script_issues_command(source));
        assert!(!script_is_continuation_only(source));
        assert_eq!(cell_script_command(source).as_deref(), Some("sleep 999"));
    }

    #[test]
    fn division_after_a_non_ascii_identifier_does_not_hide_a_later_exec_command() {
        let source = concat!(
            r#"await tools.write_stdin({session_id:8467,chars:""});"#,
            " const n = café2 / 2; ",
            r#"await tools.exec_command({cmd:"sleep 999"});"#,
        );
        assert!(script_issues_command(source));
        assert!(!script_is_continuation_only(source));
        assert_eq!(cell_script_command(source).as_deref(), Some("sleep 999"));
        let pi = concat!(
            r#"await tools.write_stdin({session_id:8467,chars:""});"#,
            " const n = πx / 2; ",
            r#"await tools.exec_command({cmd:"sleep 999"});"#,
        );
        assert!(script_issues_command(pi));
        assert_eq!(cell_script_command(pi).as_deref(), Some("sleep 999"));
    }

    #[test]
    fn a_command_inside_a_template_does_not_drop_a_later_command() {
        let source = concat!(
            r#"text(`${await tools.exec_command({cmd:"echo hi"})}`);"#,
            r#" await tools.exec_command({cmd:"sleep 999"});"#,
        );
        assert!(script_issues_command(source));
        assert_eq!(commands_from_cell_script(source), vec!["echo hi", "sleep 999"]);
        assert_eq!(cell_script_command(source).as_deref(), Some("echo hi\nsleep 999"));
    }

    #[test]
    fn nested_object_in_a_template_substitution_does_not_hide_a_later_exec_command() {
        let source = concat!(
            r#"await tools.write_stdin({session_id:8467,chars:""});"#,
            " text(`${ {a:1} }`); ",
            r#"await tools.exec_command({cmd:"sleep 999"});"#,
        );
        assert!(script_issues_command(source));
        assert!(!script_is_continuation_only(source));
        assert_eq!(cell_script_command(source).as_deref(), Some("sleep 999"));
    }

    #[test]
    fn division_after_a_closing_brace_does_not_hide_a_later_exec_command() {
        let source = concat!(
            r#"await tools.write_stdin({session_id:8467,chars:""});"#,
            " const n = {a:1} / 2; ",
            r#"await tools.exec_command({cmd:"sleep 999"});"#,
        );
        assert!(script_issues_command(source));
        assert!(!script_is_continuation_only(source));
        assert_eq!(cell_script_command(source).as_deref(), Some("sleep 999"));
    }

    #[test]
    fn a_non_shell_tools_cell_without_a_builtin_is_compute_only() {
        let source = r#"await tools.view_image({path:"/tmp/image.png"});"#;
        assert!(script_is_compute_only(source));
        assert!(!script_issues_command(source));
        assert_eq!(cell_script_command(source), None);
        assert!(!script_is_compute_only("echo hi"));
        assert!(!script_is_compute_only(
            r#"await tools.exec_command({cmd:"sleep 999"});"#
        ));
    }

    #[test]
    fn a_tools_alias_is_treated_as_command_capable() {
        assert!(!script_is_compute_only("const t = tools; text(t);"));
        assert!(!script_is_compute_only(
            r#"text(await tools["view_image"]({path:"x"}));"#
        ));
        assert!(!script_is_compute_only(r#"text(await tools.unknown_helper({}));"#));
        assert!(script_is_compute_only(r#"text(load("r").output);"#));
        assert!(script_is_compute_only(
            r#"await tools.apply_patch("*** Begin Patch\n*** End Patch");"#
        ));
    }

    #[test]
    fn value_helpers_read_object_form_exit_and_running_chunks() {
        assert!(value_reports_exit_code(
            &json!({"chunk_id":"done","session_id":8467,"exit_code":0,"output":"done"})
        ));
        assert!(!value_is_running_chunk(
            &json!({"chunk_id":"done","session_id":8467,"exit_code":0,"output":"done"})
        ));
        assert!(value_is_running_chunk(
            &json!({"chunk_id":"d0540d","session_id":8467,"output":"tick"})
        ));
        assert!(!value_reports_exit_code(
            &json!({"chunk_id":"d0540d","session_id":8467,"output":"tick"})
        ));
        assert!(!value_reports_exit_code(&json!({"type":"output_text","text":"done"})));
    }
}
