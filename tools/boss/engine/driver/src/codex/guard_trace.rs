//! Per-run record of what Boss's Codex `PreToolUse` guards actually did.
//!
//! # The gap this closes
//!
//! On the Claude path, "did the guard run, and what did it decide?" is
//! answerable after the fact: `hook_success`, `PreToolUse:Bash` and the
//! `{"decision": …}` attachment all land in the session transcript. On the
//! Codex path there was **no such signal anywhere** — the rollout JSONL the
//! engine tails carries no hook record, an approved call leaves no trace at
//! all, and a blocked one shows up only as prose inside the cell's
//! `custom_tool_call_output`. Codex's own hook failures are silent and
//! fail-open (an untrusted hook is skipped, a missing handler produces no
//! diagnostic), so "guards armed" and "guards inert" were indistinguishable
//! from anything Boss could observe.
//!
//! # Mechanism
//!
//! Every guard the Codex driver materialises is invoked through a shell
//! wrapper that runs [`GUARD_TRACE_SHIM_SCRIPT`] instead of the guard
//! directly. The shim:
//!
//! 1. runs the real guard with the payload on stdin,
//! 2. appends one JSON line per invocation to
//!    `$CODEX_HOME/`[`GUARD_TRACE_FILENAME`] — guard name, tool name,
//!    decision, reason head,
//! 3. **translates** that decision into the vocabulary Codex accepts — the
//!    guards are written in Claude Code's dialect, which Codex refuses, and a
//!    refused response is fail-open (see [`super::decision`]), and
//! 4. **fails closed**: a guard that crashes, exceeds
//!    `BOSS_GUARD_TIMEOUT_SECONDS`, exits non-zero or prints something that is
//!    not a decision becomes a `block` with a loud reason, recorded as
//!    `guard_error`. Codex would otherwise treat that silence as approval.
//!
//! The chain is anchored at both joints: the hook-trust attestation
//! content-binds each wrapper, the wrapper verifies the shim's sha256 before
//! `exec`ing it ([`GUARD_SHIM_SHA256_ENV`]), and the shim verifies the guard's
//! sha256 on every invocation ([`GUARD_SHA256_ENV`]).
//!
//! The engine reads the file at each turn boundary
//! ([`crate::codex::progress`]) and reports it as a `WorkerEvent::Notification`
//! — a summary when guards ran, and a distinct, loud marker when tool calls
//! were observed but **no** guard record exists, which is the observable
//! signature of the silent fail-open the design doc could only describe.
//!
//! Evidence and payload captures:
//! `tools/boss/docs/investigations/codex-pretooluse-guard-coverage-2026-07-29.md`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use boss_ssh_transport::shell_quote;
use serde::Deserialize;

/// Filename of the per-run guard-decision log, written under `CODEX_HOME`.
pub const GUARD_TRACE_FILENAME: &str = "guard-trace.jsonl";

/// Filename of the shim under `$CODEX_HOME/guards/`.
pub(super) const GUARD_TRACE_SHIM_FILENAME: &str = "boss_guard_trace.py";

/// Env var naming the trace file, set by each guard's wrapper.
const GUARD_TRACE_ENV: &str = "BOSS_GUARD_TRACE";

/// Env var naming the guard whose decision is being recorded.
const GUARD_NAME_ENV: &str = "BOSS_GUARD_NAME";

/// Env var carrying the expected `sha256:<hex>` of the guard executable.
///
/// Wrapping a guard would otherwise cost the hook-trust attestation its
/// content binding: the gate hashes the file at the `command` path, which is
/// now the wrapper, not the guard. Re-binding it here restores that binding
/// for the guard body and, for the guard body specifically, is stronger than
/// the arming-time hash it replaces — the shim re-verifies the guard's bytes on
/// **every invocation**, so a guard edited after arming is refused rather than
/// merely un-attested.
const GUARD_SHA256_ENV: &str = "BOSS_GUARD_SHA256";

/// Env var carrying the expected `sha256:<hex>` of the trace shim itself,
/// recorded for the trace line the wrapper writes when the check fails.
///
/// The shim sits between the attested wrapper and the content-bound guard, so
/// without this check it would be the one link in the chain nothing covers:
/// Codex's `trusted_hash` binds the hook *identity* (command path, matcher,
/// timeout) and `verify_attestation`'s content hash binds the wrapper, neither
/// of which says anything about `boss_guard_trace.py`'s bytes. Swapping the
/// shim for `print('{"decision":"approve"}')` would neutralise every guard with
/// both hashes still valid. The wrapper — which *is* attestation-bound —
/// therefore verifies the shim's digest before `exec`ing it, which anchors the
/// check in something that cannot itself be edited unnoticed.
const GUARD_SHIM_SHA256_ENV: &str = "BOSS_GUARD_SHIM_SHA256";

/// Cap on records read from one trace file, so a pathological run cannot make
/// the engine's turn-boundary read unbounded. Records past the cap are counted
/// in [`GuardTraceSummary::skipped_over_cap`] and rendered in the summary
/// rather than silently dropped, and the reported offset stops at the last
/// record actually consumed — so the next turn's read picks them up instead of
/// stepping over them.
const MAX_RECORDS_PER_READ: usize = 2000;

/// Absolute path of the guard trace for a run's `CODEX_HOME`.
pub fn guard_trace_path(codex_home: &Path) -> PathBuf {
    codex_home.join(GUARD_TRACE_FILENAME)
}

