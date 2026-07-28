# Grok `PreToolUse` decision vocabulary and tool-name map

- **Date:** 2026-07-27
- **Kind:** empirical investigation — findings + fixtures only; no engine / driver code
- **Pinned CLI:** `grok 0.2.112 (9bbd559437aa) [stable]` (`~/.grok/bin/grok`)
- **Isolation:** throwaway `GROK_HOME=/tmp/grok-t02-vocab/home` (never `~/.grok`); auth.json copied once; `[compat.claude]` / `[compat.cursor]` hooks disabled; model `grok-4.5` (not `grok-code-fast-1`)
- **Related:** [ghostty-grok-pane-viability.md](./ghostty-grok-pane-viability.md) Q5–Q6; design doc T-02 / OQ-2; fixtures under [`ghostty-grok-pane-viability-artifacts/cli/`](./ghostty-grok-pane-viability-artifacts/cli/)

## Why this investigation exists

Boss's five `PreToolUse` guard scripts emit `{"decision": "block"}` / `{"decision": "approve"}` (`tools/boss/engine/core/src/worker_setup.rs` and the inline scripts in `tools/boss/engine/driver/src/claude.rs`). The gating spike proved only that `{"decision":"deny"}` and stderr-plus-exit-2 block under Grok; `"block"` and `"approve"` were unverified.

If `"block"` is not recognised as a deny, every adapted guard can run, log, report success, and still **permit** the command — a failure mode indistinguishable from healthy operation.

Second question: which `toolName` values Grok actually puts on the wire for the tools Boss's guards care about (shell, file write, file edit, and anything that can reach `git` / `jj` / `gh`), so a payload adapter can rename them to the Claude-shaped names the guards branch on (`tool_name == "Bash"`, etc.).

## Verdict (read this first)

| Question                                                          | Answer                                                                                               |
| ----------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| Does `{"decision":"block"}` block a tool under Grok `PreToolUse`? | **No.** Fail-open. Attack file created. Hook still ran.                                              |
| Does `{"decision":"deny"}` block?                                 | **Yes.**                                                                                             |
| Does exit code 2 (stderr, no JSON) block?                         | **Yes.**                                                                                             |
| Does `{"decision":"approve"}` allow?                              | **Yes** (non-blocking). Same behavioural class as `allow`, empty stdout, unknown values.             |
| Unrecognised `decision` value?                                    | **Fail-open** (tool proceeds).                                                                       |
| What blocks PreToolUse on Grok 0.2.112?                           | **Only** documented `decision=deny` (any exit code) **or** exit code `2` without a usable deny JSON. |
| Canonical shell tool name on the wire?                            | `run_terminal_command` (not `Bash`)                                                                  |
| Canonical new-file write tool name?                               | `write` (native; **not** folded into `search_replace` on the payload)                                |
| Canonical in-place edit tool name?                                | `search_replace`                                                                                     |
| How do `git` / `jj` / `gh` appear?                                | Only as `run_terminal_command.toolInput.command` strings — no dedicated VCS tools observed           |

**Driver implication for the guard adapter (T-09):** translate Boss `block` → Grok `deny`, and Boss `approve` → Grok `allow` (or empty exit 0). Emitting Boss vocabulary unchanged **fails open on every block**.

---

## Method

1. Fresh isolated home under `/tmp/grok-t02-vocab/` with global hooks only (`$GROK_HOME/hooks/*.json`).
2. For each decision candidate, install a single `PreToolUse` command hook that always emits that candidate (or exit 2 / empty stdout), then headless:

   ```sh
   GROK_HOME=… grok -p "…write ATTACK_<tag>_OK into ATTACK_<tag>.txt…" \
     --always-approve --trust --session-id <uuid> --cwd <cwd> \
     --output-format json --model grok-4.5 --no-subagents --no-memory
   ```

