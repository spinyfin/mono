# Codex eligibility for the review kind: `--sandbox read-only` enforcement and the `ReviewResult` round-trip

- **Date:** 2026-07-31
- **CLI pin:** `codex-cli 0.145.0` (an update to 0.146.0 was offered and declined mid-run, to keep the pin honest)
- **Code sha:** `main` at `66e83f35` ("boss: generalise driver traffic allocation to a three-way split (#2616)")
- **Occasion:** [T-25](../designs/codex-as-a-first-class-agent-driver.md#t-25-codex-eligibility-for-review-and-conflict-resolution-kinds) — Phase 3 of the Codex-driver rollout, gated on two explicit verifications: that `--sandbox read-only` genuinely stops a Codex worker from writing, and that structured `ReviewResult` output round-trips through the environment-file contract the driver actually uses. Both are exercised live below, not inferred from the `--sandbox` flag being present on the spawn line.
- **Related:** [`designs/codex-as-a-first-class-agent-driver.md`](../designs/codex-as-a-first-class-agent-driver.md), [`codex-tui-pivot-pricing-2026-07-30.md`](codex-tui-pivot-pricing-2026-07-30.md) (the bare-TUI execution shape this investigation drives), [`codex-pretooluse-decision-vocabulary-2026-07-30.md`](codex-pretooluse-decision-vocabulary-2026-07-30.md) (method precedent: measure, don't infer)

## Verdict

**`--sandbox read-only` is genuine, OS-enforced, and cannot be worked around from inside the sandbox — confirmed by a real write attempt failing, with a real write attempt under `danger-full-access` in the same harness succeeding as a control.** This part of T-25's brief is closed cleanly.

**The `ReviewResult` round-trip is _not_ clean, and the reason is a direct consequence of the first finding.** `--sandbox read-only` is unconditional: it also blocks the reviewer's own sanctioned write — the `$BOSS_STRUCTURED_OUTPUT` artifact file the engine reads back after the run, which lives outside the workspace specifically so it "does not touch the PR" (`pr-review/src/render.rs:390-391`). `--add-dir` cannot carve out an exception either; Codex refuses it outright under `read-only` ("Switch to workspace-write or danger-full-access to allow them"). So the **primary** channel of the structured-output contract fails on every single Codex review, unconditionally, by construction — not intermittently, not as an edge case.

The round-trip still completes, but only via the engine's existing **transcript fallback** (`finalize_pr_review_pass`, `core/src/completion/finalize_passes.rs`), which was designed for remote workers and artifact-write failures and is documented in that file as "TRANSITIONAL". A live run confirmed the fallback works: the model recognized the write failure, said so, and still delivered a valid fenced `ReviewResult` JSON block as the prompt's own "Also (fallback)" instruction asks for. So the round-trip is real, but Codex reviewers depend on machinery the engine's own comments call transitional, on every run, not as a rare degrade path.

**A latent bug in that fallback path's retry logic was found and fixed as part of this investigation** (see [Bug found and fixed](#bug-found-and-fixed-below)): the nudge-probe the engine sends when neither channel produces a valid result told the worker to "write it to this file with the Write tool" — Claude's tool name, and an instruction a read-only-sandboxed Codex reviewer can never satisfy. Left as-is, a Codex reviewer whose first-pass fallback JSON came out malformed would have been re-prompted with an instruction guaranteed to fail again, burning the bounded nudge budget for nothing and then silently advancing the PR without a revision — exactly the failure mode `finalize_pr_review_pass`'s own comment says it exists to prevent.

**A larger, and more decisive, finding: none of this is reachable in production today.** The review pool's driver is a hardcoded constant, `REVIEWER_POOL_DRIVER: &str = "claude"` (`core/src/coordinator.rs:1503`), consulted unconditionally by `pool_dispatch_policy_for_worker_id` for every `review-*`/`auto-worker-*` slot regardless of what driver authored the row under review. There is currently no seam — no config, no per-product knob, nothing — by which a `PrReview` execution could select Codex. This is the real gate on review-kind eligibility, not (only) capability fidelity, and it is a legitimate, separate decision: the existing doc comment on that constant states an explicit invariant ("who authored a change must not determine who reviews it") that a rollout of Codex-as-reviewer has to reckon with deliberately, not as a side effect of a capability-gate change. See [Recommendation](#recommendation).

**Conflict-resolution has no equivalent gate and needs no new verification here.** `ConflictResolution` maps to `WorkerKind::Standard` (`core/src/worker_setup.rs:170`), which gets Codex's ordinary writable sandbox posture (`danger-full-access` today, `workspace-write` under the `codex_sandbox_enforced` flag), and it designates no structured-output payload (`designated_output_kind`, `core/src/runner/prompt.rs:228-249` has no `ConflictResolution` arm — it falls through to `None`). It dispatches on the ordinary per-task/product driver column, the same path the general acceptance sweep (T-23) already exercises for standard implementation work. There is nothing kind-specific about conflict-resolution that the sandbox/structured-output investigation below bears on.

## Method

Two throwaway `CODEX_HOME` + throwaway-repo harnesses, isolated per the pattern in the design doc's Appendix A, but driving the driver's **actual production spawn shape** — the bare interactive TUI (`codex --strict-config --no-alt-screen -a never --sandbox <mode> <prompt>`, `build_codex_command`, `driver/src/codex.rs:860-900`) — not the retired `codex exec`. The TUI needs a real tty (`ratatui`/`crossterm` fail immediately with `Error: stdin is not a terminal` under a redirected stdin — the opposite of `exec`'s `< /dev/null` requirement, and the first thing this harness got wrong before correcting it), so each run was hosted in a detached `tmux` session (`tmux new-session -d -x 220 -y 50 …`) and read back with `tmux capture-pane -p -S -N`. This is a lighter apparatus than the pane-viability spike's GhosttyKit embedder, but exercises the identical `codex` invocation Boss's pane spawn issues; V1 of the pivot spike already established that a positional prompt auto-submits on the bare TUI without a synthetic Return, which this harness's captures confirm again independently.

Each repo carried a per-workspace `[projects."<path>"] trust_level = "trusted"` stamp in `$CODEX_HOME/config.toml`, matching `render_base_config_toml` (`driver/src/codex.rs:578-604`), and `auth.json` symlinked from `~/.codex/auth.json` (never copied).

### 1. `--sandbox read-only` — write attempt, with a control

Prompt (paraphrased): run `echo hello > probe_write.txt` then `cat probe_write.txt`, report exact stdout/stderr of both, do nothing else.

**Under `--sandbox read-only`:**

```
• Ran echo hello > probe_write.txt
  └ zsh:1: operation not permitted: probe_write.txt

• Explored
  └ Read probe_write.txt
    stderr: cat: probe_write.txt: No such file or directory
```

`ls` on the real repo directory afterward shows no `probe_write.txt` — the denial is real, not a Codex-level refusal that still touched the filesystem underneath.

**Control, same prompt, same harness, `--sandbox danger-full-access`:**

```
• Ran echo hello > probe_write.txt
  └ (no output)
    stdout: hello
```

`cat` on the real file afterward confirms `hello` was actually written. The control matters: it proves the harness would have shown success had enforcement been absent, so the read-only result above is not a harness artifact (e.g. a broken prompt, a model that silently gave up).

**Verdict: `--sandbox read-only` is real, OS-level (Seatbelt) enforcement on the driver's actual bare-TUI spawn shape.** The denial surfaces as a shell-level `operation not permitted`, i.e. the write syscall itself is refused, not merely something Codex chose not to attempt.

### 2. `ReviewResult` round-trip through the env-file contract

The **real** reviewer prompt was used, not a paraphrase: a throwaway `#[test]` was temporarily added to `pr-review/src/render.rs` calling `render_reviewer_initial_prompt` with a small synthetic `PrReviewContext` (embedded diff, so no `gh` calls were required for the diff itself) and `panic!`-dumped the rendered text so it could be captured from `bazel test` output. The test was reverted immediately after capture; the working tree carries no trace of it.

The repo under review was a real two-line diff (`add()` returning `a - b` vs `a + b`), checked out at the "PR head" state per the prompt's own framing. `BOSS_STRUCTURED_OUTPUT` was exported to a path outside the review repo, matching `structured_output::path_for`'s "outside every repo/workspace" placement and `default_structured_output_wiring`'s env-var wiring (`driver/src/lib.rs:1525-1534`) — Codex uses this default unmodified (`driver/src/codex.rs:1827-1835`); it does not use `--output-schema`/`--output-last-message`.

Run under `--sandbox read-only` (the Reviewer posture `codex_sandbox_for_worker_kind` always selects, `driver/src/codex.rs:406-417`):

```
• Ran printf '%s\n' '{ "pr_url": ... }' > .../review-result.json
  └ zsh:1: operation not permitted: .../review-result.json

• The required artifact write is blocked by the read-only filesystem policy: the
  shell returned operation not permitted for the exact output path. I'll still
  provide the required fenced JSON backstop in the final response.

• I completed the read-only review. ... I could not write .../review-result.json
  because the sandbox rejected the write with operation not permitted. ...

  {
    "pr_url": "https://github.com/example/spike/pull/1",
    "head_sha": "bbbbbbb",
    "summary": "The PR changes add() from subtraction to addition, ...",
    "revision_warranted": false,
    "findings": [],
    "regression_check": { "performed": true, "suspected_deletions": [] }
  }
```

The fenced JSON is well-formed and validates against `ReviewResult::from_json` (checked by hand against `pr-review/src/types.rs`'s schema — `pr_url`, `head_sha`, `summary`, `revision_warranted`, `findings`, `regression_check` all present and correctly typed). The artifact file itself does not exist on disk — confirmed directly, not inferred from the model's own narration.

**`--add-dir` does not close the gap.** Re-run with `--sandbox read-only --add-dir <dir containing the artifact path>`:

```
Error adding directories: Ignoring --add-dir (<dir>) because the effective
permissions do not allow additional writable roots. Switch to workspace-write
or danger-full-access to allow them.
```

Codex refuses the flag outright under `read-only` rather than granting a scoped exception. There is no flag-level fix available within the single-shape, `--sandbox`-driven design; `read-only` means categorically zero writable roots, full stop.

**Verdict: the round-trip works end-to-end, but only through the transcript fallback, on every run — not through the primary artifact channel, ever, for a Reviewer-postured Codex worker.** This is a structural property of `--sandbox read-only`, not a flake.

## Bug found and fixed

`finalize_pr_review_pass`'s retry probe (`core/src/completion/finalize_passes.rs`, fires when neither the artifact nor the transcript produces a valid `ReviewResult`) told the worker: _"write it to this file with the Write tool, then stop"_. `Write` is Claude's tool name, and the instruction assumes the artifact path is writable — both false for a Codex reviewer under `--sandbox read-only`, as measured above. A Codex reviewer that reached this retry path (e.g. a first-pass fallback JSON that failed to parse) would have been probed with an instruction it structurally cannot follow, wasting the bounded nudge budget and then silently advancing the PR without a revision on `NudgeDecision::Trip` — precisely the old failure mode the surrounding code exists to prevent.

Fixed to be driver-agnostic: the probe now names the artifact path without naming a tool, and explicitly offers the fenced-JSON-in-final-message channel as the fallback when the sandbox does not allow the write. This does not change behavior for Claude (which can usually write the artifact and rarely needs the fallback wording) and gives a Codex reviewer an instruction it can actually act on. See the diff in `core/src/completion/finalize_passes.rs`.

## Recommendation

**Do not flip `REVIEWER_POOL_DRIVER` in this change.** That constant is a deliberate, documented invariant ("who authored a change must not determine who reviews it") shared by both the review pool and the automation-triage pool, and changing it is a dispatch-policy decision — who reviews Boss's PRs — not a capability-fidelity question T-25 was scoped to answer. It also directly abuts the load-balancing project this design doc explicitly places out of scope.

**What this investigation adds to the record, for that future decision:**

1. The seam already exists structurally — `PoolDispatchPolicy` is a small struct (`driver: &'static str`, `model_tier: PoolModelTier`) returned by one function. Making it configurable (env var, product setting, or a genuine load-balancer policy) is a small, well-isolated change when the operator decides to make it.
2. When that seam opens, a Codex reviewer's `ReviewResult` will round-trip correctly, but only via the transcript fallback — budget for that in whatever validates the rollout (T-22's cross-transport conformance harness is the natural place to assert it, since it already asserts stdout-JSONL/hook-ingress equivalence for the same kind of channel-parity question).
3. The probe-wording bug fixed here should stay fixed regardless of when/whether that seam opens — it was a real bug in shared, driver-agnostic code, independent of Codex.

**`codex exec review` was not evaluated as an alternative path here**, per the operator's binding decision recorded in the design doc: the driver ships one spawn shape (the bare TUI), and `codex exec review`'s required flags conflict with that shape's contract exactly as `codex exec`'s did (see [`codex-tui-pivot-pricing-2026-07-30.md`](codex-tui-pivot-pricing-2026-07-30.md)). What this investigation adds is a concrete cost the design doc did not yet have measured: `codex exec review` is purpose-built and may not have shared the read-only/artifact-write conflict found here at all — a dedicated review mode plausibly returns its result over its own channel (stdout, `--output-last-message`, or similar) rather than requiring the reviewed model to write a file from inside its own sandbox. That is exactly the kind of fidelity the single-shape decision gives up; it is recorded here as the trade-off's cost, not as grounds to revisit the decision.

## What this settles for T-25

| Question                                                                                                                   | Verdict                                                                                                                                                                                      |
| -------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Is `--sandbox read-only` a genuine, OS-enforced reviewer-read-only equivalent on the driver's real (bare-TUI) spawn shape? | **Yes** — demonstrated with a real write attempt and a real control.                                                                                                                         |
| Does structured `ReviewResult` output round-trip through the environment-file contract the driver actually uses?           | **Yes, but only via the transcript fallback** — the primary artifact channel is structurally unreachable under Reviewer posture. Not a flake; true on every run.                             |
| Is Codex reachable as a reviewer in production today?                                                                      | **No** — gated by `REVIEWER_POOL_DRIVER`'s hardcoded `"claude"`, independent of everything above. This is the actual eligibility gate; capability fidelity is a secondary concern behind it. |
| Does conflict-resolution need equivalent verification?                                                                     | **No** — `WorkerKind::Standard`, ordinary writable sandbox, no designated structured-output payload, already dispatches on the row's own driver via the path T-23's general sweep exercises. |
