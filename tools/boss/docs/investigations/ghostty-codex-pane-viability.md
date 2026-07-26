# Ghostty + Codex pane viability spike

- **Date:** 2026-07-26
- **Work item:** ghostty + codex pane viability
- **Kind:** empirical throwaway harness — no Boss integration, no CodexDriver code
- **Pinned versions (re-checked before each observation cluster):**
  - `codex-cli 0.145.0` (`/Users/brianduff/.local/bin/codex` → `~/.codex/packages/standalone/current/bin/codex`)
  - Ghostty `1.3.1` (stable) (`/Applications/Ghostty.app`) — Layer A outsider topology only
  - **GhosttyKit** prebuilt `ghosttykit-5659cef` (`spinyfin/ghostty-prebuilts`, same pin as `MODULE.bazel` `@ghostty_kit`, sha256 `82b8d947484a21e1a9d186628b8af5e3f2e81dc96925f3cdbc1766ececa814a1`) — Layer D embed topology
- **Host:** macOS (Darwin), no `/proc/<pid>/fd`
- **Related:** [codex-progress-channel-decision-2026-07-24.md](./codex-progress-channel-decision-2026-07-24.md); Codex driver design (PR discussion around §A-1 / OQ-5 / T-05 / T-17); PR #2363 (engine-side stdout JSONL reader)

## Why this spike exists

Two careful positions currently contradict each other on an empirical question:

1. **Design claim (§A-1):** PR #2363 landed an engine-side stdout JSONL reader, so the transport split and reader are no longer future work.
2. **Review claim:** the app owns the pty and the engine only ever receives `shell_pid`, so an engine-side reader of the worker process's stdout **cannot work**, and T-05 as written is not implementable against the current app/engine split.

Both cannot be true as statements about the _pane-hosted Boss shape_. This spike settles that (Q1) and six neighboring execution-shape questions by observation.

**Revision:** The first pass answered "can an **engine-like outsider** with only `shell_pid` read stdout?" It did **not** answer whether the **Boss app surface / GhosttyKit embedder** can observe or inject. Treating "outsider cannot open the slave" as "reading pane content is impossible" is a category error if GhosttyKit exposes surface text to the embedding process. Layer D re-tests Q1/Q2 on that honest apparatus.

## Method / apparatus

Throwaway only. Four layers of harness:

| Layer                        | What it is                                                                                                                                                                         | Used for                                                                                                                 |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| **A. Real Ghostty window**   | `open -na Ghostty.app --args -e <script>`                                                                                                                                          | Q1 outsider topology (pane-owned pty + `shell_pid` only), Q4                                                             |
| **B. Local pty-owner**       | Python `pty.openpty()` holding the master, child shell on the slave                                                                                                                | Q1 master-side capture contrast; Q2 master inject + **harness-emulated** post-exit `read`/`eval` (first pass); Q3/Q5 TUI |
| **C. Direct process / pipe** | `codex exec --json … </dev/null` with the observer owning stdout                                                                                                                   | Q6, Q7, contrast for Q1 engine-spawn                                                                                     |
| **D. GhosttyKit embed host** | Minimal AppKit process embedding libghostty via the **same APIs Boss uses** (`ghostty_surface_new` / `ghostty_surface_read_text` / `ghostty_surface_text` + `ghostty_surface_key`) | **Q1 embed topology**, **Q2 Boss-equivalent inject** (real interactive shell)                                            |

Harness scripts and selected raw captures are under [`ghostty-codex-pane-viability-artifacts/`](./ghostty-codex-pane-viability-artifacts/). Layer D source + evidence: [`ghosttykit_host/`](./ghostty-codex-pane-viability-artifacts/ghosttykit_host/).

Prompts were deliberately short (`reply with exactly: …`, `sleep N`) to limit cost/time.

---

## GhosttyKit apparatus (Layer D)

### What Boss actually uses

From `tools/boss/app-macos/Sources/Ghostty/`:

| Operation              | Boss call site                                                         | GhosttyKit C API                                                                                                           |
| ---------------------- | ---------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| Bootstrap              | `GhosttyRuntime` / `GhosttyBootstrap`                                  | `ghostty_init`, `ghostty_config_new`, `ghostty_app_new`                                                                    |
| Embed surface          | `GhosttyTerminalHostView.makeSurface`                                  | `ghostty_surface_new` with `GHOSTTY_PLATFORM_MACOS` + `nsview`                                                             |
| Spawn worker           | `TerminalLaunchSpec.initialInput` → surface config                     | `ghostty_surface_config_s.initial_input` typed into the pane shell                                                         |
| Observe pane text      | Claude monitor scrape in `GhosttyTerminalHostView.readVisibleContents` | **`ghostty_surface_read_text(surface, selection, &text)`** with `GHOSTTY_POINT_VIEWPORT` top-left → bottom-right           |
| SendToPane / intervene | `WorkersWorkspaceModel.sendToPane` → `host.submitText`                 | **`ghostty_surface_text`** (paste path) then **`ghostty_surface_key` Return** (`keycode 0x24`, `unshifted_codepoint 0x0D`) |
| Interrupt              | `host.sendInterrupt`                                                   | `ghostty_surface_key` Esc (`keycode 0x35`)                                                                                 |
| Shell pid for engine   | `ghostty_surface_foreground_pid`                                       | same                                                                                                                       |

The engine never holds the pty master; it receives `shell_pid` after attach. Observation of pane content, if any, must happen **in the app process that owns GhosttyKit** (or via a file channel such as rollout). Layer D tests that app-side path without wiring production Boss.

### Throwaway host

[`ghostty-codex-pane-viability-artifacts/ghosttykit_host/`](./ghostty-codex-pane-viability-artifacts/ghosttykit_host/) is a minimal SwiftPM AppKit executable that:

