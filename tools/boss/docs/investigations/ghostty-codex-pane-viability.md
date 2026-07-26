# Ghostty + Codex pane viability spike

- **Date:** 2026-07-26
- **Work item:** task_18c5d06ab766c0a8_167 (ghostty + codex pane viability)
- **Kind:** empirical throwaway harness — no Boss integration, no CodexDriver code
- **Pinned versions (re-checked before each observation cluster):**
  - `codex-cli 0.145.0` (`/Users/brianduff/.local/bin/codex` → `~/.codex/packages/standalone/current/bin/codex`)
  - Ghostty `1.3.1` (stable) (`/Applications/Ghostty.app`)
- **Host:** macOS (Darwin), no `/proc/<pid>/fd`
- **Related:** [codex-progress-channel-decision-2026-07-24.md](./codex-progress-channel-decision-2026-07-24.md); Codex driver design (PR discussion around §A-1 / OQ-5 / T-05 / T-17); PR #2363 (engine-side stdout JSONL reader)

## Why this spike exists

Two careful positions currently contradict each other on an empirical question:

1. **Design claim (§A-1):** PR #2363 landed an engine-side stdout JSONL reader, so the transport split and reader are no longer future work.
2. **Review claim:** the app owns the pty and the engine only ever receives `shell_pid`, so an engine-side reader of the worker process's stdout **cannot work**, and T-05 as written is not implementable against the current app/engine split.

Both cannot be true as statements about the _pane-hosted Boss shape_. This spike settles that (Q1) and six neighboring execution-shape questions by observation.

## Method / apparatus

Throwaway only. Three layers of harness:

| Layer                        | What it is                                                          | Used for                                                                        |
| ---------------------------- | ------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| **A. Real Ghostty window**   | `open -na Ghostty.app --args -e <script>`                           | Q1 (pane-owned pty + `shell_pid` only), Q4                                      |
| **B. Local pty-owner**       | Python `pty.openpty()` holding the master, child shell on the slave | Q1 (master-side capture contrast), Q2 (correct input-injection path), Q3/Q5 TUI |
| **C. Direct process / pipe** | `codex exec --json … </dev/null` with the observer owning stdout    | Q6, Q7, contrast for Q1                                                         |

Harness scripts and selected raw captures are under [`ghostty-codex-pane-viability-artifacts/`](./ghostty-codex-pane-viability-artifacts/).

Prompts were deliberately short (`reply with exactly: …`, `sleep N`) to limit cost/time.

---

## Q1 — Can a separate process read the agent's stdout when the terminal app owns the pty?

### Claim under test

Reproduce the Boss shape: pane-hosted process; outside observer holds only `shell_pid`. Is stdout readable from outside?

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

### Interpretation

**No. An outside process that holds only `shell_pid` cannot read the pane-hosted agent's stdout on this host.**

What blocks it, exactly:

1. **macOS has no `/proc/<pid>/fd/N`.** You cannot open/dup the child's stdout file descriptor by pid.
2. **The data path is master←slave.** Writes to the slave go to the terminal emulator's master buffer. Opening the slave path from a third process and reading yields **0 bytes** (observed), not a shared copy of the stream.
3. **Debugger-level access** (`ptrace` / Instruments / SIP-off `dtruss`) is the only realistic way to steal another process's fds, and is not a Boss-engine transport.

### What this settles about the #2363 dispute

| Statement                                                                                                 | Verdict against observation                                                                                        |
| --------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| "Engine-side stdout JSONL reader works when the **engine spawns** codex and owns the pipe/pty master"     | **Supported.** Direct pipe and local master harness both see full JSONL.                                           |
| "Engine-side stdout JSONL reader works when the **app owns the pty** and the engine only has `shell_pid`" | **Refuted.** Q1 observed zero readable bytes; no macOS mechanism without the master or the pipe end.               |
| "PR #2363 makes T-05 implementable against the _current_ app/engine pane split"                           | **Refuted as stated**, unless the app also forwards the stream (or the design stops relying on pane-owned stdout). |

**Subtlety (do not flatten to yes/no):** #2363's reader code can be correct and useful for an _engine-spawned_ worker. It is not a solution to the _app-owned-pty + shell_pid-only_ shape. Those are different topologies. The design and the review were talking past each other.

---

## Q2 — What happens when text is injected into the pty of a pane running `codex exec`?

### Claim under test

