# What Codex's `PreToolUse` hook actually accepts back from a guard

- **Date:** 2026-07-30
- **CLI pin:** `codex-cli 0.145.0`
- **Occasion:** every tool call by a Codex worker emitted one `PreToolUse hook returned unsupported decision:approve` per armed guard (five for a local non-revision Standard worker). Boss's guards are written in Claude Code's hook dialect and the Codex trace shim re-emitted their stdout byte-for-byte.
- **Question under test:** what is the _correct_ allow response? The design doc ([`designs/codex-as-a-first-class-agent-driver.md`](../designs/codex-as-a-first-class-agent-driver.md)) already listed the three rejected strings, but the affirmative allow token — if one exists at all — was never established. Getting this wrong reintroduces a silent fail-open, so it was measured rather than inferred.

## Verdict

**Codex's `PreToolUse` is deny-only. There is no affirmative allow token: the correct allow response is to emit nothing and exit 0.**

**A refusal must carry a non-empty reason.** `{"decision":"block"}` and `{"decision":"block","reason":""}` are both rejected — and rejection is _fail-open_, so an unexplained refusal runs the tool call. That was not merely noise, and it is the part of this bug that had teeth.

## Apparatus

A single `.*` `PreToolUse` guard was armed in a throwaway `CODEX_HOME`, printing one candidate payload and nothing else. One real `codex exec` turn was run per candidate against a prompt that forces a shell call:

```sh
CODEX_HOME=<probe home> codex exec --dangerously-bypass-hook-trust --skip-git-repo-check \
  -C <ws> -s read-only -m gpt-5.6-terra 'Use your shell tool to run exactly: echo hello. Then stop.'
```

Two observables per run, both from the session's own output: whether codex printed `hook: PreToolUse Failed`, and whether `echo hello` actually ran.

`--dangerously-bypass-hook-trust` was used deliberately — the question is decision vocabulary, not trust provisioning, which has its own gate and its own live `hooks/list` attestation ([`codex-hook-trust-provisioning-2026-07-26.md`](codex-hook-trust-provisioning-2026-07-26.md)).

**One false start worth recording, because it produced a convincingly wrong answer.** The first harness wrote a _relative_ path into `command = "…"`. Codex reported `hook: PreToolUse Failed` for every candidate — including `decision:block` — and the guard never executed at all. Read naively that says "Codex rejects everything"; it actually says the hook could not be spawned. The tell was a tee file that was never written. Any measurement here should confirm the guard ran before trusting a rejection.

## The contract, measured

| guard stdout                                                                            | codex    | tool call   |
| --------------------------------------------------------------------------------------- | -------- | ----------- |
| _(nothing, exit 0)_                                                                     | accepted | proceeds    |
| `{}`                                                                                    | accepted | proceeds    |
| `{"continue": true}`                                                                    | accepted | proceeds    |
| `{"decision":"block","reason":"…"}`                                                     | accepted | **blocked** |
| `{"hookSpecificOutput":{…,"permissionDecision":"deny","permissionDecisionReason":"…"}}` | accepted | **blocked** |
| `{"decision":"approve"}`                                                                | rejected | proceeds    |
| `{"decision":"allow"}`                                                                  | rejected | proceeds    |
| `{"decision":"deny","reason":"…"}`                                                      | rejected | proceeds    |
| `{"decision":"block"}`                                                                  | rejected | proceeds    |
| `{"decision":"block","reason":""}`                                                      | rejected | proceeds    |
| `{"hookSpecificOutput":{…,"permissionDecision":"allow"}}`                               | rejected | proceeds    |
| `{"suppressOutput": true}`                                                              | rejected | proceeds    |

Three findings beyond the reported bug:

1. **`deny` is not a synonym for `block` on the `decision` key.** `{"decision":"deny"}` is rejected, while `permissionDecision:deny` is accepted. The shim's `classify()` treated `deny` as a valid block and re-emitted it verbatim, so a guard using that spelling would have failed open. No shipping guard used it; the translation now covers it regardless.
2. **A reasonless block is a disarmed guard**, per the verdict above.
3. **`continue: true` is accepted**, though only `continue:false` appears in the binary's rejection list — so the list is not a complete description of the key's handling. `continue:false` itself was not exercised live; the model in `decision.rs` rejects it on the binary string alone, and says so.

`updatedInput` is deliberately absent from both the matrix and the model. The binary's string is `PreToolUse hook returned updatedInput without permissionDecision:allow`, which establishes only that it is refused _without_ an allow that is itself refused — not that a bare `updatedInput` is unsupported. Nobody has measured it, so nothing here claims to know.

The rejection strings are present verbatim in the shipping binary and cover more than `PreToolUse` alone:

```
PreToolUse hook returned unsupported decision:approve
PreToolUse hook returned unsupported permissionDecision:allow
PreToolUse hook returned unsupported permissionDecision:ask
PreToolUse hook returned unsupported stopReason
PreToolUse hook returned unsupported suppressOutput
PreToolUse hook returned unsupported continue:false
PreToolUse hook returned reason without decision
PreToolUse hook returned permissionDecision:deny without a non-empty permissionDecisionReason
```

## The fix, and where it lives

Translation happens at the **one choke point** every Codex guard already passes through: `emit_decision()` in the trace shim ([`engine/driver/src/codex/guard_trace.rs`](../../engine/driver/src/codex/guard_trace.rs)). The five guard bodies are unchanged — two are shared verbatim with the Claude driver and must keep speaking Claude's dialect. This mirrors the Grok driver's `translate_decision()`, which solved the same class of problem; the Codex shim was written later and chose verbatim passthrough instead.

The measured contract is encoded as executable data in [`engine/driver/src/codex/decision.rs`](../../engine/driver/src/codex/decision.rs), so tests assert _"would Codex accept this?"_ rather than restating a literal. Restating it by hand is how the bug shipped: the existing tests verified that guards fire, never that the agent accepts what they emit.

## Post-fix verification

The real shim was extracted from the source constant and run in front of a Claude-dialect guard under the same live `codex exec` harness:

| guard emits                         | codex    | tool call                                |
| ----------------------------------- | -------- | ---------------------------------------- |
| `{"decision":"approve"}`            | accepted | proceeds                                 |
| `{"decision":"block","reason":"…"}` | accepted | **blocked**, reason surfaced verbatim    |
| `{"decision":"block"}`              | accepted | **blocked**, substituted reason surfaced |
| guard crashes (`exit 3`)            | accepted | **blocked**, fail-closed reason surfaced |

No `hook: PreToolUse Failed` in any run, and `guard-trace.jsonl` still records the guard's own word (`approve` / `block` / `guard_error`), so engine-side observability is unchanged by the translation.