1. Links the **pinned** GhosttyKit xcframework (`ghosttykit-5659cef`, not the Bazel analysis stub at `ThirdParty/`).
2. Creates a window + `NSView`, `ghostty_surface_new`, default shell + `initial_input` running a pane script with `codex exec --json … "run: sleep 18; reply with exactly: gkit-embed-done"`.
3. Polls **`ghostty_surface_read_text`** (viewport + screen) every 0.5s — same selection shape Boss uses for the Claude monitor.
4. Mid-run (~t=6s) injects via **Boss-equivalent `submitText`** (`ghostty_surface_text` + Return).
5. After the script returns to the interactive shell prompt, injects again post-exit.

Commands:

```sh
cd tools/boss/docs/investigations/ghostty-codex-pane-viability-artifacts/ghosttykit_host
# materialize pinned xcframework (not committed; see README.md)
curl -fsSL -o /tmp/GhosttyKit-5659cef.tar.gz \
  "https://github.com/spinyfin/ghostty-prebuilts/releases/download/ghosttykit-5659cef/GhosttyKit-5659cef.tar.gz"
# sha256 must be 82b8d947484a21e1a9d186628b8af5e3f2e81dc96925f3cdbc1766ececa814a1
tar -xzf /tmp/GhosttyKit-5659cef.tar.gz && mv GhosttyKit.xcframework .local-GhosttyKit.xcframework
./run.sh   # swift build -c release && ghosttykit_spike
# committed evidence snapshot under evidence/
```

Pinned stamps from the successful run (`evidence/PINS.txt`, `evidence/codex_version.txt`):

```text
codex-cli 0.145.0
ghosttykit_prebuilt: ghosttykit-5659cef
observe_api: ghostty_surface_read_text
inject_api: ghostty_surface_text + ghostty_surface_key
```

### Layer D raw outcome (headline)

| Signal                                                       | Result                                                                                         |
| ------------------------------------------------------------ | ---------------------------------------------------------------------------------------------- |
| `saw_thread_started` via surface text                        | **true** at t≈2.2s                                                                             |
| `saw_gkit_embed_done` via surface text                       | **true** at t≈26.2s                                                                            |
| Full `--json` JSONL recoverable in viewport                  | **yes** (`thread.started` / `turn.*` / `item.*` / agent text)                                  |
| Mid-`codex exec` inject via GhosttyKit                       | echoed mid-stream; **not** agent-consumed                                                      |
| Post-script **interactive zsh** executed buffered mid-inject | **yes** — `injected_side_effect.txt` = `GKIT_INJECT_VIA_SURFACE_TEXT` (**not** harness `eval`) |
| Post-exit inject on idle shell                               | **yes** — `post_exit_side_effect.txt` = `GKIT_POST_EXIT_INJECT`                                |
| `codex_exit`                                                 | `0`                                                                                            |

Evidence: `ghosttykit_host/evidence/SUMMARY.txt`, `host.log`, `viewport_final.txt`.

---

## Q1 — Dual topology: outsider `shell_pid` vs GhosttyKit embedder

### Claim under test

Two **different** observers — do not collapse them:

1. **Outsider / engine-like (Layers A/B/C):** separate process holds only `shell_pid` (and maybe the slave tty path). Can it read stdout?
2. **Embedder / app-like (Layer D):** process that owns the GhosttyKit surface. Can it observe worker output via GhosttyKit APIs?

### Topology 1 — outsider with only `shell_pid` (first pass; still stands)

### What we ran

```sh
# Ghostty owns the pty; script records shell_pid + tty
open -na Ghostty.app --args -e /tmp/codex-pane-spike/run_pure.sh
# run_pure.sh essentially:
#   echo $$ > shell_pid.txt; tty > tty.txt
#   codex exec --json --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \
#     "run: sleep 20; reply with exactly: pure-done"
```

Observed process tree (excerpt):

```
  PID  PPID TTY      STAT COMMAND
18178 …     ttys023  Ss+  /usr/bin/login -flp … /tmp/codex-pane-spike/run_pure.sh
18179 …     ttys023  S+   /bin/zsh /tmp/codex-pane-spike/run_pure.sh
18546 …     ttys023  S+   …/codex exec --json … pure-done
```

`lsof` on the live codex pid (pure TTY inherit, no shell pipes):

```
COMMAND   PID   FD   TYPE DEVICE NAME
codex   18546    0u  CHR  16,23  /dev/ttys023
codex   18546    1u  CHR  16,23  /dev/ttys023
codex   18546    2u  CHR  16,23  /dev/ttys023
```

Outside observer (separate process; only knowledge = pid + `tty` from the pid file):

```python
fd = os.open("/dev/ttys023", os.O_RDONLY | os.O_NONBLOCK)
# select() for 3s, read any available data
# RESULT: TOTAL 0 bytes across six 0.5s slices
print(os.path.exists(f"/proc/{cpid}/fd/1"))  # False — macOS has no /proc
```

Raw observer log:

```
open RDONLY ok 5
select timeout slice, total_so_far 0
… (×6) …
RDONLY total bytes 0
proc fd exists? False
macOS cannot open another process's pipe end without debugger entitlements
```

`lsof /dev/ttys023` listed only the slave holders (`zsh`, `codex`) — **not** Ghostty. Ghostty holds the **master** side (a different kernel object; not visible under the slave path name).

### Contrast: when the observer owns the pipe

```sh
codex exec --json --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \
  "reply with exactly: pipe-owned" </dev/null
```

Stdout is fully readable by the parent (as expected):

```jsonl
{"type":"thread.started","thread_id":"019f9e0d-67f3-7fe1-99b4-265f71a60ec2"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"pipe-owned"}}
{"type":"turn.completed","usage":{…}}
```

