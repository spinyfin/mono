# Grok permission isolation and sandbox rule grammar

- **Date:** 2026-07-27
- **Kind:** empirical investigation — findings + throwaway harness only; **no** Boss engine / driver code
- **Pinned version:** `grok 0.2.112 (9bbd559437aa) [stable]` (`~/.grok/bin/grok` → `~/.grok/downloads/grok-0.2.112-macos-aarch64`)
- **Host:** macOS aarch64 (Seatbelt sandbox backend)
- **Related:** [ghostty-grok-pane-viability.md](./ghostty-grok-pane-viability.md) (GROK_HOME isolation, `[compat.claude]` surface disable for hooks)
- **Artifacts:** [`grok-permission-isolation-artifacts/`](./grok-permission-isolation-artifacts/)

## Why this investigation exists

Two open empirical questions for a Grok-as-first-class-driver posture:

1. **(a) Claude-settings leakage.** Under a fresh, empty, isolated `GROK_HOME` with the full `[compat.claude]` disable block, `grok inspect --json` still reports `permissions.sources: ["~/.claude/settings.local.json (settings)"]` and probes `/Library/Application Support/ClaudeCode/managed-settings.json`. Are those rules **in force**, or merely discovered-and-listed? How do we scope them out?
2. **(b) Sandbox and rule grammar.** What do the built-in `--sandbox` profiles actually enforce? Is `read-only` a genuine OS-level reviewer posture? What does `sandbox.toml` accept? What does the fail-closed error text about "direct global-hook write protection" cover? Do `--allow`/`--deny` accept Claude `Bash(rm -rf:*)` spelling, native tool names like `run_terminal_command`, or both? What does a malformed rule do at runtime?

**Hard apparatus rules**

- Never point a probe at the operator's live `~/.grok` (auth is **byte-copied** into throwaway homes).
- Do not use model id `grok-code-fast-1` (retired; silent redirect rather than error).
- Probes use `grok-4.5` with `--always-approve --trust` headless `-p` unless otherwise noted.

---

## Verdict (read this first)

### (a) Claude permission rules are **in force**, not decorative

| Observation                                                                            | Result                                                                                                                                 |
| -------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| Isolated `GROK_HOME` + full `[compat.claude]` disable                                  | `inspect` still lists `~/.claude/settings.local.json` when `HOME` is the operator home                                                 |
| Restrictive deny in throwaway `~/.claude/settings.local.json` under `--always-approve` | **Enforced.** A4: model `A4_BLOCKED`, target file absent. A7 quotes the pipeline text `Denied by permission policy: deny rule on edit` |
| `[compat.claude] permissions = false` (undocumented)                                   | **No effect.** Sources still load; there is **no** permissions cell in the compat matrix                                               |
| Scope out via throwaway `HOME`                                                         | **Works** for user-global `~/.claude/**` (sources → `loaded: 0`)                                                                       |
| Project `<cwd>/.claude/settings.json`                                                  | Still loaded when the project is trusted, **even with scoped `HOME`**                                                                  |
| Managed settings path                                                                  | Always absolute `/Library/Application Support/ClaudeCode/managed-settings.json` (not under `HOME`); inactive when absent               |

**Boss implication:** `GROK_HOME` isolation alone is **not** sufficient to drop the operator's Claude permission rules. Pair it with a **scoped `HOME`** (or an empty `~/.claude` tree the worker can see), and treat project `.claude/settings.json` as an independent load path that survives home scoping.

### (b) Sandbox is real Seatbelt enforcement; rule grammar is Claude-class names

