# How `codex exec` surfaces command exit codes, and where command output is lost

- **Date:** 2026-07-29
- **Parent project:** Codex as a first-class agent driver
- **Deliverable:** investigation writeup — **no code change**
- **Pins:** `codex-cli 0.145.0`, model `gpt-5.6-terra`, `model_reasoning_effort=low`
- **Harness + raw transcripts:** [`codex-exit-code-surfacing-artifacts/`](codex-exit-code-surfacing-artifacts/)
- **Triggering evidence:** execution `exec_18c6873d9f1460c0_1` (checkleft-sandbox, 2026-07-28 11:24 PDT), where a worker asserted `checkleft run` passed against a payload that ends mid-build with no result line and no exit code

A Codex worker reported a validation step as passing when its captured output stopped mid-build. Eight probes against a real `codex exec` reproduce that exact shape and identify its cause. The investigation also found a second, independent defect in Boss's own parser that was not part of the original question.

## Verdict

**A missing exit code is not an inherent limitation of `codex exec`.** Exit codes are emitted reliably, on both JSONL dialects, for every command that runs to completion — including chained commands, signal kills, and commands whose output is truncated.

The missing exit code is the signature of a **command that outlived the model-chosen `yield_time_ms` and was abandoned before its final chunk arrived**. When that happens the shell command never produces a completion record anywhere: the model never sees an `exit_code`, and `codex exec` still exits `0` with `turn.completed`. Nothing in the pipeline marks the step as unobserved.

Separately and independently: **Boss's rollout parser marks every Codex command result as non-error**, including commands that exited 7, 9, and 137. It looks for the exit code at a path that the CLI never produces.

## The two dialects, and which one Boss actually reads

Codex exposes the same run through two different JSONL dialects. Conflating them is what makes the original evidence look contradictory.

|                                | `codex exec --json` **stdout** | **rollout** JSONL                              |
| ------------------------------ | ------------------------------ | ---------------------------------------------- |
| Envelopes                      | `thread.*`, `turn.*`, `item.*` | `session_meta`, `event_msg`, `response_item`   |
| Carries `exit_code` / `status` | Yes, on `item.completed`       | Only inside tool-output _text_, and not always |
| Location                       | process stdout                 | `$CODEX_HOME/sessions/rollout-*.jsonl`         |
| Ingested by Boss               | **No**                         | **Yes**                                        |
| Seen by the model              | **No**                         | **Yes**                                        |

Boss declares `ProgressIngress::AgentJsonlFile` pointing at `$CODEX_HOME/sessions` with prefix `rollout-` ([`codex.rs:1261`](../../engine/driver/src/codex.rs)), for a stated reason: a pane-hosted worker's stdout belongs to Ghostty's pty master, so the engine cannot read it.

This matters for the original evidence. The countervailing sample in the task description — `"exit_code":2,"status":"failed"` — is a **stdout-dialect** envelope. It proves the CLI _can_ report exit codes, but it comes from a stream **neither Boss nor the model ever sees**. It therefore does not contradict the worker's blindness; it is orthogonal to it.

## The exec tool is a JavaScript sandbox, not a shell call

The single most consequential fact, and the one that reframes the whole question: the model does not call a shell tool with a command string. It writes JavaScript:

```text
const r = await tools.exec_command({"cmd":"sh -c 'echo LINE-ONE; echo LINE-TWO; exit 7'",
  "workdir":"…","yield_time_ms":10000,"max_output_tokens":1000});
text(JSON.stringify(r));
```

Two of those arguments are **chosen by the model, per call, with no floor and no Boss-side control**:

- `yield_time_ms` — how long to block before returning a partial result (observed: 10000 and 30000).
- `max_output_tokens` — the output budget (observed: 1000, 2000, 10000).

And the final line is a model-authored **projection**: the model decides which fields of the result reach the transcript. This is a discretionary channel where a driver would normally have a fixed contract.

## Finding 1 — exit codes are correct and present in the ordinary case

| Probe              | Command                       | `exit_code` (both dialects) | `status` |
| ------------------ | ----------------------------- | --------------------------- | -------- |
| `p1_short_nonzero` | `echo …; exit 7`              | `7`                         | `failed` |
| `p2_chain`         | `a && b && (exit 9) && never` | `9`                         | `failed` |
| `p7_signal_kill`   | `kill -KILL $$`               | `137`                       | `failed` |

