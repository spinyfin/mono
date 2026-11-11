# Antigravity CLI hook surface: identity, permissions, and turn boundaries

- **Date:** 2026-11-11
- **Provenance:** binary inspection and interactive PTY measurements of `agy` 1.1.12 and 1.5.5; documentation only, no driver implementation.
- **Related:** [driver design](../designs/antigravity-driver-fourth-driver-google-gemini-cli.md) ([PR #2739](https://github.com/spinyfin/mono/pull/2739), revised in [PR #3551](https://github.com/spinyfin/mono/pull/3551)); [Ghostty pane study](ghostty-agy-pane-viability.md) ([PR #3550](https://github.com/spinyfin/mono/pull/3550)).
- **Evidence:** the measured matrices, payload excerpts, and timelines below; existing pane-study artifacts are linked where reused.

The hook stream distinguishes a native subagent from its parent on both measured binaries, and `PreToolUse` can rewrite arguments as well as deny execution. Completion needs more care: a command can fail with an empty hook `error`, and a parent's last `Stop` is not always the last event in its family of conversations.

## Verdict

Use the parent `conversationId` to filter lifecycle events, prefer the native `SessionStart` for identity establishment, and support argument rewriting through `overwrite`. Do **not** declare `CommandOutcomeObservation` from these hooks. A matching parent `Stop` with `fullyIdle=true` is the measured idle signal; neither `PostInvocation` nor `NO_TOOL_CALL` alone establishes idleness.

Use **1.5.5 as the next conformance pin**, consistent with the revised driver design. Both versions support identity filtering and rewriting, but 1.1.12 produced a parent `fullyIdle=false` followed by a child `fullyIdle=true` with no later parent boundary during 41.6 seconds of observation. The normalizer must not turn that child's boundary into the parent's, and a hook-only implementation must not claim that it recovers this older-version case.

## Scope and method

This extends the pane study rather than repeating its discovery, trust, onboarding, or Escape matrix. Its existing evidence supplies the GhosttyKit measurements; the new measurements here use an ordinary interactive PTY, **not a GhosttyKit viewer, headless mode, or Boss dispatch**. That distinction matters: this study characterizes hook payloads and lifecycle behavior, not pane viability or fleet acceptance.

| Binary | SHA-256                                                            | Use                                                      |
| ------ | ------------------------------------------------------------------ | -------------------------------------------------------- |
| 1.1.12 | `c8fd3c0016e101689f923f82da1c068b0e6dce3abcb0089e282742693ad4d344` | Original design pin; retained for comparison             |
| 1.5.5  | `42b591f206a868dd0b924d5b6b396638a108539367695a5dbe2f7dcfdbf4d6f3` | Installed binary; matches the pane study's newer version |

The apparatus used macOS arm64, `gemini-3.8-flash-low`, a separate scoped home for each CLI session, and `AGY_CLI_DISABLE_AUTO_UPDATE=true`. Onboarding and workspace trust were pre-seeded as in the pane study. The login keychain was bridged by symlink; credentials were not printed or copied into this document. All six hooks were installed in the scoped global customization directory using the existing [hook configuration generator](ghostty-agy-pane-viability-artifacts/apparatus/tools.py) and [stdin recorder](ghostty-agy-pane-viability-artifacts/apparatus/dump_hook.sh).

There were **23 CLI sessions and 343 recorded hook payloads**, including apparatus diagnostics. Every session ended with a real process exit status of 0; that is the CLI's status, **not** the status of its tool commands. The decisive observations were:

| Probe                   | Completed observations                                                      |
| ----------------------- | --------------------------------------------------------------------------- |
| Native child identity   | One ordinary child on each version; one delayed child on 1.5.5              |
| Argument replacement    | One command on each version, with independent marker-file verification      |
| Approval cache          | Five completed cases on 1.1.12; six on 1.5.5                                |
| Review without a grant  | `force_ask`, `deny`, and an unknown verdict on both versions                |
| Permission bypass mode  | `ask`, `force_ask`, `deny`, and an unknown verdict on both versions         |
| Nonzero command outcome | One intentional `exit 7` on 1.5.5                                           |
| Real build              | One successful engine build on 1.5.5, after three failed apparatus attempts |

The raw local recordings remain under the workspace's ignored `.codex/agy-hooks/`; they are not published evidence links and are not required to read this document. The excerpts below preserve the decisive results. UUID prefixes identify actual measured conversations; omitted common payload fields are explicitly described rather than presented as a complete JSON schema.

## Subagent identity: distinct, including at Stop

**Yes: a child's `Stop.conversationId` differs from its parent's.** This was true in all three child probes. Each child also received its own `SessionStart`, `PreInvocation`, tool hooks, `PostInvocation`, transcript path, and artifact directory. Parent-side `invoke_subagent` hooks retain the parent's ID; subsequent hooks for work executed by the child carry the child's ID.

| Scenario               | Parent ID prefix | Child ID prefix | Child Stop                       | Final observed parent Stop        |
| ---------------------- | ---------------- | --------------- | -------------------------------- | --------------------------------- |
| Ordinary child, 1.1.12 | `0a3a05ed`       | `233f406b`      | `NO_TOOL_CALL`, `fullyIdle=true` | `NO_TOOL_CALL`, `fullyIdle=false` |
| Ordinary child, 1.5.5  | `42227c9d`       | `74b3e2a3`      | `NO_TOOL_CALL`, `fullyIdle=true` | `NO_TOOL_CALL`, `fullyIdle=true`  |
| Delayed child, 1.5.5   | `c27136c5`       | `ad731dcb`      | `NO_TOOL_CALL`, `fullyIdle=true` | `NO_TOOL_CALL`, `fullyIdle=true`  |

The ordinary probe asked the parent to define and invoke a native child, have it execute `printf CHILD_HOOK_OK`, send `CHILD_DONE`, and finish. The parent waited for the result before reporting `PARENT_DONE`. The recorded `define_subagent`, `invoke_subagent`, child `run_command`, and child `send_message` calls establish that these were native children, not shell processes imitating them.

Representative 1.5.5 child payload, with paths and other common fields omitted:

```json
{
  "hookEventName": "Stop",
  "conversationId": "74b3e2a3-2e36-4cfd-982a-bc499d905419",
  "executionNum": 0,
  "terminationReason": "NO_TOOL_CALL",
  "fullyIdle": true,
  "error": "",
  "lastUserInput": ""
}
```

Its parent was `42227c9d-cdd1-4aa2-b4cf-2233178cd0d1`. The child's transcript and artifact directory both contained the child's UUID. The common runtime payloads did **not** expose a parent ID, root ID, or subagent Boolean in these probes. Do not substitute `lastUserInput`, `executionNum`, the workspace path, or the transcript basename for the conversation identity.

### Ordering caveat: the older parent can stop before its child

The 1.1.12 ordinary-child timeline, in seconds from process start:

```text
12.51  parent SessionStart
12.71  parent PreInvocation invocationNum=0
20.57  parent PreToolUse invoke_subagent
24.64  parent Stop NO_TOOL_CALL fullyIdle=false
25.77  child  SessionStart
29.80  child  PreToolUse send_message CHILD_DONE
29.89  parent PreInvocation invocationNum=0
31.57  parent Stop NO_TOOL_CALL fullyIdle=false
31.85  child  Stop NO_TOOL_CALL fullyIdle=true
        no further parent hook for 41.6 seconds; observation then ended
```

The parent had already received the message and produced its final response while the child was still finishing. This is a measured missing parent idle notification in that interval, not evidence that a child shares its parent's identity. The CLI was then terminated normally with SIGTERM; there was no build or tool command left running.

For comparison, the delayed 1.5.5 child sent `EARLY_DONE`, ran `sleep 5`, and finished. Its parent stopped with `fullyIdle=false` at 16.33 s; the child stopped with `fullyIdle=true` at 22.26 s; the parent automatically re-invoked at 22.34 s and stopped with `fullyIdle=true` at 26.97 s. The parent also attempted an invalid `schedule` call despite the prompt asking it not to; the recorder captured that tool error. This sample proves the observed completion wake-up, not universal compliance with the probe prompt.

### Consequence for ingress

Claim the parent identity once, through the existing engine-owned [progress identity mechanism](../../engine/driver/src/codex/progress.rs), and reject boundary-shaped events with another ID. Children must neither replace that claim nor become eligible to finish the worker. Their ordinary progress may be retained separately if useful, but filtering a child's lifecycle event must not disable the child's `PreToolUse` guards.

In all measured fresh sessions, the parent's synchronous `SessionStart` and first `PreInvocation` preceded the tool call that created a child. This supports first-parent-event claiming **when delivery preserves that ordering**. It does not prove safety if those events are lost or replayed out of order: a later child `SessionStart` looks like another conversation start, with no parent marker in its payload. Persist identity and preserve startup delivery ordering; do not recover a missing claim by blindly accepting an arbitrary boundary event.

## Invocation events and SessionStart

`PreInvocation` and `PostInvocation` bracket a model/executor invocation, including its synchronous tool processing. They are not one-per-user-turn events. The observed single-command sequence remains:

```text
SessionStart
PreInvocation(0) → PreToolUse → PostToolUse → PostInvocation(0)
PreInvocation(1) → PostInvocation(1) → Stop
```

When a tool continues in the background, `PostInvocation` can precede `PostToolUse`. See the real-build timeline below. Thus “post invocation” does not mean all tool children have finished.

| Field or event                                         | Measured meaning                                                                           |
| ------------------------------------------------------ | ------------------------------------------------------------------------------------------ |
| `SessionStart`                                         | Once per new conversation, when its first prompt begins; also emitted for native children  |
| `invocationNum`                                        | Zero-based within an execution; resets on a later user turn or automatic execution         |
| `initialNumSteps`                                      | Present on both invocation events; the paired values matched in the single-command samples |
| `executionNum`                                         | Was 0 in every captured Stop; unsuitable as a unique turn counter                          |
| `conversationId`                                       | Stable across user follow-ups and automatic re-invocations of that conversation            |
| `hookEventName`, `cwd`, `lastUserInput`, `toolCall.id` | Present on 1.5.5; absent from the corresponding 1.1.12 samples                             |

The binary's protobuf descriptor also contains optional invocation output/thinking fields; their existence is not evidence that the CLI serializes them. The captured invocation hooks carried no model response text.

The descriptors explain another version difference: 1.1.12's `PreInvocationHookResult` contains `injectSteps` but no `terminationBehavior`; 1.5.5 contains both. The pane study's `terminationBehavior: "terminate"` reply stopped before the model call on 1.5.5 (`TERMINAL_CUSTOM_HOOK`) but was ineffective on 1.1.12. Do not assume invocation-control replies are interchangeable between those pins.

**Can the first `PreInvocation` produce `WorkerEvent::SessionStart` and the identity claim? Yes, as an adapter mapping, once.** It already carries `conversationId`, `transcriptPath`, `artifactDirectoryPath`, `modelName`, and `workspacePaths`. However, native `SessionStart` is earlier and was observed on both versions, so wiring it directly is preferable. A first-`PreInvocation` fallback must deduplicate against the persisted claim; `invocationNum == 0` alone would produce false session starts on every later execution. Neither event proves the composer was ready before the first prompt was submitted.

For 1.1.12, pass the event name in each hook command's argv, as the existing recorder does. One generic forwarder cannot distinguish all event kinds from that version's payload alone. On 1.5.5, `lastUserInput` repeats the full user prompt on hooks; the earlier pane study measured a roughly 51 KB payload for a 50 KB brief. Account for that in buffering and avoid logging whole prompts merely to record progress.

## Complete terminationReason vocabulary from the binaries

These are the distinct `EXECUTOR_TERMINATION_REASON_*` strings extracted from the executable bytes, with that prefix removed. This enumerates the embedded enum, **not a claim that every member is reachable through interactive Stop**.

| Value                       | 1.1.12  | 1.5.5   | Interactive evidence                     |
| --------------------------- | ------- | ------- | ---------------------------------------- |
| `UNSPECIFIED`               | Present | Present | Not triggered                            |
| `ERROR`                     | Present | Present | Not triggered as a Stop reason           |
| `USER_CANCELED`             | Present | Present | Escape emitted no Stop in the pane study |
| `MAX_INVOCATIONS`           | Present | Present | Not triggered                            |
| `NO_TOOL_CALL`              | Present | Present | Observed with both values of `fullyIdle` |
| `MAX_FORCED_INVOCATIONS`    | Present | Present | Not triggered                            |
| `EARLY_CONTINUE`            | Present | Present | Not triggered                            |
| `TERMINAL_STEP_TYPE`        | Present | Present | Not triggered                            |
| `TERMINAL_CUSTOM_HOOK`      | Present | Present | Observed on 1.5.5 in the pane study      |
| `INJECTED_RESPONSE`         | Present | Present | Not triggered                            |
| `MAX_TOKEN_BUDGET_EXCEEDED` | Present | Present | Not triggered                            |
| `HALTED_STEP`               | Present | Present | Not triggered                            |
| `CHAINED`                   | Absent  | Present | Not triggered                            |

The binary extraction independently reproduced the pane study's [enum evidence](ghostty-agy-pane-viability-artifacts/evidence/13-termination-reasons.txt). Reproduction against an absolute executable path:

```sh
python3 -c 'import pathlib,re,sys; b=pathlib.Path(sys.argv[1]).read_bytes(); print("\n".join(sorted({v.decode().removeprefix("EXECUTOR_TERMINATION_REASON_") for v in re.findall(rb"EXECUTOR_TERMINATION_REASON_[A-Z_]+",b)})))' /absolute/path/to/agy
```

The old embedded guide's lowercase examples (`model_stop`, `max_steps_exceeded`, `error`) do not describe the observed wire values. An adapter should retain unknown reason strings for diagnostics and handle them conservatively, rather than assume they mean success or silently omit a boundary-shaped event.

## PostToolUse does not establish command outcome

**Omit `CommandOutcomeObservation`.** Across all 36 recorded `PostToolUse` payloads, the union of fields contains `error` but no structured exit status or command output on either version. The 1.5.5 intentional failure made the distinction concrete:

```text
run_command input:       CommandLine="exit 7", WaitMsBeforeAsync=5000
tool result/transcript:  Command exited with code 7.
PostToolUse:             error=""; no exitCode, exit_code, status, result, or output
Stop:                    NO_TOOL_CALL, fullyIdle=true, error=""
```

The successful engine build also produced `error=""`. So did failed Bazel invocations during apparatus setup. An empty hook error means neither “exit 0” nor “the requested work succeeded.” Conversely, the delayed-child probe's malformed `schedule` call populated `error` with a validation failure, showing that the field can report tool-level errors.

The embedded `PostToolHookArgs` descriptor contains `step_idx`, `tool_call`, `error`, and `result`; **the descriptor's `result` was not in the runtime JSON**. A future implementation cannot claim command observation merely because a schema or transcript sometimes contains a result. That would contradict the project's [capability contract](../../engine/driver/src/lib.rs), which requires reliable per-command outcomes rather than inferred success from activity.

## PreToolUse can rewrite arguments

**It is not intrinsically deny-only.** Both binaries' `PreToolHookResult` descriptors contain `decision`, `reason`, `overwrite` (a protobuf Struct), and `permissionOverrides`, plus deprecated `allowTool`/`denyReason` fields. The live probe used the current `decision` and `overwrite` fields:

```json
{
  "decision": "allow",
  "overwrite": { "CommandLine": "printf HOOK_REWRITTEN > rewritten.txt" }
}
```

The model requested `printf MODEL_ORIGINAL > original.txt` with `WaitMsBeforeAsync=5000`. On **both** 1.1.12 and 1.5.5:

- `PreToolUse.toolCall.args.CommandLine` contained the model's original command.
- `PostToolUse.toolCall.args.CommandLine` contained the replacement command; the original wait and summary fields remained.
- `rewritten.txt` contained exactly `HOOK_REWRITTEN`; `original.txt` did not exist.

That demonstrates replacement of a top-level argument while retaining other arguments. The [current upstream hook reference](https://antigravity.google/docs/hooks) describes a shallow top-level merge; nested-object merging and conflicts between several rewriting hooks were not measured here. Do not translate a whole tool-call object into `overwrite`: its keys are argument names such as `CommandLine`, not `toolCall` or `args`.

Boss can expose rewrite-or-deny interception once its dialect adapter implements and verifies that translation. Choosing to implement only denial initially would be an explicit adapter scope choice, not a limitation of `agy`.

## Exact verdict behavior in the measured modes

The following matrix concerns a `run_command` shell tool. “Prior grant” means selecting the TUI's **always allow in this conversation** option for the same command. The complete six-case cached sequence ran on 1.5.5; the first five ran on 1.1.12. Separate sessions on both versions measured `force_ask`, `deny`, and an unknown verdict without a grant, plus explicit denial with permission bypass enabled.

| Reply decision  | Default review mode, no grant                                | Same command with prior grant          | `--dangerously-skip-permissions`        |
| --------------- | ------------------------------------------------------------ | -------------------------------------- | --------------------------------------- |
| `allow`         | Ordinary approval still required, measured in the pane study | Runs without another approval          | Runs                                    |
| `deny`          | Denies through the pre-tool hook                             | Denies despite the grant               | Denies, both versions                   |
| `ask`           | Shows approval dialog                                        | Runs without another approval          | Runs without an approval, both versions |
| `force_ask`     | Shows approval dialog, both versions                         | Shows approval again despite the grant | Runs without an approval, both versions |
| `not_a_verdict` | Denies, both versions                                        | Denies on 1.5.5 despite the grant      | Denies, both versions                   |

Observed cached-grant sequence:

```text
ask          → dialog; select “always allow in this conversation”; PostToolUse
ask          → no dialog; PostToolUse
force_ask    → dialog again; approve once; PostToolUse
allow        → no dialog; PostToolUse
deny         → no dialog; no PostToolUse
not_a_verdict→ no dialog; no PostToolUse  [sixth case completed on 1.5.5]
```

Denied calls returned control to the model and still ended with `Stop NO_TOOL_CALL fullyIdle=true error=""`. In the fresh 1.5.5 unknown-verdict probe, no marker file was created. The 1.1.12 sequence had successful `ask` and `force_ask` calls followed by the unknown verdict, which produced no corresponding `PostToolUse`. A clean Stop is therefore not evidence that a guard-approved command actually ran.

`force_ask` bypasses a cached grant in review mode, **not the global permission-bypass flag**. Consequently neither `ask` nor `force_ask` is a safe substitute for a Boss denial. Preserve the design's adapter policy: only recognized approval maps to `allow`; rejection, invalid input, unknown adapter verdicts, `ask`, and `force_ask` map to an explicit `deny`. Do not depend on the UI to enforce a guard decision.

The earlier [reply-contract measurements](ghostty-agy-pane-viability-artifacts/evidence/10-hook-reply-semantics.txt) remain applicable on both versions: empty stdout with exit 0 permits normal permission evaluation; `{}`, malformed JSON, exit 1, and exit 2 deny. This distinction matters for a progress forwarder: successful silence is valid; printing `{}` is not equivalent. Non-PreToolUse hook parse failures were logged and ignored in that study.

This is not a complete permission-policy characterization. Persisted grants, explicit permission deny rules, `permissionOverrides`, deprecated fields, and other verdicts such as `deny_unless_prior_grant` are not promoted to supported Boss behavior by these results. The [current online reference](https://antigravity.google/docs/hooks) also describes `auto_approve`; that is not evidence for enabling it on either binary pin.

## Real builds, non-idle Stop, and automatic continuation

A real `bazel build //tools/boss/engine/core:engine` completed successfully under 1.5.5. The tool used `WaitMsBeforeAsync=500`; no extra prompt or polling tool was sent after it started. Seconds below are relative to the CLI process start:

```text
 5.96  PreToolUse run_command: bazel build //tools/boss/engine/core:engine
 6.58  PostInvocation invocationNum=0        tool has moved to background
 6.68  PreInvocation  invocationNum=1
 8.98  PostInvocation invocationNum=1
 8.98  Stop NO_TOOL_CALL fullyIdle=false     BUILD_PENDING
17.33  PostToolUse error=""                 build has actually finished
17.43  PreInvocation  invocationNum=0        automatic, no user input
19.02  PostInvocation invocationNum=0
19.15  Stop NO_TOOL_CALL fullyIdle=true      BUILD_DONE
```

The completion message in the native transcript contained **`Command exited with code 0`** and Bazel's own **`Build completed successfully`**. Bazel reported 11.184 s elapsed, 1,608 action-cache hits, and 53 disk-cache hits. The engine binary was built but never launched. These are build observations, not a claim that the repository test suite ran.

Three earlier attempts failed because of the probe environment, not project changes: two default repository-cache paths were lexically inside the checkout's scoped home, and one attempt omitted the worker's required `BOSS_WORKER_TOOL_PATH`. The corrected run preserved that tool environment and used the ordinary host home **only for the Bazel subprocess**, preserving the normal cache paths. The CLI, its hooks, and conversation state stayed scoped. No cache was disabled, cleared, or deliberately invalidated. The failed attempts still supplied useful negative outcome samples, but are not counted as successful build evidence.

The real-build result confirms the pane study's [background-command trace](ghostty-agy-pane-viability-artifacts/evidence/08-midturn-input-and-background-tasks.txt): `fullyIdle=false` is an execution pause with work outstanding. The next model invocation can be triggered by a tool completion or child message, rather than a typed prompt. The same `conversationId` survives, while `invocationNum` restarts.

### Interrupt remains a separate recovery path

The pane study measured Escape during both a running command and generation on both versions: **no Stop, PostInvocation, or PostToolUse for the canceled turn**. Its [interrupt evidence](ghostty-agy-pane-viability-artifacts/evidence/07-escape-interrupt.txt) includes four original cases and two further 1.5.5 command cases. `USER_CANCELED` being in the binary enum does not repair that missing event.

Keep explicit interrupt recovery, tied to the parent and the interrupted execution. A run-private CLI log or the measured pane interruption marker can supply evidence; the ordinary transcript cannot reliably do so. Do not synthesize a successful TurnEnd from silence, a child Stop, or a subsequently completed unrelated turn.

## Reproducing and interpreting the measurements

1. Verify the executable hash. Disable its updater with the exact string `AGY_CLI_DISABLE_AUTO_UPDATE=true`; `=1` is ineffective in the pane study.
2. Provision a separate home using the pane study's [setup recipe](ghostty-agy-pane-viability-artifacts/apparatus/common.sh). Install all six global hooks with the existing `tools.py hooksjson` generator and `dump_hook.sh`. Give every hook invocation its event name in argv.
3. Start the interactive CLI with `--model gemini-3.8-flash-low --log-file <private-log>`. Submit through the composer once ready. Use `--dangerously-skip-permissions` only for the matrix cells explicitly labeled with it.
4. For replacement, use the exact reply and command above; inspect both hook payloads and marker files. For verdicts, keep the command identical across turns, select the conversation-only grant, and change `reply.PreToolUse.json` between completed turns. Observe the dialog as well as whether a PostToolUse occurred.
5. For child identity, use the exact child workflow above; correlate each child ID with its own transcript and native subagent creation. For background completion, record the first non-idle Stop, subsequent tool completion, and final parent idle Stop without typing another prompt.

The old approval dialog says “Do you want to proceed?” where 1.5.5 says “Run this command?” Two initial dialog observations were ended and the recorder corrected to recognize the actual wording; they are not counted as completed cache sequences. A feedback survey intercepted the sixth prompt of the 1.1.12 cached sequence and the third prompt of its later review sequence. Those attempts are excluded from completed-case counts; unknown-verdict and explicit-denial behavior were measured in separate sessions. The unknown verdict with a cached grant remains measured only on 1.5.5. None of these apparatus corrections is classified as product flakiness.

## Driver follow-up and remaining limits

| Area             | Implementation consequence                                                                                                        |
| ---------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| Hook wiring      | Include native `SessionStart`; retain an explicit event-name argument for older payloads                                          |
| Identity         | Claim once, persist across continuation, and filter foreign lifecycle events before completion handling; protect startup ordering |
| Interception     | Translate canonical rewrites into `overwrite`; retain explicit fail-closed verdict mapping                                        |
| Command outcomes | Leave `CommandOutcomeObservation` absent; never map empty `error` to exit 0                                                       |
| Boundaries       | Require the claimed parent's idle Stop; preserve reason/error diagnostics; do not finish at non-idle Stop                         |
| Interrupt        | Implement run-private recovery for the measured missing Escape boundary                                                           |
| Version pin      | Use 1.5.5 for subsequent conformance work; retain the 1.1.12 missing-parent-boundary case as a regression fixture                 |

The remaining limits do not require choosing between alternative product behavior to accept this investigation. They constrain what later code may claim: nested or multiple concurrent children, replay/reordering at Boss ingress, multi-hook rewrite composition, and every enum member's reachability were not exercised. Both measured 1.5.5 child runs reached a parent idle Stop; that finite observation is not a proof for every subagent schedule. Any older-version support needs an explicit reconciliation design for the missing parent notification, without accepting a child's Stop as the worker's.

No source changes are included. The follow-up code belongs in the existing hook adapter, identity normalizer, boundary recovery, and conformance work described by the driver design.
