# Do Boss's Codex PreToolUse guards actually fire? — empirical answer

- **Date:** 2026-07-29
- **Kind:** investigation with code changes (guard corrections + guard-execution observability).
- **Subject:** Boss's four/five `PreToolUse` guards materialised into `$CODEX_HOME/config.toml` by the Codex driver, against the tool surface the models Boss actually dispatches (`gpt-5.6-sol` / `gpt-5.6-terra` / `gpt-5.6-luna`).
- **Apparatus:** `codex-cli 0.145.0` on this host, an isolated `CODEX_HOME` under a scratch dir (auth snapshot byte-copied in, never the operator's `~/.codex`), `codex exec --dangerously-bypass-hook-trust` so hook trust was never in question, and a canary hook armed with `matcher = ".*"` that appended every raw payload to a log. Guard scripts under test were extracted verbatim from the Rust constants they ship as.
- **Question asked:** the guards are believed on. Are they? Two independent doubts had been raised and neither was settleable from captured state: (1) whether the current shell tool still reports `tool_name: "Bash"`, and (2) whether `tool_input` is still an object with a `command` string, given that the model now drives the shell through a JavaScript cell.

## Verdict

**The shell guards fire, and their blocks are enforced. Both stated doubts resolve in Boss's favour — and the investigation found two _different_ holes that were real.**

1. **`tool_name` is still `Bash`.** Confirmed on `gpt-5.6-sol` and `gpt-5.6-terra`.
2. **`tool_input` is still `{"command": "<shell string>"}`.** The JavaScript is real, but it is not what the hook sees — Codex hooks the _inner_ `tools.exec_command` call, not the outer cell. The premise that the guard would `shlex.split` JS source into `{cmd:jj` fragments was **describing the rollout record, not the hook payload**; those are two different surfaces and only the rollout carries JS.
3. **`apply_patch` was unguarded.** Codex's file-edit tool arrives as `tool_name: "apply_patch"` with the whole patch body in `tool_input.command` and no `file_path` key. The Boss data-directory gate (armed `.*`, so it _did_ run) looked for `file_path` and then only read `command` when `tool == "Bash"` — so it approved every Codex file edit unread.
4. **`write_stdin` defeated the push guard outright.** A cell can start a long-lived process with `exec_command` (guards approve `sh -s`) and then feed it arbitrary command lines with `tools.write_stdin`, which fires **no hook at all**. Demonstrated: `jj git push --dry-run` executed that way with the PR-redirect guard armed, and the guard was never consulted.
5. **Codex app/MCP tools bypassed every command guard.** `tools.mcp__codex_apps__github_create_pull_request` (and `create_branch`, `create_commit`, `update_ref`, `merge_pull_request`, …) are available to the cell, authenticated as the operator's GitHub account. They _do_ fire `PreToolUse` — as `tool_name: "mcp__codex_apps__github__<verb>"`, which no `matcher = "Bash"` guard sees.
6. **There was no signal anywhere that a guard ran.** Codex's rollout carries no hook record. An approved guard left no trace at all; a blocked one appeared only as prose inside the cell's `custom_tool_call_output`.

Findings 4 and 5 mean the worker prompt's assertion — "a PreToolUse hook blocks direct push/PR-create attempts and redirects you to cube" — was **false in two reachable ways** while reading as true. Both are closed by this change; neither was closed by weakening the assertion.

## 1. Why the doubt existed: the models changed underneath the evidence

The design doc's live capture (`tool_name: "Bash"`, `tool_input: {"command": "echo hooktest"}`) was taken on `gpt-5.5`. `codex debug models` explains the gap in one column:

| model                 | `tool_mode`      | `shell_type`    |
| --------------------- | ---------------- | --------------- |
| `gpt-5.6-sol`         | `code_mode`      | `shell_command` |
| `gpt-5.6-terra`       | `code_mode_only` | `shell_command` |
| `gpt-5.6-luna`        | `code_mode_only` | `shell_command` |
| `gpt-5.5`             | _(none)_         | `shell_command` |
| `gpt-5.4`             | _(none)_         | `shell_command` |
| `gpt-5.3-codex-spark` | _(none)_         | `shell_command` |

Boss dispatches `gpt-5.6-terra` for Standard work and `gpt-5.6-sol` for investigations, and `gpt-5.6-luna` is on the menu. **Every model Boss dispatches is a code-mode model; the model the evidence came from is not.** That is a real basis for doubt, and it is why re-testing on the dispatched model — not the documented one — was the right instinct.

## 2. What code mode actually does to the payload

A `gpt-5.6-terra` turn asked to run three commands in one cell produced this **rollout** record:

```json
{
  "type": "custom_tool_call",
  "name": "exec",
  "input": "const r = await tools.exec_command({\"cmd\":\"echo alpha ; echo beta ; jj git push --dry-run .\",\"workdir\":\"…\",\"yield_time_ms\":30000,\"max_output_tokens\":2000});\ntext(r.output);\n"
}
```

So the rollout record on a code-mode model is one `custom_tool_call` named `exec` whose input is JavaScript source. The **hook** for the same call was:

```json
{
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": { "command": "echo alpha ; echo beta ; jj git push --dry-run ." },
  "permission_mode": "bypassPermissions",
  "model": "gpt-5.6-terra",
  "session_id": "…",
  "turn_id": "…",
  "tool_use_id": "…"
}
```

**Codex hooks the bridge call, not the cell.** `tools.exec_command` is normalised into Claude's `Bash` shape before `PreToolUse` runs, so a guard reading `tool_input.command` sees a shell command string on a code-mode model exactly as it does on `gpt-5.5`. The keys are unchanged from the documented capture: `session_id`, `turn_id`, `transcript_path`, `cwd`, `model`, `permission_mode`, `tool_name`, `tool_input`, `tool_use_id`.

Two corollaries, both load-bearing:

- **The outer cell produces no `PreToolUse` event.** A cell that calls no tool (verified with one printing `Object.keys(tools)`) fires nothing, with a `.*` matcher armed. Guards see tool calls, never cells.
- **A `block` is enforced pre-execution.** With the real `PR_REDIRECT_GUARD_COMMAND` armed, `jj git push --dry-run` inside a cell died with `Command blocked by PreToolUse hook: …` and never ran. The guard is not advisory.

## 3. The cell runtime is sealed — which is what makes the shell path worth guarding

Asked to print its own globals, the cell reported:

```text
typeof require    -> undefined
typeof process    -> undefined
typeof fetch      -> undefined
Object.keys(globalThis) -> ["tools","ALL_TOOLS","clearTimeout","setTimeout","text","image",
                            "audio","generatedImage","store","load","notify","yield_control","exit"]
```

No `child_process`, no network, no filesystem except through `tools`. So the reachable execution surface is exactly the `tools` object, and the guard question reduces to: which `tools` entries fire a hook, and under what name?

## 4. Coverage map (measured, not inferred)

| route                             | fires `PreToolUse`? | `tool_name`                       | `tool_input`                    | seen by a `matcher = "Bash"` guard? |
| --------------------------------- | ------------------- | --------------------------------- | ------------------------------- | ----------------------------------- |
| `tools.exec_command`              | **yes**             | `Bash`                            | `{"command": "<shell string>"}` | yes                                 |
| the code-mode cell itself         | **no**              | —                                 | —                               | n/a                                 |
| `tools.apply_patch`               | **yes**             | `apply_patch`                     | `{"command": "<patch body>"}`   | **no** (needs `.*`)                 |
| `tools.write_stdin`               | **no**              | —                                 | —                               | **no — unhookable**                 |
| `tools.mcp__codex_apps__github_*` | **yes**             | `mcp__codex_apps__github__<verb>` | the tool's own arguments        | **no** (needs `.*`)                 |

The `write_stdin` row is the one that cannot be fixed by a matcher: there is no payload to match. Reproduction, with the PR-redirect guard armed:

```text
cell: const s = await tools.exec_command({cmd:'sh -s', yield_time_ms:1500});
      await tools.write_stdin({session_id: s.session_id, chars: 'jj git push --dry-run\n'});
guard trace: one PreToolUse for `sh -s` (approved). Nothing for write_stdin.
output:      jj git push --dry-run
             Error: There is no jj repo in "."      <- jj ran
```

`jj` was reached. It failed only because the scratch cwd is not a repo.

## 5. What changed in the tree

**Guard corrections**

1. **All three shared deny guards fail closed on an unreadable payload.** `BOSS_LAUNCH_GUARD_COMMAND`, `PR_REDIRECT_GUARD_COMMAND` and `REVISION_PR_GUARD_COMMAND` previously did `inp.get('tool_input',{}).get('command','')` — which raises (guard dies, Codex treats that as approval) when `tool_input` is a bare string, and silently approves when `command` is absent. They now share one preamble that blocks with an explicit reason when the payload cannot be read as a shell command. Same behaviour on the Claude path; the shapes it rejects do not occur there, which is the point — if they ever start occurring it will be loud.
2. **The data-directory gate reads `apply_patch`.** It extracts every path named by a `*** Add File:` / `*** Update File:` / `*** Delete File:` / `*** Move to:` header, and applies its `$VAR`/`~` substring belt to the patch text. It still approves tools it has nothing to say about (a read, an image view) — silence for "no candidate path" is correct — but a payload it _should_ have read and cannot now blocks.
3. **A new Codex-only tool-surface guard** (`matcher = ".*"`, armed for every Codex worker kind) closes the two routes a command matcher structurally cannot reach:
   - every `mcp__*` tool call is denied, with a reason redirecting to `gh` / `cube pr create`. Deny-by-default rather than an allowlist of read-only verbs, because the app catalog drifts and an allowlist does not; Boss injects no MCP tooling of its own, so nothing legitimate is lost.
   - invocations whose only effect is to open a stdin-driven command channel are denied — a bare interpreter, `sh -s`, `-i`, `-`, and interactive editors/pagers with shell escapes. This is the only hookable point on the `write_stdin` path: with no session to write into, there is nothing to feed. `bash -lc '…'`, `python3 -c '…'`, `python3 script.py`, `sqlite3 db 'sql'` are all unaffected.

**Observability**

4. **Every materialised guard now runs under a trace shim.** It records one JSON line per invocation to `$CODEX_HOME/guard-trace.jsonl` (guard, tool, decision, reason head, exit code, session id), re-emits the guard's own decision verbatim, and — the part that matters — **converts a guard that crashes, exits non-zero, or prints something that is not a decision into a `block`**. Codex would otherwise read that silence as approval.
5. **The engine reports the trace at each turn boundary.** The rollout progress session emits `[codex-guard-trace] …` with the counts and every block, or — when the turn ran tool calls and _no_ guard record exists — `[codex-guards-silent] …`, logged at `error` with its own counter. That second signal is the detection half of the hook-trust question the design doc left open: an untrusted or unexecutable hook is skipped with no stream event, and this is the only way Boss can tell that from a healthy run.

## 6. Live validation of the shipped shape

With the shipped shim and wrappers materialised by hand exactly as `materialize_guards` writes them, one `gpt-5.6-terra` turn asked to run `echo alpha`, then `jj git push --dry-run`, then a GitHub app tool:

```text
01_boss_launch_guard         Bash                                     approve
02_codex_tool_surface_guard  Bash                                     approve
03_pr_redirect_guard         Bash                                     approve      <- echo alpha
03_pr_redirect_guard         Bash                                     block        Workers must not push branches or open PRs…
01_boss_launch_guard         Bash                                     approve
02_codex_tool_surface_guard  Bash                                     approve
02_codex_tool_surface_guard  mcp__codex_apps__github__get_user_login   block        Blocked: Boss workers must not act through Codex app/MCP tools…
```

Codex reported both blocks to the model and executed neither. A second turn attempting the `sh -s` + `write_stdin` bypass was refused at the session start, and the trace recorded the refusal.

## 7. What remains open

- **`--dangerously-bypass-hook-trust` was used for these captures**, deliberately: the question under test was payload shape and guard behaviour, not trust provisioning, which has its own gate (`codex-hook-trust-provisioning-2026-07-26.md`) and its own live `hooks/list` attestation. The `[codex-guards-silent]` signal is what covers the trust failure mode at run time.
- **The rollout normaliser mislabels a code-mode cell's command.** `canonical_rollout_tool_call` maps `name: "exec"` → `tool_name: "Bash"` and then looks for `cmd` in the input; for a code-mode cell the input is JS _source_, not JSON, so the whole JS body becomes the reported "command". That is a progress-display fidelity bug, not a guard bug (the guards read the hook payload, which is correct), and it is left as-is here.
- **The interactive-interpreter list is a list.** It covers the shells, the common REPLs, and the editors/pagers with shell escapes; a determined agent could find an interactive stdin consumer that is not on it. The structural fix would be Codex disabling `write_stdin` per-config, which `--strict-config` shows no key for on 0.145.0 (`tools.write_stdin` is rejected as unknown).
