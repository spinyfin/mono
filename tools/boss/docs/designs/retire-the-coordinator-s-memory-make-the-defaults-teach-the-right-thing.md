# Design: Retire the coordinator's memory — make the defaults teach the right thing

- **Date:** 2026-07-24
- **Project:** `proj_18c50f417ff59af8_160` — Retire the coordinator's memory
- **Execution:** `exec_18c5248567032b88_6` (`project_design`)
- **Source analysis:** full read of the coordinator memory store (2026-07-23), classified against `main@origin` `0dfa2ecb2178`; every code claim re-verified against `main@origin` `3aeba8d7` (see [Verification](#verification-what-changed-since-the-sweep)); membership reconciled 2026-07-24 against the 107 files the store now holds.
- **Folds existing prior work:** the A1 (lease lifecycle) and A2 (recovery-patch apply) fixes shipped in `#2216`, the already-filed conflict-worker fix, and the worker proposal API + taxonomy read-access design (pointed at, not duplicated — see A11).

## TL;DR

Three-fifths of the Boss coordinator's private memory store (61 of the 102 classified notes) is an unfiled bug report: workarounds for defaults that make the wrong thing easy, and manual procedures that exist only because Boss lacks a verb or a view. This design converts those 61 notes into a concrete, dependency-ordered [implementation list](#implementation-list) that fixes the defaults and builds the missing surfaces, so the knowledge lives in the product instead of in a notebook that dies with each session. It then defines a **retention policy** that keeps the store from rebuilding itself: memory is for the operator's personal working style and facts about the operator only; anything describing Boss's own behaviour is a defect that must become a work item, not a note.

Re-verification against `main@origin` `3aeba8d7` found that **much more has already landed than the sweep recorded** — the store carries stale "already fixed?" notes. A1 (lease lifecycle) and A2 (recovery-patch apply) landed in `#2216`, and **the entire Phase-3 defect batch the sweep proposed — A6 (nudge gate), A7 (reviewer parsing + conflict-watch), A8 (doc-link gate), A9 (`uninstall` scoping), A12 (checkleft base), A13 (cube clone/GC) — is already fixed with regression tests.** That collapses roughly a third of the proposed work into _verify-and-retire_ stubs and moves the design's centre of gravity onto the surfaces that genuinely do not exist yet: the diagnostic verb (B1 `doctor`), honest observability (A3 `agents list`), the vocabulary and field fixes that let the prompt shrink (A5, A10, A4), and the durable-knowledge relocation (B2, B3, retention policy).

## Goals

- Convert all 61 Category-A (code default / product defect) and Category-B (missing surface) memories into PR-sized, dependency-ordered implementation items, each naming the specific memory files it retires so completion is _measurable_, not asserted.
- Fix the defaults so the wrong thing becomes hard to do, rather than documenting the workaround. Every item is scored against the operator's organising question: _"is that actually a prompt thing, or a defaults thing?"_ — a rule asking the coordinator to remember not to trip over a default is a defect in the default.
- Build the missing read/diagnostic surfaces (`bossctl doctor`, honest `agents list`, structured fields, read-only CLI verbs) that today only exist as hand-written decision trees in private memory.
- Cut the coordinator prompt down to the judgement rules code genuinely cannot encode, and remove the ~90 lines that restate CLI shapes the CLI itself knows.
- Establish a **durable knowledge home and a retention/curation policy** so operational runbooks reach workers (repo `AGENTS.md`/`docs/`) and the memory store does not silently rebuild into a second bug tracker.

## Non-goals

- **Not an implementation.** This run delivers the design doc only. No `.rs`/`.ts`/`.swift`/build-file edits; follow-up work items are filed against this doc after approval.
- **No "banned phrases" or "known caveats" prompt section.** Three memories are already exactly that; one carries a recurrence log proving it does not stick. If a surface induces a wrong model, change the surface.
- **No duplication of the worker proposal API** (worker proposal + taxonomy read-access design). A11 points at it.
- **No Boss-internal mirror of design docs or PR artifacts.** GitHub is source of truth; Boss stores `(repo, path, ref)` only.
- **No further nudge-loop circuit breaker (A6) or doc-link point-patch (A8).** Fourth and sixth attempts respectively; both are scoped root-cause-or-escalate with a live acceptance test, not another throttle or unit-test-only fix.
- **Do not touch the slot model.** Operator-owned, "a project for another day." Slot-adjacent fixes stay tactical and avoid deepening slot coupling.
- **Nothing that reclaims workspaces faster, caps workspace count, or adds a shared bazel cache.** All three explicitly rejected by the operator.
- **Do not automate memory deletion.** T-prune presents a list for operator approval; deletion stays a human act.

## The finding (background)

A full read of the store classified every note. The table below is the surviving classification, reconciled against the 107 files the store holds today:

| Category                                  |   Count | Meaning                                                                                               |
| ----------------------------------------- | ------: | ----------------------------------------------------------------------------------------------------- |
| A — code default / product defect         |      40 | Memory exists because a default or surface makes the wrong thing easy. Fix the code; the memory dies. |
| B — missing tool / missing surface        |      21 | Memory encodes a manual procedure that exists only because Boss lacks a verb, view, or log.           |
| C — genuine behavioural contract          |      23 | Judgement rules no default can enforce. Prompt content.                                               |
| D — personal recall / operator preference |      18 | Legitimately private. Stays as memory.                                                                |
| E — stale / redundant / wrong             |       0 | Already in the prompt, superseded, or a fixed bug. The whole category has been deleted.               |
| **Total classified**                      | **102** |                                                                                                       |

The store holds **107** files. 102 of them carry a classification above; the residual five were written after the sweep and are unclassified — the first job of T-prune is to classify them. Category E is empty because every note in it has been deleted, along with six Category-A notes whose defects were verified fixed (A7 ×2, A9, A12, A13 ×2) and one Category-C note whose contradiction the operator has since resolved by deletion.

This project covers **A and B — all 61 items.** C informs the prompt-surgery item; D stays where it is.

The organising principle, verbatim from the operator when the coordinator proposed encoding an operating rule into the system prompt:

> "is that actually a prompt thing? It feels like a 'cube should have longer leases by default' thing."

Every item below is scored against that question.

## Verification: what changed since the sweep

The sweep's line numbers were captured at `0dfa2ecb2178`. This doc re-verified the load-bearing claims at `main@origin` `3aeba8d7` via four independent read-only passes. The store is known to carry stale entries, and re-verification confirmed that heavily: **eight of the fourteen Category-A items (A1-A14) are already fixed.** The table below is the authoritative status; the implementation list folds the fixed ones as verify-and-retire stubs.

| Item                                     | Sweep status   | Verified status at `3aeba8d7`        | Evidence                                                                                                                                                                                                                                                                                                             |
| ---------------------------------------- | -------------- | ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **A1** lease lifecycle                   | open           | **DONE** (residual: TTL _value_, D5) | `reset_workspace_guarded` refuses reset of dirty `@` → `LeaseExpiredWorkspaceDirty` (`app.rs:5269`); durable quarantine (`app.rs:1651`, reason const `:42`); release preserves unpushed work unless `--force-reset` (`app.rs:5362`)                                                                                  |
| **A2** apply recovery patch              | open           | **DONE**                             | `recovery_apply.rs:319` runs real `git apply --3way`, wired cube-first from dispatch (`coordinator.rs:5278`; `mark_patch_consumed :5376`). Six sweep sites now, not five                                                                                                                                             |
| **A3** `agents list` honesty             | open           | **OPEN**                             | `LiveWorkerState` has no `pool`, no exec `kind` (`protocol/src/live_worker_state.rs:105-184`); reducer never assigns `model` — stays launch default (`engine/core/src/live_worker_state.rs:171-206`); `activity` has no stale-timer downgrade                                                                        |
| **A4** work-item lookup                  | open           | **OPEN (minus selector)**            | `task list` omits chore+revision kinds (`engine/core/src/work/workitems.rs:36`); `task show --json` wraps under dynamic `.task`/`.chore` label (`cli/main.rs:4184`). **Chore full-id selector ALREADY FIXED** (`cli/main.rs:3143`, test `:12341`) → that sub-item is dropped                                         |
| **A5** delete "pool" vocab               | open           | **OPEN**                             | Prompt literal at `BossPaneModel.swift:321` still says "view the cube pool" (only `workspace summary`; no `workspace list`). `SlotBusy` (`engine_app.rs:269`) already carries + logs `occupying_run_id` → that half is largely addressed; the vocabulary rename is the real remaining work                           |
| **A6** nudge gate                        | open (4th try) | **DONE**                             | `resolve_bound_pr_url` falls back to chain-root PR (`completion.rs:4501`); `CiRemediation`/`RevisionImplementation` with null PR are parked, never nudged (`:1940`,`:1962`); `clear_pending_probes` de-arms on marker signal (`:1611`); regression test `:11636`                                                     |
| **A7** reviewer parsing / conflict-watch | open           | **DONE**                             | `suspected_deletions` is `#[serde(skip_deserializing)]`, derived from `findings` (`pr-review/types.rs:230`); parse uses `match` and surfaces errors (`completion.rs:2861`, `parsing.rs:94`). `mergeable=UNKNOWN` → indeterminate, conflict path skipped (`merge_poller.rs:1959`; on-Stop guard `completion.rs:5654`) |
| **A8** doc-link gate                     | open (6th try) | **DONE**                             | Routing + diagnostics now outside the `kind==Design && project` gate; `uses_task_doc` branch covers investigations (`completion.rs:3736-3793`, esp. `:3765`)                                                                                                                                                         |
| **A9** `uninstall` scoping               | open           | **DONE**                             | `using_default_install_root = BOSS_INSTALL_ROOT.is_err()` (`cli/main.rs:9801`); refuses to stop the engine when `BOSS_INSTALL_ROOT` is set (`:9857`); test `sandbox_uninstall_does_not_kill_dummy_engine`                                                                                                            |
| **A10** structured fields + comment verb | open           | **OPEN**                             | No `boss task/chore comment` verb (only `boss comment reply` for the answer-agent, `cli/main.rs:604`); comment engine/app machinery exists to reuse. Effort-provenance + blocked-prose fields still absent                                                                                                           |
| **A11** importer project-scoping         | open           | **OPEN**                             | Importer enumerates only `projectV2` items (`github_tracker/src/github.rs:436`); a `gh issue create` without `--project` is invisible forever                                                                                                                                                                        |
| **A12** checkleft `Scenario::Local` base | open           | **DONE**                             | `select_base_local` now prefers `origin/<default_branch>` before the bare branch (`checkleft/src/change_detection/base.rs:298-305`)                                                                                                                                                                                  |
| **A13** cube clone / GC hang             | open           | **DONE**                             | `auto_create_workspace` uses `jj workspace add` on the shared store (`app.rs:4215`); GC fetch is timeout-bounded via `run_jj_network` and the per-repo flock is dropped before network ops (`app.rs:1620`, timeout `:5908`); the GC heartbeat landed                                                                 |
| **A14** `create-revision --no-autostart` | open           | **DONE**                             | Honoured at `cli/main.rs:7287` (`.autostart(!ctx.no_autostart)`); rest of the A14 batch still open                                                                                                                                                                                                                   |
| **B1** `bossctl doctor`                  | missing        | **OPEN**                             | No `doctor` verb anywhere in `bossctl`. `bossctl dispatch diagnose <exec-id>` exists (`bossctl/main.rs:259`,`:1034`) and should be extended, not duplicated                                                                                                                                                          |

**Consequence for the plan:** A1, A2, A6, A7, A8, A9, A12, A13, plus A4's selector and A14's `create-revision`, are fixed. The proposed Phase 3 (independent defect fixes) is therefore almost entirely a _memory-retirement_ exercise, not new engineering. The real remaining build is A3, A4 (list/show), A5 (vocab), A10, A11, the A14 remainder, B1-B4, the prompt surgery, and the retention policy. Items still marked _confirm at HEAD_ in the implementation list had unconfirmed status at doc-write time (e.g. residual boss-side jj) and the implementer must re-check before coding.

## Alternatives considered

### Alternative 1 — Keep the memory, improve the prompt (status quo, scaled up)

Leave the defaults alone; move the highest-value memories into the coordinator system prompt and write better caveats. **Rejected.** This is precisely what the store already is, and it has a measured failure record: the "pool" vocabulary has a recurrence log, the nudge-gate workaround was documented across four filed work items and still fired 20 times in one run, and the `task show` wrapper shape is documented twice yet still tripped. Prose in the prompt does not reach workers, grows without bound, and dies with the session's context. The operator's own framing rejects this: a rule to remember a default is a defect in the default.

### Alternative 2 — Generate the mechanical prompt sections from the CLI schema

Roughly 40% of the ~280-line prompt literal (`BossPaneModel.swift:308-590`) restates CLI shapes (`jq` recipes, selector forms, flag names). Generate those sections from the CLI's own `clap` schema so they cannot drift, leaving only judgement rules hand-maintained. **Partially adopted, deferred as a follow-up (D1).** The generation machinery is real work with its own failure modes (build-time coupling of the Swift app to the Rust CLI schema), and most of the mechanical lines disappear anyway once A4/A10 remove the _reason_ they exist (the CLI stops needing a `jq 'keys'` workaround once `task show` doesn't wrap by kind). So the v1 move is _delete the obsoleted lines by fixing the surfaces_, and revisit schema-generation only for what remains. Captured as a decision, not a v1 blocker.

### Alternative 3 — Chosen: fix the defaults, build the missing surfaces, then shrink the prompt and set a retention policy

Treat each Category-A/B memory as a defect ticket against a default or a missing surface. Fix the code so the wrong model becomes un-thinkable (rename "pool", make `agents list` honest, add the structured fields), build the one high-leverage diagnostic verb (`bossctl doctor`) that retires twelve decision-tree memories at once, migrate runbooks to repo docs where workers can read them, then do the prompt surgery the fixes enable, and finally lock in a retention policy so the store cannot rebuild. This is the design below.

## Chosen approach

The work splits into four movements, executed roughly in phase order but with heavy intra-phase parallelism (see the [parallelism summary](#parallelism-summary)):

1. **Highest-leverage surfaces and vocabulary (Phase 1).** `bossctl doctor` (B1, retires 12); honest `agents list` with pool + exec-kind (A3, retires 7); unified work-item lookup (A4, retires 4); delete the "pool" vocabulary (A5, retires 3). A1/A2 are already landed and only need their memories retired, plus the one-line TTL follow-up.
2. **Structured fields, then prompt surgery (Phase 2).** First-class fields for effort provenance, blocked-reason prose, and a `boss <kind> comment` verb (A10) — the single biggest prompt-shrinker. Then the prompt surgery those fields and A4/A5 enable: delete ~90 lines of CLI-defect documentation, add the surviving Category-C rules, resolve the contradiction. Then the decision-record surface (B2) and the repo-docs migration (B3), and finally the operator-approved memory-pruning pass.
3. **Independent defect fixes (Phase 3) — mostly already landed.** A6, A7, A8, A9, A12, A13 are verified fixed. A6 and A8 are 4th-/6th-attempt items whose notes retire only after a _live_ check, and the A7 ci-rebounce signature is not confirmed fixed and stays open. The only genuinely open Phase-3 engineering is A11 (importer project-scoping) and the A14 remainder (each a small, independent PR).
4. **Retention policy (cross-cutting, lands with the Phase-2 prompt surgery).** The curation rule that keeps the store from rebuilding, plus the durable-knowledge home decision.

The rest of this section resolves the six decisions the design task must land; the [implementation list](#implementation-list) is the PR-sized decomposition.

### The six decisions

**D1 — Should the coordinator prompt be generated from the CLI schema?**
_Decision:_ Not in v1; delete the obsoleted lines by fixing the surfaces first (Alternative 2). Most mechanical lines exist only because a surface is wrong; A4 and A10 remove the _reason_ for the `jq 'keys'` and tag-composition recipes. After the surgery, re-measure what mechanical content remains and file schema-generation as a separate design if it is still more than a handful of lines. Recorded as a `future / not a v1 blocker` entry.

**D2 — Is there a mechanism to propose a prompt amendment?**
_Decision:_ Ride on the worker proposal API. Prompt amendments are semantically the same act as a worker proposing a taxonomy write: a change the worker cannot make directly that an operator must approve. Do not build a parallel path. This design adds one requirement to that API's scope as a _dependency note_, not a duplicate item: the proposal target enum must be able to name "coordinator prompt" as a proposable artifact. Flagged as an open question for the proposal-API owner (see Risks).

**D3 — Where does durable operational knowledge live, and who can read it?**
_Decision:_ Repo `AGENTS.md` / `docs/` is the home for anything a **worker** needs (runbooks, build-toolchain fixes, deploy topology). Coordinator memory is invisible to workers and must stop being a runbook store. The seven stranded runbooks (B3) migrate to the relevant repo, one chore each, and the memory is deleted only after the doc lands and is confirmed readable. There is no automated sync between memory and repo docs — the retention policy (D4) prevents the divergence at the source by keeping behavioural knowledge out of memory entirely.

**D4 — Retention/curation policy for coordinator memory going forward.**
_Decision, adopt the proposal:_ **Memory is for the operator's personal working style and facts about the operator only. Anything describing Boss's or cube's behaviour is a bug report and must become a work item (or a repo doc), never a note.** This is added to the coordinator prompt as a short Category-C rule (it is a judgement rule code cannot enforce) during the Phase-2 surgery. T-prune is the one-time application; the rule is what makes it stick. Category-D (personal recall) and legitimately-private operator preferences remain.

**D5 — Correct cube lease TTL and release policy.**
_Decision:_ The correctness half is already solved (dirty state survives expiry via guard+quarantine+preserve). The remaining question is the _value_. Standing direction is to bias toward keeping dirty state reachable, so: **hold the lease until the execution reaches a terminal state, with a long wall-clock backstop (proposed 4h) rather than 30m.** Since a dirty workspace is now preserved-and-quarantined rather than reset, a longer TTL costs only a slower reclaim of _clean_ idle workspaces, which are cheap to re-create. This is a one-line const change plus a heartbeat-cadence review, filed as T-A1-ttl. The operator should confirm the 4h backstop figure (open question).

**D6 — Resolve the prompt contradictions.** Two remain, one needing the operator's ruling:

- _Take-the-conn persistence._ The prompt says the mode persists until explicitly revoked; `feedback_take_the_conn_does_not_persist_implicitly` quotes the operator saying the opposite. Needs a yes/no ruling, surfaced in the attentions manifest; T-prompt-surgery applies whichever wins.
- _Investigation doc-link affordance._ The prompt asserts an affordance the engine gate (A8) provably could not produce before the fix. T-prompt-surgery removes the stale claim; A8 has made it true for real. No operator ruling needed.

### Acceptance criterion for the project

The project is complete when **every one of the 61 Category-A/B memory files has been retired** — either because the item that retires it has merged and the memory was deleted in the operator-approved pruning pass, or because re-verification showed the memory was already stale. Each item below names the files it retires; T-prune is the reconciliation that checks the list is exhausted, and that also classifies the five post-sweep notes the sweep never saw. "Done" is thus countable against 61, not asserted.

## Risks / open questions

- **A6 and A8 verified fixed, but they were 4th- and 6th-attempt items and prior attempts shipped test-only while live behaviour did not change.** The current code has the right shape _and_ regression tests, so T-P3-verify-retire treats them as done — but the verifier must confirm live, not just from green unit tests. If live behaviour still contradicts (a nudge loop still fires on a marker-less compliant reply; an investigation still shows no doc link in the running app), re-open that single item as a root-cause-or-escalate work item with a live acceptance test. This is the one place the "already fixed" verdict carries residual risk.
- **D2 depends on the worker-proposal-API owner accepting "coordinator prompt" as a proposal target.** If they decline, prompt amendments keep the file-a-chore loop and D2 becomes a no-op. Surfaced in the attentions manifest.
- **D5 backstop value (4h?)** is a guess pending operator confirmation.
- **D6 take-the-conn ruling** must land before T-prompt-surgery can be written; it gates that item. Surfaced in the attentions manifest.
- **The `model` field on `agents list` (A3)** may be unfixable if the source is genuinely unavailable at render time. The instruction stands: if it cannot be made authoritative, delete the field rather than ship a lie.
- **Several "already fixed?" memories** (residual boss-side jj in A14; the manual-testing carve-out in A14) had unconfirmed status at doc-write time. Each implementer must re-verify at HEAD before coding; if already fixed, the item collapses to a memory-retire only.
- **The store drifts under the plan.** The classification is a snapshot of 107 files; notes are written continuously. T-prune reconciles against the store as it stands at the time it runs, not against this document's counts.

## Implementation list

This is the authoritative section: each entry below is self-contained, and a planner can materialize it without reading anything above. Entries are in dependency order. Effort hints: `trivial | small | medium | large`. "Retires" is the number of memory files the item lets T-prune delete; named files are verified present in the store today. All paths are repo-relative and verified at `main@origin` `3aeba8d7`.

Two items are already landed and appear first as verify-and-retire stubs, so their memories are accounted for in the acceptance count.

### Phase 1 — surfaces and vocabulary (mostly parallel)

**T-A2-retire — Confirm recovery-patch apply landed; retire its memories.**
_Brief:_ `apply_recovery_patch` now runs a real `git apply --3way` and is wired cube-first from dispatch reconciliation. Confirm the behaviour against the existing tests, then mark the A2 memories for deletion in the pruning pass. No code.
_Lands in:_ read-only confirmation at `tools/boss/engine/core/src/recovery_apply.rs:319` and `tools/boss/engine/core/src/coordinator.rs:5278` (consumption marker at `:5376`).
_Effort:_ `trivial`. _Retires:_ 2. _Depends on:_ none.

**T-A1-retire — Confirm no-reset-when-dirty landed; retire its memories.**
_Brief:_ the guard + quarantine + preserve-on-release behaviour is on `main`. Confirm, then mark the A1 memories for deletion. The TTL _value_ is a separate item (T-A1-ttl). No code.
_Lands in:_ read-only confirmation at `tools/cube/src/app.rs:5269` (`reset_workspace_guarded`), `:1651` (quarantine call, reason const at `:42`), `:5362` (`reset_workspace_on_release`).
_Effort:_ `trivial`. _Retires:_ 4 (named: `feedback_preferred_workspace_ignored_when_dirty`, `feedback_cube_owns_workspace_usability`, `project_err_toward_recoverability_over_workspace_thrift`). _Depends on:_ none.

**T-A1-ttl — Raise `DEFAULT_LEASE_TTL_SECS` and review heartbeat cadence.**
_Brief:_ change the const to the operator-confirmed backstop (proposed 4h, replacing 30m), audit that the three apply sites still make sense, and add or adjust a test. Single file, single subsystem.
_Lands in:_ `tools/cube/src/app.rs:36` (`const DEFAULT_LEASE_TTL_SECS: i64 = 1800;`); apply sites at `:990` (lease), `:1888` (heartbeat), `:2006` (explicit-TTL fallback).
_Effort:_ `trivial`. _Retires:_ 0 (folded into T-A1-retire's 4). _Depends on:_ **operator confirmation of the D5 value** (attentions manifest).

**T-B1-doctor — `bossctl doctor <work-item-id | exec-id>`: signature-matching diagnostic. ★ highest leverage.**
_Brief:_ walk `executions/<id>/dispatch.jsonl`, `engine-trace.jsonl`, and live-status; match the mechanically-detectable failure signatures the coordinator currently keeps as hand-written decision trees — `stage_stalled` at `worker_claimed` >30s; `redundant_spawn` at a completed `live_execution_id`; leaked-claim `pool_exhausted` with idle workers; `shell_pid:0` followed by completion 3ms later; `before_commit_sha == head_sha_at_trigger` rebounce; all-leases-timeout-at-30s. Print the matched signature, the evidence lines, and the known recovery. One subsystem reading existing artifacts.
_Lands in:_ `tools/boss/bossctl/src/main.rs` — extend the existing `Diagnose` subcommand (`:259`, handler `dispatch_diagnose` at `:1034`) rather than adding a parallel verb. One memory exists only because `dispatch diagnose` was not found.
_Effort:_ `large`. _Retires:_ 12 (named: `reference_automation_dispatch_stall_at_worker_claimed`, `reference_automation_failed_retrying_redundant_spawn_zombie`, `reference_automation_pool_claim_leak_diagnostic`, `reference_premature_run_terminalization_shell_pid0`, `reference_dispatch_jsonl_diagnostic`, `reference_duplicate_dispatch_shared_workspace_diagnostic`, `reference_bossctl_dispatch_diagnose_for_dispatch_failures`, `reference_worker_hang_backgrounded_bazel_wait_loop`). _Depends on:_ none. Parallel with all other Phase-1 items (distinct files).

**T-A3-agentslist — `agents list`: add `pool` + exec `kind`; make `activity`/`model` honest.**
_Brief:_ add `pool` and execution `kind` to `LiveWorkerState` and populate them in the engine reducer; make `activity` downgrade on a stale `last_event_at` instead of silently showing `spawning` after `events.sock` degrades; make `model` authoritative in the `SessionStart` reducer arm (the hook payload must carry the model id) or **delete the field** if it cannot be. If the span is too wide for one PR, split the `bossctl` renderer from the protocol/reducer change.
_Lands in:_ `tools/boss/protocol/src/live_worker_state.rs:105-184` (struct), `tools/boss/engine/core/src/live_worker_state.rs:171-206` (reducer, `SessionStart` model arm), plus `bossctl` rendering.
_Effort:_ `medium`. _Retires:_ 7 (named: `feedback_dont_trust_agents_list_activity`, `feedback_dont_trust_agents_list_model_field`, `reference_worker_pool_names_and_pool_routing_diagnostic`). _Depends on:_ none. **File overlap with T-A5-slotbusy** (both touch `protocol/`): serialise — A3 first, A5 forward-ports.

**T-A4-lookup — Unify work-item lookup: `task list` must not omit kinds; `show` must not wrap by kind.**
_Brief:_ `boss task list` must include chore and revision kinds (or return an explicit hint) instead of silently dropping them; `task show --json` must not wrap the row under its dynamic `.task`/`.chore` label, so `.task.status` on a chore stops returning null. CLI list/show handlers plus the engine list RPC. The chore full-id selector is already fixed (`tools/boss/cli/src/main.rs:3143`, test `:12341`) — that sub-item is dropped.
_Lands in:_ `tools/boss/engine/core/src/work/workitems.rs:36` (`kind_returned_by_list_tasks`), `tools/boss/cli/src/main.rs:4184` (the `label: task_json` wrapper).
_Effort:_ `small`-`medium`. _Retires:_ 4 (named: `reference_task_list_excludes_chores_and_revisions`, `reference_task_show_wraps_by_kind`, `reference_boss_task_by_pr`). _Depends on:_ none. Parallel with B1/A3/A5.

**T-A5-slotbusy — Delete "pool" vocabulary from capacity surfaces.**
_Brief:_ rename or remove "pool" from `bossctl workspace summary` output and any bounded-looking rendering, so a fully-leased list stops reading as a fixed exhaustible resource. Does **not** edit the Swift prompt literal — that is T-prompt-surgery. The `SlotBusy` half is largely addressed already: the variant carries and logs `occupying_run_id` into `dispatch.jsonl`; add an explicit `slot_id` echo only if the diagnostic still cannot identify the squatting pane.
_Lands in:_ `tools/boss/protocol/src/engine_app.rs:269` (`SlotBusy`) and the `bossctl workspace summary` renderer.
_Effort:_ `small`. _Retires:_ 4 (named: `feedback_automation_means_pool_not_item_kind`, `feedback_workspace_count_is_irrelevant_to_dispatch`, `feedback_dont_extrapolate_pool_failures`, `reference_slotbusy_is_engine_app_slot_desync_not_capacity`). _Depends on:_ T-A3-agentslist (shared `protocol/` files — forward-port A3's changes preservingly).

### Phase 2 — structured fields, prompt surgery, knowledge home (ordered)

**T-A10-fields — First-class fields: effort provenance, blocked-reason prose, `boss <kind> comment` verb.**
_Brief:_ add structured fields for (a) effort-classification provenance and (b) a blocked-reason prose field distinct from the short pill, and add a `boss <kind> comment` CLI verb over the existing comment machinery. This removes the free-text `[effort-*]` tag-stuffing that races the autostart worker. Reuse, do not rebuild: the engine and app comment layers already exist.
_Lands in:_ `tools/boss/cli/src/main.rs:604` (`enum CommentCommand` — today only `boss comment reply` for the answer-agent), `tools/boss/engine/core/src/app/comments.rs`, `tools/boss/protocol/src/types/task.rs:386` (`struct Task`; `blocked_reason` at `:496`, `effort_level` at `:541` — the struct is on the `bon` builder, so additive fields need no construction-site churn).
_Effort:_ `medium`. _Retires:_ 4 (named: `feedback_blocked_reason_is_a_short_label_not_prose`). _Depends on:_ none functionally, but sequence after Phase 1 so the prompt surgery has all its inputs.

**T-A10-tooltip — App: surface blocked-reason prose as a tooltip.**
_Brief:_ render the new prose field from T-A10-fields as a hover tooltip, distinct from the title-cased pill. App-macos only.
_Lands in:_ `tools/boss/app-macos/Sources/` (the work-item row renderer).
_Effort:_ `small`. _Retires:_ 0 (folded into T-A10-fields). _Depends on:_ T-A10-fields.

**T-prompt-surgery — Coordinator prompt surgery.**
_Brief:_ delete the ~90 lines of CLI-defect documentation made obsolete by A4/A5/A10; add the surviving Category-C judgement rules; add the D4 retention rule; apply the operator's D6 take-the-conn ruling; remove the stale doc-link affordance claim. Edit the Swift string literal, **not** the runtime `CLAUDE.md`, which the app rewrites on launch.
_Lands in:_ `tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift:308-590` (`bossSystemPrompt`); the "view the cube pool" line is at `:321`.
_Effort:_ `medium`. _Retires:_ 0 directly (it is what makes the C-category rules stick). _Depends on:_ T-A4-lookup, T-A5-slotbusy, T-A10-fields must merge first (their fixes must land before the lines they obsolete are deleted); **D6 operator ruling** (attentions manifest).

**T-B2-decision — Decision record: "considered and declined / operator-owned".**
_Brief:_ a `wontfix`/`decided` state on work items, or a lightweight `boss decision` record attached to a product, surfaced when filing something semantically near it. Engine + CLI plus a minimal app surface.
_Lands in:_ `tools/boss/engine/core/src/work/` (new state or record), `tools/boss/cli/src/main.rs` (verb), `tools/boss/protocol/src/types.rs`.
_Effort:_ `medium`. _Retires:_ 6 (named: `feedback_dont_file_fixes_for_deliberate_design_choices`, `project_slot_model_rethink_future`, `project_no_checkleft_all_gating_for_now`, `project_local_concurrency_ceiling_remote_is_the_plan`, `project_api_breaking_surface_check_removed`). _Depends on:_ none; parallel with T-A10-fields — confirm no schema-migration collision, otherwise serialise the migrations.

**T-B3-runbooks — Migrate 7 runbooks to repo `AGENTS.md`/`docs/`.**
_Brief:_ one chore per runbook. Land each as repo doc content in the relevant repo, confirm it is worker-readable, then delete the memory. Docs-only, no code.
_Lands in:_ the target repo's `AGENTS.md` or `docs/` — one per note: bazel/Xcode LaunchServices resolution (`reference_bazel_xcode_locator_uses_launchservices`), TestFlight codesign keychain hang (`reference_testflight_codesign_keychain_hang_signature`), Buildkite heterogeneous-queue push auth (`reference_bazel_any_queue_heterogeneous_push_auth`), flunge deploy topology + `/statusz` (`reference_flunge_deploy_topology_statusz`), where checkleft lives (`reference_checkleft_lives_in_mono`), `LINT.IfChange` markers need an enabled instance (`reference_checkleft_ifchange_markers_need_enabled_instance`), the checkleft sandbox repo (`reference_checkleft_sandbox_repo`).
_Effort:_ `small` each (7 items). _Retires:_ 7 (all named above). _Depends on:_ D3 (settled here); parallel across repos.

**T-B4-readonly — Missing read-only CLI verbs.**
_Brief:_ CLI verbs for comments, answer-agent runs, execution history, and by-execution lookup, so the coordinator never falls back to raw `sqlite3` on `state.db`. Engine read RPCs + CLI. Fold the already-filed read-only-verbs work rather than duplicate it.
_Lands in:_ `tools/boss/cli/src/main.rs` (verbs), `tools/boss/engine/core/src/app/` (read RPCs).
_Effort:_ `medium`. _Retires:_ 2 (named: `feedback_prefer_clis_over_sql_for_runtime_state`). _Depends on:_ none; parallel with B2/B3.

**T-prune — Memory pruning pass (operator-approved).**
_Brief:_ reconcile the full 61-file A/B list against what has merged, classify the notes written after the sweep, and present the deletion list for operator approval. Do **not** automate deletion. This is the acceptance-count check for the whole project.
_Lands in:_ the coordinator memory store (coordinator-only state; no repo change). Produces a proposed deletion list, nothing else.
_Effort:_ `small`. _Retires:_ n/a — it is the reconciliation. _Depends on:_ **everything above** (a memory is only retired once its item merged).

### Phase 3 — independent defect fixes (mostly already landed)

**T-P3-verify-retire — Live-verify A6 and A8; retire their memories only on confirmation.**
_Brief:_ A6 (nudge gate) and A8 (doc-link routing) have the right code shape _and_ regression tests, but are 4th-/6th-attempt items with a history of tests-pass-but-app-fails. Confirm each **live in the running app**, then retire its notes. **No code** unless a live check contradicts, in which case re-open that single item as a root-cause-or-escalate work item with a live acceptance test. The A7 ci-rebounce signature is **not** confirmed fixed — its note stays live and is not retired here.
_Lands in:_ read-only verification at `tools/boss/engine/core/src/completion.rs:4501` (`resolve_bound_pr_url`), `:1940`/`:1962` (park guards), `:1611` (`clear_pending_probes`), test at `:11636`; and `tools/boss/engine/core/src/completion.rs:3736-3793` (doc-link routing, `uses_task_doc` at `:3765`).
_Effort:_ `small`. _Retires:_ 4, each only after live confirmation: `reference_produce_pr_nudge_loop_diagnostic`, `reference_yellow_parked_idle_nudge_circuit_breaker` (A6); `reference_investigation_doc_link_gate_excludes_investigations`, `reference_investigation_doc_link_chronic_regression` (A8). _Depends on:_ none.

**T-A14-batch — Smaller defects, filed individually (one PR each, not one row).**
_Brief:_ each of the following is its own `trivial`/`small` PR and must be re-verified at HEAD first, since several may already be fixed the way `create-revision --no-autostart` was (`tools/boss/cli/src/main.rs:7287`): a cancel/prune verb (no `--status cancelled` today); `bossctl dispatch` group help should expose the mutating `pause`/`resume` verbs; stamp a real `engine_build_sha` into live-status (bazel zeroes mtimes); quieten wrapper stderr chatter that breaks `2>&1 | jq`; `create-revision` warns when the PR's producing worker is still live; reconcile the `AGENTS.md`-says-push-docs-to-main rule against the runtime soft-block; move the manual/GUI-testing deferral carve-out into the **reviewer** prompt (confirmed **not** present in reviewer-prompt source — real remaining work); `stop` falls through to `reap` or says so; delete residual boss-side jj now that `cube workspace lease --resume_pr` exists (verify first); completion verifies the remote (branch-ahead/`mergeable`) before marking done; CI-fix revisions check the merge-queue build, not PR-head; cancel reaps the process before freeing the lease so a duplicate is not dispatched into an occupied workspace. Fold the already-filed conflict-worker fix rather than duplicate it.
_Lands in:_ spread across `tools/boss/cli/src/main.rs`, `tools/boss/bossctl/src/main.rs`, `tools/boss/engine/core/src/completion.rs`, `tools/boss/engine/core/src/merge_poller.rs`, and the reviewer prompt source — one subsystem per PR.
_Effort:_ mixed `trivial`-`medium`, one PR each. _Retires:_ ~13 across the batch (named: `feedback_manual_testing_deferrals_exempt_from_reviewer`, `reference_bossctl_dispatch_pause_resume`, `feedback_no_stderr_merge_for_json`, `reference_merge_queue_failures_invisible_at_pr_head`, `reference_worker_resolved_but_never_pushed`, `feedback_doc_only_chores_still_open_pr`, `reference_conflict_worker_vacuous_prerebase_check`). _Depends on:_ none; highly parallel (distinct subsystems).

**T-A11-importer — GitHub issue importer: ingest issues regardless of project membership.**
_Brief:_ the importer enumerates only `projectV2` members, so a worker's `gh issue create` without `--project` is invisible forever. Make it also ingest by repo/label so the escape hatch works. Separable `small` fix; the proposal-API half is the worker proposal API — do not rebuild it.
_Lands in:_ `tools/boss/github_tracker/src/github.rs:436` (`fetch_items`).
_Effort:_ `small`. _Retires:_ 1 (named: `reference_importer_scopes_to_github_project`). The other two A11 memories retire when the worker proposal API lands — tracked there, not here. _Depends on:_ none.

### Deferred / not a v1 blocker

- **Generate mechanical prompt sections from CLI schema (D1).** Revisit after T-prompt-surgery re-measures residual mechanical content. `future`.
- **Prompt amendments on the worker-proposal-API path (D2).** Requires that API to accept "coordinator prompt" as a proposal target; tracked as a dependency note on its design item, not an item here. `future`.
- **Deterministic conflict ladder (0/42 lifetime success).** Named in A14 but is a `large` project of its own; do not fold it into the batch. `future / not a v1 blocker`.
- **App-side pool/lane visibility (B6).** The app surface has the same gap as `agents list`; file it after T-A3-agentslist lands the protocol fields it would render. `future`.

### Parallelism summary

- **Phase 1:** T-B1-doctor and T-A4-lookup run fully parallel; T-A3-agentslist → T-A5-slotbusy serialised (shared `protocol/` files). T-A1-retire, T-A2-retire, and T-A1-ttl are trivial and parallel with everything.
- **Phase 2:** T-A10-fields, T-B2-decision, T-B4-readonly, and T-B3-runbooks (×7) run parallel across distinct subsystems — confirm no schema-migration collision between A10 and B2. T-A10-tooltip follows T-A10-fields. T-prompt-surgery gates on A4 + A5 + A10 **and** the D6 ruling. T-prune gates on all.
- **Phase 3:** T-P3-verify-retire, T-A11-importer, and every item inside T-A14-batch are independent and parallel across distinct subsystems.

> **Note on retirement counts.** The per-item "Retires: N" figures are indicative: some notes are load-bearing for two items, and the named subsets are the files whose subject maps unambiguously onto the item. They do not need to sum to exactly 61. **T-prune is the authoritative reconciliation** — it walks the A/B set against what merged and presents the exact deletion list for approval. Completion is measured there, not by summing these hints.
