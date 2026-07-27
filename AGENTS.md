- always use minimal bazel visibility, never default to public. Maintain bazel visibility health.
- Documentation-only changes (markdown files, design docs, plans, READMEs) should be pushed directly to `main` instead of opening a PR.
- **HARD RULE: always use Bazel for local builds and tests. Never invoke `cargo build`, `cargo test`, or any bare `cargo` command directly.** Direct `cargo` is slow, uncached, and does not match what CI builds. Bazel is the canonical, cached path — it is what CI runs, and it is what you must run locally. For the engine: `bazel test //tools/boss/engine/...`.
- `checkleft` (the linter) lives in this repo, at `tools/checkleft/` — it is not a standalone external repo. Don't go hunting elsewhere for it; it's published to crates.io and released as prebuilt binaries from here (see `tools/checkleft/docs/buildkite-release-setup.md`). A private manual playground that _consumes_ those prebuilts (Rust + Bazel, rules_multitool) lives at `brianduff/checkleft-sandbox` — see `tools/checkleft/docs/checkleft-sandbox.md`.
- **Run `checkleft run` with no flags.** Its change detection scopes the run to what you actually touched, which is what makes it fast in a monorepo — no SHA or base-ref plumbing needed. **Do not run `checkleft --all`.** It is reserved for CI's dedicated integrity pipeline, for work that modifies checkleft itself, or for a case with a strong stated justification. `--all` is not a stricter superset of the default: checks with `changed_lines_only` become a no-op under it, so it reports every pre-existing violation in the repo and buries the findings that belong to your change.

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

- Bazel / Xcode LaunchServices pin on macOS hosts:
  [`tools/boss/docs/mac-toolchain-xcode-pinning.md`](tools/boss/docs/mac-toolchain-xcode-pinning.md)
- Boss forensic surfaces (`engine-audit.log`, per-task cost / transcripts):
  [`tools/boss/docs/forensic-surfaces.md`](tools/boss/docs/forensic-surfaces.md)
- Post-crash orphan recovery:
  [`tools/boss/docs/post-crash-recovery.md`](tools/boss/docs/post-crash-recovery.md)
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
