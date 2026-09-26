# Validation exit criteria and the test infrastructure to satisfy them

- **Date:** 2026-07-28
- **Product:** Boss
- **Project:** Validation exit criteria and the test infrastructure to satisfy them (`proj_18c69b165e60a088_4a`)
- **Status:** proposed — design only, no implementation in this PR
- **Related designs:** [`test-instance-isolation.md`](test-instance-isolation.md), [`agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md`](agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md), [`automated-reviewer-pass-on-every-agent-authored-pr.md`](automated-reviewer-pass-on-every-agent-authored-pr.md), [`engine-dispatch-instrumentation.md`](engine-dispatch-instrumentation.md), [`worker-screenshot-evidence-attachments.md`](worker-screenshot-evidence-attachments.md)

## TL;DR

Boss does not have a unit-testing problem — it has an **evidence** problem and a **seam** problem. The system-test substrate largely exists already (an in-process engine on a real socket, a hermetic Bazel sandbox, a driver trait with a conformance harness). What is missing is a fake outside world, a way to prove validation claims from tool-produced evidence, and any notion of test tiers.

This design proposes: four validation modes, of which a non-trivial work item declares **one or more at filing time** and evidences at PR time; automatic evidence adapters for tools such as Bazel, with an **engine-recorded** command wrapper where no native evidence exists; a `testkit` crate that brings up a fully isolated Boss instance with a scripted fake agent driver, fake `gh`, and fake `cube`; Bazel test tiers expressed as tags with a separate CI lane; and driveable/capturable remote control of the macOS app.

## Method

The first deliverable of the parent project was an inventory before design. Everything in §1 was read from the tree at `main` (workspace HEAD, `7599e205`). Where this document asserts a gap, it names the file that has it.

---

## 1. Inventory: what actually exists today

### 1.1 What is already good — and load-bearing

| Capability                                                          | Where                                                                                                            | Notes                                                                                                                                                                                                                                                                                           |
| ------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| In-process engine on a real Unix socket, temp DB, torn down on drop | `tools/boss/engine/core/tests/common/mod.rs` (`TestEngine`)                                                      | This **is** a system-test harness. 145 lines. Supports on-disk DB, control token, injectable `MergeProbe`.                                                                                                                                                                                      |
| A second, near-duplicate copy of the same harness                   | `tools/boss/cli/tests/harness/mod.rs`                                                                            | Independently evolved. Duplication is a finding, not a feature.                                                                                                                                                                                                                                 |
| Hermetic test sandbox with a PATH boundary                          | `tools/test-sandbox/` + `.bazelrc`                                                                               | `test --run_under=//tools/test-sandbox:hermetic_test_wrapper` strips host PATH at the user-code boundary, so test code **cannot** discover `gh`, `bk`, `codex`, `claude`, or `cube`. Network denied by default; opt-in via `network_enabled_rust_test`. Credential env vars explicitly blanked. |
| Test-instance isolation guard                                       | `tools/boss/engine/core/tests/isolation_guard.rs`, `engine/core/src/app/isolation.rs`                            | `IsolationPaths::derive` relocates DB, events socket, pid file and control token off a non-default `--socket-path`, and `ensure_isolated` refuses to start if any path still lands on production. Integration-tested.                                                                           |
| Agent-driver abstraction                                            | `tools/boss/engine/driver/` — `AgentDriver` trait, `DriverRegistry`, `CapabilitySet`, `CapabilityResolver`       | Drivers describe their own capabilities. `DriverRegistry::default().slugs()` already enumerates the matrix.                                                                                                                                                                                     |
| Reference-driver conformance harness                                | `engine/core/src/conformance/`                                                                                   | Claude byte-for-byte goldens (`settings.json`, `CLAUDE.md`), stdout-vs-hook ingress equivalence, turn-boundary equivalence, Codex version pinning, native-dialect transcript normalize that **fails closed when a driver is registered without a fixture**.                                     |
| Cube behind a trait                                                 | `engine/core/src/cube_commands.rs` — `CubeJsonTransport`                                                         | Already fakeable in-process. `BOSS_CUBE_CMD` relocates the binary.                                                                                                                                                                                                                              |
| macOS app headless capture                                          | `app-macos/Sources/BossCapture.swift`                                                                            | `--capture-to <png>`; refuses to run unless `BOSS_SOCKET_PATH` points at a non-production socket.                                                                                                                                                                                               |
| Bazel sharding, sized tests                                         | `engine/core/BUILD.bazel`                                                                                        | `engine_lib_test` runs `shard_count = 11` off a rules_rust backport patch. macOS tests split across four `macos_unit_test` targets.                                                                                                                                                             |
| Schema migration tests                                              | `engine/core/src/work/{migrations_a,migrations_b,migrations_boothby}.rs`, `work/tests/schema_migration_tests.rs` | Migrations are tested; the _invariants_ are not snapshotted (see §1.2).                                                                                                                                                                                                                         |
| Forensic dispatch-event stream                                      | `engine/dispatch-events/`, per-execution mirror at `<root>/executions/<id>/dispatch.jsonl`                       | Rich enough to reconstruct failures after the fact.                                                                                                                                                                                                                                             |