A local pty-owner that holds the **master** also captures every byte of codex JSONL (see Q2 harness: `master_capture bytes: 898`).

### Interpretation (topology 1 only)

**No. An outside process that holds only `shell_pid` cannot read the pane-hosted agent's stdout on this host.**

What blocks it, exactly:

1. **macOS has no `/proc/<pid>/fd/N`.** You cannot open/dup the child's stdout file descriptor by pid.
2. **The data path is master←slave.** Writes to the slave go to the terminal emulator's master buffer. Opening the slave path from a third process and reading yields **0 bytes** (observed), not a shared copy of the stream.
3. **Debugger-level access** (`ptrace` / Instruments / SIP-off `dtruss`) is the only realistic way to steal another process's fds, and is not a Boss-engine transport.

This is the **engine-only** topology. It does **not** speak to whether the GhosttyKit owner can observe surface text — that is Topology 2 below.

### Topology 2 — embedding process owns GhosttyKit (Layer D)

```sh
# ghosttykit_spike (Layer D) owns surface; polls:
#   ghostty_surface_read_text(surface, GHOSTTY_POINT_VIEWPORT, …)
# while pane shell runs codex exec --json …
```

Observed from the **same process** that called `ghostty_surface_new` (`evidence/host.log`):

```text
[2.2] OBSERVED thread.started in surface text
[26.2] OBSERVED gkit-embed-done in surface text
```

`evidence/viewport_final.txt` contains the full exec JSONL dialect, including:

```jsonl
{"type":"thread.started","thread_id":"019f9e22-a1d8-7660-8657-7c39f7a6e43e"}
{"type":"turn.started"}
{"type":"item.completed","item":{…,"text":"gkit-embed-done"}}
{"type":"turn.completed","usage":{…}}
```

JSONL is recovered as **rendered terminal text** (what the surface shows), not as a raw master-fd stream. For `codex exec --json` this is effectively byte-faithful line content; a TUI would be screen-scraped (cursor noise), which is a different recovery quality.

`GHOSTTY_POINT_SCREEN` returned the same content as viewport for this short session (1997 bytes each).

### Q1 dual-topology summary

| Topology                                     | Observer                     | Result                                                                                         |
| -------------------------------------------- | ---------------------------- | ---------------------------------------------------------------------------------------------- |
| App-owned pty, outsider has only `shell_pid` | Engine-like separate process | **Cannot read** (0 bytes; no `/proc` fd; slave open ≠ master stream)                           |
| Engine spawns and owns pipe/pty master       | Engine parent                | **Can read** full JSONL                                                                        |
| **GhosttyKit embedder owns surface**         | App process (Boss-like)      | **Can observe** via `ghostty_surface_read_text` — full JSONL / agent text recovered in Layer D |

### What this settles about the #2363 dispute (revised)

| Statement                                                                                                 | Verdict against observation                                                                                |
| --------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| "Engine-side stdout JSONL reader works when the **engine spawns** codex and owns the pipe/pty master"     | **Supported.** Direct pipe and local master harness both see full JSONL.                                   |
| "Engine-side stdout JSONL reader works when the **app owns the pty** and the engine only has `shell_pid`" | **Refuted.** Outsider Q1 observed zero readable bytes.                                                     |
| "The **app** (GhosttyKit owner) can observe pane-hosted worker output"                                    | **Supported (Layer D).** `ghostty_surface_read_text` recovers exec JSONL while codex runs.                 |
| "PR #2363 makes T-05 implementable against the _current_ app/engine pane split **from the engine alone**" | **Still refuted** for engine-only topology. App-forwarding or file-tail still required for engine ingress. |
| "Reading pane content is impossible because outsider cannot open the slave"                               | **Category error.** Outsider block ≠ embedder block.                                                       |

**Subtlety (do not flatten):** #2363's reader remains correct for engine-spawn. Pane-hosted progress for the **engine** still needs app-forwarded stream, rollout tail, or equivalent — but the **app** is not blind: GhosttyKit already exposes the surface text Boss uses for Claude monitoring.

---

## Q2 — Dual topology: master inject vs GhosttyKit SendToPane

### Claim under test

What happens when text is injected into a pane running `codex exec` — via (B) raw master write / harness-emulated post-exit shell, vs (D) Boss-equivalent GhosttyKit `submitText` into a real interactive shell.

### Apparatus honesty (read this first)

The positive "execution" result was **not** pure interactive-shell observation under real Ghostty zsh.

`pty_owner.py` (layer B) owns the pty master and runs a **scripted** slave zsh that, **after** `codex exec` exits, **explicitly** does:

```sh
# harness-emulated post-exit shell — not a real interactive Ghostty prompt
if read -r LINE; then
  print -r -- "$LINE" > …/shell_got_line.txt
  eval "$LINE"
  echo $? > …/eval_exit.txt
fi
```

So the empirical record is:

| Layer of claim                                 | What was actually observed                                                                                                                        |
| ---------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Buffer survives**                            | Master-side inject while `codex exec` is foreground is **echoed** into the master capture mid-JSONL and is **still available** after codex exits. |
| **Subsequent shell read can consume the line** | Harness `read -r LINE` successfully returns the injected line into `shell_got_line.txt`.                                                          |
| **Execution**                                  | Observed **only via that harness-emulated post-exit `read`/`eval`**, not via a real Ghostty interactive zsh reading the line as a typed command.  |

Do **not** restate this as "we watched an interactive shell execute injected text" without disclosing the constructed `read`/`eval`. The harness deliberately emulates "shell regains foreground and runs the next line" so we can measure buffer survival + consumption; it is a stand-in, not the full interactive prompt stack.