Because `codex exec` runs one turn and exits with stdin closed / not consuming typed input, injected prose lands in the pty buffer and is then **executed by the shell** when it regains the foreground.

### What we ran

**Failed path (slave write from outsider, real Ghostty):** writing the payload to `/dev/ttys*` with `os.open(…, O_WRONLY)` while codex was foreground produced no side effect and no shell-consumed line. `TIOCSTI` failed with `PermissionError: [Errno 13] Permission denied`. Slave-side write is **not** a reliable stand-in for "typed into the pane."

**Correct path (master write — what a terminal app / `SendToPane` actually does):** local pty-owner harness (`pty_owner.py`):

```text
injecting via master: b'echo INJECTED_VIA_MASTER > /tmp/codex-pane-spike/injected_side_effect.txt\n'
… codex runs sleep 18, exits 0 …
shell_got_line: 'echo INJECTED_VIA_MASTER > /tmp/codex-pane-spike/injected_side_effect.txt\n'
injected_side_effect: 'INJECTED_VIA_MASTER\n'
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

| Outcome                | Observed?        |
| ---------------------- | ---------------- |
| Inert no-op            | No (master path) |
| Buffered-and-discarded | No (master path) |
| **Shell-executes-it**  | **Yes**          |

### Interpretation

**This is a live footgun.** While `codex exec` is foreground:

- It does **not** consume the injected line as agent input (positional prompt already supplied; stdin is the tty but unused for the turn).
- The line sits in the tty input buffer / is echoed.
- When codex exits and the shell reads the next line, **the shell executes the injected text.**

Boss `SendToPane` (or any master-side write) while a `codex exec` worker is mid-turn is therefore **not** "hygiene" — it is a safety boundary. A guard ("is this worker accepting typed input") is a **safety fix**, not optional polish.

Nuance: the footgun is about _shell post-exit_, not about codex interpreting the inject as a new turn. `codex exec` itself did not run the injected command.

---

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

| Design / review claim                                                          | Observation                                                            | Supports / Refutes                                                             |
| ------------------------------------------------------------------------------ | ---------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| Engine can read worker stdout JSONL when it owns the pipe/spawn                | Pipe parent reads full JSONL                                           | **Supports**                                                                   |
| Engine can read worker stdout JSONL given only `shell_pid` under app-owned pty | 0 bytes; no `/proc` fd access; slave open does not see master stream   | **Refutes**                                                                    |
| PR #2363 alone makes pane-shaped T-05 implementable                            | Reader helps only if topology feeds it a stream                        | **Refutes** (as a pane-topology claim)                                         |
| Progress ingress could be `AgentJsonlFile { discovery }` via rollout           | File exists, grows live, discoverable by thread_id / dir watch         | **Supports**                                                                   |
| `StdoutJsonl` as progress ingress for **pane-hosted** workers                  | Blocked by Q1 unless app forwards stdout                               | **Refutes for pane shape**                                                     |
| `StdoutJsonl` for **engine-spawned** `codex exec --json`                       | Works end-to-end                                                       | **Supports**                                                                   |
| Non-interactive v1 (`codex exec --json`) is observable at all                  | Fully observable via stdout _and_ rollout                              | **Supports** (operator's "non-interactive v1 is acceptable")                   |
| Injected pane text during `codex exec` is inert                                | Shell executes post-exit (master inject)                               | **Refutes** — footgun                                                          |
| `SendToPane` guard is mere hygiene                                             | Master inject → shell exec                                             | **Refutes** — **safety fix**                                                   |
| Positional prompt auto-submits like Claude CLI                                 | Yes for TUI; exec also runs prompt without Enter                       | **Supports**                                                                   |
| `--no-alt-screen` usable in Ghostty                                            | No alt-screen CSI; turn succeeds; TERM=xterm-ghostty                   | **Supports**                                                                   |
| Esc aborts turn, process lives, further turns possible                         | `turn_aborted` + second `task_complete`                                | **Supports**                                                                   |
| T-17 `exec resume` probe/nudge viable (OQ-5 never spiked)                      | Resume delivers prompt; new `turn.started`                             | **Supports**                                                                   |
| TUI rollout has same `aggregated_output` richness as `--json`                  | Different schema (`custom_tool_call_output` vs item.aggregated_output) | **Refutes equality**; both are rich enough for progress with different parsers |

### Immediate decisions this unblocks

1. **Codex v1 shape:** Non-interactive `codex exec --json` is empirically fine for observation **if and only if** the observer owns stdout **or** tails the rollout. Pane-owned stdout alone is not enough (Q1).
2. **Progress ingress:**
   - Pane-hosted workers → **`AgentJsonlFile` / rollout tail** (or app-forwarded stream — not observed here).
   - Engine-spawned workers → **`StdoutJsonl` is valid** (and nicer: `command_execution.aggregated_output` already normalized).  
     Reusing `engine/transcript-tail` for discovery+tail matches Q7.
3. **`SendToPane` guard:** **Safety fix** (Q2), not hygiene.
4. **T-17 `exec resume`:** **Viable** (Q6). OQ-5 can be closed on the "does it deliver / does `turn.started` fire" axis.

CodexDriver skeleton + spawn/provisioning taxonomy rows that assumed "engine reads pane stdout" are **premise-affected** and should be rewritten around the topology that actually works.

---

## Surprises (not on the original seven)

1. **`codex exec` under a shell pipeline changes fd topology.** `codex … | tee file` makes codex stdout a pipe even inside Ghostty; pure TTY inherit is the honest pane shape. Measure both, don't confuse them.
2. **Slave-path write ≠ typed input on modern macOS.** `TIOCSTI` is permission-denied; writing the slave is unreliable. Only master-side inject (app / pty owner) reproduces `SendToPane`.
3. **Stdout JSONL and rollout JSONL are different dialects.** A driver cannot treat them as the same parser with a different source. Abort lives in rollout `event_msg.turn_aborted`; exec stdout uses `turn.started` / `turn.completed` / `item.*`.
4. **TUI positional prompt auto-runs but does not auto-exit.** Process lifecycle differs sharply from `codex exec`.
5. **Resume reuses `thread_id` and emits `thread.started` again** on the new process — handy for correlation, easy to misread as a brand-new thread if you only key on the event type.
6. **Ghostty on macOS CLI:** `ghostty +new-window` is unsupported; `open -na Ghostty.app --args -e …` is the working launch path. `login -flp` wraps `-e` commands.
7. **Rollout appears essentially immediately** (t≈0 in dir watch) — discovery latency is not a practical obstacle.

---

## Residual unknowns

- **App-forwarded stream:** If the Boss macOS app ever grew "mirror pane bytes to engine," Q1's block would lift without changing codex. Not tested (no Boss integration in this spike).
- **`codex exec` + Esc:** Esc abort was tested on **TUI**, not on `exec` (exec is non-interactive; there is no Esc surface). Abort-via-signal (SIGINT/SIGTERM) to `exec` mid-turn was not spiked.
- **`--ephemeral`:** Claimed to skip session files; not re-verified.
- **Windows / Linux pane topology:** macOS-specific blocks (`/proc` absence, `TIOCSTI` denial) may differ; Linux `/proc/pid/fd` still does not let you _read_ another process's open write-end usefully without `splice`/ptrace games — likely same conclusion, different error path.
- **Rollout rotation / multi-day sessions / resume path stability across days:** not stressed.
- **Whether Ghostty itself can be asked (API) for scrollback text** as an alternative observer channel: not tested.
- **Cost / rate limits:** one TUI run printed a usage-limit tip ("2 usage limit resets available"); not characterized further.

---

## Appendix — pinned version stamps

```text
$ codex --version
codex-cli 0.145.0

$ /Applications/Ghostty.app/Contents/MacOS/ghostty +version
Ghostty 1.3.1
  channel: stable
```

Re-check these before treating any follow-up observation as comparable.

## Appendix — artifact index

| File                                                                      | Contents                            |
| ------------------------------------------------------------------------- | ----------------------------------- |
| `ghostty-codex-pane-viability-artifacts/pty_owner.py`                     | Q1/Q2 master-owned pty harness      |
| `ghostty-codex-pane-viability-artifacts/owner_run.log`                    | Q2 footgun result                   |
| `ghostty-codex-pane-viability-artifacts/q5_harness.py` / `q5b_harness.py` | Esc abort harnesses                 |
| `ghostty-codex-pane-viability-artifacts/q5b.log`                          | Esc + second-turn success log       |
| `ghostty-codex-pane-viability-artifacts/q6_out1.jsonl` / `q6_out2.jsonl`  | exec + exec resume                  |
| `ghostty-codex-pane-viability-artifacts/q7_stdout.jsonl`                  | exec --json stdout for tail session |