**Post-inventory update:** [PR #2621](https://github.com/spinyfin/mono/pull/2621) landed on 2026-08-08 and added `boss attach`: workers can submit PNG/JPEG evidence to an engine-owned, content-addressed store and put the resulting per-work-item gallery URL in the PR. The chosen approach below treats that as existing infrastructure rather than proposing another screenshot store.

**138 `rust_test` targets, 9 `macos_unit_test`, 3 `sh_test`.** Of the Rust targets, ten in `engine/core` are integration-style (`tests/*.rs`); the rest are `#[cfg(test)]` unit tests. So system testing is not absent — it is **unowned, unnamed, and unenforced**.

### 1.2 The gaps, mapped to the six failures

| Failure from the brief                                                    | Root gap                                                                                                                                                                                                                                                                      | Evidence in tree                                                                                |
| ------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| Worker self-reported a truncated `checkleft run` as passing               | The command invocation yielded only partial output and a live session handle; no terminal result containing an exit code was ever observed, but the worker later claimed the check passed. Nothing in the tree records argv + status + output for a claimed validation step.  | — (absence)                                                                                     |
| `worker_blocked` attention items invisible for two months                 | **Writes are tested; reads are not.** `completion/tests/t02.rs` asserts "exactly one `worker_blocked` attention item must be filed" — by inspecting the store. Nothing asserts a _client's actual query_ returns it.                                                          | `engine/core/src/completion/tests/t02.rs:1084`                                                  |
| Crash-recovery apply never ran (flag defaulted false on one of two paths) | No multi-path coverage requirement; no test drives both dispatch paths. `BOSS_RECOVERY_DIR` is process-global, which forces the recovery tests into an awkward serialised shape.                                                                                              | `engine/core/src/coordinator_tests/recovery.rs:8`                                               |
| Dispatch sink corrupted records under concurrency                         | **Still present.** `JsonlFileSink::append_line` writes body and newline as two separate `write_all` calls on an `O_APPEND` fd, and `emit` holds no lock. Two concurrent emits can interleave a body between another record's body and its newline.                            | `engine/dispatch-events/src/lib.rs:783-791`                                                     |
| Prompts named tools the driver lacks; hardcoded developer directory       | Prompt composition has 2208 lines of tests — but they are `assert!(prompt.contains(…))`. Substring assertions cannot catch content that is _present and wrong_, and there is **no golden for the composed task prompt** (goldens cover only `settings.json` and `CLAUDE.md`). | `engine/core/src/runner/prompt/compose_prompt_tests.rs`, `engine/core/src/conformance/goldens/` |
| Push gate laundered a guess into a root-cause assertion                   | Diagnostic strings are not treated as a tested contract.                                                                                                                                                                                                                      | — (absence)                                                                                     |

### 1.3 Additional gaps found

- **No Bazel test tags at all.** `grep 'tags = ' --include=BUILD.bazel` over `tools/` and `lib/` returns nothing; 84 of 84 sized targets are `size = "small"`. There is no tier vocabulary, no tag filter, and therefore no way to run "just the fast ones" or "just the slow ones".
- **One CI lane does everything.** `.buildkite/pipeline.yml` has `bazel-build-test`, `mac-app-build`, `checks`. A slow or flaky system test is indistinguishable from a broken unit test at the commit-status level.
- **No flakiness machinery.** No `flaky =`, no `--runs_per_test` lane, no quarantine tag, no owner mapping.
- **`gh` is hardcoded.** `github/src/gh_runner.rs:289` (`fn gh_command`) does `Command::new("gh")` with no env override. It is a single funnel — one seam to add — but today it does not exist. The hermetic wrapper means tests simply cannot reach `gh` at all; they cannot reach a _fake_ one either.
- **No fake agent driver.** There is no `ScriptedDriver`/`FakeDriver` in the registry. Every behavioural test of the spawn→pane→hook→completion path therefore stops short of the driver boundary or stubs above it.
- **No app remote control beyond a single screenshot.** `--capture-to` renders one frame of whatever the app opens with. There is no way to navigate to a screen, select a work item, open a viewer, or dump view state.
- **169 `FrontendRequest` variants** with no wire-golden corpus. The engine↔app contract is large and its compatibility is untested.

### 1.4 Answering the operator's four concerns against the inventory

1. **How do we write system tests?** We already do — `TestEngine` + `BossClient` over a real socket. What's missing is a name, a tier, and a fake outside world.
2. **Integration vs system — same thing?** No, and the distinction is worth keeping. This design defines **system test = whole system, one process, real wire**; **integration test = multiple real processes/binaries**. RPC-replay is a _third_, orthogonal technique (contract capture), useful for testing the Swift app against real engine output without an engine.
3. **Build time / flakiness?** Partly solved by accident: the existing integration tests are `size = "small"` and fast because they are in-process. The answer is to keep it that way by construction (fakes, not real subprocesses, for the default lane) and put genuinely slow things behind tags in a separate CI lane.
4. **Remote control?** Engine: essentially solved (169-verb socket + isolation guard). App: barely started (`--capture-to` only). Drivers: not started (no fake).

---

## 2. Goals

- Every non-trivial work item **declares**, at filing time, one or more validation requirements it must satisfy.
- A validation claim is backed by **machine-collected evidence**, not by a sentence in a PR body.
- Boss can bring up a **fully isolated instance** — engine, app, fake externals, fake agent driver — from one command, in a test or on a developer's machine, with nothing leaking into production state.
- The default CI lane stays fast; slower and riskier tests live in a named tier with their own lane and their own flakiness accounting.
- **Token cost is a first-class constraint**: no test in any per-PR lane spawns a real model.
- The six failure classes in the brief are each caught by a specific, named mechanism in this design (see §5.14).

## 3. Non-goals

- **Back-filling exit criteria onto already-filed work.** Explicitly out of scope per the project brief.
- **Replacing unit tests.** The `unit` mode remains the right requirement for most changes; the point is to stop it being the _only_ requirement available.
- **A coverage mandate.** See §4.2 for why coverage is rejected as the gate.
- **Testing against real GitHub, real Buildkite, real cube, or real models in any per-PR lane.** A nightly live-smoke lane is proposed as deferred scope, not v1.
- **Re-designing test-instance isolation.** [`test-instance-isolation.md`](test-instance-isolation.md) already proposes `BOSS_PROFILE` and a path resolver. This project _consumes_ that; it does not re-litigate it.
- **A general-purpose UI automation framework.** The app needs a scriptable capture mode, not XCUITest.
- **Changing how the reviewer LLM works.** This design gives the existing review lane new machine-checkable inputs; it does not change the reviewer's architecture.

## 4. Alternatives considered

### 4.1 PR-body declaration only, enforced by the reviewer LLM

The cheapest option: require a `## Validation` section, let the automated reviewer judge it.

**Rejected.** This is self-report with extra steps, and it reproduces the exact failure in the brief — a worker wrote "Passed `checkleft run`" and the reviewer had no way to know the command was truncated. An LLM reading a PR body cannot distinguish a true claim from a plausible one. The mechanism must make the claim _checkable against something the worker did not author_. A `## Validation` section is still part of the chosen design — but as a pointer to evidence, never as the evidence.

### 4.2 Coverage thresholds as the gate

Instrument with `cargo-llvm-cov` (or a Bazel coverage lane), require N% on changed lines.

**Rejected.** Every one of the six failures had **covered lines**. The dispatch sink's `append_line` is executed by tests. The `worker_blocked` filing path is covered. The prompt composer is covered by 2208 lines of assertions. Coverage measures _execution_, not _observation_ — it cannot tell you that the assertion was `contains()` rather than a golden, or that nobody ever read the row back. Coverage also imposes real cost on a Bazel repo (instrumented builds thrash the disk cache) for a signal that would not have moved on any of the motivating defects. Worth having as a diagnostic later; wrong as a gate.

### 4.3 End-to-end tests that spawn real agent workers

Let the system test spawn a real `claude`/`codex` process against a scratch repo and assert the outcome.

**Rejected.** Nondeterministic by construction, unbounded in wall-clock, and directly at odds with the brief's statement that token cost is a first-class constraint. A per-PR lane that costs money per run is a lane that gets disabled. The scripted fake driver (§5.4) gives the same _path_ coverage — the real spawn flow, real pane wiring, real hook ingress, real completion parsing — with the model replaced by a replayed script. A small real-agent smoke lane is proposed as **deferred**, nightly, and non-blocking.

### 4.4 Deterministic-scheduler simulation (loom / madsim / turmoil) for the engine

Run the whole engine under a simulated runtime with controlled interleavings.

**Rejected for v1.** The engine's I/O is real Unix sockets, real SQLite, and `tokio::process` — all of which would have to be rewritten against a runtime shim before any of these tools apply. `loom` in particular does not model file-descriptor-level races like the `O_APPEND` double-write, which is the actual bug we have. Targeted concurrency hammering plus fault injection at existing trait seams (§5.7) catches that class at a fraction of the cost. Revisit if a race appears that hammering cannot reproduce.

### 4.5 A test-only "mock mode" flag threaded through production code

Add `if cfg.test_mode { … }` branches so production paths can be short-circuited in tests.

**Rejected, emphatically.** The crash-recovery failure _was_ a path-dependent flag default: one of two dispatch paths silently defaulted a flag to false, and the recovery apply path never ran in production for 108 patches. Adding more test-only branches multiplies precisely that failure mode. **The invariant for this design: fakes substitute at existing trait or process boundaries, and the code path under test is byte-for-byte the production path.** The scripted driver is a registered driver, not a bypass. The fake `gh` is a different binary, not a different branch.

### 4.6 Wrapper-only command recording vs tool-native evidence

One option is to require every validation command to run through `boss validate run`. It gives Boss a uniform record for any executable and captures the terminal exit status even when the worker transcript is truncated. Its weakness is behavioural: workers will sometimes forget the wrapper. Rejecting an unrecorded claim catches the omission, but only after spending a review cycle on avoidable process failure.

The alternative is to consume diagnostics the tool already produces. Bazel's Build Event Protocol and test-result artifacts can identify the invocation, selected targets, and outcomes without any special worker behaviour. The current products using Boss are Bazel workspaces, so supporting Bazel first is useful rather than speculative coupling. The costs are that this is build-system-specific, local Bazel logs need a configured durable destination rather than relying on an overwritten `command.log`, and tools such as `checkleft` do not yet expose an equivalent receipt.

**Chosen: an adapter-based hybrid.** An evidence-provider interface lets Boss import tool-native records automatically, with a Bazel provider first. CI already guarantees that affected Bazel test targets are selected, so the validation declaration names the target and criterion while the provider verifies the PR-head CI result; the worker does not separately attest that the target "ran." `boss validate run` remains the generic fallback for commands with no native provider. This keeps the engine adaptable beyond Bazel while removing the forgettable wrapper from the common Bazel path.

---

## 5. Chosen approach

### 5.1 The taxonomy and where the declaration lives

Four modes, represented as a collection of `validation_requirements` on the work item (child rows in the database, plumbed through `boss-protocol`'s `Task`). Each requirement records a mode and the observable criterion it covers. The three substantive modes are composable: a UI change can require unit coverage for its state transformation, an integration test for engine↔app wiring, **and** a manually inspected screenshot. `exempt` is the only exclusive value and cannot coexist with another requirement.

| Mode        | Meaning                                                                                     | Evidence required                                                                               |
| ----------- | ------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `unit`      | A purely functional criterion is exercised observably by a unit test.                       | Named Bazel target(s), the criterion each covers, and a green PR-head CI result.                |
| `automated` | A criterion is covered by a checked-in system or integration test that fails on regression. | Named Bazel target(s) at tier S or I, the criterion each covers, and a green PR-head CI result. |
| `manual`    | A criterion is not reliably testable by automated means and must be validated by hand.      | A captured artifact that the reviewer can inspect (§5.2).                                       |
| `exempt`    | The entire work item is trivial, so no validation is required.                              | None — but exemption must be explicit and is mutually exclusive with all other modes.           |

**Declared at filing time, by the coordinator.** Filing time is the load-bearing decision; selecting exactly one mode is not. The coordinator authors most briefs and knows what "done" means before a worker starts; discovering the criteria at PR time means discovering them _after_ the design decisions that made them hard to satisfy. Requirements are absent on legacy rows; an empty requirement set on a non-trivial row is a review-lane finding, not a hard engine error.

**Definition of "non-trivial"** — fail closed. A work item is non-trivial **unless** it is one of: documentation/comment-only; a pure rename or move with no behaviour delta; a dependency/lockfile/version bump with no code change; or explicitly filed at `effort: trivial` by a human. Everything else needs at least one substantive validation requirement.

The **`## Validation` PR-body section** remains required — but its role is to _name the evidence_, not to be it. Format:

```
## Validation
Requirements:
- unit — parser rejects a terminal result with no exit code
  - //tools/boss/engine/core:validation_parser_test
- automated — an execution-keyed attention is visible through the coordinator's read API
  - //tools/boss/engine/core:attention_reachability_test
- manual — the app renders the validation requirements and their evidence links
  - Evidence: see this PR's `## Evidence` section (screenshot gallery URL from `boss attach`)
```

### 5.2 Evidence, not assertion

Three evidence channels can be combined just like the requirements. **A PR body claim with no matching evidence record is a review-lane finding.**

**(a) Automatic provider evidence, Bazel first.** Evidence providers automatically import durable diagnostics where a tool exposes them; Bazel is the first provider. The PR body names targets so the reviewer can judge whether each target covers its stated criterion. A checker confirms each label exists and that the applicable PR-head CI lane is green, importing the Build Event Protocol or `bazel-testlogs/**/test.xml` artifacts that the `bazel-build-test` step already uploads when it needs target-level detail. It does not ask the worker to assert that each target ran: the existing affected-target CI machinery owns selection and already guarantees that affected tests execute.

**(b) `boss validate run`: the generic fallback.** For commands with no provider — `checkleft` does not yet expose an equivalent receipt — `boss validate run -- <cmd…>` executes the command, captures argv, full stdout/stderr, exit status, and wall-clock, and writes a JSON record to `<state root>/executions/<execution_id>/validation/<slug>.json`. The worker prompt uses the wrapper only for these uncovered commands. The PR body cites the resulting provider record; the review lane resolves it and reads the **actual exit code**.

This is the direct fix for the truncated-`checkleft` failure described in §1.2: the command returned partial output plus a still-live session handle, the worker never polled that handle to a terminal result with an exit code, and nevertheless reported that `checkleft run` passed. A tool-native receipt or the fallback wrapper makes the terminal result independent of transcript truncation, pane scrollback, and worker interpretation. Absence of any provider record for a claimed command is itself the finding. The wrapper can still be forgotten for a tool without native evidence; that omission becomes a review finding, while the adapter-first design avoids that failure mode for Bazel, the common case.

**(c) Captured artifacts for a `manual` requirement.** PNG/JPEG evidence reuses the shipped [`boss attach` mechanism](worker-screenshot-evidence-attachments.md): the engine validates and stores the image outside the recyclable workspace, associates it with the execution and work item, and returns a gallery URL for the PR's `## Evidence` section. A checker resolves the attachment row rather than trusting the URL alone. Non-image evidence such as an engine RPC trace or a dispatch-event slice remains under `executions/<id>/validation/`, with a small manifest in the existing per-execution forensic directory ([`forensic-surfaces.md`](../forensic-surfaces.md)).

**Who checks:** the automated reviewer already runs on every agent-authored PR. It gains a deterministic pre-pass — a Rust checker that resolves the claims _before_ the LLM sees the diff — so a nonexistent Bazel label, a red PR-head CI result, an absent command record, or a missing attachment is a mechanical finding, not a judgement call.

### 5.3 Test tiers, in Bazel terms

| Tier                | Definition                                                                                                                                 | Bazel                                                         | CI lane                                                         |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------- | --------------------------------------------------------------- |
| **U — unit**        | In-process, no sockets, no subprocesses, tempdir I/O at most.                                                                              | `size = "small"`, no tag                                      | `bazel-build-test` (every PR)                                   |
| **S — system**      | Whole system in one process over the real wire: `TestEngine` on a real UDS, real DB, real driver registry; outside world faked in-process. | `size = "small"`, `timeout = "moderate"`, `tags = ["system"]` | `bazel-build-test` (every PR)                                   |
| **I — integration** | Multiple real processes: the engine binary, `boss` CLI, `boss-event` shim, the app in capture mode. Outside world faked via fake binaries. | `size = "medium"`/`"large"`, `tags = ["integration"]`         | **new** `system-integration` step (every PR, own commit status) |
| **E — exogenous**   | Touches the real network or real host tools. Real-agent smoke, live `gh`.                                                                  | `tags = ["exogenous", "manual"]`                              | nightly only; never blocks a PR                                 |

Two `defs.bzl` macros — `boss_system_test` and `boss_integration_test` — set size, timeout, tags and the hermetic-wrapper interaction in one place, so the tier is declared once and cannot drift. Default `test --test_tag_filters=-exogenous` in `.bazelrc` keeps `bazel test //...` sane for developers.

**Why tier S stays in the fast lane:** the existing integration tests are already `size = "small"` and pass under a 300 s moderate timeout on 6–7× slower Linux CI runners. Keeping S in-process and fake-backed is what preserves that. The rule is structural, not aspirational: **tier S may not spawn a subprocess.** If a test needs one, it is tier I by definition and moves lanes.

**Why tier I gets its own commit status:** so a flake there is attributable, quarantinable, and visibly distinct from a broken unit test — which is precisely what one undifferentiated lane cannot give you.

### 5.4 Faking the outside world

The rule from §4.5 holds throughout: **substitute at an existing boundary; never add a test-only branch to a production path.**

**Agent drivers — the scripted driver.** A new driver registered under slug `scripted`, implementing the real `AgentDriver` trait. Its "worker process" is a tiny in-tree binary that replays a JSONL script: emit these stdout events, call these hooks, write this PR-URL file, exit with this status. Because it is a _registered driver_, everything downstream of `SpawnRequest` — pane wiring, hook ingress, progress normalisation, turn boundaries, completion parsing, PR-URL capture — executes the production path. Deterministic, free, and fast.

Scripts live as fixtures next to the tests. A script can express: a clean run; a `[blocked]` marker; a `[effort-escalation]` marker; a crash mid-turn; a hung turn; a truncated stdout stream. Those are the shapes we keep getting wrong.

The scripted driver's `CapabilitySet` is **configurable per script**, which is what makes it useful for the capability-gate paths rather than just the happy path.

**GitHub.** `github/src/gh_runner.rs:289`'s `fn gh_command` is a single funnel. Add a `BOSS_GH_CMD` override there (matching the existing `BOSS_CUBE_CMD` precedent), and a `fake-gh` binary that serves a JSON fixture state: repos, PRs, check runs, review comments, merge state. Tier S can use the in-process path; tier I points `BOSS_GH_CMD` at the fake binary. Buildkite is only ever read through check runs, so the `gh` fake covers it — no separate `bk` fake is needed for v1.

**cube.** `CubeJsonTransport` is already a trait, so tier S fakes it in-process today. Tier I gets a `fake-cube` binary behind the existing `BOSS_CUBE_CMD`, sharing the fixture-state machinery with `fake-gh`.

**One fixture-state format for both fakes.** A single JSON document describing the external world, loaded by whichever fake binary needs it, mutated in place so a test can assert on the _post-state_ (e.g. "a PR was opened with this branch and this body"). This makes the fakes assertable, not just permissive.

### 5.5 `testkit`: one isolated instance, one call

A new `tools/boss/testkit` crate (`testonly`), which:

- **Consolidates** the two duplicated `TestEngine` harnesses (`engine/core/tests/common/mod.rs` and `cli/tests/harness/mod.rs`) into one `BossInstance` fixture. Duplicated harnesses drift, and drift in a harness is invisible.
- Brings up a full isolated instance: state root, socket, DB, events socket, control token, dispatch-event root, feature-flags file, recovery dir — all under one tempdir, all routed through `IsolationPaths::derive` so the existing guard is the thing being exercised.
- Wires the fakes: scripted driver registered, fake `gh` and fake `cube` state seeded, clock injectable.
- Exposes assertion helpers over the **read surfaces**, not the store: `instance.client().list_attentions(…)` rather than `instance.db().query(…)`.
- Ships a `bazel run //tools/boss/testkit:instance -- --root <dir>` binary so a developer (or a worker satisfying a `manual` requirement) can bring the same instance up by hand and point the app at it.

`BOSS_RECOVERY_DIR` being process-global (`coordinator_tests/recovery.rs:8`) is a known wart that forces serialisation; `testkit` should thread it through config rather than env where it can, and mark the tests that still need process-global state as `tags = ["exclusive"]`.

### 5.6 Remote control, including the UI

**Engine: already there.** 169 RPC verbs on a Unix socket, plus an isolation guard that makes pointing at a test instance safe. No new work beyond `testkit` wiring.

**App: three missing pieces.**

1. **Scriptable drive mode.** `--script <file.json>` alongside the existing `--capture-to`: an ordered list of actions executed on the main actor — `select_work_item`, `open_viewer`, `set_filter`, `wait_for`, `capture`, `dump_state` — each producing a PNG and/or a JSON view-state dump. This is deliberately _not_ XCUITest: it runs in-process via the same `cacheDisplay` path `BossCapture.swift` already uses, so it needs no screen-recording permission, takes no focus, and works headless in CI.
2. **Stable element identity.** Accessibility identifiers on the views a script needs to address, so scripts refer to `taskList.row[short_id=3907]` rather than coordinates. Without this, scripts are unmaintainable.
3. **A one-command wrapper.** `boss ui capture --script … --out …` that brings up a `testkit` instance, seeds it, drives the app, submits image output through the existing `boss attach` API, and records any non-image trace under `executions/<id>/validation/`. This is what makes `manual` evidence cheap enough that workers will actually produce it.

The existing safety interlock — capture refuses to run unless `BOSS_SOCKET_PATH` is non-production (`BossCapture.swift:96`) — extends to script mode unchanged.

**Engine↔app contract: record/replay.** With `testkit`, record `FrontendRequest`/`FrontendEvent` traces from real flows into a checked-in corpus, then replay them against the Swift view models in `macos_unit_test` without an engine. This gives the app real engine output to test against, and gives the 169-variant wire surface a compatibility gate. This is the "RPC replay" the brief asks about — and note it is a _contract_ technique, distinct from both system and integration testing.

### 5.7 Concurrency, multi-path, and fault injection

**Concurrency hammer + verifier.** A `testkit` helper that runs N concurrent emitters against a sink for M iterations, then **re-reads and parses every record**. The dispatch-sink bug is caught the moment the verifier requires every line of `current.jsonl` to be valid JSON and the record count to match. Applied to every append-only writer: the dispatch sink, the audit log, the engine trace, the per-execution mirror. Run under `#[tokio::test(flavor = "multi_thread")]` at tier S.

**Multi-path coverage as an explicit obligation.** The recovery failure was two dispatch paths where only one set a flag. Where a capability is reachable by more than one route, the test must be **parameterised over the routes**, not written against whichever one the author had in mind. This cannot be fully mechanised, so it is a review-lane prompt: for an `automated` requirement, the PR body states which paths reach the behaviour and the reviewer checks the test covers each. Where the routes are enumerable in code (dispatch kinds, driver slugs, task kinds), the test iterates the enum so a new variant fails closed — the pattern `conformance/native_transcript.rs` already uses.

**Fault injection.** The fakes are the injection points: `fake-gh` can return a rate-limit, a 500, a truncated body; the scripted driver can die mid-turn or stall. No new production seams required.

### 5.8 Prompt and contract goldens

Replace substring assertions with goldens for the thing that is actually a contract: **what a worker receives**.

- A golden corpus over the composed task prompt, indexed by **driver slug × task kind** (`Chore`, `Design`, `Investigation`, `ProjectTask`, `Revision`, `Followup`, `DesignPostmortem`, `Task`), extending `conformance/goldens/`.
- `BLESS=1` regeneration, so updating them is one command and the diff is reviewable.
- A **lint over the goldens** — this is the part that would have caught the actual defects:
  - no absolute host paths (`/Users/`, `/home/`) anywhere in a golden;
  - no tool name that the driver's own `CapabilitySet` says it does not provide. Derived mechanically from `CapabilitySet::provides`, so it is not a hand-maintained blocklist that goes stale.
- `compose_prompt_tests.rs` (2208 lines) shrinks to the cases that genuinely need behavioural assertions; the rest becomes golden diffs.

### 5.9 Read surfaces are behaviour

A checked-in **surface registry**: a table mapping each observable row kind (attention kind, execution state, blocked signal, dispatch stage…) to the client-visible RPC(s) that must return it, and to which client issues that query.

One conformance test iterates the registry: seed a row of each kind through the production write path, then assert it comes back through **each declared read surface**, using the same request the real client sends. A row kind absent from the registry fails the test — so adding a new attention kind without declaring where it surfaces is a compile-or-test failure, not a two-month silence.

This is the direct fix for the invisible-`worker_blocked` failure, and it is cheap: `TestEngine` + `BossClient` already do both halves.

### 5.10 Schema and migration invariants

- **Schema golden.** Migrate from empty, dump the schema (tables, indices, CHECK constraints, triggers), diff against a checked-in `schema.sql.golden`. A migration that silently drops the CHECK forcing `work_item_id` NULL on execution-keyed rows becomes a red diff.
- **Equivalence property.** For every migration version _n_: `migrate(fresh_db_at_v_n)` must produce a schema identical to `create_fresh_at_head`. This is the invariant that catches "the migration and the fresh-create path diverged".
- **Legacy corpus.** A small set of checked-in, anonymised DBs in real historical shapes, each migrated forward in a test. Synthetic DBs do not have the rows that break migrations.

### 5.11 Observability as an assertion surface

Yes — promote `DispatchEvent` to a tested contract, with the **same tolerance policy already written for the Codex stream** (`conformance/mod.rs`): additive fields and unknown enum variants tolerated; removals and semantic changes to existing fields fail loudly.

Concretely: wire-goldens per stage variant, plus tier-S tests asserting the _stage sequence_ for canonical flows (successful dispatch, blocked worker, crash-and-recover). This makes the forensic stream a first-class assertion target — the thing that was already good enough to reconstruct failures becomes good enough to prevent them.

The cost is explicit and accepted: the dispatch-event schema becomes an interface with compatibility obligations. Given it is already read by `bossctl` and the macOS app, it is one in practice already.

### 5.12 Driver matrix

A `driver_matrix!` declarative macro that expands a test body once per registered slug from `DriverRegistry::default().slugs()`. Registering a fourth driver then makes every matrix test cover it automatically — the fail-closed property `native_transcript.rs` already demonstrates.

Enforcement of the _choice_ (matrix vs driver-agnostic) is scoped, not universal: a checkleft check over test modules in the crates that actually touch driver behaviour (`engine/core`'s `completion`, `runner`, `spawn_flow`, `worker_setup`, and `engine/driver`) requiring each test file to carry either a `driver_matrix!` use or an explicit `// DRIVER-AGNOSTIC: <reason>` marker. Repo-wide enforcement would be noise; these are the modules where "works on Claude, untested on Codex" actually happens.

### 5.13 Flakiness policy

- **`--flaky_test_attempts` is banned.** Retrying a flaky test is a bypass, and this repo's hard rule is to fix at the root.
- **Detection:** a nightly lane runs `--runs_per_test=20 --test_tag_filters=system,integration`. Any target that is not 20/20 is flaky.
- **Response — dogfood Boss.** A flake auto-files a Boss chore against the owner, where **owner = the last non-trivial author of the test file**. Boss files its own work; this is the natural mechanism and it exercises it.
- **Quarantine:** add `tags = ["quarantine"]`, which excludes the target from the PR lanes but keeps it in the nightly lane. Quarantine is a holding pen, not a graveyard.
- **Budget:** at most 5 quarantined targets repo-wide, and no target quarantined longer than 14 days. Exceeding either raises a blocking attention item for the operator. Both numbers are opening bids — see §6.

### 5.14 What "fixed" looks like for the six failures

| Failure                                        | Mechanism that catches it                                                             |
| ---------------------------------------------- | ------------------------------------------------------------------------------------- |
| Truncated `checkleft run` reported as passing  | §5.2(b) tool-native receipt or wrapped terminal status — worker cannot author it      |
| Invisible `worker_blocked` attention items     | §5.9 surface registry — fails closed on an undeclared row kind                        |
| Crash-recovery apply never ran                 | §5.7 multi-path obligation + §5.4 scripted driver can actually kill a worker mid-turn |
| Dispatch sink concurrency corruption           | §5.7 hammer + re-parse verifier                                                       |
| Prompts with wrong tools / hardcoded directory | §5.8 prompt goldens + capability-derived lint                                         |
| Push gate laundering a guess as root cause     | §5.11 diagnostic strings as tested contract, asserted from the dispatch stream        |

All six. That was the bar the brief set.

---

## 6. Risks and open questions

**The declaration could still become a checkbox.** The design's answer is that `unit`/`automated` requirements resolve to Bazel targets and green PR-head CI evidence, while `manual` requirements resolve to an inspectable artifact. The residual risk is a worker naming a green target that does not actually cover the criterion. Nothing mechanical catches that — it is what the reviewer LLM is for, and it is why each requirement records the criterion and the PR body maps every target to one, rather than listing labels without meaning.

**The scripted driver could drift from real drivers.** A fake that diverges from reality is worse than no fake. Mitigation: the scripted driver is subject to the same conformance harness as the real ones (`native_transcript`, ingress equivalence). Open question: should there be a periodic real-agent comparison run to detect divergence?

**Tier I lane cost on macOS.** App-involving integration tests need a macOS agent, and the `macos-arm64` queue is the scarcer resource. If the tier-I lane grows, PR latency grows. Mitigation: keep app tests to the record/replay path (tier U, no engine) wherever possible and reserve tier I for flows that genuinely need two processes.

**A new child-row table plus a `ValidationRequirement` collection on `Task` is a schema migration and a protocol change.** It touches `boss-protocol`, the DB mappers (which per repo convention stay struct-literal), the CLI, and the app. It is deliberately split across several tasks in the implementation breakdown below for that reason.

**Adding evidence obligations slows workers down.** Workers now author a `## Validation` mapping from each filed requirement to its evidence, and use the wrapper only for commands with no tool-native provider. That is real friction, though narrower than a universal wrapper requirement would be. The bet is that it is cheaper than the half-day the Xcode-gate failure cost. Worth revisiting after a month of data.

**`BOSS_RECOVERY_DIR` process-global state** forces some tests to serialise. Fixing it properly means threading it through config, which is a small refactor of a production path — in scope, but it is production code changing for testability, and that should be a conscious call.

Machine-actionable decisions are in the sibling `.attentions.json` manifest.

---

## Proposed implementation task breakdown

Entries are PR-sized and in dependency order. Parallelism notes appear per entry.

### 1. Consolidate the duplicate `TestEngine` harnesses into a `testkit` crate

Create `tools/boss/testkit` (`testonly`) and move the two near-identical `TestEngine` implementations (`engine/core/tests/common/mod.rs`, `cli/tests/harness/mod.rs`) into a single `BossInstance` fixture, preserving every existing option (on-disk DB, control token, injectable `MergeProbe`). Update both call sites to depend on the new crate. No behaviour change to any test; this is the foundation every later entry builds on.

- Effort hint: `small`
- Dependencies: none
- Scope: in-scope

### 2. Full-instance isolation in `testkit` (`BossInstance::spawn_isolated`)

Extend `BossInstance` to bring up every isolated surface under one tempdir — state root, DB, control socket, events socket, pid file, control token, dispatch-event root, feature-flags file, recovery dir — routed through the existing `IsolationPaths::derive` so the production guard is the code being exercised. Add the `bazel run //tools/boss/testkit:instance` binary for manual bring-up.

- Effort hint: `medium`
- Dependencies: Consolidate the duplicate `TestEngine` harnesses into a `testkit` crate
- Scope: in-scope

### 3. Bazel test-tier macros and tag filters

Add `boss_system_test` and `boss_integration_test` macros to `tools/test-sandbox/defs.bzl` setting size, timeout, and `tags` per §5.3, and add `test --test_tag_filters=-exogenous` to `.bazelrc`. Retag the ten existing `engine/core` integration-style targets and the `cli` engine-backed tests as tier S. No new tests here — vocabulary only.

- Effort hint: `small`
- Dependencies: none
- Scope: in-scope
- May run in parallel with entries 1 and 2 (different packages, no file overlap).

### 4. Buildkite `system-integration` lane

Add a pipeline step running `--test_tag_filters=system,integration` with its own `github_commit_status` context, and register it in `.buildkite/REQUIRED_CHECKS.md`. Keep `bazel-build-test` filtering to unit+system so tier I is attributable on its own status.

- Effort hint: `small`
- Dependencies: Bazel test-tier macros and tag filters
- Scope: in-scope

### 5. Scripted fake agent driver

Add a `scripted` driver implementing `AgentDriver` in `tools/boss/engine/driver`, whose worker process is a small in-tree replay binary consuming a JSONL script (stdout events, hook calls, PR-URL file write, exit status) with a per-script configurable `CapabilitySet`. Registered in `DriverRegistry` so the production spawn path is what executes.

- Effort hint: `medium`
- Dependencies: none
- Scope: in-scope
- Parallel with entries 1–4, **but** co-edits `engine/driver/src/registry.rs` with entry 9 (`driver_matrix!`). Land this one first; entry 9 forward-ports over it, integrating rather than replacing.

### 6. Scripted-driver fixture corpus and spawn→completion system test

Author the script fixtures for the shapes that keep breaking — clean run, `[blocked]` marker, `[effort-escalation]`, crash mid-turn, hung turn, truncated stdout — and a tier-S test driving each through the real spawn→pane→hook→completion path via `BossInstance`. This is the entry that proves the fake actually exercises production code rather than sitting beside it.

- Effort hint: `medium`
- Dependencies: Full-instance isolation in `testkit`; Scripted fake agent driver
- Scope: in-scope

### 7. `BOSS_GH_CMD` seam and `fake-gh` binary

Add the env override at the single `gh_command` funnel in `github/src/gh_runner.rs:289`, and a `fake-gh` binary serving a mutable JSON fixture state (repos, PRs, check runs, review comments, merge state) so tests can assert on post-state. Define the shared fixture-state format here; entry 8 reuses it.

- Effort hint: `medium`
- Dependencies: none
- Scope: in-scope
- Parallel with entries 1–6.

### 8. `fake-cube` binary behind `BOSS_CUBE_CMD`

A cube fake covering the lease and PR surfaces, reusing the fixture-state format from `fake-gh`. The in-process `CubeJsonTransport` trait fake stays as the tier-S path; this is the tier-I binary.

- Effort hint: `small`
- Dependencies: `BOSS_GH_CMD` seam and `fake-gh` binary
- Scope: in-scope

### 9. `driver_matrix!` macro and migration of existing per-driver loops

Add a declarative macro expanding a test body once per `DriverRegistry::default().slugs()` entry, and convert the three existing hand-rolled loops (`completion/tests/t02.rs:1148`, `conformance/native_transcript.rs:98`, `driver/src/registry.rs:217`) to use it.

- Effort hint: `small`
- Dependencies: Scripted fake agent driver
- Scope: in-scope

### 10. Checkleft check enforcing driver-matrix declaration

A check over test modules in `engine/driver` and `engine/core`'s `completion`/`runner`/`spawn_flow`/`worker_setup` requiring each test file to carry a `driver_matrix!` use or an explicit `// DRIVER-AGNOSTIC: <reason>` marker. Scoped deliberately — repo-wide would be noise.

- Effort hint: `medium`
- Dependencies: `driver_matrix!` macro and migration of existing per-driver loops
- Scope: in-scope

### 11. Composed-prompt golden corpus with `BLESS` regeneration

Extend `engine/core/src/conformance/goldens/` to cover the composed task prompt across driver slug × `TaskKind`, with a `BLESS=1` regeneration mode. Do not delete `compose_prompt_tests.rs` in this entry — add goldens alongside it.

- Effort hint: `medium`
- Dependencies: none
- Scope: in-scope
- Parallel with entries 1–10.

### 12. Prompt golden lint

Assert over every golden that it contains no absolute host path (`/Users/`, `/home/`) and no tool name the driver's own `CapabilitySet::provides` says it lacks — the forbidden set derived mechanically from the capability data, not hand-maintained.

- Effort hint: `small`
- Dependencies: Composed-prompt golden corpus with `BLESS` regeneration
- Scope: in-scope

### 13. Retire redundant substring assertions in `compose_prompt_tests.rs`

Now that goldens cover the composed output, reduce the 2208-line substring-assertion file to the cases that need genuine behavioural assertions. Sweep-style cleanup, deliberately sequenced after the goldens that replace it so coverage never dips.

- Effort hint: `medium`
- Dependencies: Prompt golden lint
- Scope: in-scope

### 14. Read-surface registry and reachability conformance test

A checked-in table mapping each observable row kind to the client-visible RPC(s) that must return it and to the client that issues each query, plus one test that seeds each kind through the production write path and asserts it returns through every declared surface. Fails closed on an undeclared kind.

- Effort hint: `medium`
- Dependencies: Full-instance isolation in `testkit`
- Scope: in-scope

### 15. Concurrency hammer harness and append-only sink verifier

A `testkit` helper running N concurrent emitters for M iterations then re-reading and parsing every record, applied to the dispatch sink, per-execution mirror, audit log, and engine trace. Expected to fail on landing against `JsonlFileSink::append_line` (`engine/dispatch-events/src/lib.rs:783-791`), whose two unlocked `write_all` calls are the live form of the recorded corruption bug.

- Effort hint: `medium`
- Dependencies: Full-instance isolation in `testkit`
- Scope: in-scope

### 16. Fix the dispatch sink's non-atomic append

Make `append_line` write body and newline as one buffer under a per-path lock, so the verifier from the previous entry passes. Separate PR from the test that proves it, so the failing-then-passing transition is visible in history.

- Effort hint: `trivial`
- Dependencies: Concurrency hammer harness and append-only sink verifier
- Scope: in-scope

### 17. `DispatchEvent` wire goldens and stage-sequence system tests

Golden files per stage variant under the Codex-stream tolerance policy (additive tolerated, removals and semantic changes fail), plus tier-S assertions on the stage _sequence_ for successful dispatch, blocked worker, and crash-and-recover flows.

- Effort hint: `medium`
- Dependencies: Scripted-driver fixture corpus and spawn→completion system test; Fix the dispatch sink's non-atomic append
- Scope: in-scope

### 18. Schema golden and migrate-vs-fresh equivalence test

Dump the schema after migrating from empty, diff against a checked-in `schema.sql.golden`, and assert `migrate(fresh_at_v_n)` is schema-identical to `create_fresh_at_head` for every version. Catches a migration silently dropping a load-bearing CHECK.

- Effort hint: `small`
- Dependencies: none
- Scope: in-scope
- Parallel with entries 1–17.

### 19. Legacy-DB corpus migration tests

Check in a small set of anonymised DBs in real historical shapes and migrate each forward in a test. A sweep over real data, sequenced after the equivalence machinery it uses.

- Effort hint: `medium`
- Dependencies: Schema golden and migrate-vs-fresh equivalence test
- Scope: in-scope

### 20. `validation_requirements`: schema, migration, and protocol types

Add child rows storing each work item's validation mode and criterion, plus the corresponding `ValidationRequirement` collection on `boss-protocol`'s `Task` (per the builder convention: `#[builder(default)]` alongside `#[serde(default)]`). Enforce that `exempt` cannot coexist with substantive requirements. Protocol/engine only — no CLI or app surface in this entry.

- Effort hint: `medium`
- Dependencies: none
- Scope: in-scope
- Parallel with entries 1–19.

### 21. CLI and `bossctl` surfaces for `validation_requirements`

Set/read the requirement collection from `boss task update`, `boss task create`, and the corresponding `bossctl` verbs; include it in `boss context` output so a worker can see every obligation.

- Effort hint: `small`
- Dependencies: `validation_requirements`: schema, migration, and protocol types
- Scope: in-scope

### 22. Display `validation_requirements` in the macOS app

Surface all requirements on the work-item detail view and expose mode filters on the board. App-only; parallel with entry 21 (different subsystem, no file overlap).

- Effort hint: `small`
- Dependencies: `validation_requirements`: schema, migration, and protocol types
- Scope: in-scope

### 23. Validation evidence providers and generic command recorder

Introduce the provider interface from §4.6 and a Bazel provider that imports durable PR-head CI diagnostics without worker action. Add `boss validate run` as the fallback for uncovered commands; it writes argv, full stdout/stderr, exit status, and duration to `<state root>/executions/<id>/validation/<slug>.json` so the worker cannot author its own exit code.

- Effort hint: `medium`
- Dependencies: `validation_requirements`: schema, migration, and protocol types
- Scope: in-scope

### 24. Worker prompt instructions for the validation contract

Update the composed worker prompt so workers name every validation requirement and its evidence in the `## Validation` PR-body section, and use `boss validate run` only when no tool-native provider covers the command. Golden diffs from entry 11 make the change reviewable.

- Effort hint: `small`
- Dependencies: Validation evidence providers and generic command recorder; Composed-prompt golden corpus with `BLESS` regeneration
- Scope: in-scope

### 25. `## Validation` section parser and deterministic evidence checker

Parse the PR-body section, map every filed requirement to cited evidence, resolve Bazel labels and the applicable green PR-head CI result, and resolve command-provider records and attachment rows. Emit mechanical findings for missing labels, red CI, absent records, and uncovered requirements — a deterministic pre-pass ahead of the reviewer LLM.

- Effort hint: `medium`
- Dependencies: Validation evidence providers and generic command recorder; CLI and `bossctl` surfaces for `validation_requirements`
- Scope: in-scope

### 26. Wire the evidence checker into the review lane

Feed the checker's findings into the existing automated-reviewer pass so an unbacked validation claim becomes a review finding, and an empty requirement set on a non-trivial item is flagged. Gate on the composability and exemption semantics from §5.1.

- Effort hint: `small`
- Dependencies: `## Validation` section parser and deterministic evidence checker
- Scope: in-scope

### 27. Coordinator brief template: author requirements at filing time

Update the coordinator's task-filing prompt and templates so one or more `validation_requirements` are written with the brief, with the §5.1 "non-trivial" definition and composability stated inline.

- Effort hint: `small`
- Dependencies: CLI and `bossctl` surfaces for `validation_requirements`
- Scope: in-scope

### 28. Accessibility identifiers on driveable app views

Add stable accessibility identifiers to the views a capture script must address (work-item list rows, detail panes, viewer toggles, filter controls). Pure annotation, no behaviour change.

- Effort hint: `small`
- Dependencies: none
- Scope: in-scope
- Must land **before** the next entry: both edit the same SwiftUI view files, and the script mode addresses elements by the identifiers this entry introduces.

### 29. App scripted drive mode (`--script`)

Extend `BossCapture.swift` with an ordered JSON action list (`select_work_item`, `open_viewer`, `set_filter`, `wait_for`, `capture`, `dump_state`) executed on the main actor via the existing in-process `cacheDisplay` path, preserving the non-production-socket interlock at `BossCapture.swift:96`.

- Effort hint: `medium`
- Dependencies: Accessibility identifiers on driveable app views
- Scope: in-scope

### 30. `boss ui capture` wrapper

One command that brings up a `testkit` instance, seeds fixture state, drives the app with a script, submits PNG/JPEG output through the existing attachment API, and writes non-image traces into `executions/<id>/validation/` — making `manual` evidence cheap enough to actually be produced.

- Effort hint: `small`
- Dependencies: App scripted drive mode (`--script`); Full-instance isolation in `testkit`
- Scope: in-scope

### 31. Engine↔app RPC trace record/replay corpus

Record `FrontendRequest`/`FrontendEvent` traces from real `testkit` flows into a checked-in corpus and replay them against the Swift view models in `macos_unit_test`, giving the 169-variant wire surface a compatibility gate without needing a live engine.

- Effort hint: `medium`
- Dependencies: Full-instance isolation in `testkit`; Accessibility identifiers on driveable app views
- Scope: in-scope

### 32. Nightly flake-detection lane

A scheduled Buildkite pipeline running `--runs_per_test=20 --test_tag_filters=system,integration`, parsing results, and auto-filing a Boss chore against the last non-trivial author of any target that is not 20/20.

- Effort hint: `medium`
- Dependencies: Buildkite `system-integration` lane
- Scope: in-scope

### 33. Quarantine tag and budget enforcement

Honour `tags = ["quarantine"]` by excluding those targets from the PR lanes while keeping them in the nightly lane, and raise a blocking attention item when the repo exceeds the quarantine budget or a target has been quarantined past the age limit.

- Effort hint: `small`
- Dependencies: Nightly flake-detection lane
- Scope: in-scope

### 34. Thread `BOSS_RECOVERY_DIR` through config instead of process-global env

Remove the process-global env dependency that forces `coordinator_tests/recovery.rs` to serialise, so recovery tests can run concurrently. Production code changing for testability — call it out explicitly in the PR.

- Effort hint: `small`
- Dependencies: Full-instance isolation in `testkit`
- Scope: deferred (future / not a v1 blocker) — a test-ergonomics improvement, not a gap in the validation mechanism; land it once `testkit` has settled.

### 35. Adopt `BOSS_PROFILE` full-instance launcher

The single-knob profile resolver and per-profile `UserDefaults` suite proposed in [`test-instance-isolation.md`](test-instance-isolation.md).

- Effort hint: `medium`
- Dependencies: none
- Scope: deferred (future / not a v1 blocker) — owned by the test-instance-isolation design; this project consumes it rather than re-implementing it, and `testkit`'s tempdir-based isolation is sufficient for v1.

### 36. Nightly real-agent smoke lane

A small tier-E lane that spawns one real agent worker against a scratch repo nightly, to detect divergence between the scripted driver and real driver behaviour.

- Effort hint: `medium`
- Dependencies: Scripted-driver fixture corpus and spawn→completion system test
- Scope: deferred (future / not a v1 blocker) — costs real tokens on every run; land only once the scripted driver has enough production mileage to make divergence the interesting question.

### 37. Buildkite flake dashboard

A `bk`-backed view of flake rates per target over time, so the quarantine budget is managed from data rather than from whoever noticed most recently.

- Effort hint: `medium`
- Dependencies: Nightly flake-detection lane
- Scope: deferred (future / not a v1 blocker) — the auto-filed chores from entry 32 are sufficient signal for v1.

### 38. Back-fill `validation_requirements` on existing work items

A sweep setting requirements on already-filed rows.

- Effort hint: `large`
- Dependencies: CLI and `bossctl` surfaces for `validation_requirements`
- Scope: deferred (future / not a v1 blocker) — **explicitly out of scope per the project brief**; listed so the decision is visible rather than silently omitted.

### 39. Coverage instrumentation as a diagnostic

A non-gating coverage lane, used to find untested regions rather than to enforce a threshold.

- Effort hint: `medium`
- Dependencies: Buildkite `system-integration` lane
- Scope: deferred (future / not a v1 blocker) — rejected as a _gate_ in §4.2; may still be worth having as a diagnostic once the tiers exist.

### Parallelism summary

At depth 0, entries **1, 3, 5, 7, 11, 18, 20, 28, 35** may all start immediately — they touch `testkit`, `defs.bzl`, `engine/driver`, `github`, `conformance`, `work/migrations`, `boss-protocol`, and the app respectively, with no file overlap.

Two ordering constraints are file-overlap rather than logical dependencies, and are called out on the entries themselves: **5 before 9** (both edit `engine/driver/src/registry.rs`) and **28 before 29** (both edit the same SwiftUI view files). In each case the later entry must forward-port the earlier one's changes preservingly.

Entries **21 and 22** are genuinely parallel at their depth (CLI vs app). Entries **15/16** and **17** are serialised deliberately so the sink fix lands as a visible red-to-green transition.