| Profile                             | Empirically (macOS, CWD **not** under `/tmp`)                                                                                                                                                      |
| ----------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `off` / `none`                      | No sandbox; no `sandbox-events.jsonl`                                                                                                                                                              |
| `workspace`                         | Write CWD + `$GROK_HOME` + `/tmp` (+ macOS temp dirs); outside sibling write → `operation not permitted`                                                                                           |
| `read-only`                         | **Cannot write CWD** (OS `Operation not permitted` + `FsViolation`); can write `$GROK_HOME` and `/tmp`                                                                                             |
| `strict`                            | Outside read blocked; CWD write allowed (like workspace write set) + restricted read set                                                                                                           |
| Custom `sandbox.toml`               | `extends` ∈ {`workspace`,`devbox`,`read-only`,`strict`}; `read_only` / `read_write` / `deny` / `restrict_network` honored; bad `extends=off` and brace-glob deny **refuse to start** (fail closed) |
| Direct global-hook write protection | Under `workspace`/`read-only`/`strict`: `$GROK_HOME/hooks/` and `hooks-paths` are kernel write-denied; other `$GROK_HOME` files remain writable                                                    |

**Critical footgun:** every built-in profile that sandboxes still lists **all of `/tmp` and `/private/tmp`** as writable. A probe (or worker cwd) under `/tmp/...` makes `read-only` look like a no-op for project writes. Cube workspaces under `~/.local/share/cube/workspaces/…` are fine; **do not validate reviewer isolation on a `/tmp` cwd**.

**Rule grammar**

| Spelling                                                                    | CLI behavior                                                                                                                                                                                           |
| --------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `Bash`, `Bash(rm -rf *)`, `Bash(rm -rf:*)`, `Edit`, `Write`, `Read`, `Grep` | Accepted; **enforced** (deny wins over always-approve)                                                                                                                                                 |
| `run_terminal_command`, `run_terminal_cmd` (bare)                           | Accepted at flag parse; **does not match** the shell tool (effective fail-open for that intent)                                                                                                        |
| `run_terminal_command(*)`                                                   | **Rejected:** `unknown tool prefix: run_terminal_command`                                                                                                                                              |
| `((((`                                                                      | With a real session (`-p` / TUI start): **rejected** `malformed rule: missing closing parenthesis`. With `--version` only: short-circuits and **appears** to accept (do not trust version-only checks) |
| `Agent(model:opus)`                                                         | **Rejected:** `unknown tool prefix: Agent`                                                                                                                                                             |
| `NotARealTool` (bare unknown)                                               | Accepted and **ignored** (fail-open)                                                                                                                                                                   |

---

## Method / apparatus

| Layer                 | What                                                                                                                       |
| --------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| Scratch root          | `$HOME/.cache/grok-perm-isolation-probe/` (non-`/tmp` for sandbox truth)                                                   |
| Isolated homes        | `$PROBE/homes/<probe>/` with byte-copied `auth.json` + fixture `config.toml`                                               |
| Throwaway Claude home | `$PROBE/claude_throwaway/...` as `HOME` so `~/.claude` is not the operator tree                                            |
| Headless driver       | `grok -p … --always-approve --trust --session-id <uuid> --cwd <cwd> --output-format json --model grok-4.5`                 |
| Evidence              | [`grok-permission-isolation-artifacts/evidence/`](./grok-permission-isolation-artifacts/evidence/)                         |
| Re-run harness        | [`grok-permission-isolation-artifacts/scripts/run_probes.sh`](./grok-permission-isolation-artifacts/scripts/run_probes.sh) |

Official docs consulted on-box (same install): `~/.grok/docs/user-guide/18-sandbox.md`, `22-permissions-and-safety.md`, `05-configuration.md` (compat cells).

---

## (a) Claude-settings leakage — detail

### A0 — Baseline leak under isolated GROK_HOME

With:

```toml
# $GROK_HOME/config.toml
[compat.claude]
hooks = false
agents = false
skills = false
plugins = false
rules = false
```

and `HOME` = operator home, `grok inspect --json` reports (shape):

```json
"permissions": {
  "sources": ["/Users/…/.claude/settings.local.json (settings)"],
  "loaded": 1,
  "managedSettingsPath": "/Library/Application Support/ClaudeCode/managed-settings.json",
  "managedSettingsExists": false,
  "managedSettingsActive": false
}
```