**Stdin closed vs open-tty pane:** this Q2 harness is an **open-tty** shape — codex inherits the slave as stdin/stdout/stderr (a pane-like fd topology). It is **not** `codex exec … </dev/null` (stdin closed). The accurate statement is: positional prompt already supplied; **stdin is the open tty but unused for turn input**; inject is not treated as a new agent prompt. Layer C pipe runs (`</dev/null`) are a different apparatus and were not the Q2 footgun path.

**Slave write on real Ghostty (layer A)** remains a separate, non-representative negative: outsider `O_WRONLY` to the slave path + `TIOCSTI` did not reproduce typed input. That finding still stands and does **not** refute master-side inject risk.

### What we ran

**Failed path (slave write from outsider, real Ghostty — not representative of SendToPane):** writing the payload to `/dev/ttys*` with `os.open(…, O_WRONLY)` while codex was foreground produced no side effect and no shell-consumed line. `TIOCSTI` failed with `PermissionError: [Errno 13] Permission denied`. Slave-side write is **not** a reliable stand-in for "typed into the pane."

**Master-write path (what a terminal app / `SendToPane` actually does):** local pty-owner harness (`pty_owner.py`) — master `os.write`, then harness post-exit `read`/`eval` as disclosed above:

```text
injecting via master: b'echo INJECTED_VIA_MASTER > /tmp/codex-pane-spike/injected_side_effect.txt\n'
… codex runs sleep 18, exits 0 …
shell_got_line: 'echo INJECTED_VIA_MASTER > /tmp/codex-pane-spike/injected_side_effect.txt\n'
injected_side_effect: 'INJECTED_VIA_MASTER\n'   # via harness eval, not interactive zsh
eval_exit: '0'
```

Master capture during the run (echo of the injected line appears mid-JSONL stream; codex does not treat it as a prompt):

```jsonl
{"type":"thread.started","thread_id":"019f9e06-5428-7a30-aaf1-9bc86ed996a1"}
{"type":"turn.started"}
echo INJECTED_VIA_MASTER > /tmp/codex-pane-spike/injected_side_effect.txt
{"type":"item.completed","item":{…,"text":"Running the requested command."}}
{"type":"item.started","item":{…,"command":"/bin/zsh -lc 'sleep 18'",…}}
…
{"type":"item.completed","item":{…,"text":"pty-owner-done"}}
{"type":"turn.completed",…}
```

### Outcome classification

| Outcome                                            | Observed?                      | Apparatus note                                                                                         |
| -------------------------------------------------- | ------------------------------ | ------------------------------------------------------------------------------------------------------ |
| Inert no-op                                        | No (master path)               | Line is echoed and remains readable after exit                                                         |
| Buffered-and-discarded                             | No (master path)               | Harness `read` still gets the full line                                                                |
| **Line available for post-exit shell consumption** | **Yes**                        | Master inject + open-tty buffer                                                                        |
| **Execution of injected text**                     | **Yes, harness-emulated only** | Explicit post-exit `read -r` + `eval "$LINE"` in `pty_owner.py` — **not** pure interactive Ghostty zsh |
| Real Ghostty interactive shell auto-exec           | **Not observed**               | Not claimed                                                                                            |

### Interpretation (Layer B apparatus)

**Footgun risk is real on Layer B; execution claim was apparatus-qualified.**

While `codex exec` is foreground on an open-tty pane shape:

- It does **not** consume the injected line as agent input (positional prompt already supplied; stdin is the open tty but unused for the turn).
- The line is echoed on the master and **survives in the input buffer** across codex exit.
- A **subsequent shell read can consume that line**. In Layer B, consumption + execution were produced by the harness's explicit `read`/`eval` after codex exited — a deliberate stand-in, not pure interactive zsh.

Layer D (next) removes the interactive-shell caveat.

### Topology D — GhosttyKit SendToPane-equivalent (Layer D; stronger apparatus)

**Apparatus honesty:** Layer D inject uses the **same path as Boss `SendToPane` / `GhosttyTerminalHostView.submitText`**: `ghostty_surface_text` then `ghostty_surface_key` Return. The pane shell is GhosttyKit's default interactive zsh (not a constructed `read`/`eval`). Mid-inject runs while `codex exec` is foreground; post-exit inject runs after the pane script returns to the interactive prompt.

What we ran (see `ghosttykit_host/Sources/main.swift`, `evidence/host.log`):

```text
[6.2] INJECT mid_codex body=echo GKIT_INJECT_VIA_SURFACE_TEXT > …/injected_side_effect.txt
[6.2] INJECT mid_codex Return sent
… codex continues sleep 18 …
[26.2] OBSERVED gkit-embed-done in surface text
[26.2] SIDE_EFFECT after mid inject (interactive shell): GKIT_INJECT_VIA_SURFACE_TEXT
[26.2] INJECT post_exit body=echo GKIT_POST_EXIT_INJECT > …/post_exit_side_effect.txt
[26.2] INJECT post_exit Return sent
… post_exit_side_effect: GKIT_POST_EXIT_INJECT
codex_exit: 0
```

Viewport mid-stream shows the inject **echoed** into the JSONL visual stream (codex does not treat it as a new agent prompt):

```jsonl
{"type":"turn.started"}
{"type":"item.completed","item":{…,"text":"Running the requested command."}}
echo GKIT_INJECT_VIA_SURFACE_TEXT > …/injected_side_effect.txt
{"type":"item.started","item":{…,"command":"/bin/zsh -lc 'sleep 18'",…}}
…
{"type":"item.completed","item":{…,"text":"gkit-embed-done"}}
{"type":"turn.completed",…}
SCRIPT_DONE
… % echo GKIT_INJECT_VIA_SURFACE_TEXT > …/injected_side_effect.txt   # interactive zsh ran buffered line
… % echo GKIT_POST_EXIT_INJECT > …/post_exit_side_effect.txt
```