/// The trace shim, materialised verbatim as an executable `.py`. Invoked as
/// `python3 <shim> <guard-executable>`; reads the hook payload on stdin and
/// writes the guard's decision to stdout.
pub(super) const GUARD_TRACE_SHIM_SCRIPT: &str = r#"#!/usr/bin/env python3
"""Boss guard-trace shim for Codex PreToolUse hooks.

Runs one Boss guard, records what it decided, and re-emits that decision *in
Codex's own vocabulary*. Boss's guards speak Claude Code's hook dialect, which
Codex refuses outright -- and a refused response is fail-open, so an untranslated
`block` is a disarmed guard, not merely a noisy one. See `emit_decision`.

A guard that crashes, exits non-zero, or prints something that is not a decision
becomes a hard `block`: Codex treats a broken hook as approval, so the shim is
what turns a broken guard into a loud refusal instead of a silent hole.

A guard that hangs is the same silent fail-open: Codex's own hook timeout
would eventually give up and treat the hook as absent. The shim therefore
imposes its own, shorter budget and converts an overrun into the same loud
`block`.

Usage: python3 boss_guard_trace.py <guard-executable>
Env:   BOSS_GUARD_TRACE   path of the JSONL trace to append to (optional)
       BOSS_GUARD_NAME    label recorded for this guard (optional)
       BOSS_GUARD_SHA256  expected `sha256:<hex>` of the guard file; a mismatch
                          blocks (optional, but always set in production)
       BOSS_GUARD_TIMEOUT_SECONDS  per-guard wall-clock budget; an overrun
                          blocks (optional, default below)
"""
import hashlib
import json
import os
import subprocess
import sys
import time

APPROVE_DECISIONS = ("approve", "allow")
BLOCK_DECISIONS = ("block", "deny")

# Wall-clock budget for one guard. Deliberately between the slowest guard's own
# budget (the checkleft pre-push gate allows checkleft 300s) and Codex's default
# 600s command-hook timeout: high enough that a legitimately slow guard finishes
# and reports, low enough that Boss -- not Codex's fail-open -- is what decides
# what a hung guard means.
DEFAULT_TIMEOUT_SECONDS = 420.0


def timeout_seconds():
    """The configured budget, or the default when unset/unparseable."""
    try:
        configured = float(os.environ.get("BOSS_GUARD_TIMEOUT_SECONDS", ""))
    except ValueError:
        return DEFAULT_TIMEOUT_SECONDS
    return configured if configured > 0 else DEFAULT_TIMEOUT_SECONDS


GUARD_ERROR = (
    "Blocked (fail-closed): the Boss guard {guard} did not return a usable "
    "decision, so Boss cannot confirm this tool call is allowed. A guard that "
    "cannot answer is treated as a refusal, never as approval. Detail: {detail}"
)

# Substituted when a guard blocks without saying why. Codex rejects a block
# whose reason is missing or empty, and a rejected response runs the tool call
# anyway -- so an unexplained refusal would silently become an approval.
BLOCK_WITHOUT_REASON = (
    "Blocked by a Boss guard, which did not record a reason. Treat this as a "
    "refusal and do not retry the call"
)


def emit_decision(decision, reason):
    """Write one decision in Codex's PreToolUse dialect.

    Codex's PreToolUse is deny-only. It has no affirmative allow token at all:
    `decision:approve`, `decision:allow` and `permissionDecision:allow` are each
    rejected, and a rejected response is fail-open -- Codex logs the hook as
    failed and runs the call regardless. So the allow path here writes *nothing*,
    which is the only thing Codex accepts as "proceed".

    That also rules out re-emitting the guard's stdout verbatim, which is what
    this shim used to do. Claude's extra fields are not merely ignored by Codex;
    `suppressOutput`, `stopReason` and `continue:false` are named rejections, so
    passing them through converts a decision into a hook failure.

    A block must carry a non-empty reason for the same reason.
    """
    if decision in BLOCK_DECISIONS:
        text = reason if isinstance(reason, str) else ""
        if not text.strip():
            text = BLOCK_WITHOUT_REASON
        sys.stdout.write(json.dumps({"decision": "block", "reason": text}))


def record(payload_text, decision, reason, exit_code, detail):
    """Append one line to the trace. Never raises: tracing must not break a hook."""
    path = os.environ.get("BOSS_GUARD_TRACE", "")
    if not path:
        return
    tool = None
    session = None
    try:
        payload = json.loads(payload_text)
        if isinstance(payload, dict):
            tool = payload.get("tool_name")
            session = payload.get("session_id")
    except Exception:
        pass
    line = {
        "ts": round(time.time(), 3),
        "guard": os.environ.get("BOSS_GUARD_NAME") or "unknown",
        "tool": tool if isinstance(tool, str) else None,
        "session_id": session if isinstance(session, str) else None,
        "decision": decision,
        "reason": (reason or "")[:400],
        "exit_code": exit_code,
    }
    if detail:
        line["detail"] = detail[:400]
    try:
        with open(path, "a") as trace_file:
            trace_file.write(json.dumps(line) + "\n")
    except Exception:
        pass


