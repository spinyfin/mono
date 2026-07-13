# Required checks for branch protection

This file is the authoritative list of check names wired into GitHub branch protection on `main`. It exists so that **renaming a step key in `pipeline.yml` must be accompanied by a matching branch-protection update** — the two are a paired contract.

GitHub's branch protection rule references exact check names. If a step key changes in `pipeline.yml` but this file and branch protection are not updated in lockstep, the gate silently waits forever for a check that will never arrive.

## Format

Buildkite emits check names in the form `buildkite/mono/<step-key>`, where `<step-key>` matches the `key:` field in `pipeline.yml`.

## Current required checks

These checks are currently **required** by branch protection on `main` (block merge if red):

| Check name                        | Step key in pipeline.yml |
| --------------------------------- | ------------------------ |
| `buildkite/mono/bazel-build-test` | `bazel-build-test`       |
| `buildkite/mono/mac-app-build`    | `mac-app-build`          |
| `buildkite/mono/checks`           | `checks`                 |

## How contexts are emitted

Each gating step in `pipeline.yml` carries one explicit `notify: github_commit_status: { context: "buildkite/mono/<name>" }` block, where `<name>` matches the step's `key:` field 1:1, decoupling the context from the step `label:` (which may include emoji and can be changed freely without affecting the gate).

`bazel-build-test` runs the former `bazel-build` and `bazel-test` steps back to back in a single step (so the test phase reuses the build phase's local bazel outputs instead of rebuilding on a second agent). Build vs. test failure attribution lives in the step's log groups (`--- [bazel-build] building` / `--- [bazel-test] testing`), not in separate GitHub checks.

The resulting context names are `buildkite/mono/<step-key>` — e.g. `buildkite/mono/checks`.

## Rollout: rename complete (`bazel-build` + `bazel-test` → `bazel-build-test`)

The rename from the legacy `bazel-build` / `bazel-test` contexts to the consolidated `bazel-build-test` context is complete:

1. `bazel-build-test` emitted all three contexts (`bazel-build`, `bazel-test`, and the new `bazel-build-test`) during the transition, so no in-flight PR was ever stranded against `required_status_checks`.
2. `buildkite/mono/bazel-build-test` was confirmed posting as a real, resolving check on recent `main` commits.
3. An operator with admin on `spinyfin/mono` flipped `required_status_checks` on 2026-07-13 to require `buildkite/mono/bazel-build-test`, `buildkite/mono/mac-app-build`, and `buildkite/mono/checks`; the two legacy contexts are no longer required.
4. The `bazel-build-test` step's two legacy `notify` entries have been removed, since branch protection no longer requires them.

## Rename contract

If you rename a step key in `pipeline.yml`:

1. Update the `notify: github_commit_status: { context: ... }` value in that step to match.
2. Update the table above.
3. Update the `required_status_checks` in GitHub branch protection (Settings → Branches → main).
4. Verify the new check name appears in a real build before the old protection rule is deleted — otherwise the gate silently drops out.

Open a PR that touches both files (`pipeline.yml` and this file) atomically.

## ci_watch remediation budget

`ci_watch` (engine) acts only on **required** check failures. The per-PR remediation budget defaults to **3 fix attempts** (configurable per-PR via `tasks.ci_attempt_budget` or per-product via `products.ci_attempt_budget`). A `auto_pr_maintenance_disabled` product flag or a per-PR opt-out label silences all automated CI remediation for that scope. See `tools/boss/engine/src/ci_watch.rs` for details.