`externalCompat.cells` correctly shows Claude skills/rules/agents/hooks disabled from config. **There is no `permissions` surface in that matrix.** Official compat table only covers `skills`, `rules`, `agents`, `mcps`, `hooks`, `sessions`.

Project instructions from `~/.claude/Claude.md` appear with `"disabled": true, "compatibilityStatus": "disabled"` when `agents`/`rules` cells are off — discovery without application for that surface. Permission rules do **not** get the same treatment.

### A1 — Scoped empty HOME clears user-global sources

`HOME=/tmp-or-cache/.../home_empty` (no `.claude` tree), same isolated `GROK_HOME`:

```json
"permissions": { "sources": [], "loaded": 0, ... }
"projectInstructions": []
```

Managed settings path remains the absolute LaunchServices path (expected: not home-relative).

### A2 / A4 — Rules are enforced under always-approve

Throwaway `HOME` + `~/.claude/settings.local.json`:

```json
{
  "permissions": {
    "deny": ["Bash", "Edit", "Write", "Bash(*)"],
    "allow": ["Read", "Grep"]
  }
}
```

- `inspect`: `loaded: 6`, source path under the throwaway home.
- Control (A3, empty HOME): write of `probe_a3_write.txt` succeeds (`A3_DONE`, file contains `A3_OK`).
- Deny (A4): model replies `A4_BLOCKED`; **file absent**. Committed A4 JSON records only the model reply (and thought "write was blocked by permissions") — not a verbatim denial string. The explicit tool-denial quote appears in A7 (`Tool denied: Denied by permission policy: deny rule on edit`).

So the inspect `sources` list is not a curiosity: those rules participate in the same permission pipeline that docs describe as surviving always-approve (`deny` > mode pass-through).

### A5 / A6 — No undocumented compat kill-switch found

| Attempt                                                                   | Effect                                                                                              |
| ------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| Real `HOME` + isolated `GROK_HOME`                                        | Still loads operator `~/.claude/settings.local.json`                                                |
| `[compat.claude] permissions = false` (and `mcps`/`sessions = false`)     | Inspect still loads the same source; `permissions` is not a documented/effectual cell               |
| Env cells `GROK_CLAUDE_{SKILLS,RULES,AGENTS,HOOKS,MCPS,SESSIONS}_ENABLED` | Exist for the named surfaces; **no** `GROK_CLAUDE_PERMISSIONS_ENABLED` string in the 0.2.112 binary |

**Supported isolation levers for user-global Claude permissions**

1. **Scoped `HOME`** for the worker process (preferred for total `~/.claude` quarantine). Note: this also relocates anything else that keys off `HOME` (not `GROK_HOME`) — including paths tools resolve as `~/…`. Keep `GROK_HOME` as the auth/session root so `auth.json` does not move with `HOME`.
2. **Empty or policy-controlled `~/.claude/settings*.json`** visible to that `HOME`.
3. **Not sufficient alone:** isolated `GROK_HOME` + `[compat.claude]` surface disables.

### A7 — Project `.claude/settings.json` survives HOME scoping

With scoped empty `HOME`, trusted project, and `<cwd>/.claude/settings.json` deny on Bash/Edit/Write:

- `inspect`: `sources: ["…/cwd/.claude/settings.json (settings)"], loaded: 3` when `projectTrusted: true`.
- Headless write: `Tool write was not executed: Denied by permission policy: deny rule on edit`.

If `projectTrusted` is false (no `--trust` / trust pre-seed / `GROK_FOLDER_TRUST=0`), inspect may show `loaded: 0` even when a project file exists — trust gates discovery in inspect. Runtime with `--trust` still applies the file.

**Boss implication:** a shared monorepo that commits restrictive `.claude/settings.json` will bind Grok workers regardless of operator-home quarantine. Mono today has `.claude/CLAUDE.md` but no permission-bearing `settings.json` in the workspace root used for this spike.

---

## (b) Sandbox and rule grammar — detail