def classify(stdout):
    """(decision, reason) from a guard's stdout, or (None, detail) if unusable."""
    text = (stdout or "").strip()
    if not text:
        return None, "guard produced no output"
    try:
        parsed = json.loads(text)
    except Exception as error:
        return None, "guard output was not JSON (%s)" % error
    if not isinstance(parsed, dict):
        return None, "guard output was not a JSON object"
    decision = parsed.get("decision")
    reason = parsed.get("reason")
    if not isinstance(decision, str):
        hook_output = parsed.get("hookSpecificOutput")
        if isinstance(hook_output, dict):
            decision = hook_output.get("permissionDecision")
            # In this dialect the explanation travels under its own key. Without
            # this the reason would be dropped and the block re-emitted with the
            # generic fallback, losing what the guard actually objected to.
            if not isinstance(reason, str):
                reason = hook_output.get("permissionDecisionReason")
    if not isinstance(decision, str):
        return None, "guard output carried no decision field"
    lowered = decision.lower()
    if lowered in APPROVE_DECISIONS or lowered in BLOCK_DECISIONS:
        return lowered, reason if isinstance(reason, str) else ""
    return None, "guard decision %r is not approve/block" % decision


def main():
    if len(sys.argv) < 2:
        # No guard to run: refuse rather than approve an unguarded call.
        detail = "shim invoked with no guard path"
        message = GUARD_ERROR.format(guard="(unknown)", detail=detail)
        record("", "guard_error", message, None, detail)
        emit_decision("block", message)
        return 0

    guard = sys.argv[1]
    payload_text = sys.stdin.read()

    expected = os.environ.get("BOSS_GUARD_SHA256", "")
    if expected:
        try:
            digest = "sha256:" + hashlib.sha256(open(guard, "rb").read()).hexdigest()
        except Exception as error:
            digest = "unreadable (%s)" % error
        if digest != expected:
            detail = "guard bytes do not match the attested content hash (%s)" % digest
            message = GUARD_ERROR.format(guard=os.path.basename(guard), detail=detail)
            record(payload_text, "guard_error", message, None, detail)
            emit_decision("block", message)
            return 0

    command = [sys.executable, guard] if guard.endswith(".py") else [guard]

    budget = timeout_seconds()
    try:
        completed = subprocess.run(
            command,
            input=payload_text,
            capture_output=True,
            text=True,
            timeout=budget,
        )
        stdout, stderr, code = completed.stdout, completed.stderr, completed.returncode
    except subprocess.TimeoutExpired:
        detail = "guard timed out after %gs and was killed" % budget
        message = GUARD_ERROR.format(guard=os.path.basename(guard), detail=detail)
        record(payload_text, "guard_error", message, None, detail)
        emit_decision("block", message)
        return 0
    except Exception as error:
        stdout, stderr, code = "", str(error), None

    decision, reason_or_detail = classify(stdout)
    if decision is None or code != 0:
        detail = reason_or_detail if decision is None else "guard exited %s" % code
        if stderr:
            detail = "%s; stderr: %s" % (detail, stderr.strip()[:200])
        message = GUARD_ERROR.format(guard=os.path.basename(guard), detail=detail)
        record(payload_text, "guard_error", message, code, detail)
        emit_decision("block", message)
        return 0

    record(payload_text, decision, reason_or_detail, code, None)
    emit_decision(decision, reason_or_detail)
    return 0


if __name__ == "__main__":
    sys.exit(main())
"#;

/// Body of the `sh` wrapper Codex invokes for one guard.
///
/// A wrapper (rather than a bare guard path) is what carries the trace
/// environment: Codex's hook `command` is a single path with no argv, and the
/// trust gate hashes that string, so the per-guard context has to live inside
/// the file.
///
/// The wrapper is the only file in the chain the attestation content-binds, so
/// it is also where the shim's own bytes are checked before it runs — see
/// [`GUARD_SHIM_SHA256_ENV`].
pub(super) fn wrapper_body(
    shim: &Path,
    shim_sha256: &str,
    guard: &Path,
    guard_name: &str,
    guard_sha256: &str,
    trace_path: &Path,
    extra_env: &[(&str, String)],
) -> String {
    let shim_display = shim.display().to_string();
    let trace_display = trace_path.display().to_string();

    let mut body = String::from("#!/bin/sh\n");
    for (key, value) in extra_env {
        body.push_str(&format!("export {key}={}\n", shell_quote(value)));
    }
    body.push_str(&format!("export {GUARD_TRACE_ENV}={}\n", shell_quote(&trace_display)));
    body.push_str(&format!("export {GUARD_NAME_ENV}={}\n", shell_quote(guard_name)));
    body.push_str(&format!("export {GUARD_SHA256_ENV}={}\n", shell_quote(guard_sha256)));
    body.push_str(&format!(
        "export {GUARD_SHIM_SHA256_ENV}={}\n",
        shell_quote(shim_sha256)
    ));

    // Verify the shim before exec'ing it. Blocks rather than approving when the
    // digest cannot be computed at all: an unhashable shim is an unverified
    // shim.
    let detail = format!("the Boss guard-trace shim at {shim_display} does not match its attested content hash");
    let message = format!(
        "Blocked (fail-closed): {detail}, so Boss cannot confirm this tool call was \
         guarded. A guard chain that cannot prove its own integrity is treated as a \
         refusal, never as approval. Report this to the operator."
    );
    let decision = super::decision::block_response(&message);
    let record = serde_json::json!({
        "guard": guard_name,
        "decision": "guard_error",
        "reason": message,
        "detail": detail,
    })
    .to_string();

    body.push_str(&format!(
        "shim_sha256=$(python3 -c 'import hashlib,sys;print(\"sha256:\"+hashlib.sha256(open(sys.argv[1],\"rb\").read()).hexdigest())' {} 2>/dev/null) || shim_sha256=''\n",
        shell_quote(&shim_display),
    ));
    body.push_str(&format!(
        "if [ \"$shim_sha256\" != {} ]; then\n",
        shell_quote(shim_sha256)
    ));
    body.push_str(&format!(
        "  printf '%s\\n' {} >> {} 2>/dev/null || true\n",
        shell_quote(&record),
        shell_quote(&trace_display),
    ));
    body.push_str(&format!("  printf '%s' {}\n", shell_quote(&decision)));
    body.push_str("  exit 0\nfi\n");

    body.push_str(&format!(
        "exec python3 {} {}\n",
        shell_quote(&shim_display),
        shell_quote(&guard.display().to_string()),
    ));
    body
}