The model's view carries the exit code as a sibling field of the output:

```text
{"chunk_id":"66d4c6","wall_time_seconds":0.0000028,"exit_code":7,
 "original_token_count":5,"output":"LINE-ONE\nLINE-TWO\n"}
```

**Chain semantics (probe 2):** `a && b && c` surfaces the **chain's** exit code — `9`, the failing element's — not the last element's, and not `0`. This is just `zsh -lc` semantics and is the behaviour Boss wants.

**Signals (probe 7):** a `SIGKILL` surfaces as `137` (128 + 9), `status: failed`. There is no separate signal field; a consumer must infer signals from the `128 + N` convention.

## Finding 2 — truncation is middle-out, explicitly signalled, and preserves the exit code

Probe 3 ran `seq 1 300000; echo TAIL-MARKER-XYZZY; exit 5` (~1.05 MB). Truncation is applied in **two layers**, both announced in-band:

```
Warning: truncated output (original token count: 11827)     <- outer, on the tool-output block
Total output lines: 1

{"chunk_id":"5ce387",…,"exit_code":5,"original_token_count":497229,
 "output":"Warning: truncated output (original token count: 497229)\n
           ... 940337 bytes omitted ...\n\n1\n2\n3\n…"}       <- inner, on max_output_tokens
```

Properties that matter:

- **Middle-out, not tail-drop.** The head _and_ the tail survive; the middle is replaced by an explicit byte count. The model confirmed it saw `TAIL-MARKER-XYZZY`, the last line before exit.
- **`exit_code` always survives**, because it is a sibling of `output`, not inside it.
- **Truncation is observable** to a careful reader: both `original_token_count` and an explicit `... N bytes omitted ...` marker are present.
- **But truncation destroys machine-readability.** The elided JSON no longer parses, so any consumer that `JSON.parse`s the payload loses the exit code entirely even though it is textually present.

**This rules truncation out as the explanation for the original evidence.** Middle-out truncation would have preserved the build's final lines and its exit code. The evidence payload instead simply _stops_, with the tail absent. That is a different failure.

## Finding 3 — the root cause: the yield-and-abandon path

Probe 6 is the reproduction. It ran a ~48 s command whose exit code was randomised so the model could not infer it, and explicitly licensed the answer `NONE`.

What the model saw, in order:

1. `exec_command(…, yield_time_ms: 30000)` →
   `Script running with cell ID 1 / Wall time 11.1 seconds / Output:` — **no output, no `exit_code`**
2. `wait` →
   `Script completed / Wall time 17.7 seconds / Output:` followed by
   `{"chunk_id":"d0540d","wall_time_seconds":30.0,"session_id":8467,"original_token_count":14,"output":"tick-1\n…\ntick-8\n"}`
   — partial output, **no `exit_code` field at all**

The model then stopped and answered `observed_exit=NONE`.

**The trap is the wording.** `Script completed` refers to the **JavaScript cell** finishing, _not_ the shell command. At step 2 the command was still running — only 8 of 12 ticks had been emitted — yet the payload says "completed". A worker that reads "Script completed" as "the command finished" will conclude success from a payload that contains no exit code.

Meanwhile, on the stdout dialect for the same run:

```
item.started   command_execution   exit_code=None  status=in_progress
                          <- no item.completed, ever
turn.completed
```

and the `codex exec` process **exited 0**.

So in the abandon case there is no completion record _anywhere_ — not in the rollout, not on stdout. The only trace is a dangling `item.started` that never resolves. Neither `turn.completed` nor the CLI's own exit status reflects that a command was left unfinished.

**Probe 4 is the control:** the identical command, with the model choosing to poll one more time, does receive the final chunk carrying `"exit_code":4`. Nothing is inherently unobservable — the difference is purely whether the model kept polling.

## Finding 4 — the model's projection can silently discard output

In probe 8 the model wrote its own projection:

```js
text(r.exit_code);
```

The resulting tool output in the transcript is the single byte `7`. The command's actual stdout — `LINE-ONE\nLINE-TWO` — **never entered the rollout at all**, and so never entered Boss's transcript. It exists only in the stdout dialect that nobody reads.

This is a second, independent way for command output to vanish, with no truncation marker and no warning: not a transport limit, but the model electing not to forward it. Probe 2 shows a milder version (`text(JSON.stringify({exit_code:r.exit_code,output:…}))` — a hand-picked subset).