### Built-in profiles (docs + macOS Seatbelt evidence)

From on-box `18-sandbox.md` and live `sandbox-events.jsonl` `ProfileApplied` rows:

| Profile         | FS read                                        | FS write                                        | Child network (Linux) | macOS network note     |
| --------------- | ---------------------------------------------- | ----------------------------------------------- | --------------------- | ---------------------- |
| `off` (default) | unrestricted                                   | unrestricted                                    | unrestricted          | n/a                    |
| `none`          | same as `off` (accepted alias; no events file) |                                                 |                       |                        |
| `workspace`     | everywhere                                     | CWD + `$GROK_HOME` + `/tmp` + macOS temp dirs   | allowed               | unrestricted child net |
| `devbox`        | everywhere                                     | top-level dirs except `/data` (disposable VM)   | allowed               | no hook write-protect  |
| `read-only`     | everywhere                                     | `$GROK_HOME` + `/tmp` + temp dirs (**not** CWD) | blocked¹              | ¹ no-op on macOS       |
| `strict`        | CWD + system paths                             | CWD + `$GROK_HOME` + `/tmp` + temp dirs         | blocked¹              | ¹ no-op on macOS       |

Live enforcement proofs (cwd under `~/.cache/…`, **not** `/tmp`):

| Probe                              | Profile     | Result                                                                                                           |
| ---------------------------------- | ----------- | ---------------------------------------------------------------------------------------------------------------- |
| Write `sb_ro_write.txt` in CWD     | `read-only` | **Blocked** — tool + shell `Operation not permitted`; `FsViolation` write event; file absent; model `RO_BLOCKED` |
| Write `/tmp/grok_sb_ro_tmp_ok.txt` | `read-only` | **Allowed** (exit 0) while CWD write still denied                                                                |
| Write CWD + sibling outside dir    | `workspace` | CWD ok; outside `operation not permitted`                                                                        |
| Read outside `secret.txt`          | `strict`    | **Blocked** — `FsViolation` read; `STRICT_READ_BLOCKED`                                                          |
| Write outside                      | `off`       | Allowed (control)                                                                                                |

**`read-only` is a genuine reviewer-style kernel posture** for the project tree, not a soft "please don't write" policy — **provided the workspace is not under `/tmp`**. Session state under `$GROK_HOME` remains writable by design.

### `/tmp` always writable — validation hazard

`ProfileApplied` for `read-only` includes:

```text
read_write_paths: [$GROK_HOME, /tmp, /var/tmp, /private/tmp, /private/var/tmp, /private/var/folders, /var/folders/…]
```

Early probes that used `/tmp/grok-perm-isolation/cwd` **incorrectly** showed `read-only` allowing project writes. Re-ran under `~/.cache/grok-perm-isolation-probe/cwd` for the verdict above.

### `sandbox.toml` schema (accepted fields)

```toml
[profiles.NAME]
extends = "workspace"          # workspace | devbox | read-only | strict (NOT off/none)
restrict_network = true        # child net; Linux-effective
read_only = ["/data"]          # extra read-only paths
read_write = ["/tmp/scratch"]  # extra writable paths
deny = ["**/.env", "**/*.pem"] # kernel read+write/rename deny; globs: *, ?, **, [classes]
```

| Invalid config                     | Behavior (0.2.112)                                                                                                                                                                                      |
| ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `extends = "off"`                  | Refuse start: `Profile '…' extends 'off', but 'off'/'none' is not a valid base profile` + `could not apply the '…' sandbox profile (including direct global-hook write protection); refusing to start.` |
| `deny = ["*.{pem,key}"]`           | Refuse start: unsupported `{` brace alternation (fail closed)                                                                                                                                           |
| Built-in name collision (`devbox`) | Built-in wins over user definition (docs)                                                                                                                                                               |

Custom profile `probe_custom` (extends workspace, `deny` on outside secret, `read_only` on outside dir): secret read → `FsViolation`; CWD write succeeded.

