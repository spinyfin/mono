# host-hooks

Buildkite agent hooks that take the per-host heavy-Bazel lock (`../steps/host-bazel-lock.sh`) for the whole job, for pipelines whose repos have not adopted the lock in their own step scripts (`appoint`, `flunge-ci`). They are installed on the Mac hosts, not run from a checkout. Install steps and rationale: [`../mac-agents-runbook.md`](../mac-agents-runbook.md).

- `pre-command` — acquires the lock (held by the job bootstrap process) when `BUILDKITE_PIPELINE_SLUG` is in `CI_HOST_BAZEL_LOCK_HOOK_PIPELINES` (default `appoint flunge-ci`).
- `pre-exit` — releases it.