### Outcome classification (dual topology)

| Outcome                             | Layer B (pty master + harness `eval`) | Layer D (GhosttyKit + interactive zsh)                           |
| ----------------------------------- | ------------------------------------- | ---------------------------------------------------------------- |
| Inert no-op                         | No                                    | No                                                               |
| Echoed mid-JSONL while codex runs   | Yes                                   | Yes                                                              |
| Codex treats inject as agent prompt | No                                    | No                                                               |
| Buffer survives codex exit          | Yes                                   | Yes                                                              |
| Subsequent shell consumes line      | Yes (harness `read -r`)               | **Yes (real interactive zsh)**                                   |
| Execution of injected command       | Yes, **harness-emulated `eval` only** | **Yes, interactive shell** (`GKIT_INJECT_VIA_SURFACE_TEXT` file) |
| Post-exit SendToPane on idle shell  | Not primary                           | **Yes** (`GKIT_POST_EXIT_INJECT`)                                |

### Interpretation (revised)

**Footgun is confirmed on the honest GhosttyKit / interactive-shell apparatus, not only the harness stand-in.**

While `codex exec` is foreground on an open-tty pane shape:

- It does **not** consume the injected line as agent input.
- GhosttyKit inject (`ghostty_surface_text` + Return) is echoed and **survives** in the tty input path across codex exit.
- When the pane returns to an **interactive shell**, that shell **executes** the buffered line (Layer D side-effect file). Layer B's harness `read`/`eval` was a valid stand-in; Layer D removes the "not pure interactive zsh" caveat for the execution claim.

**Design implication (strengthened):** Boss `SendToPane` while a `codex exec` worker is mid-turn is a **safety issue**, not hygiene. A guard ("is this worker accepting typed input") remains required for real interactive panes.

Nuance unchanged: the footgun is post-exit shell consumption of buffered input, not codex interpreting the inject as a new turn.

---

> **Q3–Q7:** CLI-level results; Layer D does not change them. Left as first-pass observations below.

## Q3 — Does `codex "$(cat prompt.txt)"` auto-submit its positional prompt?

### What we ran

Interactive TUI (not `exec`), positional prompt, **no keypress**:

```sh
script -q /tmp/codex-pane-spike/q3q4_script.txt \
  codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox \
  -C <repo> \
  "reply with exactly the single word: autoq3" \
  </dev/null
```

(Also reproduced under a local pty master and under a real Ghostty window for Q4.)

### Observed

- TUI started and **immediately began the turn** without Enter / any keypress.
- Agent final answer `autoq3` appeared in the capture.
- Session **did not exit** after the single turn — TUI remained interactive at the composer (Ghostty script had not reached `EXIT:$?` while the rollout already showed `task_complete` / `agent_message: ghostty-q4-ok`).

Rollout `session_meta` for the Ghostty TUI run:

```json
{
  "originator": "codex-tui",
  "cli_version": "0.145.0",
  "source": "cli",
  "session_id": "019f9e0b-5f0d-7382-aea9-c9fa7aeb400a"
}
```

### Interpretation

**Yes — positional prompt auto-submits**, matching the `claude "$(cat prompt.txt)"` shape for _starting_ the turn.

**Subtlety:** unlike `codex exec` (one turn then process exit), the interactive TUI **stays alive** after the positional turn completes. Auto-submit ≠ auto-exit. Drivers that need "process ends when turn ends" want `codex exec`, not bare `codex`.

---

## Q4 — Does `--no-alt-screen` behave sanely in a Ghostty pane?

### What we ran

```sh
open -na Ghostty.app --args -e /tmp/codex-pane-spike/q4_ghostty.sh
# inside: codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox -C <repo> \
#          "reply with exactly: ghostty-q4-ok"
```

Plus `script(1)` capture of the same flags for sequence inspection.

### Observed

- `TERM=xterm-ghostty` inside the pane.
- Capture binary contained **no** alt-screen enter/leave sequences:
  - `\x1b[?1049h` / `\x1b[?1049l`: **absent**
  - `\x1b[?47h` / `\x1b[?47l`: **absent**
- Turn completed successfully (`agent_message: ghostty-q4-ok`, `task_complete`).
- No obvious corruption of the capture stream beyond normal TUI cursor-addressing noise (characters are individually cursor-positioned; stripping CSI yields a readable transcript).
- Help text for the flag matches intent: _"Runs the TUI in inline mode, preserving terminal scrollback history."_

### Interpretation

**Yes — sane enough for a pane.** No alt-screen buffer swap, scrollback preserved by design of the flag, readable session, successful turn on Ghostty 1.3.1. Not a blocker for a TUI-in-pane design (if one were chosen).

---

## Q5 — Does Esc abort the current turn without killing the process?

### What we ran

Local pty master hosting interactive TUI with a long `sleep 90` turn; send `Esc` at t≈7s; at t≈12s type a follow-up and submit with CR.

Harness: `ghostty-codex-pane-viability-artifacts/q5b_harness.py`.

### Observed (raw harness log)

```
pid=74245
t=7.0 ESC
t=12.1 alive=True
t=12.1 submitted follow-up with CR
final alive=True bytes=42014
```

Rollout
`~/.codex/sessions/2026/07/26/rollout-2026-07-26T03-52-12-019f9e0d-cc26-7ef0-a121-bc6eeff24bad.jsonl`:

```json
{"type":"turn_aborted","turn_id":"019f9e0d-cdad-7753-8fa6-4029b64492cf","reason":"interrupted",…,"duration_ms":6344}
```

Then a **second** turn on the same session/process:

```json
{"type":"task_started","turn_id":"019f9e0d-fc91-73a3-b535-c17cd0290c36",…}
{"type":"user_message","message":"reply with exactly: second-turn-ok",…}
{"type":"agent_message","message":"second-turn-ok","phase":"final_answer",…}
{"type":"task_complete","turn_id":"019f9e0d-fc91-73a3-b535-c17cd0290c36","last_agent_message":"second-turn-ok",…}
```

UI chrome (compacted capture) also showed: `Conversation interrupted - tell the model what to do differently` and `esc to interrupt`.

### Interpretation

**Yes.**

- Esc produces a real **`turn_aborted`** event (`reason: "interrupted"`) in the rollout.
- The **process survives**.
- The **session accepts another turn** (new `task_started` / `user_message` / `task_complete`).

Note: the abort event lives in the **rollout / event_msg schema** (`payload.type = "turn_aborted"`), not in the `codex exec --json` stdout schema (which we did not see emit `turn_aborted` because exec was not the vehicle for Esc).

---

## Q6 — Does `codex exec resume` deliver a follow-up prompt?

### What we ran

```sh
codex --version   # codex-cli 0.145.0

# initial
codex exec --json --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \
  "reply with exactly: session-alpha-start" </dev/null
# → thread_id=019f9e07-1ad7-7080-9671-f2d57ae792a3

# resume
codex exec resume --json --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \
  019f9e07-1ad7-7080-9671-f2d57ae792a3 \
  "reply with exactly: session-alpha-resume" </dev/null
```

### Observed

Initial (`q6_out1.jsonl`):

```jsonl
{"type":"thread.started","thread_id":"019f9e07-1ad7-7080-9671-f2d57ae792a3"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"session-alpha-start"}}
{"type":"turn.completed","usage":{…}}
```

Resume (`q6_out2.jsonl`):

```jsonl
{"type":"thread.started","thread_id":"019f9e07-1ad7-7080-9671-f2d57ae792a3"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"session-alpha-resume"}}
{"type":"turn.completed","usage":{…}}
```

Stderr on resume: empty. Exit code: 0.

Same `thread_id` on resume. A fresh **`turn.started`** appears — usable as delivery confirmation.

### Interpretation

**Yes — T-17's `exec resume` probing mechanism is viable on 0.145.0.**

- Follow-up prompt is delivered and answered.
- `turn.started` (and subsequent `item.*` / `turn.completed`) confirm delivery on stdout JSONL.
- Resume is a **new process** that reattaches to the thread; it is not "inject into a live pane." That distinction matters for lifecycle design but does not block the probe/nudge shape.

---

## Q7 — Does a rollout / JSONL event file exist on disk, and can it be tailed?

### Path and discovery

Pattern observed:

```text
~/.codex/sessions/{YYYY}/{MM}/{DD}/rollout-{local-timestamp}-{thread_id}.jsonl
```

Examples:

```text
~/.codex/sessions/2026/07/26/rollout-2026-07-26T03-48-52-019f9e0a-bf69-7cc0-8bbb-6848d03fb1c1.jsonl
```

| Property                                  | Observation                                                                                           |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| Exists for `codex exec`                   | Yes (`originator: "codex_exec"`, `source: "exec"`)                                                    |
| Exists for TUI                            | Yes (`originator: "codex-tui"`, `source: "cli"`)                                                      |
| Path fully predictable a priori           | **No.** Directory is date-partitioned; filename embeds a local start timestamp **and** the thread id. |
| Discoverable once `thread_id` known       | **Yes** — glob `**/rollout-*-{thread_id}.jsonl` under `~/.codex/sessions`.                            |
| Discoverable by watching the sessions dir | **Yes** — new file appeared at t≈0.00s after process start in the live-tail experiment.               |
| `--ephemeral`                             | Not exercised; help text says it skips persisting session files.                                      |

### Live tail

Started `codex exec --json … "run: sleep 10; …"`, watched the sessions day dir:

```
started 34080
discovered rollout at t=0.00s: …/rollout-2026-07-26T03-48-52-019f9e0a-bf69-7cc0-8bbb-6848d03fb1c1.jsonl
exit 0 final_bytes 52749 lines 18
size growth samples: [(t0, 18614), (t0.5, 18614), (t1.0, 48090), … (t15, 52749)]
event_types sequence: session_meta, event_msg/task_started, response_item…, event:task_complete
```

File grew while the session ran — **tailable**.

### Schema richness: rollout vs `exec --json` stdout

These are **different event dialects**.

**Stdout (`--json`)** — compact driver-oriented stream:

```jsonl
{"type":"thread.started","thread_id":"…"}
{"type":"turn.started"}
{"type":"item.started","item":{"id":"item_0","type":"command_execution","command":"/bin/zsh -lc 'sleep 10'","aggregated_output":"","exit_code":null,"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_0","type":"command_execution","command":"/bin/zsh -lc 'sleep 10'","aggregated_output":"","exit_code":0,"status":"completed"}}
{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"q7-tail-done"}}
{"type":"turn.completed","usage":{…}}
```

Command executions carry **`aggregated_output`** on the item (empty string for `sleep`, but the field is present and populated for commands that print — see prior investigation `codex-progress-channel-decision-2026-07-24.md` for `echo` with `aggregated_output":"hooktest-exec\n"`).

**Rollout file** — richer internal transcript, different shapes:

- `session_meta`, `world_state`, `turn_context`
- `event_msg`: `task_started`, `user_message`, `agent_message`, `token_count`, `task_complete`, **`turn_aborted`**
- `response_item`: developer/user/assistant messages, `reasoning`, `custom_tool_call` / `custom_tool_call_output`