## Finding 5 — a sandbox denial does not produce a non-zero exit code

Probe 5 ran under `--sandbox read-only` (Boss's Reviewer mode):

```
BEFORE-WRITE
touch: ./sandbox-probe-file.txt: Operation not permitted
AFTER-WRITE
```

`exit_code: 0`, `status: completed`, on **both** dialects.

The denial is visible only as text on stderr. Because the enclosing shell continued, the exit code is `0`. **A sandbox denial is therefore undetectable from `exit_code`/`status` alone** — it is a genuinely distinct failure shape, exactly as suspected. Any Boss-side "did this command fail" check built solely on exit codes will pass a run whose writes were all silently refused.

## Finding 6 — Boss's rollout parser cannot detect a failed command at all

This was not part of the original question but is the most directly actionable defect found.

[`canonical_rollout_tool_output`](../../engine/codex-rollout/src/lib.rs) derives `is_error` like this:

```rust
let is_error = obj.get("metadata")
    .and_then(|meta| meta.get("exit_code"))
    .and_then(exit_code_nonzero)
    .unwrap_or(false);
```

It expects `metadata.exit_code`. The CLI emits `exit_code` at the **top level** of the chunk object, and that object is itself a _string inside a text block of an array_. The array branch of the parser is hardcoded `is_error: false`.

Measured across all 12 tool-output records in the eight probes:

|                                               | count      |
| --------------------------------------------- | ---------- |
| Records where `metadata.exit_code` is present | **0 / 12** |
| Records with a top-level `exit_code`          | 5 / 12     |
| Records Boss classifies as `is_error: true`   | **0 / 12** |

Boss marks the exit-7, exit-9, and exit-137 failures as non-errors. The `metadata.exit_code` shape the parser implements is documented in that file as "the shell tool dialect"; the JS `exec` tool this CLI version actually uses does not produce it.

## Answers to the questions asked

**Is a missing exit code ever an inherent limitation of this mode?**
**No.** Every command that runs to completion reports `exit_code` and `status` on both dialects (probes 1, 2, 3, 5, 7), including under truncation and under both sandbox modes. A missing exit code always means the command was still running when the model stopped polling (probe 6), and probe 4 shows the same command reporting `exit_code: 4` when polling continues.

**Where exactly was the `checkleft run` output lost?**
**It was not lost in transport, and it was not truncated.** The most strongly supported explanation is that the quoted payload _is_ a complete partial yield chunk of a still-running command: `repobin: building //tools/checkleft:checkleft...` is precisely where a slow build sits when the yield window expires. Truncation is excluded by Finding 2 — it is middle-out and would have preserved the build's final lines and its exit code, plus an explicit `... N bytes omitted ...` marker that the evidence does not contain.

I could **not** re-run that original execution (different repo and session), so this is inference from a reproduced matching signature, not a direct replay. Two secondary candidates remain consistent with the same evidence and cannot be separated without the original rollout's tool-call inputs: a model-authored projection that dropped fields (Finding 4), and a `max_output_tokens` low enough to elide the middle (Finding 2). All three share the same fix surface.

**What is the truncation behaviour?**
Two layers, both in-band and both announced. Inner: `max_output_tokens`, model-chosen per call (observed 1000–10000), middle-out with an exact `... N bytes omitted ...` byte count and `original_token_count`. Outer: a cap on the tool-output block itself, announced as `Warning: truncated output (original token count: N)`. Head and tail both survive, and `exit_code` always survives. It is observable to a worker that looks — but it destroys JSON parseability, so a structured consumer loses the exit code even though a human reader can still see it.

**What contract can Boss rely on for "this command failed"?**

| Field                                  | Source                 | Reliable?         | Failure mode                                                                            |
| -------------------------------------- | ---------------------- | ----------------- | --------------------------------------------------------------------------------------- |
| `item.completed.exit_code` / `.status` | stdout dialect         | Yes when present  | **Boss does not ingest this stream**; absent entirely for abandoned commands            |
| top-level `exit_code` in the chunk     | rollout tool output    | Yes when present  | absent while running; unparseable after truncation; droppable by the model's projection |
| `metadata.exit_code`                   | what Boss parses today | **Never present** | wrong path for this CLI version                                                         |
| `Script completed` text                | rollout tool output    | **No**            | refers to the JS cell, not the command                                                  |
| `turn.completed`, CLI exit status      | both                   | **No**            | both report success with a command left unfinished                                      |

The honest summary: **there is currently no field Boss reads that can express "this command failed."** The only sound positive signal available today is a top-level `exit_code` in the rollout chunk — and its _absence_ must be treated as "unobserved", never as "passed".

**What would have to change so a worker cannot report a step passed when no exit code was observed?**

1. **Fix the parser path** (small, necessary, insufficient alone). Read top-level `exit_code`, and parse the JSON embedded in text blocks. Restores `is_error` for the 5/12 records that carry an exit code. Does nothing for the abandon case, where no exit code exists at all.
2. **Detect the dangling command.** At `turn.completed`, any `command_execution` with `item.started` and no `item.completed` is an unobserved command. This is the only signal that directly catches the reported failure. Requires the stdout dialect, which Boss does not currently ingest — so it costs a transport change (tee stdout to a run-private file alongside the pane).
3. **Pin the model's discretion.** Impose a minimum `yield_time_ms` and a fixed result projection via `CODEX_HOME` config or prompt contract, so long builds cannot be abandoned early and fields cannot be dropped. Cheap; but prompt-level constraints are advisory, not enforced, and this fights the tool's design rather than observing it.
4. **Make validation claims structured and deny-by-default.** Require a worker asserting "checks passed" to cite an observed exit code per command; treat an unsupported claim as a failed run. Strongest guarantee and the only one that also covers Finding 5 (sandbox denials, where the exit code is `0` and honest). Largest surface, and it belongs to the driver abstraction rather than to Codex.

Options 1 and 2 are complementary and together cover every reproduction here; 4 is the durable guarantee. **Do not treat 3 as sufficient on its own.**

## Notes for the driver abstraction

- **`ProgressFidelity::Rich` currently overstates Codex.** The driver claims rich fidelity, but its ingress cannot express command failure at all. The abstraction needs a capability that distinguishes "reports per-command exit status" from "reports activity only", or the scheduler will assume a guarantee that is not there.
- **Sandbox denial is a failure class that exit codes cannot carry** (Finding 5). Claude workers get this via hook denials; Codex has no equivalent signal. This is a genuine gap in the abstraction, not a Codex quirk to work around.
- **Load-balancing seam** (out of scope to build, per the project brief): any cross-driver dispatch needs a normalised per-command outcome — `{command, exit_code: Option<i32>, observed: bool}` — because Codex's "unobserved" state has no Claude counterpart, and a scheduler that maps it to "success" reproduces exactly this bug at fleet scale.

## Method and limitations

Eight probes, each a real `codex exec` run using Boss's exact production spawn line ([`codex.rs:702`](../../engine/driver/src/codex.rs)) — `--json --strict-config --skip-git-repo-check --sandbox <mode> -m <model>` — with a run-private `CODEX_HOME` containing a copied `auth.json`, mirroring `codex_home_for_run`. Both dialects were captured for every probe. The harness, prompts, and raw JSONL are in [`codex-exit-code-surfacing-artifacts/`](codex-exit-code-surfacing-artifacts/); `harness/README.md` documents re-running them.

**Deviation from the requested method, stated plainly.** The brief asked for the probes to run inside a GhosttyKit-hosted pane. They did not. The existing harness requires a ~140 MB uncommitted xcframework and a focus-stealing GUI window on the operator's machine. The substantive variable that hosting would introduce is whether codex's stdout is a **tty** rather than a pipe, so probe 8 isolates exactly that with a real PTY (`harness/pty_probe.py`). The result is byte-identical envelopes (`exit_code: 7`, `status: failed`; only CRLF line endings differ) and an identically-shaped rollout. Combined with the fact that Boss's ingress is a file written by codex independently of its terminal, the findings are host-independent. A GhosttyKit run would still be the stricter check, and is the obvious thing to add if any of this is disputed.

**Other limitations.** All probes used `gpt-5.6-terra` at `model_reasoning_effort=low`; `yield_time_ms` and `max_output_tokens` are model-chosen, so a different model or effort may pick different values and change how often the abandon path is hit — the _mechanism_ is not model-specific, but its _frequency_ is unmeasured. Probe 3's `stdout.jsonl` is committed with its 1 MB `aggregated_output` elided (marked in-band); everything else is verbatim. The original failing execution was not replayed.

Per the investigation scope, no code was changed. Follow-up code changes are filed separately.
