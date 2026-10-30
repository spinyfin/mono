- always use minimal bazel visibility, never default to public. Maintain bazel visibility health.
- Engine DB migrations use unique UTC `YYYYMMDD_HHMM_short_snake_name` ids in `tools/boss/engine/core/src/work/schema_migration_registry.rs`, with `legacy_version: None`. Add a separate registry entry; never bump an integer counter or change a shipped id. Migrations from parallel PRs must be independent. If a migration depends on another PR's migration, wait for that PR to merge before building on it. See [database migrations](tools/boss/docs/database-migrations.md) for bootstrap, ordering, and upgrade tests.
- Documentation-only changes (markdown files, design docs, plans, READMEs) should be pushed directly to `main` instead of opening a PR.
- **HARD RULE: always use Bazel for local builds and tests. Never invoke `cargo build`, `cargo test`, `cargo fmt`, or any bare `cargo` command directly.** Direct `cargo` is slow, uncached, and does not match what CI builds. Bazel is the canonical, cached path — it is what CI runs, and it is what you must run locally. For the engine: `bazel test //tools/boss/engine/...`. `cargo fmt` is not a harmless exception to this: it shells out to `cargo metadata`, which creates a `target/` directory (with `CACHEDIR.TAG` and `.rustc_info.json`) even though it compiles nothing — it is still the forbidden bare-`cargo` shape. Use `checkleft fix` to reformat Rust files, or invoke the `rustfmt` binary directly.
- **HARD RULE: run tests only via `bazel test`. Never execute a built test binary directly (`bazel-bin/...`), and never use `bazel run` on a test target as a substitute.** Both bypass `tools/test-sandbox/hermetic_test_wrapper.sh`, which `bazel test` applies via `run_under`. The wrapper redirects `HOME` into the test's private tmpdir and, on macOS, applies a seatbelt profile that denies writes under `/Users` and denies `process-exec` of `tmux`, `claude`, `codex`, and `gh`. Skip it and a test runs with your real `HOME` and no sandbox — it can write into live Boss state under `~/Library/Application Support/Boss` and reach the real tmux server. A test result obtained outside the sandbox is not evidence: a test that passes unsandboxed can fail under `bazel test` and vice versa. If a test fails only outside the sandbox, that is not flakiness — do not dismiss it as environmental. `--test_filter` does work for `rust_test` here (mono patches `TESTBRIDGE_TEST_ONLY` support into `rules_rust` — see `third_party/patches/rules_rust-0.70.0-test-wrapper.patch`), and a filter that matches nothing fails loudly rather than reporting an empty green run. It is still usually unnecessary — Bazel's own change detection already scopes a run to what's affected — but narrowing with `--test_filter` is never a reason to run a binary out of `bazel-bin/`.
- `checkleft` (the linter) lives in this repo, at `tools/checkleft/` — it is not a standalone external repo. Don't go hunting elsewhere for it; it's published to crates.io and released as prebuilt binaries from here (see `tools/checkleft/docs/buildkite-release-setup.md`). A private manual playground that _consumes_ those prebuilts (Rust + Bazel, rules_multitool) lives at `brianduff/checkleft-sandbox` — see `tools/checkleft/docs/checkleft-sandbox.md`.
- **Checkleft bypass directives are uniform.** Every bypass is `BYPASS_<NAME>=<reason>` with a free-form reason; a check must never define its own directive syntax or parse limits, counts, flags or other data out of the reason. Thresholds and parameterised exceptions belong in `CHECKS.yaml` or a separate reviewed mechanism. See the design principle in `tools/checkleft/userdoc/docs/bypass.md`.
- **Run `checkleft run` with no flags.** Its change detection scopes the run to what you actually touched, which is what makes it fast in a monorepo — no SHA or base-ref plumbing needed. **Do not run `checkleft --all`.** It is reserved for CI's dedicated integrity pipeline, for work that modifies checkleft itself, or for a case with a strong stated justification. `--all` is not a stricter superset of the default: checks with `changed_lines_only` become a no-op under it, so it reports every pre-existing violation in the repo and buries the findings that belong to your change.
- **HARD RULE: do not wrap `bazel build`, `bazel test`, `bazel run`, or `checkleft run` in a shell `timeout`.** Bazel's own per-test timeouts (test size and timeout attributes) are what bound tests. A build that is still compiling with no errors is making progress, even when it is slow because other builds share the machine. If a wait bound is needed (for example a polling yield), keep polling the running command instead of killing it. If a hard cap is ever unavoidable, it must be at least two hours, and a timeout must never be treated as a reason to rerun the same command under the same cap.