Command execution in the rollout is **`custom_tool_call` named `exec`**, with output in `custom_tool_call_output` (e.g. `"Script completed\nWall time 10.1 seconds\nOutput:\n"` or `"aborted by user after 0.1s"`) — **not** the stdout `command_execution` / `aggregated_output` item shape.

TUI and exec rollouts share this internal schema. TUI does **not** lack rollout events relative to exec; both write the same family of records. The _stdout JSONL_ channel simply does not exist for a pure TUI session (no `--json`).

### Interpretation

- **File exists and can be tailed** while the session runs.
- Path requires discovery (dir watch and/or thread_id glob), not a hard-coded template alone.
- **TUI rollout is event-rich**, including abort; it does **not** carry the same `command_execution.aggregated_output` shape as `--json` stdout — it uses `custom_tool_call_output` instead.
- For a non-interactive v1, **either** stdout JSONL (if engine owns spawn) **or** rollout tail works as an observation channel; they are not drop-in identical parsers.

---

## Design-claims matrix

| Design / review claim                                                             | Observation                                                                          | Supports / Refutes                                            |
| --------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------ | ------------------------------------------------------------- |
| Engine can read worker stdout JSONL when it owns the pipe/spawn                   | Pipe parent reads full JSONL                                                         | **Supports**                                                  |
| Engine can read worker stdout JSONL given only `shell_pid` under app-owned pty    | 0 bytes; no `/proc` fd access; slave open does not see master stream                 | **Refutes**                                                   |
| **App / GhosttyKit owner can observe pane-hosted worker output**                  | Layer D: `ghostty_surface_read_text` recovers full exec JSONL + agent text           | **Supports**                                                  |
| PR #2363 alone makes pane-shaped T-05 implementable **from the engine**           | Reader helps only if topology feeds the engine a stream                              | **Refutes** (engine-only); app can still scrape/forward       |
| Progress ingress could be `AgentJsonlFile { discovery }` via rollout              | File exists, grows live, discoverable by thread_id / dir watch                       | **Supports**                                                  |
| `StdoutJsonl` as progress ingress for **pane-hosted** workers **into the engine** | Blocked by outsider Q1 unless app forwards or engine uses file channel               | **Refutes for engine←pane without forward**                   |
| `StdoutJsonl` for **engine-spawned** `codex exec --json`                          | Works end-to-end                                                                     | **Supports** (keep)                                           |
| Non-interactive v1 (`codex exec --json`) is observable at all                     | Fully observable via stdout (engine-spawn), rollout, **or app surface scrape**       | **Supports**                                                  |
| Injected pane text during `codex exec` is inert                                   | Master + GhosttyKit inject echoed + survives; **interactive zsh executes** (Layer D) | **Refutes inert**                                             |
| `SendToPane` guard is mere hygiene                                                | Layer D interactive-shell execution after mid-inject                                 | **Refutes** — **safety fix**                                  |
| Positional prompt auto-submits like Claude CLI                                    | Yes for TUI; exec also runs prompt without Enter                                     | **Supports** (Q3; unchanged)                                  |
| `--no-alt-screen` usable in Ghostty                                               | No alt-screen CSI; turn succeeds; TERM=xterm-ghostty                                 | **Supports** (Q4; unchanged)                                  |
| Esc aborts turn, process lives, further turns possible                            | `turn_aborted` + second `task_complete`                                              | **Supports** (Q5; unchanged)                                  |
| T-17 `exec resume` probe/nudge viable (OQ-5 never spiked)                         | Resume delivers prompt; new `turn.started`                                           | **Supports** (Q6; unchanged)                                  |
| TUI rollout has same `aggregated_output` richness as `--json`                     | Different schema (`custom_tool_call_output` vs item.aggregated_output)               | **Refutes equality**; both rich enough with different parsers |

### Immediate decisions this unblocks

1. **Codex v1 shape:** Non-interactive `codex exec --json` is empirically fine for observation **when** the observer owns stdout, tails the rollout, **or is the GhosttyKit embedder** scraping surface text. Pane-owned stdout alone is not enough for an **engine outsider** (Q1 topology 1).
2. **Progress ingress:**
   - Engine reading **pane-hosted** workers → still needs **`AgentJsonlFile` / rollout tail** or an **app-forwarded** channel (surface scrape lives in the app, not the engine).
   - Engine-spawned workers → **`StdoutJsonl` is valid** (keep).
   - App-local features (Claude-monitor-style) → **`ghostty_surface_read_text` is valid now** (Layer D).
3. **`SendToPane` guard:** **Safety fix** (Q2 Layer D), not hygiene — interactive shell executed buffered inject after `codex exec`.
4. **T-17 `exec resume`:** **Viable** (Q6). Unchanged by embed revision.

CodexDriver skeleton + spawn/provisioning taxonomy rows that assumed "engine reads pane stdout via shell_pid" remain **premise-affected**. Rows that assumed "nobody can read pane content" should distinguish engine vs app.

---

## Surprises (not on the original seven)

1. **`codex exec` under a shell pipeline changes fd topology.** `codex … | tee file` makes codex stdout a pipe even inside Ghostty; pure TTY inherit is the honest pane shape. Measure both, don't confuse them.
2. **Slave-path write ≠ typed input on modern macOS.** `TIOCSTI` is permission-denied; writing the slave is unreliable. Only master-side / GhosttyKit inject reproduces `SendToPane`.
3. **Stdout JSONL and rollout JSONL are different dialects.** A driver cannot treat them as the same parser with a different source. Abort lives in rollout `event_msg.turn_aborted`; exec stdout uses `turn.started` / `turn.completed` / `item.*`.
4. **TUI positional prompt auto-runs but does not auto-exit.** Process lifecycle differs sharply from `codex exec`.
5. **Resume reuses `thread_id` and emits `thread.started` again** on the new process — handy for correlation, easy to misread as a brand-new thread if you only key on the event type.
6. **Ghostty on macOS CLI:** `ghostty +new-window` is unsupported; `open -na Ghostty.app --args -e …` is the working launch path. `login -flp` wraps `-e` commands.
7. **Rollout appears essentially immediately** (t≈0 in dir watch) — discovery latency is not a practical obstacle.
8. **GhosttyKit surface scrape recovers full `codex exec --json` lines** (Layer D) — the embedder is not subject to the outsider `shell_pid` block. This was the category-error correction for the outsider-vs-embedder conflation.
9. **Interactive zsh really does execute buffered GhosttyKit inject** after `codex exec` (Layer D side-effect files) — stronger than the Layer B harness `eval` stand-in.