/// One recorded guard invocation.
#[derive(Debug, Clone, Deserialize)]
pub struct GuardTraceRecord {
    /// Guard label (the materialised wrapper's stem).
    #[serde(default)]
    pub guard: String,
    /// Tool name from the hook payload, when it carried one.
    #[serde(default)]
    pub tool: Option<String>,
    /// `approve`, `block`, or `guard_error`.
    #[serde(default)]
    pub decision: String,
    /// Head of the guard's reason, as recorded.
    #[serde(default)]
    pub reason: String,
}

impl GuardTraceRecord {
    fn is_block(&self) -> bool {
        self.decision == "block" || self.decision == "deny"
    }

    fn is_guard_error(&self) -> bool {
        self.decision == "guard_error"
    }
}

/// Aggregate of the guard records observed for one turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct GuardTraceSummary {
    /// Records whose decision approved the call.
    pub approvals: usize,
    /// Records whose decision blocked the call.
    pub blocks: usize,
    /// Records where the guard itself failed and the shim blocked for it.
    pub guard_errors: usize,
    /// Lines that were not parseable records — kept visible rather than
    /// dropped, because a corrupt trace is itself a signal.
    pub unparseable_lines: usize,
    /// Records present in the trace but past [`MAX_RECORDS_PER_READ`] for this
    /// read. Not lost: the reported offset does not advance over them, so the
    /// next read consumes them — but they are reported here so a turn whose
    /// guard activity is only partly summarised says so.
    pub skipped_over_cap: usize,
    /// `guard: reason head` for each block / guard error, in order.
    pub notable: Vec<String>,
}

impl GuardTraceSummary {
    /// Total guard invocations recorded. Derived rather than stored so the
    /// count can never disagree with the three decision buckets it sums.
    pub fn invocations(&self) -> usize {
        self.approvals + self.blocks + self.guard_errors
    }
}

/// One turn-boundary read of a trace file.
pub(super) struct TraceRead {
    /// Records parsed by this read.
    pub records: Vec<GuardTraceRecord>,
    /// Lines consumed, to resume from on the next read. Never advances past a
    /// line this read did not consume.
    pub next_line: usize,
    /// Lines that were not parseable records.
    pub unparseable_lines: usize,
    /// Records left for the next read because this one hit the cap.
    pub skipped_over_cap: usize,
}

/// Read trace records starting at line `from_line`.
///
/// Returns the records and the line count consumed, so the caller can resume
/// after the same offset on the next turn without re-reporting. A missing file
/// is not an error: it means no guard has run yet (which is exactly what the
/// caller wants to know).
///
/// Reading stops at [`MAX_RECORDS_PER_READ`]; the offset then stays at the last
/// consumed line and the remaining records are counted, so the cap defers work
/// to the next read rather than dropping it.
pub(super) fn read_records_from(path: &Path, from_line: usize) -> TraceRead {
    let Ok(file) = std::fs::File::open(path) else {
        return TraceRead {
            records: Vec::new(),
            next_line: from_line,
            unparseable_lines: 0,
            skipped_over_cap: 0,
        };
    };
    let mut records = Vec::new();
    let mut unparseable_lines = 0usize;
    let mut skipped_over_cap = 0usize;
    let mut consumed = from_line;
    let mut line_index = 0usize;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        line_index += 1;
        if line_index <= from_line {
            continue;
        }
        // Past the cap: count what is left and leave the offset behind it, so
        // the next read starts on the first line this one did not parse.
        if records.len() >= MAX_RECORDS_PER_READ {
            if !line.trim().is_empty() {
                skipped_over_cap += 1;
            }
            continue;
        }
        consumed = line_index;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<GuardTraceRecord>(&line) {
            Ok(record) => records.push(record),
            Err(_) => unparseable_lines += 1,
        }
    }
    TraceRead {
        records,
        next_line: consumed,
        unparseable_lines,
        skipped_over_cap,
    }
}

/// Fold one read's records into a summary.
pub(super) fn summarize(read: &TraceRead) -> GuardTraceSummary {
    let mut summary = GuardTraceSummary {
        unparseable_lines: read.unparseable_lines,
        skipped_over_cap: read.skipped_over_cap,
        ..Default::default()
    };
    for record in &read.records {
        if record.is_guard_error() {
            summary.guard_errors += 1;
        } else if record.is_block() {
            summary.blocks += 1;
        } else {
            summary.approvals += 1;
        }
        if record.is_block() || record.is_guard_error() {
            let head: String = record.reason.chars().take(160).collect();
            let tool = record.tool.as_deref().unwrap_or("?");
            summary.notable.push(format!("{} on {tool}: {head}", record.guard));
        }
    }
    summary
}