## No Boss work-item ids in PRs, commits, or source

`spinyfin/mono` runs `boss-ism/pr-text-leakage` (changeset scope) and
`boss-ism/file-text-leakage` (changed-lines file scope) in root
`CHECKS.yaml`. Both forbid internal Boss work-item id shapes
(`T<n>` / `P<n>`) in worker-authored text:

- **PR titles and bodies**, and **commit messages** — hard error via
  `boss-ism/pr-text-leakage` (`\b[TP]\d+\b`). A PR cannot cite the
  work-item id that spawned it, and the check also trips on incidental
  text such as a quoted commit title that embeds a short id like
  `(T` + digits + `)`.
- **Source and docs** — `boss-ism/file-text-leakage` flags the same
  shape on changed lines (floored at three digits to avoid ISO-8601 /
  percentile false positives). Confirmed when a recovered patch whose
  comments cited a work-item id failed local `checkleft run`.

**Cite the public PR instead** (e.g. `mono#2303`), never the work-item
id. Do not reference a work-item id _anywhere_ a worker writes —
commit messages, code comments, design docs, or PR body.

If you hit this check, **fix the text at the root**. Do not add a
bypass, exclusion, or allowlist entry for the check. That is the same
root-cause rule as the section below.

## Operational docs workers should know

- Boss macOS app rules (no modal dialogs for non-actionable status or
  error messages; which inline/banner/log surface to use instead):
  [`tools/boss/app-macos/AGENTS.md`](tools/boss/app-macos/AGENTS.md)
- Bazel / Xcode LaunchServices pin on macOS hosts:
  [`tools/boss/docs/mac-toolchain-xcode-pinning.md`](tools/boss/docs/mac-toolchain-xcode-pinning.md)
- Boss forensic surfaces (`engine-audit.log`, per-task cost / transcripts):
  [`tools/boss/docs/forensic-surfaces.md`](tools/boss/docs/forensic-surfaces.md)
- Worker liveness contract (what the agent indicator derives from; how the
  engine converges when it has lost track of a live worker):
  [`tools/boss/docs/worker-liveness-contract.md`](tools/boss/docs/worker-liveness-contract.md)
- Failure-signal lifecycle (what raises an attention item / the card's
  "Failed to start" banner, and what specifically lowers it again):
  [`tools/boss/docs/attention-lifecycle.md`](tools/boss/docs/attention-lifecycle.md)
- Post-crash orphan recovery:
  [`tools/boss/docs/post-crash-recovery.md`](tools/boss/docs/post-crash-recovery.md)
- Execution recovery pointers (the three bookmarks per execution, how they
  get damaged, and how dispatch reconciles or retires them):
  [`tools/boss/docs/execution-recovery-pointers.md`](tools/boss/docs/execution-recovery-pointers.md)