### Direct global-hook write protection

Docs + probe under `--sandbox workspace`:

| Path                                                | Write under workspace                  |
| --------------------------------------------------- | -------------------------------------- |
| `$GROK_HOME/hooks/probe.json`                       | **Denied** (`operation not permitted`) |
| `$GROK_HOME/hooks-paths`                            | **Denied**                             |
| `$GROK_HOME/session_write_test.txt` (ordinary file) | **Allowed**                            |

Covers Grok-owned direct disk hook sources only. Claude/Cursor global settings paths are **not** covered by this Seatbelt write-deny (compat discovery remains a separate gate). Symlinked `$GROK_HOME` is refused at sandbox start (docs).

Fail-closed error strings for unusable custom profiles always mention **"including direct global-hook write protection"** even when the root cause is an invalid `extends` or glob — that clause is part of the generic refuse-to-start path, not a separate check that failed independently.

### Permission rule grammar (`--allow` / `--deny`)

Aligned with `22-permissions-and-safety.md`:

- Recognized tool prefixes: `Bash`, `Read` (+ `NotebookRead`), `Edit` (+ `Write`, `NotebookEdit`), `Grep` (+ `Glob`), `MCPTool`, `WebFetch`, `WebSearch`, bare `*`.
- Structured native config uses lowercase classes: `bash`, `read`, `edit`, `grep`, `mcp`, `webfetch`, `websearch`.
- Claude `cmd:*` suffix form works: `Bash(rm -rf:*)` accepted; combined with `Bash(rm *)` / `Bash(rm -f *)` blocked `rm -f victim.txt` while still allowing a non-rm write.
- **Native Grok tool ids** (`run_terminal_command`, `run_terminal_cmd`) are **not** valid rule prefixes for shell. Use `Bash` / `Bash(…)`.
- `Edit` deny also affects shell writes that touch files (docs + B2 behavior).

### Malformed / unknown rules — fail-open vs fail-closed

| Input                                  | Session start                                                             | Runtime effect                                       |
| -------------------------------------- | ------------------------------------------------------------------------- | ---------------------------------------------------- |
| `--deny '(((('`                        | **Fail closed** (`malformed rule: missing closing parenthesis`, exit ≠ 0) | Never runs                                           |
| `--deny '((((` + `--version`           | Exit 0 (version path does not validate rules)                             | n/a — **false sense of acceptance**                  |
| `--deny 'Agent(model:opus)'`           | **Fail closed** (`unknown tool prefix: Agent`)                            | Never runs                                           |
| `--deny 'run_terminal_command(*)'`     | **Fail closed** (`unknown tool prefix: run_terminal_command`)             | Never runs                                           |
| `--deny 'run_terminal_command'` (bare) | Starts                                                                    | Rule inert for shell (fail-open **for that intent**) |
| `--deny 'NotARealTool'`                | Starts                                                                    | Rule inert (fail-open)                               |

So the fail-open shape this project cares about is **not** "silent accept of `((((`" on a real worker launch — 0.2.112 rejects that before the model runs. The residual fail-open shapes are **unknown bare tool names** and **wrong tool vocabulary** (`run_terminal_command` instead of `Bash`), which accept cleanly and never match.

Settings-file rules (Claude JSON) document a softer path: unrecognized tools "skipped with a warning rather than failing the load." CLI flags for parenthesized unknown prefixes are stricter.

---

## Recommendations for a Grok worker posture (Boss)

These are findings-driven recommendations, not implemented in this PR.