---

## Residual unknowns

- **App-forwarded stream to engine:** Layer D proves the app can scrape; it does **not** implement engine IPC for that scrape. Wiring surface text (or a master tee) into the engine remains design work.
- **Whether surface scrape is a good long-term progress channel:** Recovered text is rendered scrollback, not a dedicated master tee. Fine for short `codex exec --json` lines; weaker for noisy TUI. Rollout tail may still be preferable for engine ingress.
- **`codex exec` + Esc:** Esc abort was tested on **TUI**, not on `exec` (exec is non-interactive; there is no Esc surface). Abort-via-signal (SIGINT/SIGTERM) to `exec` mid-turn was not spiked.
- **`--ephemeral`:** Claimed to skip session files; not re-verified.
- **Windows / Linux pane topology:** macOS-specific outsider blocks (`/proc` absence, `TIOCSTI` denial) may differ; embedder APIs are GhosttyKit/libghostty-specific.
- **Rollout rotation / multi-day sessions / resume path stability across days:** not stressed.
- **Deep scrollback beyond viewport/screen:** Layer D used `GHOSTTY_POINT_VIEWPORT` and `GHOSTTY_POINT_SCREEN` (same content in the short run). Very long sessions may need scrolling/selection variants; not stressed.
- **Cost / rate limits:** one TUI run printed a usage-limit tip; not characterized further.

---

## Appendix — pinned version stamps

```text
$ codex --version
codex-cli 0.145.0

$ /Applications/Ghostty.app/Contents/MacOS/ghostty +version
Ghostty 1.3.1
  channel: stable

$ # GhosttyKit (Layer D) — MODULE.bazel @ghostty_kit
ghosttykit-5659cef
sha256: 82b8d947484a21e1a9d186628b8af5e3f2e81dc96925f3cdbc1766ececa814a1
url: https://github.com/spinyfin/ghostty-prebuilts/releases/download/ghosttykit-5659cef/GhosttyKit-5659cef.tar.gz
```

Re-check these before treating any follow-up observation as comparable.

## Appendix — artifact index

| File                                                                      | Contents                                                                     |
| ------------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| `ghostty-codex-pane-viability-artifacts/pty_owner.py`                     | Q1/Q2 master-owned pty harness (post-exit `read`/`eval` is harness-emulated) |
| `ghostty-codex-pane-viability-artifacts/owner_run.log`                    | Q2 harness summary log                                                       |
| `ghostty-codex-pane-viability-artifacts/master_capture.txt`               | Q2 master-side capture (inject echo mid-JSONL)                               |
| `ghostty-codex-pane-viability-artifacts/shell_got_line.txt`               | Q2 line consumed by harness `read -r` after codex exit                       |
| `ghostty-codex-pane-viability-artifacts/injected_side_effect.txt`         | Q2 side effect from harness `eval` (not interactive zsh)                     |
| `ghostty-codex-pane-viability-artifacts/eval_exit.txt`                    | Q2 harness `eval` exit code                                                  |
| `ghostty-codex-pane-viability-artifacts/run_pure.sh`                      | Q1 Ghostty pure-TTY spawn script                                             |
| `ghostty-codex-pane-viability-artifacts/q3q4_clean.txt`                   | Q3/Q4 cleaned TUI capture (no `?1049` alt-screen CSI)                        |
| `ghostty-codex-pane-viability-artifacts/q4_env.txt`                       | Q4 `TERM=xterm-ghostty` under Ghostty                                        |
| `ghostty-codex-pane-viability-artifacts/q5_harness.py` / `q5b_harness.py` | Esc abort harnesses                                                          |
| `ghostty-codex-pane-viability-artifacts/q5b.log`                          | Esc + second-turn success log                                                |
| `ghostty-codex-pane-viability-artifacts/q6_out1.jsonl` / `q6_out2.jsonl`  | exec + exec resume                                                           |
| `ghostty-codex-pane-viability-artifacts/q7_stdout.jsonl`                  | exec --json stdout for tail session                                          |
| `ghostty-codex-pane-viability-artifacts/q7_rollout_sample.jsonl`          | Q7 rollout file sample (same thread as `q7_stdout.jsonl`)                    |
| `ghostty-codex-pane-viability-artifacts/ghosttykit_host/`                 | **Layer D** throwaway GhosttyKit embed host (source + README + run.sh)       |
| `ghosttykit_host/Sources/main.swift`                                      | Embed host: surface create, `read_text` poll, `submitText` inject            |
| `ghosttykit_host/evidence/SUMMARY.txt`                                    | Layer D headline results                                                     |
| `ghosttykit_host/evidence/host.log`                                       | Layer D timed host log (observe + inject)                                    |
| `ghosttykit_host/evidence/viewport_final.txt`                             | Layer D full surface scrape (JSONL + injects)                                |
| `ghosttykit_host/evidence/injected_side_effect.txt`                       | Mid-inject executed by **interactive zsh**                                   |
| `ghosttykit_host/evidence/post_exit_side_effect.txt`                      | Post-exit SendToPane-equivalent success                                      |
