# Codex progress channel: hooks or stdout JSONL

- **Date:** 2026-07-24
- **Execution:** `exec_18c56a3b5c2447f0_a8` (chore_implementation)
- **Work item:** Decide the Codex progress channel: hooks or stdout JSONL (unblocks T3528, T3529)
- **Parent projects:** P1422 progress-ingress rows ([agent-driver-abstraction design doc](../designs/agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md)); P3330 Codex driver design (`codex-as-a-first-class-agent-driver.md`, still open/unmerged as PR [#2285](https://github.com/spinyfin/mono/pull/2285))
- **Method:** live, read-only probes against `codex-cli 0.145.0` on this host — a scratch `CODEX_HOME` + scratch git repo, hooks configured via `config.toml`, driven through `codex exec --json` (the non-interactive mode a driver would actually invoke). No code changed, no taxonomy rows touched.

This chore does not implement anything. It answers the single empirical question three taxonomy rows (T3325, T3328, T3511, T3512) disagree about — does Codex's progress arrive over hooks or a stdout stream — and states, per affected row, what should change. The taxonomy edits are for the Boss to apply.

## TL;DR

**Codex does both, verified live on this host, in the same invocation.** `codex exec --json` fires Claude-wire-compatible hooks (`SessionStart`, `PreToolUse`, `PostToolUse`, `Stop`, …) _and_ emits a native, typed JSONL event stream on stdout (`thread.started` / `turn.started` / `item.*` / `turn.completed`). Neither claim in the contradiction (T3325/T3511 "hooks including Stop" vs. T3328/T3512 "stdout JSONL") is false. They were answering different questions.

**Decision: stdout JSONL is the Codex driver's progress channel. Hooks are not.** Not because hooks don't fire — they do, reliably — but because Codex's hook trust model fails open and silently: an untrusted or misconfigured hook is skipped with zero observable signal (no error, no missing-event log line). That is disqualifying for a liveness/progress signal specifically — a progress-via-hooks design could produce a worker that silently reports nothing and never completes, indistinguishable from a genuine hang. Stdout JSONL has no such failure mode: it requires no trust record, no settings file, cannot be silently skipped, and is already the required transport for the design's other three seams (turn boundary, PR-URL capture, transcript path discovery).

Hooks are _not_ rejected outright — they remain the mechanism for a different capability, `ToolUseInterception` (command guardrails via `PreToolUse` deny), where no equally-robust alternative exists yet. Progress and interception are independent capabilities riding different transports; this is not a "pick one channel for everything" decision.

This finding, and the reasoning above, matches the (still open, unmerged) P3330 Codex driver design doc almost exactly — I ran the same experiment independently before reading it in detail, then cross-checked against it. Where this doc adds something P3330 does not cover at all: **T3528 and T3529, which P3330 never mentions.**

## Ground truth (codex-cli 0.145.0, verified live 2026-07-24)

```
$ codex --version
codex-cli 0.145.0
```

`codex features` confirms `hooks` is `stable`/enabled on this build (not experimental, not under-development).

### Reproduction

Scratch `CODEX_HOME/config.toml`:

```toml
model = "gpt-5.5"
model_reasoning_effort = "low"

[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = ".../hooklog.sh"

[[hooks.PreToolUse]]
[[hooks.PreToolUse.hooks]]
type = "command"
command = ".../hooklog.sh"

[[hooks.PostToolUse]]
[[hooks.PostToolUse.hooks]]
type = "command"
command = ".../hooklog.sh"

[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = ".../hooklog.sh"
```

Run: `CODEX_HOME=<scratch> codex exec --json --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust "run: echo hooktest-exec" < /dev/null` inside a scratch git repo.

**stdout (JSONL, `--json` flag):**

```jsonl
{"type":"thread.started","thread_id":"019f974c-3d59-7533-b320-3963123c809b"}
{"type":"turn.started"}
{"type":"item.started","item":{"id":"item_2","type":"command_execution","command":"/bin/zsh -lc 'echo hooktest-exec'","aggregated_output":"","exit_code":null,"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"/bin/zsh -lc 'echo hooktest-exec'","aggregated_output":"hooktest-exec\n","exit_code":0,"status":"completed"}}
{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"hooktest-exec"}}
{"type":"turn.completed","usage":{"input_tokens":25699,"cached_input_tokens":14080,"cache_write_input_tokens":0,"output_tokens":37,"reasoning_output_tokens":0}}
```

**Hooks fired, same run, same process** (payloads captured by `hooklog.sh`):

```jsonl
{"session_id":"019f974c-...","hook_event_name":"SessionStart","model":"gpt-5.5","permission_mode":"bypassPermissions","source":"startup"}
{"session_id":"019f974c-...","turn_id":"019f974c-...","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo hooktest-exec"},"tool_use_id":"call_..."}
{"session_id":"019f974c-...","turn_id":"019f974c-...","hook_event_name":"PostToolUse","tool_name":"Bash","tool_response":"hooktest-exec\n","tool_use_id":"call_..."}
{"session_id":"019f974c-...","turn_id":"019f974c-...","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"hooktest-exec"}
```

The `Stop` payload's shape (`hook_event_name`, `stop_hook_active`, `last_assistant_message`) is Claude Code's wire format verbatim. This directly confirms the T3325/T3511 claim: **a `Stop` hook exists on Codex 0.145.0 and fires reliably under `codex exec`.**

Both channels — hooks and stdout JSONL — were live in the identical invocation. Neither is speculative; the question was never "does X exist," it was "which should the driver depend on."

## Reconciling the `Stop` hook claim

T3325 and T3511 are correct that a `Stop` hook exists and fires — that part of "Codex can use `ToolUseInterception` the way Claude does today" (T3511) is empirically sound, and it is sound for the purpose it was actually invoked for: interception/guardrails, not progress.

It is not the right basis for turn-boundary detection, for a reason independent of whether it fires: Codex also emits `turn.completed` natively on stdout, which is a _structurally_ stronger boundary signal than any hook — it requires no settings file, no shim binary on `PATH`, no persisted trust record, and it cannot be silently skipped the way a hook can. `turn.completed` also carries per-turn token usage, which no hook payload does.

The decisive asymmetry: for **progress**, there is already an unconditional, trust-free, native channel on the wire (stdout), so depending on hooks instead would trade a robust channel for a fragile one with no offsetting benefit — and hooks fail open and silently, which is actively dangerous for a liveness signal specifically (an unnoticed skipped hook reads as "worker is fine and quiet," not "signal lost"). For **interception**, there is no such alternative yet — a `PATH`-shim guardrail is a real, better option but is unbuilt follow-on work — so hooks remain the mechanism there, deliberately and asymmetrically. Progress and interception are not required to agree on a transport.

## Decision: primary progress channel

**stdout JSONL (`codex exec --json`) is the Codex driver's progress channel. Hook callbacks are not used for progress**, for the reasons above, in order of weight:

1. Fail-open silence is disqualifying for a liveness signal specifically — an untrusted/misconfigured/missing-binary hook produces zero observable signal, so a hook-fed progress channel cannot be distinguished from "worker hung" when it silently stops.
2. Stdout is already present, typed, and structured (`thread.started` / `turn.started` / `item.started` / `item.completed` / `turn.completed`) with zero extra provisioning — no trust record, no settings file, no `--dangerously-bypass-hook-trust`.
3. `turn.completed` carries per-turn token usage for free (useful to a future rate-limit/balancer seam).
4. It avoids stacking a second, hook-shaped dependency on top of the guardrail-trust story that `ToolUseInterception` is already stuck carrying.

This is not "both channels work, arbitrarily pick one" — they are being put to genuinely different, non-competing uses (progress over stdout, interception over hooks), which is why "both work" does not, by itself, unblock anything: the unblocking fact is which capability each channel is assigned to.

## Row-by-row reconciliation

- **T3328** (`ProgressIngress::HookCallback` vs `ProgressIngress::StdoutJsonl` transport split) — **the split itself is correct and should proceed**, but if scoped only to stop the `.expect()` panic on an empty hook map (i.e. just tolerate a hookless driver), that is insufficient: it would leave a Codex worker provisioned but observing nothing. It needs to actually define and wire the `StdoutJsonl` arm through `ProgressObservationWiring`, not just make the `HookCallback` arm optional.

- **T3512** (stdout JSONL reader implementation) — **real, on the critical path, not speculative.** It is the concrete implementation the decision above requires. Its existing downstream dependents (T3324, T3514, T1483) are correctly chained on it and should stay that way.

- **T3528** ("liveness contract for non-hook drivers: staleness floor + resolve `progress_fidelity`") — **unblock, and re-scope its premise.** It was blocked on "is Codex a non-hook driver," phrased as if that were still open. It is now decided, and the answer is yes _for progress specifically_: Codex's progress arrives over `StdoutJsonl`, not hook cadence, even though Codex separately has hooks for interception. The existing staleness sweep (`stale_worker_sweep.rs`, keyed on hook cadence today) and the never-consulted `progress_fidelity()` both need a real answer for a driver whose liveness signal is a stdout stream, not a hook. This is Codex-relevant now, not a latent gap for a hypothetical third driver — recommend dropping "non-hook drivers" framing in favor of "drivers using `ProgressIngress::StdoutJsonl`," and unblocking it as a Codex-critical-path item. I found no coverage of staleness/liveness in the P3330 design doc (PR #2285) — this remains genuinely open design work, not something already answered elsewhere.

- **T3529** ("derive `AwaitingInput` without Claude's Notification-to-Stop proximity") — **unblock, same reasoning, reinforced by an independent Codex-specific fact.** Codex has no `Notification` hook event in the set verified here, so the `Notification`→`Stop` proximity heuristic has no direct analog regardless of which channel carries progress. Separately, `codex exec` is one turn per process — a follow-up/probe requires `codex exec resume`, a _new process_, not an injected message into a live session (the P3330 doc calls this its least-validated area). `AwaitingInput` needs a genuinely new derivation for Codex; this task is not speculative and should not wait on a hypothetical driver.

- **T3325** (`TurnBoundary` trait method + engine synthesizer) — **correctly scoped in substance but should be split.** The trait method (`turn.completed` → `WorkerEvent::Stop`) is real, Codex-critical work. The synthesizer half — inferring a turn boundary from a lower-fidelity channel when neither hooks nor native turn events exist — is not needed for Codex, since Codex's `turn.completed` is a native, better-than-Claude's-hook signal. Recommend re-scoping T3325 down to the trait method and splitting the synthesizer into its own, lower-priority task for a hypothetical future driver that has neither hooks nor turn events. (This exact split is independently proposed in the P3330 doc.)

## What this resolves vs. what it doesn't

Resolved by this chore: which channel Codex progress should use (stdout JSONL), why hooks are not it despite genuinely existing and firing, and what changes in each of the five named rows.

Not resolved here (explicitly out of scope per the brief — this chore decides, T3328/T3512 implement): the transport-split implementation itself, the stdout reader, and the staleness/`AwaitingInput` derivation logic for T3528/T3529. Those remain real, now-unblocked follow-on work.

One adjacent fact worth surfacing for the coordinator: P3330's design doc (PR #2285) is still open/unmerged and is corroborating, not authoritative-by-merge, evidence for this decision — my own reproduction above stands on its own regardless of that PR's status, but three of the five rows here (T3328, T3512, T3325) also appear in that PR's proposed amendments, so reviewing/merging it would remove duplicated reasoning between the two documents.