1. **Always** use a Boss-owned `GROK_HOME` (already prior-art).
2. **Also** set a worker-scoped `HOME` (or provision an empty `HOME/.claude`) so operator `settings.local.json` cannot inject deny/allow/defaultMode. Keep auth only under `GROK_HOME`.
3. Keep full `[compat.claude]` surface disables for hooks/agents/skills/plugins/rules (prior art) — but **do not** treat that as permission isolation.
4. Audit project `.claude/settings.json` / `.claude/settings.local.json` in the workspace the worker trusts; they load independently of `HOME`.
5. For reviewer / read-only workers: `--sandbox read-only` (kernel-enforced) **plus** permission denies if you also want soft policy messages; ensure cwd is **not** under `/tmp`.
6. Prefer Claude-class rule spellings (`Bash(…)`, `Edit`, `Read`) in `--deny` / `[permission]`; never rely on `run_terminal_command` as a deny prefix.
7. Validate deny rules with a real `-p` probe (or `inspect` + a forced tool call), not `grok --deny … --version`.
8. Custom sandbox profiles: fail-closed on bad config is good; keep profiles under `$GROK_HOME/sandbox.toml` for the worker, not the operator's live `~/.grok`.

---

## Negatives / limits

1. **macOS-only enforcement data** for Seatbelt; Linux Landlock/bwrap/seccomp network claims taken from docs, not re-proven here.
2. Child-network blocking on `read-only`/`strict` is a **documented no-op on macOS** — not re-tested as a positive control.
3. Did not fuzz the full settings-file skip/warn path for every unknown tool string (CLI parenthesized form was the fail-closed focus).
4. Operator real `settings.local.json` on this host had a single allow (`mcp__acp__Bash`) and no denies — leakage risk is host-dependent; the throwaway restrictive file is the proof of enforcement.
5. Cost: headless probes spent real `grok-4.5` turns; harness keeps prompts short.

---

## Appendix A — Scratch locations

| Path                                                                  | Purpose                                                                                    |
| --------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| `~/.cache/grok-perm-isolation-probe/`                                 | Primary non-`/tmp` probe root (homes, cwd, outside, results)                               |
| `/tmp/grok-perm-isolation/`                                           | Early probes (valid for permission rules; **invalid** alone for sandbox write conclusions) |
| `tools/boss/docs/investigations/grok-permission-isolation-artifacts/` | Committed fixtures, harness, sample evidence                                               |

## Appendix B — How to re-run

```bash
# From a machine with grok 0.2.112+ and a one-time interactive login:
cd tools/boss/docs/investigations/grok-permission-isolation-artifacts
./scripts/run_probes.sh          # all groups
./scripts/run_probes.sh a        # Claude leakage only
./scripts/run_probes.sh parse    # rule parse checks (no model)
./scripts/run_probes.sh c        # sandbox (requires non-/tmp PROBE_ROOT; default is ~/.cache/…)
```

Environment overrides: `GROK_BIN`, `MODEL` (must not be `grok-code-fast-1`), `PROBE_ROOT`, `AUTH_SRC`, `REAL_HOME` (operator home for A5/A6; defaults to `$HOME` at script start, before probes rewrite `HOME`).

## Appendix C — Evidence index

| Group          | Paths under `grok-permission-isolation-artifacts/evidence/`                                                                                                                                                                                                        |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Claude leakage | `a_claude/a1_inspect_scoped_home.json`, `a2_inspect_deny.json`, `a3_control.json`, `a4_claude_deny.json`, `a5_real_home_inspect.json`, `a6_compat_perms.json`, `a7c_inspect.json`, `a7c_run.json`                                                                  |
| Rule grammar   | `b_rules/b1_*.json`, `b2_cli_deny_edit.*` (`Edit` deny), `b3_*.json`, `b4_*.json`, `b4b_native_cmd.*` (`run_terminal_cmd` fail-open), `b5_malformed.err` (`malformed rule…`), `b5c_unrecognized.err`, `b5c2.json` — all harness-reproducible via `run_probes.sh b` |
| Sandbox        | `c_sandbox/sb_ro_nontmp.json`, `sb_ws_nontmp.json`, `sb_strict_nontmp.json`, `sb_ro_tmp_ok.json`, `sb_custom.json`, `sb_hook.json`, `sb_bad.err`, `sb_badglob.err`, `sandbox_events_merged.jsonl`                                                                  |
