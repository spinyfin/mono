# Design: Retire the coordinator's memory — make the defaults teach the right thing

- **Date:** 2026-07-23
- **Project:** `proj_18c50f417ff59af8_160` — Retire the coordinator's memory
- **Execution:** `exec_18c5248567032b88_6` (`project_design`)
- **Source analysis:** full read of all 127 coordinator memory files (2026-07-23), classified against `main@origin` `0dfa2ecb2178`; verified anew for this doc against `main@origin` `3aeba8d7` (see [Verification](#verification-what-changed-since-the-sweep)).
- **Folds existing prior work:** the A1 (lease lifecycle) and A2 (recovery-patch apply) fixes shipped in `#2216`, the already-filed conflict-worker fix, and the worker proposal API + taxonomy read-access design (pointed at, not duplicated — see A11).

## TL;DR

Two-thirds of the Boss coordinator's private memory store (67 of 127 notes) is an unfiled bug report: workarounds for defaults that make the wrong thing easy, and manual procedures that exist only because Boss lacks a verb or a view. This design converts those 67 notes into a concrete, dependency-ordered task list that fixes the defaults and builds the missing surfaces, so the knowledge lives in the product instead of in a notebook that dies with each session. It then defines a **retention policy** that keeps the store from rebuilding itself: memory is for the operator's personal working style and facts about the operator only; anything describing Boss's own behaviour is a defect that must become a work item, not a note.

Re-verification for this doc against `main@origin` `3aeba8d7` found that **much more has already landed than the sweep recorded** — the store contains stale "already fixed?" notes. A1 (lease lifecycle) and A2 (recovery-patch apply) landed in `#2216`, and **the entire Phase-3 defect batch the sweep proposed — A6 (nudge gate), A7 (reviewer parsing + conflict-watch), A8 (doc-link gate), A9 (`uninstall` scoping), A12 (checkleft base), A13 (cube clone/GC) — is already fixed with regression tests.** That collapses roughly a third of the proposed work into _verify-and-retire_ stubs and moves the design's centre of gravity onto the surfaces that genuinely do not exist yet: the diagnostic verb (B1 `doctor`), honest observability (A3 `agents list`), the vocabulary/field fixes that let the prompt shrink (A5, A10, A4), and the durable-knowledge relocation (B2, B3, retention policy). Since this doc was written, 24 of those redundant and verified-fixed notes have been deleted outright — their retirement is already complete, so they are struck from the task list below and from the acceptance count (see [Already retired since this doc was written](#already-retired-since-this-doc-was-written)).

## Goals

- Convert all 67 Category-A (code default / product defect) and Category-B (missing surface) memories into PR-sized, dependency-ordered implementation tasks, each naming the specific memory files it retires so completion is _measurable_, not asserted.
- Fix the defaults so the wrong thing becomes hard to do, rather than documenting the workaround. Every task is scored against the operator's organising question: _"is that actually a prompt thing, or a defaults thing?"_ — a rule asking the coordinator to remember not to trip over a default is a defect in the default.
- Build the missing read/diagnostic surfaces (`bossctl doctor`, honest `agents list`, structured fields, read-only CLI verbs) that today only exist as hand-written decision trees in private memory.
- Cut the coordinator prompt down to the judgement rules code genuinely cannot encode, and remove the ~90 lines that restate CLI shapes the CLI itself knows.
- Establish a **durable knowledge home and a retention/curation policy** so operational runbooks reach workers (repo `AGENTS.md`/`docs/`) and the memory store does not silently rebuild into a second bug tracker.

## Non-goals

- **Not an implementation.** This run delivers the design doc only. No `.rs`/`.ts`/`.swift`/build-file edits; follow-up tasks are filed against this doc after approval.
- **No "banned phrases" or "known caveats" prompt section.** Three memories are already exactly that; one carries a recurrence log proving it does not stick. If a surface induces a wrong model, change the surface.
- **No duplication of the worker proposal API** (worker proposal + taxonomy read-access design). A11 points at it.
- **No Boss-internal mirror of design docs or PR artifacts.** GitHub is source of truth; Boss stores `(repo, path, ref)` only.
- **No further nudge-loop circuit breaker (A6) or doc-link point-patch (A8).** Fourth and sixth attempts respectively; both are scoped root-cause-or-escalate with a live acceptance test, not another throttle or unit-test-only fix.
- **Do not touch the slot model.** Operator-owned, "a project for another day." Slot-adjacent fixes stay tactical and avoid deepening slot coupling.
- **Nothing that reclaims workspaces faster, caps workspace count, or adds a shared bazel cache.** All three explicitly rejected; the last was filed on a wrong premise and deleted.
- **Do not automate memory deletion.** The pruning task presents a list for operator approval; several Category-E entries are flagged "confirm before deleting" for good reason.

## The finding (background)

A full read of all 127 memory files classified every entry:

| Category                                  |   Count | Meaning                                                                                               |
| ----------------------------------------- | ------: | ----------------------------------------------------------------------------------------------------- |
| A — code default / product defect         |      46 | Memory exists because a default or surface makes the wrong thing easy. Fix the code; the memory dies. |
| B — missing tool / missing surface        |      21 | Memory encodes a manual procedure that exists only because Boss lacks a verb, view, or log.           |
| C — genuine behavioural contract          |      24 | Judgement rules no default can enforce. Prompt content.                                               |
| D — personal recall / operator preference |      18 | Legitimately private. Stays as memory.                                                                |
| E — stale / redundant / wrong             |      18 | Already in the prompt, superseded, or a fixed bug. Delete.                                            |
| **Total**                                 | **127** |                                                                                                       |

This project covers **A and B — all 67 items.** C and E inform the prompt-surgery and pruning tasks but are not themselves built.

The organising principle, verbatim from the operator when the coordinator proposed encoding an operating rule into the system prompt:

> "is that actually a prompt thing? It feels like a 'cube should have longer leases by default' thing."

Every task below is scored against that question.

## Verification: what changed since the sweep

The sweep's line numbers were captured at `0dfa2ecb2178`. This doc re-verified the load-bearing claims at `main@origin` `3aeba8d7` via four independent read-only passes. The store is known to contain stale entries, and re-verification confirmed that heavily: **eight of the fourteen Category-A items (A1-A14) are already fixed.** The table below is the authoritative status; the task breakdown folds the fixed ones as verify-and-retire stubs.

| Item                                     | Sweep status   | Verified status at `3aeba8d7`        | Evidence                                                                                                                                                                                                                                                                                                                         |
| ---------------------------------------- | -------------- | ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **A1** lease lifecycle                   | open           | **DONE** (residual: TTL _value_, D5) | `reset_workspace_guarded` refuses reset of dirty `@` → `LeaseExpiredWorkspaceDirty` (`app.rs:5269`); durable quarantine (`app.rs:1651-1735`, reason const `:42`); release preserves unpushed work unless `--force-reset` (`app.rs:5362`)                                                                                         |
| **A2** apply recovery patch              | open           | **DONE**                             | `recovery_apply.rs:319` runs real `git apply --3way`, wired cube-first from dispatch (`coordinator.rs:5278`/`:4564`; `CubeInPlace` skip `:5310`; `mark_patch_consumed :5376`). Six sweep sites now, not five                                                                                                                     |
| **A3** `agents list` honesty             | open           | **OPEN**                             | `LiveWorkerState` has no `pool`, no exec `kind` (`protocol/src/live_worker_state.rs:104-184`); reducer never assigns `model` — stays launch default (`engine/core/src/live_worker_state.rs:171-206`); `activity` has no stale-timer downgrade                                                                                    |
| **A4** work-item lookup                  | open           | **OPEN (minus selector)**            | `task list` omits chore+revision kinds (`engine/core/src/work/workitems.rs:36`); `task show --json` wraps under dynamic `.task`/`.chore` label (`cli/main.rs:4184`). **Chore full-id selector ALREADY FIXED** (`main.rs:3143`, test `:12341`) → that memory moves to Category E                                                  |
| **A5** delete "pool" vocab               | open           | **OPEN**                             | Prompt literal at `BossPaneModel.swift:321` still says "view the cube pool" (only `workspace summary`; no `workspace list`). `SlotBusy` (`engine_app.rs:269`) already carries + logs `occupying_run_id` → that half is largely addressed; the vocabulary rename is the real remaining work                                       |
| **A6** nudge gate                        | open (4th try) | **DONE**                             | `resolve_bound_pr_url` falls back to chain-root PR (`completion.rs:4501-4522`); `CiRemediation`/`RevisionImplementation` with null PR are parked, never nudged (`:1940`,`:1962`); `clear_pending_probes` de-arms on marker signal (`:1611`); regression test `:11636`                                                            |
| **A7** reviewer parsing / conflict-watch | open           | **DONE**                             | `suspected_deletions` is `#[serde(skip_deserializing)]`, derived from `findings` (`pr-review/types.rs:230-231`); parse uses `match` and surfaces errors (`completion.rs:2861`, `parsing.rs:94`). `mergeable=UNKNOWN` → indeterminate, conflict path skipped (`merge_poller.rs:1959`,`:2915`; on-Stop guard `completion.rs:5654`) |
| **A8** doc-link gate                     | open (6th try) | **DONE**                             | Routing + diagnostics now outside the `kind==Design && project_id` gate; `uses_task_doc` branch covers investigations (`completion.rs:3736-3793`, esp. `:3765`,`:3783`)                                                                                                                                                          |
| **A9** `uninstall` scoping               | open           | **DONE**                             | `using_default_install_root = BOSS_INSTALL_ROOT.is_err()` (`cli/main.rs:9801`); refuses to stop the engine when `BOSS_INSTALL_ROOT` is set (`:9857`); test `sandbox_uninstall_does_not_kill_dummy_engine`                                                                                                                        |
| **A10** structured fields + comment verb | open           | **OPEN**                             | No `boss task/chore comment` verb (only `boss comment reply` for the answer-agent, `main.rs:604`); comment engine/app machinery exists to reuse. Effort-provenance + blocked-prose fields still absent                                                                                                                           |
| **A11** importer project-scoping         | open           | **OPEN**                             | Importer enumerates only `projectV2` items (`github_tracker/src/github.rs:436`); a `gh issue create` without `--project` is invisible forever                                                                                                                                                                                    |
| **A12** checkleft `Scenario::Local` base | open           | **DONE**                             | `select_base_local` now prefers `origin/<default_branch>` before the bare branch (`checkleft/src/change_detection/base.rs:298-305`)                                                                                                                                                                                              |
| **A13** cube clone / GC hang             | open           | **DONE**                             | `auto_create_workspace` uses `jj workspace add` on the shared store (`app.rs:4215`); GC fetch is timeout-bounded via `run_jj_network` and the per-repo flock is dropped before network ops (`app.rs:1620`, timeout `:5908`); the GC heartbeat landed                                                                             |
| **A14** `create-revision --no-autostart` | open           | **DONE**                             | Honoured at `app.rs:7287` (`.autostart(!ctx.no_autostart)`); rest of the A14 batch still open                                                                                                                                                                                                                                    |
| **B1** `bossctl doctor`                  | missing        | **OPEN**                             | No `doctor` verb anywhere in `bossctl`. `bossctl dispatch diagnose <exec-id>` exists (`main.rs:259`,`:1034`) and should be extended, not duplicated                                                                                                                                                                              |

**Consequence for the plan:** A1, A2, A6, A7, A8, A9, A12, A13, plus A4's selector and A14's `create-revision`, are fixed. The proposed Phase 3 (independent defect fixes) is therefore almost entirely a _memory-retirement_ exercise, not new engineering. The real remaining build is A3, A4 (list/show), A5 (vocab), A10, A11, the A14 remainder, B1-B4, the prompt surgery, and the retention policy. Items still marked _confirm at HEAD_ in the task breakdown had unconfirmed status at doc-write time (e.g. per-installation permission mode, residual boss-side jj) and the implementer must re-check before coding.

### Already retired since this doc was written

While extracting the verify-then-delete set for this project, the coordinator re-verified against `main@origin` `94c122fe` (2026-07-24) and **deleted 24 memory files outright.** For each, the underlying fix has shipped _and_ the note is gone, so its retirement is already complete: no task should be scheduled to retire it. These 24 are removed from every "Retires: N" count below and from T-prune's reconciliation set — T-prune starts from the remaining, still-present files only and must not re-list any of these.

**Redundant with the checked-in prompt (`BossPaneModel.swift`) — deleted (11):** `feedback_bake_audit_line_into_create`, `feedback_boss_cli_heredoc_for_descriptions`, `feedback_use_short_ids_in_chat`, `project_auto_design_task`, `feedback_reach_for_investigation_tasks`, `feedback_direction_change_on_pr_chore_is_a_revision`, `feedback_parity_ports_verify_against_current_head`, `feedback_chore_scope_fix_not_diagnose`, `feedback_prefer_subagents_for_prework`, `feedback_fork_background_agent_for_repo_prework`, `feedback_no_dispatch_followup`.

**Superseded / fixed-bug notes — deleted (7):** `feedback_bossctl_probe_doesnt_reach_live_workers` (fixed #461/#463), `feedback_notify_live_worker_on_chore_update` (engine propagates on chore update), `feedback_cost_aware_dispatch` (effort/model system shipped), `project_fable_floor_for_design_family` (superseded by the effort/model work), `feedback_chore_update_needs_full_id` (short-id selector shipped; the A4 sub-item was already dropped), `project_no_comment_subcommand` (prompt now covers the description-tag guidance), `reference_bossctl_reap_broken_for_coordinator` (reap fixed).

**Verified-fixed code defects — note deleted, code already merged (6):** `reference_cube_clone_regression_pr126_incident` and `reference_resume_pr_lease_timeout_signature` (A13), `feedback_admin_verbs_must_scope_to_install_root` (A9), `reference_checkleft_push_gate_silently_noops_in_worker_ws` (A12), `reference_review_result_suspected_deletions_type_mismatch` and `reference_review_lane_flap_conflict_watch_unknown_mergeable` (A7). The A9/A12/A13 notes are fully retired; A7 keeps two un-retired notes (see T-P3-verify-retire).

**Still in scope — NOT deleted, retirement gated on live verification or an operator ruling:** A6 (`reference_produce_pr_nudge_loop_diagnostic`, `reference_yellow_parked_idle_nudge_circuit_breaker`) and A8 (`reference_investigation_doc_link_gate_excludes_investigations`, `reference_investigation_doc_link_chronic_regression`) are fixed in code but are 4th-/6th-attempt items and require **live** verification before their notes retire; the A7 ci-rebounce note (`reference_review_lane_flapping_ci_rebounce_diagnostic`) is **not** confirmed fixed and stays open; `feedback_manual_testing_deferrals_exempt_from_reviewer` is **not** done — the carve-out is not present in the reviewer-prompt source; `feedback_no_bypass_permissions` was **slimmed to a personal env fact** (Category D), not deleted, though its per-installation permission-mode code deliverable (part of A14) has shipped; `reference_jj_revision_pr_branch_checkout_recipe` is a still-accurate jj how-to that retires only via a code cleanup, not by deleting the note; `feedback_preferred_workspace_ignored_when_dirty` retires via an operator-approved verify-and-retire stub (overlaps live recovery behaviour). The two prompt-contradiction notes (`feedback_take_the_conn_does_not_persist_implicitly`, `feedback_audit_for_discussion_is_inline_not_a_chore`) are Category-C contradictions awaiting the D6 operator ruling and remain in scope as decisions to resolve.

## Alternatives considered

### Alternative 1 — Keep the memory, improve the prompt (status quo, scaled up)

Leave the defaults alone; move the highest-value memories into the coordinator system prompt and write better caveats. **Rejected.** This is precisely what the store already is, and it has a measured failure record: the "pool" vocabulary has a recurrence log, the nudge-gate workaround was documented across four filed tasks and still fired 20 times in one run, and the `task show` wrapper shape is documented twice yet still tripped. Prose in the prompt does not reach workers, grows without bound, and dies with the session's context. The operator's own framing rejects this: a rule to remember a default is a defect in the default.

### Alternative 2 — Generate the mechanical prompt sections from the CLI schema

Roughly 40% of the 301-line prompt restates CLI shapes (`jq` recipes, selector forms, flag names). Generate those sections from the CLI's own `clap` schema so they cannot drift, leaving only judgement rules hand-maintained. **Partially adopted, deferred as a follow-up (D1).** The generation machinery is real work with its own failure modes (build-time coupling of the Swift app to the Rust CLI schema), and most of the mechanical lines disappear anyway once A4/A10 remove the _reason_ they exist (the CLI stops needing a `jq 'keys'` workaround once `task show` doesn't wrap by kind). So the v1 move is _delete the obsoleted lines by fixing the surfaces_, and revisit schema-generation only for what remains. Captured as a decision, not a v1 blocker.

### Alternative 3 — Chosen: fix the defaults, build the missing surfaces, then shrink the prompt and set a retention policy

Treat each Category-A/B memory as a defect ticket against a default or a missing surface. Fix the code so the wrong model becomes un-thinkable (rename "pool", make `agents list` honest, add the structured fields), build the one high-leverage diagnostic verb (`bossctl doctor`) that retires twelve decision-tree memories at once, migrate runbooks to repo docs where workers can read them, then do the prompt surgery the fixes enable, and finally lock in a retention policy so the store cannot rebuild. This is the design below.

## Chosen approach

The work splits into four movements, executed roughly in phase order but with heavy intra-phase parallelism (see the task graph):

1. **Highest-leverage surfaces and vocabulary (Phase 1).** `bossctl doctor` (B1, retires 12); honest `agents list` with pool + exec-kind (A3, retires 7); unified work-item lookup (A4, retires 4-5); delete the "pool" vocabulary and give `SlotBusy` a `slot_id` (A5, retires 3). A1/A2 are already landed and only need their memories retired.
2. **Structured fields, then prompt surgery (Phase 2).** First-class fields for effort provenance, blocked-reason prose, and a `boss <kind> comment` verb (A10) — the single biggest prompt-shrinker. Then the prompt surgery those fields and A4/A5 enable: delete ~90 lines of CLI-defect documentation, add the surviving Category-C rules, resolve the contradictions. Then the decision-record surface (B2) and the repo-docs migration (B3), and finally the operator-approved memory-pruning pass.
3. **Independent defect fixes (Phase 3) — mostly already landed, and partly already retired.** A6, A7, A8, A9, A12, A13 are verified fixed. The A9/A12/A13 notes and two of A7's four notes have since been deleted outright (retirement complete — see [Already retired](#already-retired-since-this-doc-was-written)); A6, A8, and the A7 ci-rebounce note stay as _live_-verification-gated stubs. The only genuinely open Phase-3 engineering is A11 (importer project-scoping) and the A14 remainder (each a small, independent PR).
4. **Retention policy (cross-cutting, lands with Phase 2 prompt surgery).** The curation rule that keeps the store from rebuilding, plus the durable-knowledge home decision.

The precise, PR-sized decomposition — with the memory files each task retires and the dependency edges — is the [Proposed implementation task breakdown](#proposed-implementation-task-breakdown). The rest of this section resolves the six decisions the design task must land.

### The six decisions

**D1 — Should the coordinator prompt be generated from the CLI schema?**
_Decision:_ Not in v1; delete the obsoleted lines by fixing the surfaces first (Alternative 2). Most mechanical lines exist only because a surface is wrong; A4 and A10 remove the _reason_ for the `jq 'keys'` and tag-composition recipes. After the surgery, re-measure what mechanical content remains and file schema-generation as a separate design if it is still more than a handful of lines. Recorded as a `future / not a v1 blocker` entry.

**D2 — Is there a mechanism to propose a prompt amendment?**
_Decision:_ Ride on the worker proposal API. Prompt amendments are semantically the same act as a worker proposing a taxonomy write: a change the worker cannot make directly that an operator must approve. Do not build a parallel path. This design adds one requirement to that API's scope as a _dependency note_, not a duplicate task: the proposal target enum must be able to name "coordinator prompt" as a proposable artifact. Flagged as an open question for the proposal-API owner (see Risks).

**D3 — Where does durable operational knowledge live, and who can read it?**
_Decision:_ Repo `AGENTS.md` / `docs/` is the home for anything a **worker** needs (runbooks, build-toolchain fixes, deploy topology). Coordinator memory is invisible to workers and must stop being a runbook store. The seven stranded runbooks (B3) migrate to the relevant repo, one chore each, and the memory is deleted only after the doc lands and is confirmed readable. There is no automated sync between memory and repo docs — the retention policy (D4) prevents the divergence at the source by keeping behavioural knowledge out of memory entirely.

**D4 — Retention/curation policy for coordinator memory going forward.**
_Decision, adopt the proposal:_ **Memory is for the operator's personal working style and facts about the operator only. Anything describing Boss's or cube's behaviour is a bug report and must become a work item (or a repo doc), never a note.** This is added to the coordinator prompt as a short Category-C rule (it is a judgement rule code cannot enforce) during the Phase-2 surgery. The pruning task (Task 15) is the one-time application; the rule is what makes it stick. Category-D (personal recall) and legitimately-private operator preferences remain.

**D5 — Correct cube lease TTL and release policy.**
_Decision:_ The correctness half is already solved (dirty state survives expiry via guard+quarantine+preserve). The remaining question is the _value_. Standing direction is to bias toward keeping dirty state reachable, so: **hold the lease until the execution reaches a terminal state, with a long wall-clock backstop (proposed 4h) rather than 30m.** Since a dirty workspace is now preserved-and-quarantined rather than reset, a longer TTL costs only a slower reclaim of _clean_ idle workspaces, which are cheap to re-create. This is a one-line const change plus a heartbeat-cadence review, filed as a `trivial` task (A1-residual). The operator should confirm the 4h backstop figure (open question).

**D6 — Resolve the prompt contradictions.** These need the operator's ruling, surfaced in the attentions manifest:

- _Take-the-conn persistence._ Prompt says the mode persists until explicitly revoked; a later memory quotes the operator saying the opposite. Needs a yes/no ruling; the prompt-surgery task applies whichever wins.
- _3-call tripwire vs "ground this discussion."_ Prompt says any investigation needing a third tool call moves to a background agent; a memory records the operator saying an investigation chore was not needed. Reconcilable (a subagent is not a chore), but the prompt must say so explicitly. The surgery task adds the distinction.
- _Investigation doc-link affordance._ The prompt asserts an affordance the engine gate (A8) provably cannot produce. The surgery task removes the false claim; A8 makes it true for real.

### Acceptance criterion for the project

The project is complete when **every one of the 67 Category-A/B memory files named in the task breakdown has been retired** — either because the task that retires it has merged and the memory was deleted in the operator-approved pruning pass, or because re-verification showed the memory was already stale (moves to Category E). Each task below lists its retired files explicitly; the pruning task (Task 15) is the reconciliation that checks the list is exhausted. "Done" is thus countable against 67, not asserted. Six of those 67 are **already retired** (the verified-fixed A7/A9/A12/A13 code-defect notes deleted on 2026-07-24; see [Already retired](#already-retired-since-this-doc-was-written)), so the remaining count to reconcile is smaller; the pruning pass must treat those six as done and not re-schedule them.

## Risks / open questions

- **A6 and A8 verified fixed, but they were 4th- and 6th-attempt items and prior attempts shipped test-only while live behaviour did not change.** The current code has the right shape _and regression tests_, so T-P3-verify-retire treats them as done — but the verifier must confirm live, not just from green unit tests. If live behaviour still contradicts (a nudge loop still fires on a marker-less compliant reply; an investigation still shows no doc link in the running app), re-open that single item as a root-cause-or-escalate task with a live acceptance test. This is the one place the "already fixed" verdict carries residual risk.
- **D2 depends on the worker-proposal-API owner accepting "coordinator prompt" as a proposal target.** If they decline, prompt amendments keep the file-a-chore loop and D2 becomes a no-op. Surfaced in the attentions manifest.
- **D5 backstop value (4h?)** is a guess pending operator confirmation.
- **D6 contradictions** need operator rulings before the prompt-surgery task can be written; they gate Task 8. Surfaced in the attentions manifest.
- **The `model` field on `agents list` (A3)** may be unfixable if the source is genuinely unavailable at render time. The instruction stands: if it cannot be made authoritative, delete the field rather than ship a lie.
- **Several "already fixed?" memories** (chore full-id selector in A4; per-installation permission mode in A14; residual boss-side jj in A14) had unconfirmed status at doc-write time. Each implementer must re-verify at HEAD before coding; if already fixed, the item collapses to a memory-retire only.

## Proposed implementation task breakdown

Tasks are listed in dependency order. Effort hints: `trivial | small | medium | large`. "Retires" names the memory-file count each task lets the pruning pass delete. Tasks at the same depth with no file overlap may run in parallel (noted).

Two tasks are already landed and appear first as verify-and-retire stubs so their memories are accounted for in the acceptance count.

### Phase 1 — surfaces and vocabulary (mostly parallel)

**T-A2-retire — Confirm recovery-patch apply landed; retire memories.**
Scope: `recovery_apply.rs` + cube-first wiring (`coordinator.rs:5278`) are on `main` via `#2216`. Confirm behaviour with the existing tests, then mark the 2 A2 memories for deletion in the pruning pass. No code.
Effort: `trivial`. Retires: 2. Depends on: none.

**T-A1-retire — Confirm no-reset-when-dirty landed; retire memories; open TTL-value follow-up.**
Scope: the guard+quarantine+preserve behaviour (`app.rs:5269`, `:1651-1735`, `:5362`) is on `main`. Confirm, mark the 4 A1 memories for deletion. Separately, file the TTL-value change (D5) as its own trivial task below. No code here.
Effort: `trivial`. Retires: 4. Depends on: none.

**T-A1-ttl — Raise `DEFAULT_LEASE_TTL_SECS` and review heartbeat cadence.**
Scope: change the const at `tools/cube/src/app.rs:36` to the operator-confirmed backstop (proposed 4h), audit the three apply sites (lease `:990`, heartbeat `:1888`/`:2006`) still make sense, add/adjust a test. Single-file, single-subsystem.
Effort: `trivial`. Retires: 0 (folded into A1's 4). Depends on: operator confirmation of D5 value (attentions).

**T-B1-doctor — `bossctl doctor <work-item-id | exec-id>` signature-matching diagnostic. ★ highest leverage.**
Scope: walk `executions/<id>/dispatch.jsonl`, `engine-trace.jsonl`, and live-status; match the named, mechanically-detectable signatures (`stage_stalled` at `worker_claimed` >30s; `redundant_spawn` at a completed `live_execution_id`; leaked-claim `pool_exhausted` with idle workers; `shell_pid:0` then completion 3ms later; `before_commit_sha==head_sha_at_trigger` rebounce; all-leases-timeout-at-30s). Print the matched signature, the evidence lines, and the known recovery. This is one subsystem (`bossctl`) reading existing artifacts. First check whether `bossctl dispatch diagnose` already covers part of it and extend rather than duplicate (one memory exists only because `diagnose` wasn't found).
Effort: `large`. Retires: 12. Depends on: none. Parallel with all other Phase-1 tasks (distinct files).

**T-A3-agentslist — `agents list`: add `pool` + exec `kind`; make `activity`/`model` honest.**
Scope: add `pool` and execution `kind` to `LiveWorkerState` (`protocol/src/live_worker_state.rs:104-184`) and populate them in the engine reducer (`engine/core/src/live_worker_state.rs:171-206`); make `activity` downgrade on stale `last_event_at` instead of silently showing `spawning` after `events.sock` degrades; make `model` authoritative in the `SessionStart` reducer arm (the hook payload must carry the model id) or **delete the field** if it cannot be. Protocol + engine reducer + `bossctl` rendering. If that spans too far for one PR, split the renderer (`bossctl`) from the protocol/reducer change.
Effort: `medium`. Retires: 7. Depends on: none. **File overlap with T-A5-slotbusy** (both touch `protocol/`): serialise — A3 first, A5 forward-ports.

**T-A4-lookup — Unify work-item lookup: `task list` must not omit kinds; `show` must not wrap by kind.**
Scope: `boss task list` must include chore and revision kinds (or return an explicit hint), not silently drop them (`engine/core/src/work/workitems.rs:36` `kind_returned_by_list_tasks`); `task show --json` must not wrap under the row's dynamic `.task`/`.chore` label (`cli/main.rs:4184`) so `.task.status` on a chore stops returning null. CLI list/show handlers + the engine list RPC. **The chore full-id selector is already fixed** (verified `main.rs:3143`, test `:12341`) — that sub-item is dropped; its memory moves to Category E.
Effort: `small`-`medium`. Retires: 4. Depends on: none. Parallel with B1/A3/A5.

**T-A5-slotbusy — Delete "pool" vocabulary from capacity surfaces.**
Scope: rename/remove "pool" from the `bossctl workspace summary` output and any bounded-looking rendering so a fully-leased list stops reading as a fixed exhaustible resource. Does **not** edit the Swift prompt literal (that is Task 8, which removes the "view the cube pool" line at `BossPaneModel.swift:321`). Note: the `SlotBusy` half is largely already addressed — the variant (`protocol/src/engine_app.rs:269`) already carries and logs `occupying_run_id` in `dispatch.jsonl`; only add an explicit `slot_id` echo if the diagnostic still can't identify the squatting pane. Protocol/bossctl.
Effort: `small`. Retires: 3. Depends on: T-A3-agentslist (shared `protocol/` files — forward-port A3's changes preservingly).

### Phase 2 — structured fields, prompt surgery, knowledge home (ordered)

**T-A10-fields — First-class fields: effort provenance, blocked-reason prose, `boss <kind> comment` verb.**
Scope: add structured columns/fields for (a) effort-classification provenance and (b) a blocked-reason prose field surfaced as a tooltip (separate from the short pill), and add a `boss <kind> comment` CLI verb over the existing engine/app comment machinery (`engine/core/src/app/comments.rs`, `app-macos/Sources/Comments/*.swift` — reuse, do not rebuild). This removes the free-text `[effort-*]` tag-stuffing that races the autostart worker. Protocol + engine + CLI; if the app-side tooltip render is separable, split it as T-A10-tooltip.
Effort: `medium`. Retires: 4. Depends on: none functionally, but sequence after Phase 1 so the prompt surgery has all its inputs.

**T-A10-tooltip — App: surface blocked-reason prose as a tooltip.**
Scope: render the new prose field (from T-A10-fields) as a hover tooltip in the app, distinct from the title-cased pill. App-macos only.
Effort: `small`. Depends on: T-A10-fields.

**T-prompt-surgery — Coordinator prompt surgery. (Task 8)**
Scope: edit the Swift string literal in `tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift` (**not** the runtime `CLAUDE.md`, which the app rewrites on launch): delete the ~90 lines of CLI-defect documentation made obsolete by A4/A5/A10; add the surviving Category-C judgement rules; add the D4 retention rule; resolve the D6 contradictions per the operator's rulings. Single file, single subsystem.
Effort: `medium`. Depends on: T-A4-lookup, T-A5-slotbusy, T-A10-fields (their fixes must land before the lines they obsolete are deleted); **D6 operator rulings** (attentions manifest). Cannot start until those merge.

**T-B2-decision — Decision record: "considered and declined / operator-owned".**
Scope: a `wontfix`/`decided` state on work items, or a lightweight `boss decision` record attached to a product, surfaced when filing something semantically near it. Engine + CLI (+ minimal app surface).
Effort: `medium`. Retires: 6. Depends on: none; parallel with T-A10-fields (distinct subsystem area — confirm no schema-migration collision, otherwise serialise the migrations).

**T-B3-runbooks — Migrate 7 runbooks to repo `AGENTS.md`/`docs/`.**
Scope: one chore per runbook (bazel/Xcode LaunchServices resolution; TestFlight codesign keychain hang; Buildkite queue push-auth; flunge deploy topology + `/statusz`; where checkleft lives; `LINT.IfChange` markers; checkleft-sandbox repo). Land each as repo doc content in the relevant repo, confirm worker-readable, then the memory is deleted. Docs-only.
Effort: `small` each (7 tasks). Retires: 7. Depends on: D3 (settled here); parallel across repos.

**T-B4-readonly — Missing read-only CLI verbs.**
Scope: CLI verbs for comments, answer-agent runs, execution history, and by-exec lookup so the coordinator never falls back to raw `sqlite3` on `state.db`. Engine read RPCs + CLI. Fold the already-filed read-only-verbs work rather than duplicate.
Effort: `medium`. Retires: 2. Depends on: none; parallel with B2/B3.

**T-prune — Memory pruning pass (operator-approved). (Task 15)**
Scope: reconcile the full 67-file A/B list plus the Category-E deletions against what has merged; present the deletion list for operator approval; do **not** automate deletion. **24 files are already retired** (the 18 Category-E redundant/superseded notes plus the 6 verified-fixed A7/A9/A12/A13 code-defect notes — see [Already retired](#already-retired-since-this-doc-was-written)); this pass starts from the remaining, still-present files and must not re-list any of the deleted ones. This is the acceptance-count check.
Effort: `small`. Depends on: **everything above** (a memory is only retired once its task merged).

### Phase 3 — independent defect fixes (mostly already landed)

**T-P3-verify-retire — Live-verify the still-gated Phase-3 fixes; retire only their memories.**
Scope: A9 (`uninstall` refuses the global default when `BOSS_INSTALL_ROOT` set, `cli/main.rs:9801`), A12 (checkleft `select_base_local` prefers `origin`, `base.rs:298`), and A13 (`jj workspace add` + timeout-bounded GC fetch, `app.rs:4215`/`:1620`) are confirmed fixed **and their notes are already deleted** (see [Already retired](#already-retired-since-this-doc-was-written)) — nothing left to do for them. Two of A7's four notes (`suspected_deletions` type-mismatch, conflict-watch `UNKNOWN`→indeterminate, `merge_poller.rs:1959`) are likewise fixed and already deleted. What remains is a **live**-verification gate, not a mark-and-prune: A6 (nudge gate: chain-root PR resolution + park guards + `clear_pending_probes`, `completion.rs:4501`/`:1611`, test `:11636`) and A8 (doc-link routing outside the gate, `completion.rs:3736-3793`) have the right code shape _and_ regression tests but are 4th-/6th-attempt items with a history of test-passes-but-app-fails, so their notes (`reference_produce_pr_nudge_loop_diagnostic`, `reference_yellow_parked_idle_nudge_circuit_breaker`; `reference_investigation_doc_link_gate_excludes_investigations`, `reference_investigation_doc_link_chronic_regression`) retire only after confirming live in the running app. The A7 ci-rebounce signature (`reference_review_lane_flapping_ci_rebounce_diagnostic`) is **not** confirmed fixed and stays open — its note is not retired here. **No code** unless a live check contradicts, in which case re-open that single item as a root-cause-or-escalate task with a live acceptance test.
Effort: `small`. Retires: 4 (A6×2, A8×2 — each only after live confirmation; the A7/A9/A12/A13 fixed-defect notes are already deleted and the A7 ci-rebounce note stays open). Depends on: none.

**T-A14-batch — Smaller defects, filed individually (one PR each, not one row).**
Scope: each is its own trivial/small PR, and each must be re-verified at HEAD first (several may already be fixed like `create-revision --no-autostart` was, `app.rs:7287`): a cancel/prune verb (no `--status cancelled` today); `bossctl dispatch` group help exposes the mutating `pause`/`resume` verbs; stamp a real `engine_build_sha` into live-status (bazel zeroes mtimes); quieten wrapper stderr chatter that breaks `2>&1 | jq`; `create-revision` warns when the PR's producing worker is still live; reconcile the `AGENTS.md`-says-push-docs-to-main vs runtime-soft-blocks disagreement; move the manual/GUI-testing deferral policy to the **reviewer** prompt (**NOT done**: the carve-out is not present in reviewer-prompt source, so `feedback_manual_testing_deferrals_exempt_from_reviewer` stays live and this is real remaining work); per-installation permission-mode (**DONE**: the per-installation permission-mode setting shipped, so the "ship the setting" deliverable is satisfied; the `feedback_no_bypass_permissions` note was slimmed to a personal env fact, not retired); `stop` falls through to `reap` or says so; delete residual boss-side jj now that `cube workspace lease --resume_pr` exists (verify); completion verifies the remote (branch-ahead/`mergeable`) before marking done (B5 half); CI-fix revisions check the merge-queue build not PR-head; cancel reaps the process before freeing the lease so a duplicate isn't dispatched into the occupied workspace. Fold the already-filed conflict-worker fix rather than duplicate.
Effort: mixed `trivial`-`medium`, one PR each. Retires: ~13 across the batch. Depends on: none; highly parallel (distinct subsystems).

**T-A11-importer — GitHub issue importer: ingest issues regardless of project membership.**
Scope: the importer enumerates only `projectV2` members (`github_tracker/src/github.rs:436`), so a worker's `gh issue create` without `--project` is invisible forever. Make it also ingest by repo/label so the escape hatch works. **Separable `small` fix; the proposal-API half is the worker proposal API — do not rebuild it.**
Effort: `small`. Retires: 1 (the importer half; the other 2 A11 memories retire when the worker proposal API lands — tracked there, not here). Depends on: none.

### Deferred / not a v1 blocker

- **Generate mechanical prompt sections from CLI schema (D1).** Revisit after T-prompt-surgery re-measures residual mechanical content. `future`.
- **Prompt amendments on the worker-proposal-API path (D2).** Requires that API to accept "coordinator prompt" as a proposal target; tracked as a dependency note on its design task, not a task here. `future`.
- **Deterministic conflict ladder (0/42 lifetime success).** Named in A14 but is a `large` project of its own; do not fold into the batch. `future / not a v1 blocker`.
- **App-side pool/lane visibility (B6).** The app surface has the same gap as `agents list`; file after T-A3-agentslist lands the protocol fields it would render. `future`.

### Parallelism summary

- **Phase 1:** T-B1-doctor, T-A4-lookup run fully parallel; T-A3-agentslist → T-A5-slotbusy serialised (shared `protocol/` files). T-A1-retire/T-A2-retire are trivial confirmations, parallel with everything.
- **Phase 2:** T-A10-fields, T-B2-decision, T-B4-readonly, T-B3-runbooks(×7) run parallel (distinct subsystems; confirm no schema-migration collision between A10 and B2). T-A10-tooltip after T-A10-fields. T-prompt-surgery gates on A4+A5+A10 **and** D6 rulings. T-prune gates on all.
- **Phase 3:** T-P3-verify-retire, T-A11-importer, and every item inside T-A14-batch are independent and parallel across distinct subsystems.

> **Note on retirement counts.** The per-task "Retires: N" figures are indicative and inherit the sweep's own ranges (e.g. A4 "4-5", A14 "~13"); some overlap and some items already moved to Category E on re-verification. They do not need to sum to exactly 67. **T-prune is the authoritative reconciliation**: it walks the full 67-file A/B list against what merged and presents the exact deletion set for approval. Completion is measured there, not by summing these hints.