3. Pass criterion: attack file **absent** ⇒ blocked; **present** with expected contents ⇒ allowed. Hook stdin was always captured to prove the hook ran (so "allowed" is not "hook skipped").
4. Separate run with a dump-all hook set to capture real payloads for `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `Notification`, while forcing write + search_replace + shell (including `which git jj gh`) and a background `sleep` (to fire `Notification` / `get_command_or_subagent_output`).

Evidence: [`cli/decision_vocab/`](./ghostty-grok-pane-viability-artifacts/cli/decision_vocab/), [`cli/hook_payloads/`](./ghostty-grok-pane-viability-artifacts/cli/hook_payloads/).

---

## Part 1 — PreToolUse decision vocabulary

### Official doc (bundled `10-hooks.md`, grok 0.2.112)

For **PreToolUse** stdout JSON the guide documents only:

- Allow: `{"decision": "allow"}`
- Deny: `{"decision": "deny", "reason": "…"}`

Exit codes:

| Exit  | Meaning                      |
| ----- | ---------------------------- |
| `0`   | Success / allow              |
| `2`   | Explicit deny (`PreToolUse`) |
| other | Fail-open                    |

Quote: _“Only an explicit `deny` decision returned by the hook blocks a tool call.”_ Failures (timeouts, crashes, malformed output) are fail-open.

Separately, **`decision: "block"` is documented for `Stop` / `SubagentStop` stop-gates**, not for PreToolUse. That naming collision is the root of the Boss vocabulary hazard: Boss reused Claude Code's PreToolUse `block`/`approve` pair; Grok's PreToolUse pair is `deny`/`allow`, and Grok's `block` means “do not stop the turn.”

### Empirical matrix (this run)

Probe target: create `ATTACK_<tag>.txt` via the **write** tool (not shell).

| Hook stdout / exit                         | Attack file                   | Interpretation                                      |
| ------------------------------------------ | ----------------------------- | --------------------------------------------------- |
| `{"decision":"deny","reason":…}` exit 0    | **absent**                    | **BLOCKS**                                          |
| `{"decision":"block","reason":…}` exit 0   | **present** `ATTACK_block_OK` | **FAILS OPEN** — Boss block vocabulary not honoured |
| `{"decision":"approve","reason":…}` exit 0 | **present**                   | allows (non-blocking)                               |
| `{"decision":"allow","reason":…}` exit 0   | **present**                   | allows (documented)                                 |
| empty stdout, exit 0                       | **present**                   | allows                                              |
| `{"ok":true}` (no `decision` key), exit 0  | **present**                   | fail-open                                           |
| stderr text, **exit 2**, no JSON           | **absent**                    | **BLOCKS**                                          |
| `{"decision":"foobar",…}`                  | **present**                   | fail-open (unrecognised)                            |
| `{"decision":"permit",…}`                  | **present**                   | fail-open (unrecognised)                            |

Raw stdin captures proving each hook fired: `cli/decision_vocab/pre_<tag>.raw`. Agent JSON: `cli/decision_vocab/agent_*.json`. Table: `cli/decision_vocab/matrix.tsv`.

### What this means for Boss

| Boss guard emits                              | Grok PreToolUse effect today | Required adapter translation                            |
| --------------------------------------------- | ---------------------------- | ------------------------------------------------------- |
| `{"decision":"block","reason":…}`             | **permits the tool**         | → `{"decision":"deny","reason":…}`                      |
| `{"decision":"approve"}`                      | permits the tool             | → `{"decision":"allow"}` or exit 0 empty                |
| (none of the five emit `deny` / exit 2 today) | n/a                          | exit-2 remains a valid alternate deny path if ever used |

**Do not** assume that mapping `block`→`block` is safe. It is the silent-unguarded failure mode this investigation was opened to catch.

**Do not** conflate Stop-hook `block` with PreToolUse deny. A shared “decision” string across event types would mis-handle stop gates if the adapter is not event-scoped.

---

## Part 2 — Tool-name map

### Observed on the wire (this run)

| Grok `toolName`                  | Boss / Claude branch name | `toolInput` keys (observed)                     | Notes                                              |
| -------------------------------- | ------------------------- | ----------------------------------------------- | -------------------------------------------------- |
| `run_terminal_command`           | `Bash`                    | `command`, `description`; optional `background` | All shell, including `git` / `jj` / `gh`           |
| `write`                          | `Write`                   | `file_path`, `content`                          | **Create / overwrite file.** Distinct native name. |
| `search_replace`                 | `Edit`                    | `file_path`, `old_string`, `new_string`         | In-place edit.                                     |
| `get_command_or_subagent_output` | _(none in Boss guards)_   | `task_ids`, `timeout_ms`                        | Background-task poll; not a VCS surface.           |

Fixtures: `PreToolUse.<toolName>.sample.json` and matching `PostToolUse.<toolName>.sample.json`.

### Matcher aliases (bundled docs — for hook **matchers**, not payload fields)

From `10-hooks.md` “Tool Name Aliases” (matcher side only):

| Matcher alias (Claude)       | Maps to native tool    |
| ---------------------------- | ---------------------- |
| `Bash`                       | `run_terminal_command` |
| `Read`                       | `read_file`            |
| `Edit`, `Write`, `MultiEdit` | `search_replace`       |
| `Grep`                       | `grep`                 |
| `Glob`, `ListDir`            | `list_dir`             |
| `WebSearch`                  | `web_search`           |
| `Task`                       | `spawn_subagent`       |

**Important discrepancy:** matcher docs fold `Write` → `search_replace`, but **payloads still emit `toolName: "write"`** for create-file calls (confirmed both in the original pane-viability spike and this run). An adapter that only rewrites `search_replace` → `Edit` and ignores `write` will leave Boss file-path guards seeing `tool_name="write"` after a naive camelCase→snake_case pass — and Boss scripts read `tool_name`, not `toolName`.

Matchers keep the original name too (`Bash` matches both `Bash` and `run_terminal_command`), so **matcher registration can keep `matcher: "Bash"`** and still fire on Grok shell calls. Payload **rewriting** is still required for the guard script body, which compares equality to `"Bash"`.

### What Boss guards actually branch on

From the embedded scripts in `worker_setup.rs` / `claude.rs`:

- Shell / push / PR / boss-launch guards: `tool_name == "Bash"` (or `!= "Bash"` early-approve) and then `tool_input.command`.
- Data-dir write guard: collects `file_path` / `notebook_path` / `path` from any tool, **and** tokenises `command` when `tool == "Bash"`.

So the minimum adapter rename set for guard fidelity is:

```text
run_terminal_command  →  Bash
write                 →  Write
search_replace        →  Edit
```

plus key renames `toolName`→`tool_name`, `toolInput`→`tool_input`, `hookEventName`→`hook_event_name` (out of scope for this investigation but required by the same adapter).

### VCS tools (`git` / `jj` / `gh`)

No dedicated Grok tools for VCS were observed. The forced shell command

```text
echo SHELL_OK > toolmap_shell.txt && which git jj gh || true
```

arrived as a single `run_terminal_command` PreToolUse with that full string in `toolInput.command` (see `PreToolUse.run_terminal_command.sample.json`). Boss push/PR guards that inspect Bash command text therefore apply unchanged **after** the name rewrite.

### PostToolUse `toolResult.type` (secondary signal)

| `toolName`             | Observed `toolResult.type` |
| ---------------------- | -------------------------- |
| `write`                | `SearchReplace`            |
| `search_replace`       | `SearchReplace`            |
| `run_terminal_command` | `Bash`                     |

Useful for progress normalisers; not a substitute for PreToolUse `toolName` when gating.

---

## Part 3 — Hook payload fixtures

Refreshed real captures (isolated home, grok 0.2.112) for every event the original spike observed:

| Event                                             | Fixture                                                                                           |
| ------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| `SessionStart`                                    | `cli/hook_payloads/SessionStart.sample.json`                                                      |
| `UserPromptSubmit`                                | `cli/hook_payloads/UserPromptSubmit.sample.json`                                                  |
| `PreToolUse` (generic = `write`)                  | `cli/hook_payloads/PreToolUse.sample.json`                                                        |
| `PreToolUse` per tool                             | `PreToolUse.write`, `.search_replace`, `.run_terminal_command`, `.get_command_or_subagent_output` |
| `PostToolUse` (generic = `write`)                 | `cli/hook_payloads/PostToolUse.sample.json`                                                       |
| `PostToolUse` per tool                            | matching `PostToolUse.<tool>.sample.json`                                                         |
| `Stop`                                            | `cli/hook_payloads/Stop.sample.json`                                                              |
| `Notification` (`notificationType=task_complete`) | `cli/hook_payloads/Notification.sample.json`                                                      |

Common stdin shape (all events): camelCase keys; `hookEventName` is **snake_case** (`pre_tool_use`, `session_start`, …); env carries `GROK_HOOK_EVENT` (snake), `GROK_SESSION_ID`, `GROK_HOME`, `GROK_WORKSPACE_ROOT`, `CLAUDE_PROJECT_DIR`.

Events registered but not hit by these short prompts (unchanged from the spike): `SessionEnd`, `Subagent*`, `*Compact`, `StopFailure`, `PermissionDenied`, `PostToolUseFailure`.

---

## Adapter contract (input to T-09)

Minimum behaviour the driver-owned canonicalisation adapter must implement for guards to be real:

1. **Input rewrite (Grok → Boss):**
   - Keys: `hookEventName`→`hook_event_name`, `toolName`→`tool_name`, `toolInput`→`tool_input`, … (full map is T-10 / progress normaliser territory; guards need at least the tool trio + `cwd`).
   - Values: `run_terminal_command`→`Bash`, `write`→`Write`, `search_replace`→`Edit`.
2. **Output rewrite (Boss → Grok PreToolUse):**
   - `decision=block` → `decision=deny` (preserve `reason`).
   - `decision=approve` → `decision=allow` (or empty exit 0).
   - Pass through `deny` / `allow` if ever emitted.
   - Prefer JSON deny over exit-2 so reasons reach the model.
3. **Negative acceptance test (T-09):** fixture worker attempts each blocked class of command and is **demonstrably refused** (file absent / push not performed). “Hook ran” alone is insufficient — that is exactly the healthy-looking fail-open path for untranslated `block`.

---

## Non-findings / limits

- Characterised against **grok 0.2.112 only**. Re-run the matrix on CLI upgrades before trusting the adapter.
- Did not re-test rewrite / `updatedInput` (already fail in the pane-viability spike).
- Did not characterise Stop-hook `block` empirically (docs are clear; out of PreToolUse scope).
- Did not exhaust every built-in tool name (`read_file`, `grep`, MCP `server__tool`, …). Matcher alias table covers the documented set; only Boss-guard-relevant tools were forced on the wire.
- Headless `-p` was used for decision probes (same hook runner as interactive; payload shape matches the earlier interactive spike dumps).

---

## Open questions (not blocking this finding)

1. Should the adapter also accept Claude-shaped guard output `hookSpecificOutput.permissionDecision` if any future guard emits it? (None of Boss's five do today.)
2. Is there any Grok version flag / config that enables Claude PreToolUse vocabulary (`block`/`approve`) natively? Not observed; not required if the adapter translates.
3. `write` vs matcher alias `Write`→`search_replace`: confirm on the next CLI release whether create-file ever collapses to `search_replace` only.