- Self-update attempt log (where the durable update trail lives, how to
  follow one attempt across the old app, the relaunch helper and the new
  app, and where a rollback's reason is recorded):
  [`tools/boss/docs/update-attempt-log.md`](tools/boss/docs/update-attempt-log.md)
- Crash watchdog (why an abort always kills the app, and
  `BOSS_CRASH_WATCHDOG_SECONDS`):
  [`tools/boss/docs/crash-watchdog.md`](tools/boss/docs/crash-watchdog.md)
- Coordinator session handoff (what the outgoing coordinator session
  writes, how the incoming one is briefed, and the three states it can
  report):
  [`tools/boss/docs/coordinator-session-handoff.md`](tools/boss/docs/coordinator-session-handoff.md)
- Per-product coordinator guidance (`BOSS_COORDINATOR.md` at a product
  repo's root: coordinator-only rules, how the engine loads them from
  GitHub, and why they never go in `AGENTS.md`):
  [`tools/boss/docs/coordinator-product-guidance.md`](tools/boss/docs/coordinator-product-guidance.md)
- Worker tool environment (why bare `checkleft` resolves to the repo's
  Bazel build in a worker, how the composed `PATH` is sealed across
  driver tool shells, and what that seal does not cover):
  [`tools/boss/docs/worker-tool-environment.md`](tools/boss/docs/worker-tool-environment.md)
- Background-sweep telemetry (per-pass duration / items / pass-count
  metrics for the envelope watchdog, terminal-work sweep, dead-pane
  terminal scan, JSONL tail poll and recovery-pointer passes, tick
  intervals for the two singleton sweeps, and what each one's cost scales
  with):
  [`tools/boss/docs/background-sweep-telemetry.md`](tools/boss/docs/background-sweep-telemetry.md)
- Run process teardown (how the engine and cube find and kill what a run
  left running in its workspace, why working directory rather than process
  group, and how a workspace's Bazel server is told apart and spared):
  [`tools/boss/docs/run-process-teardown.md`](tools/boss/docs/run-process-teardown.md)
- Operator runbooks:
  [`tools/boss/docs/runbooks/`](tools/boss/docs/runbooks/)

## Prefer crates over modules for distinct units of functionality (Rust)

We generally keep distinct units in their own crates rather than as modules inside a larger crate: bazel incrementality is per-crate, so smaller crates mean smaller rebuild and retest scopes. When adding or extracting such a unit, it is OK to do light dependency/interface refactoring to support the split — e.g. introduce a small trait or plain context type at the boundary, or move shared types down into a lower-level crate. Keep each crate's dependency list minimal and the edges one-directional: a transport/utility/pipeline crate must never import from the higher-level crate that consumes it; if a cycle threatens, the shared types belong in a lower crate, not in the consumer. "Generally" means use judgment: a tiny glue module doesn't need a crate; a unit with its own vocabulary, tests, and multiple consumers does.

Precedent: the `claude_client` extraction (PR #1702) pulled the Claude API transport out of `engine/core` into `tools/boss/claude_client`, with a one-way `engine` → `claude_client` edge.

## Hard constraint: fix failing checks at the root cause; never bypass them

When a CI check or repository check (checkleft, file-size, lint, test) is failing, fix the underlying problem. The following are forbidden bypasses — do NOT do any of them:

- Adding a file to a check exclusion or allowlist (`CHECKS.yaml` `exclude_files`, checkleft excludes, lint-disable comments, etc.) to suppress the failure.
- Setting `allow_bypass`, using an override flag, or invoking any bypass/override mechanism on a check.
- Passing `--no-verify` / skipping git hooks; adding broad `#[allow(...)]` / `// swiftlint:disable` / `# noqa` annotations solely to suppress a warning or error.
- Deleting, `#[ignore]`-ing, `xfail`-ing, skipping, or weakening assertions in a failing test to make it pass.
- Raising a threshold or limit (e.g. `max_lines` in a file-size check) solely to accommodate the offending file without reducing its size.

Required behavior: fix the real problem — split the oversized file, fix the lint/compile error, fix the test failure, resolve the root cause. If a check genuinely SHOULD be relaxed (a legitimately needed exclusion or threshold change), that is a human decision — STOP and surface it for operator approval with full justification. Do not decide this autonomously.

### `change/file-count` failures: split the change, do not ask for an exception

When `change/file-count` fires on feature work, the remedy is to split the change into stacked PRs along capability boundaries, each coherent and independently reviewable. Do not request a `BYPASS_CHANGE_FILE_COUNT` exception or a higher limit for feature work. Exceptions are only for the cases named in `CHECKS.yaml`: coordinated renames and generated trees that cannot be split.

Check the footprint early, not after the work is built. Once the shape of the change is clear, and again before final validation, run the file-count check or `jj diff --stat` against the base. If the change is heading past the limit, stop and split it before building the rest:

- When the worker instructions enable completion declarations, carve out the first coherent capability as a prerequisite with `"$BOSS_BIN" propose done --outcome blocked --summary "<blocker>" --answer-type create-prerequisite-task --prerequisite-name "<short task name>" --prerequisite-brief "<standalone description of that first PR>" --explanation "<Context, Yes, No, Recommendation, and optional Details>"`. The brief must stand alone: a different worker picks it up cold. The current task then delivers the remainder once the prerequisite has merged.
- If completion declarations are disabled, report the dependency through the blocker channel in the worker instructions so the repo owner can arrange the prerequisite.
- Keep the split on capability boundaries, as above.
- When completion declarations are enabled, do not file a plain blocker, a plain yes/no question or `boss propose followup-task` for this. A yes/no approval only re-dispatches the same task, and `followup-task` is the non-blocking "nice to have later" path: it creates no task until a human batch-accepts it, and it adds no dependency edge, so the split never lands.

## Builder pattern convention

Structs with **more than 5 fields** in `boss-protocol` (and in `boss-engine`'s internal types) use `#[derive(bon::Builder)]` with `#[builder(on(String, into))]`. This prevents additive-change PRs from touching every construction site across the repo.

Rules:

- `Option<T>` fields are automatically optional in the builder (bon defaults them to `None`).
- Non-optional fields that have a sensible runtime default (e.g. `autostart = true`, `priority = "medium"`, `last_status_actor = "human"`) carry `#[builder(default = ...)]`; use the existing `default_*()` helpers from `types.rs`.
- Fields with no sensible default remain required in the builder — omitting them is a compile error.
- When adding a new **optional** field to a builder-equipped struct: add `#[builder(default)]` (or `#[builder(default = expr)]`) alongside any `#[serde(default)]`. Existing construction sites need no changes.
- When adding a new **required** field: that is an explicit breaking change — call it out in the PR description. All construction sites must be updated.
- The production **DB mapper functions** (`map_task`, `map_product`, etc. in `work.rs`) continue to use struct literals — they must explicitly set every field from named columns, and a compile error when a new column isn't mapped is desirable. Do not convert DB mappers to builder calls.
- When calling `Option<String>` setter methods on a builder with `on(String, into)`: pass the inner string value directly (e.g. `.started_at("2026-01-01")`), **not** wrapped in `Some(...)`. To pass a dynamic `Option<&str>` or `Option<String>`, use the `maybe_field_name()` variant (e.g. `.maybe_repo_remote_url(repo)`).

Structs currently on the builder pattern: `Task`, `WorkExecution`, `Product`, `Project` (all in `boss-protocol/src/types.rs`).

## Running Boss itself

Never launch the installed `/Applications/Boss.app`, never `open -a
Boss`, and never start an engine that can reach production state.
This machine is someone's laptop: an unisolated app launch puts a
window on their screen and terminates the engine they are using.
Bare `bazel run //tools/boss/app-macos:Boss` (no isolation env) is
also blocked.

To exercise a real engine, start an isolated one:

```sh
env -u BOSS_EVENTS_SOCKET bazel run //tools/boss/engine/core:engine -- \
  --socket-path /tmp/boss-test-$(uuidgen).sock
```

Any `--socket-path` other than the production socket puts the engine
in test-fixture mode, where the db, events socket, pid file and
control token are all derived from that socket's path — this
includes the legacy `/tmp/boss-engine.sock`, which is a test
fixture, not a safe alternative. Unset
`BOSS_EVENTS_SOCKET` because your pane inherits one pointing at
production. Point a client at the same `--socket-path` to drive it.

To screenshot the real Boss UI quietly, launch an isolated capture
instance (both env vars required):

```sh
BOSS_SOCKET_PATH=/tmp/boss-shot-$(uuidgen).sock BOSS_ENGINE_AUTOSTART=0 \
  bazel run //tools/boss/app-macos:Boss -- --capture-to /tmp/shot.png
```

The instance renders itself in-process via `cacheDisplay` and exits;
it never shows a window, never takes focus, and needs no
screen-recording permission. Read the PNG back and state in the PR
what you verified and what you could not.

`bazel build` and `bazel test` are unaffected.