/// One-line rendering of a summary, for the notification body.
pub(super) fn render_summary(summary: &GuardTraceSummary) -> String {
    let mut text = format!(
        "{} guard invocation(s): {} approved, {} blocked, {} guard error(s)",
        summary.invocations(),
        summary.approvals,
        summary.blocks,
        summary.guard_errors
    );
    if summary.unparseable_lines > 0 {
        text.push_str(&format!("; {} unreadable trace line(s)", summary.unparseable_lines));
    }
    if summary.skipped_over_cap > 0 {
        text.push_str(&format!(
            "; {} record(s) past the {MAX_RECORDS_PER_READ}-record read cap, reported next turn",
            summary.skipped_over_cap
        ));
    }
    for note in &summary.notable {
        text.push_str(&format!("; {note}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::decision::{self, Verdict};
    use std::io::Write as _;

    /// `sha256:<hex>` of a file, mirroring what the driver bakes into the
    /// wrapper. Computed with python so the test needs no hashing dependency.
    fn sha256_of(path: &Path) -> String {
        let out = std::process::Command::new("python3")
            .arg("-c")
            .arg("import hashlib,sys;print('sha256:'+hashlib.sha256(open(sys.argv[1],'rb').read()).hexdigest())")
            .arg(path)
            .output()
            .expect("python3 must be available");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("boss-guard-trace-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Materialise the shim plus a stub guard, run it, and return
    /// (what Codex would do with the shim's stdout, trace lines).
    ///
    /// The verdict — rather than the raw bytes — is what these tests assert on:
    /// the question is never "what did Boss print?" but "does the agent accept
    /// it?". Asserting on the literal is what let the `approve` bug ship.
    fn run_shim(tag: &str, guard_body: &str, payload: &str) -> (Verdict, Vec<serde_json::Value>) {
        run_shim_with_env(tag, guard_body, payload, &[])
    }

    /// [`run_shim`] with extra environment for the shim process.
    fn run_shim_with_env(
        tag: &str,
        guard_body: &str,
        payload: &str,
        extra_env: &[(&str, &str)],
    ) -> (Verdict, Vec<serde_json::Value>) {
        let dir = temp_dir(tag);
        let shim = dir.join(GUARD_TRACE_SHIM_FILENAME);
        std::fs::write(&shim, GUARD_TRACE_SHIM_SCRIPT).unwrap();
        let guard = dir.join("stub_guard.py");
        std::fs::write(&guard, guard_body).unwrap();
        let trace = dir.join(GUARD_TRACE_FILENAME);

        let mut command = std::process::Command::new("python3");
        command
            .arg(&shim)
            .arg(&guard)
            .env(GUARD_TRACE_ENV, &trace)
            .env(GUARD_NAME_ENV, "stub_guard")
            .env(GUARD_SHA256_ENV, sha256_of(&guard));
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("python3 must be available");
        child.stdin.as_mut().unwrap().write_all(payload.as_bytes()).unwrap();
        drop(child.stdin.take());
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let verdict = decision::verdict(&stdout);
        assert!(
            !matches!(verdict, Verdict::Rejected(_)),
            "the shim emitted something Codex refuses ({verdict:?}), which fails open and runs \
             the tool call anyway\nstdout={stdout:?}\nstderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let lines = std::fs::read_to_string(&trace)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("trace line must be JSON"))
            .collect();
        (verdict, lines)
    }

    /// The reason text of a [`Verdict::Block`], or a panic naming what came
    /// back instead.
    fn block_reason(verdict: &Verdict) -> &str {
        match verdict {
            Verdict::Block(reason) => reason,
            other => panic!("expected a block Codex accepts, got {other:?}"),
        }
    }

    const PAYLOAD: &str = r#"{"tool_name":"Bash","session_id":"sess-1","tool_input":{"command":"echo hi"}}"#;

    #[test]
    fn a_claude_dialect_approval_becomes_the_silence_codex_accepts() {
        // The shipped bug: the guards emit Claude's `approve`, the shim used to
        // re-emit it verbatim, and Codex refused it on every single tool call.
        // Codex has no allow token, so an accepted approval is an empty stdout.
        let (verdict, lines) = run_shim(
            "approve",
            "import json,sys\nsys.stdin.read()\nprint(json.dumps({'decision':'approve'}))\n",
            PAYLOAD,
        );
        assert_eq!(verdict, Verdict::Allow);
        assert_eq!(lines.len(), 1, "one invocation must be recorded");
        assert_eq!(lines[0]["decision"], "approve");
        assert_eq!(lines[0]["tool"], "Bash");
        assert_eq!(lines[0]["guard"], "stub_guard");
        assert_eq!(lines[0]["session_id"], "sess-1");
    }

    #[test]
    fn block_reason_is_preserved_verbatim() {
        let (verdict, lines) = run_shim(
            "block",
            "import json,sys\nsys.stdin.read()\nprint(json.dumps({'decision':'block','reason':'nope'}))\n",
            PAYLOAD,
        );
        assert_eq!(block_reason(&verdict), "nope");
        assert_eq!(lines[0]["decision"], "block");
        assert_eq!(lines[0]["reason"], "nope");
    }

    #[test]
    fn crashing_guard_becomes_a_block_not_an_approval() {
        // The whole point: Codex treats a broken hook as approval. The shim
        // must convert that into a refusal and say so.
        let (verdict, lines) = run_shim("crash", "import sys\nsys.stdin.read()\nraise SystemExit(3)\n", PAYLOAD);
        assert!(
            block_reason(&verdict).contains("fail-closed"),
            "reason must be explicit about failing closed: {verdict:?}"
        );
        assert_eq!(lines[0]["decision"], "guard_error");
    }

    #[test]
    fn guard_printing_garbage_becomes_a_block() {
        let (verdict, lines) = run_shim("garbage", "import sys\nsys.stdin.read()\nprint('not json')\n", PAYLOAD);
        assert!(matches!(verdict, Verdict::Block(_)), "{verdict:?}");
        assert_eq!(lines[0]["decision"], "guard_error");
    }

    #[test]
    fn silent_guard_becomes_a_block() {
        let (verdict, lines) = run_shim("silent", "import sys\nsys.stdin.read()\n", PAYLOAD);
        assert!(matches!(verdict, Verdict::Block(_)), "{verdict:?}");
        assert_eq!(lines[0]["decision"], "guard_error");
    }

    #[test]
    fn a_hung_guard_times_out_into_a_block() {
        // A guard that never answers is the same silent fail-open as one that
        // crashes: Codex's own hook timeout would treat it as absent. The shim
        // has to be what decides, on its own budget.
        let (verdict, lines) = run_shim_with_env(
            "timeout",
            "import sys,time\nsys.stdin.read()\ntime.sleep(30)\n",
            PAYLOAD,
            &[("BOSS_GUARD_TIMEOUT_SECONDS", "1")],
        );
        let reason = block_reason(&verdict);
        assert!(reason.contains("timed out"), "reason must name the timeout: {reason}");
        assert_eq!(lines[0]["decision"], "guard_error");
        assert!(
            lines[0]["detail"].as_str().unwrap().contains("timed out"),
            "{:?}",
            lines[0]
        );
    }

    #[test]
    fn an_unparseable_timeout_override_falls_back_to_the_default_budget() {
        // A malformed override must not disable the budget or wedge the guard.
        let (verdict, _) = run_shim_with_env(
            "timeout-bad-env",
            "import json,sys\nsys.stdin.read()\nprint(json.dumps({'decision':'approve'}))\n",
            PAYLOAD,
            &[("BOSS_GUARD_TIMEOUT_SECONDS", "not-a-number")],
        );
        assert_eq!(verdict, Verdict::Allow);
    }

    #[test]
    fn hook_specific_output_dialect_is_translated_and_keeps_its_reason() {
        // `decision:deny` is *not* a synonym for `block` to Codex -- it is a
        // rejection, so passing this dialect through unchanged would fail open.
        // The reason lives under its own key in this dialect and must survive.
        let (verdict, lines) = run_shim(
            "dialect",
            "import json,sys\nsys.stdin.read()\nprint(json.dumps({'hookSpecificOutput':{'permissionDecision':'deny','permissionDecisionReason':'not allowed here'}}))\n",
            PAYLOAD,
        );
        assert_eq!(block_reason(&verdict), "not allowed here");
        assert_eq!(lines[0]["decision"], "deny", "the trace keeps the guard's own word");
    }

    /// The regression test for the shipped bug, stated as the property that was
    /// never checked: whatever a Boss guard says, Codex must *accept* what the
    /// shim emits on its behalf. Every dialect below is one a guard in this repo
    /// can produce — the Claude-native `approve`/`block` the shared path and
    /// checkleft guards emit, and the `hookSpecificOutput` forms.
    ///
    /// Before the translation, the first four rows all made Codex log
    /// `PreToolUse hook returned unsupported …` and run the tool call anyway.
    #[test]
    fn every_dialect_a_boss_guard_emits_is_one_codex_accepts() {
        let cases: &[(&str, &str, bool)] = &[
            ("approve", r#"{'decision':'approve'}"#, false),
            ("allow", r#"{'decision':'allow'}"#, false),
            (
                "perm-allow",
                r#"{'hookSpecificOutput':{'hookEventName':'PreToolUse','permissionDecision':'allow'}}"#,
                false,
            ),
            ("deny-word", r#"{'decision':'deny','reason':'no'}"#, true),
            ("block", r#"{'decision':'block','reason':'no'}"#, true),
            (
                "perm-deny",
                r#"{'hookSpecificOutput':{'permissionDecision':'deny','permissionDecisionReason':'no'}}"#,
                true,
            ),
        ];
        for (tag, emitted, expect_block) in cases {
            let body = format!("import json,sys\nsys.stdin.read()\nprint(json.dumps({emitted}))\n");
            // run_shim already fails the test if Codex would reject the output.
            let (verdict, _) = run_shim(&format!("dialect-{tag}"), &body, PAYLOAD);
            if *expect_block {
                assert!(
                    matches!(verdict, Verdict::Block(_)),
                    "{tag}: a refusing guard must still refuse after translation, got {verdict:?}"
                );
            } else {
                assert_eq!(
                    verdict,
                    Verdict::Allow,
                    "{tag}: an approving guard must let the call run"
                );
            }
        }
    }

    #[test]
    fn a_guard_that_blocks_without_saying_why_still_blocks() {
        // Codex rejects a reasonless block, and a rejected response runs the
        // call — so passing the guard's silence through would quietly disarm it.
        // The shim substitutes a reason rather than emitting one Codex discards.
        let (verdict, lines) = run_shim(
            "block-no-reason",
            "import json,sys\nsys.stdin.read()\nprint(json.dumps({'decision':'block'}))\n",
            PAYLOAD,
        );
        assert!(
            !block_reason(&verdict).trim().is_empty(),
            "a block must carry a reason or Codex discards it: {verdict:?}"
        );
        assert_eq!(lines[0]["decision"], "block");
    }

    #[test]
    fn a_guard_whose_bytes_do_not_match_the_attested_hash_is_refused() {
        // Wrapping cost the trust gate its content binding on the guard file
        // itself (it hashes the wrapper now), so the shim re-checks the guard's
        // bytes on every invocation. A guard edited after arming must block,
        // not run.
        let dir = temp_dir("tamper");
        let shim = dir.join(GUARD_TRACE_SHIM_FILENAME);
        std::fs::write(&shim, GUARD_TRACE_SHIM_SCRIPT).unwrap();
        let guard = dir.join("stub_guard.py");
        std::fs::write(
            &guard,
            "import json,sys\nsys.stdin.read()\nprint(json.dumps({'decision':'approve'}))\n",
        )
        .unwrap();
        let trace = dir.join(GUARD_TRACE_FILENAME);

        let mut child = std::process::Command::new("python3")
            .arg(&shim)
            .arg(&guard)
            .env(GUARD_TRACE_ENV, &trace)
            .env(GUARD_NAME_ENV, "stub_guard")
            .env(
                GUARD_SHA256_ENV,
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            )
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("python3 must be available");
        child.stdin.as_mut().unwrap().write_all(PAYLOAD.as_bytes()).unwrap();
        drop(child.stdin.take());
        let out = child.wait_with_output().unwrap();
        let verdict = decision::verdict(&String::from_utf8_lossy(&out.stdout));
        assert!(
            block_reason(&verdict).contains("content hash"),
            "reason must name the content-hash mismatch: {verdict:?}"
        );
        let recorded = std::fs::read_to_string(&trace).unwrap();
        assert!(recorded.contains("guard_error"), "{recorded}");
    }

    #[test]
    fn reading_resumes_after_the_reported_offset() {
        let dir = temp_dir("read");
        let path = dir.join(GUARD_TRACE_FILENAME);
        std::fs::write(
            &path,
            "{\"guard\":\"a\",\"decision\":\"approve\"}\n{\"guard\":\"b\",\"decision\":\"block\",\"reason\":\"no\",\"tool\":\"Bash\"}\n",
        )
        .unwrap();

        let first = read_records_from(&path, 0);
        assert_eq!(first.records.len(), 2);
        assert_eq!(first.next_line, 2);
        assert_eq!(first.unparseable_lines, 0);
        assert_eq!(first.skipped_over_cap, 0);

        // A second turn must not re-report the same records.
        let again = read_records_from(&path, first.next_line);
        assert!(again.records.is_empty(), "already-reported records must not repeat");
        assert_eq!(again.next_line, 2);
    }

    #[test]
    fn missing_trace_file_reads_as_no_records() {
        let dir = temp_dir("missing");
        let read = read_records_from(&dir.join(GUARD_TRACE_FILENAME), 0);
        assert!(read.records.is_empty());
        assert_eq!(read.next_line, 0);
        assert_eq!(read.unparseable_lines, 0);
    }

    #[test]
    fn corrupt_lines_are_counted_not_dropped() {
        let dir = temp_dir("corrupt");
        let path = dir.join(GUARD_TRACE_FILENAME);
        std::fs::write(&path, "{\"guard\":\"a\",\"decision\":\"approve\"}\nnot-json\n").unwrap();
        let read = read_records_from(&path, 0);
        assert_eq!(read.records.len(), 1);
        assert_eq!(read.unparseable_lines, 1);
    }

    #[test]
    fn the_read_cap_defers_records_instead_of_dropping_them() {
        // The cap used to advance the offset over a line it never parsed, so
        // the record on that line was skipped forever. Nothing may be lost:
        // what the cap holds back must arrive on the next read, and be counted
        // in the meantime.
        let dir = temp_dir("cap");
        let path = dir.join(GUARD_TRACE_FILENAME);
        let total = MAX_RECORDS_PER_READ + 2;
        let body: String = (0..total)
            .map(|index| format!("{{\"guard\":\"g{index}\",\"decision\":\"approve\"}}\n"))
            .collect();
        std::fs::write(&path, body).unwrap();

        let first = read_records_from(&path, 0);
        assert_eq!(first.records.len(), MAX_RECORDS_PER_READ);
        assert_eq!(first.next_line, MAX_RECORDS_PER_READ);
        assert_eq!(first.skipped_over_cap, 2, "the held-back records must be counted");
        assert!(
            render_summary(&summarize(&first)).contains("2 record(s) past the"),
            "the summary must say the turn is only partly reported"
        );

        let second = read_records_from(&path, first.next_line);
        assert_eq!(second.records.len(), 2, "the held-back records must arrive next read");
        assert_eq!(second.records[0].guard, format!("g{MAX_RECORDS_PER_READ}"));
        assert_eq!(second.next_line, total);
        assert_eq!(second.skipped_over_cap, 0);
    }

    #[test]
    fn summary_counts_and_names_the_notable_decisions() {
        let records = vec![
            GuardTraceRecord {
                guard: "01_boss_launch_guard".into(),
                tool: Some("Bash".into()),
                decision: "approve".into(),
                reason: String::new(),
            },
            GuardTraceRecord {
                guard: "02_pr_redirect_guard".into(),
                tool: Some("Bash".into()),
                decision: "block".into(),
                reason: "use cube".into(),
            },
            GuardTraceRecord {
                guard: "03_checkleft_push_guard".into(),
                tool: Some("Bash".into()),
                decision: "guard_error".into(),
                reason: "guard exited 3".into(),
            },
        ];
        let summary = summarize(&TraceRead {
            records,
            next_line: 4,
            unparseable_lines: 1,
            skipped_over_cap: 0,
        });
        assert_eq!(summary.invocations(), 3);
        assert_eq!(summary.approvals, 1);
        assert_eq!(summary.blocks, 1);
        assert_eq!(summary.guard_errors, 1);
        assert_eq!(summary.unparseable_lines, 1);
        let rendered = render_summary(&summary);
        assert!(rendered.contains("3 guard invocation(s)"), "{rendered}");
        assert!(
            rendered.contains("02_pr_redirect_guard on Bash: use cube"),
            "{rendered}"
        );
        assert!(rendered.contains("unreadable trace line"), "{rendered}");
    }

    #[test]
    fn wrapper_body_carries_trace_env_and_extra_env() {
        let body = wrapper_body(
            Path::new("/home/guards/boss_guard_trace.py"),
            "sha256:shim99",
            Path::new("/home/guards/00_path_guard.py"),
            "00_path_guard",
            "sha256:abc123",
            Path::new("/home/guard-trace.jsonl"),
            &[("BOSS_DATA_DIR", "/data/Boss dir".to_owned())],
        );
        assert!(body.starts_with("#!/bin/sh\n"), "{body}");
        assert!(body.contains("export BOSS_DATA_DIR='/data/Boss dir'"), "{body}");
        assert!(
            body.contains("export BOSS_GUARD_TRACE='/home/guard-trace.jsonl'"),
            "{body}"
        );
        assert!(body.contains("export BOSS_GUARD_NAME='00_path_guard'"), "{body}");
        assert!(body.contains("export BOSS_GUARD_SHA256='sha256:abc123'"), "{body}");
        assert!(body.contains("export BOSS_GUARD_SHIM_SHA256='sha256:shim99'"), "{body}");
        assert!(
            body.contains("exec python3 '/home/guards/boss_guard_trace.py' '/home/guards/00_path_guard.py'"),
            "{body}"
        );
    }

    /// Run a materialised wrapper end-to-end and return
    /// (stdout decision, trace lines).
    ///
    /// `shim_body` of `None` removes the shim entirely, so its digest cannot be
    /// computed at all.
    fn run_wrapper(tag: &str, shim_body: Option<&str>) -> (Verdict, Vec<serde_json::Value>) {
        let dir = temp_dir(tag);
        let shim = dir.join(GUARD_TRACE_SHIM_FILENAME);
        // Hash the *real* shim, then materialise whatever body the test wants:
        // that is exactly the "shim replaced after arming" shape.
        std::fs::write(&shim, GUARD_TRACE_SHIM_SCRIPT).unwrap();
        let real_shim_sha256 = sha256_of(&shim);
        match shim_body {
            Some(body) => std::fs::write(&shim, body).unwrap(),
            None => std::fs::remove_file(&shim).unwrap(),
        }
        let guard = dir.join("stub_guard.py");
        std::fs::write(
            &guard,
            "import json,sys\nsys.stdin.read()\nprint(json.dumps({'decision':'approve'}))\n",
        )
        .unwrap();
        let trace = dir.join(GUARD_TRACE_FILENAME);

        let wrapper = dir.join("00_stub_guard.sh");
        std::fs::write(
            &wrapper,
            wrapper_body(
                &shim,
                &real_shim_sha256,
                &guard,
                "00_stub_guard",
                &sha256_of(&guard),
                &trace,
                &[],
            ),
        )
        .unwrap();

        let mut child = std::process::Command::new("sh")
            .arg(&wrapper)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("sh must be available");
        child.stdin.as_mut().unwrap().write_all(PAYLOAD.as_bytes()).unwrap();
        drop(child.stdin.take());
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let verdict = decision::verdict(&stdout);
        assert!(
            !matches!(verdict, Verdict::Rejected(_)),
            "the wrapper emitted something Codex refuses ({verdict:?}), which fails open\nstdout={stdout:?}\nstderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let lines = std::fs::read_to_string(&trace)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("trace line must be JSON"))
            .collect();
        (verdict, lines)
    }

    #[test]
    fn the_wrapper_runs_the_guard_when_the_shim_matches_its_hash() {
        let (verdict, lines) = run_wrapper("wrapper-ok", Some(GUARD_TRACE_SHIM_SCRIPT));
        assert_eq!(verdict, Verdict::Allow);
        assert_eq!(lines.len(), 1, "the shim must still have recorded the invocation");
        assert_eq!(lines[0]["decision"], "approve");
    }

    #[test]
    fn a_shim_replaced_after_arming_blocks_instead_of_neutralising_every_guard() {
        // The hole this closes: the attestation content-binds the wrapper and
        // the shim re-checks the guard, but nothing covered the shim's own
        // bytes. Swapping it for a blanket approve would disarm every guard
        // with both hashes still valid, so the wrapper -- which *is* bound --
        // verifies the shim before exec'ing it.
        let (verdict, lines) = run_wrapper(
            "wrapper-tampered",
            Some("import sys\nsys.stdin.read()\nprint('{\"decision\":\"approve\"}')\n"),
        );
        let reason = block_reason(&verdict);
        assert!(
            reason.contains("shim") && reason.contains("content hash"),
            "reason must name the shim hash mismatch: {reason}"
        );
        assert_eq!(lines.len(), 1, "the refusal must be recorded in the trace");
        assert_eq!(lines[0]["decision"], "guard_error");
        assert_eq!(lines[0]["guard"], "00_stub_guard");
    }

    #[test]
    fn a_missing_shim_blocks_too() {
        // A shim whose digest cannot be computed is an unverified shim, and the
        // wrapper must not fall through to running it.
        let (verdict, _) = run_wrapper("wrapper-missing-shim", None);
        assert!(matches!(verdict, Verdict::Block(_)), "{verdict:?}");
    }
}
