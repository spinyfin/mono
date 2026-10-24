# Revision recovery and disposable PR bookmarks

## Evidence and ownership

The reported appoint PR #139 failure names the error emitted by `engine/recovery/src/execution_bookmark/reconcile.rs::resolve_pr_head`. The earlier mono PR #3234 conflict reaches the same function. These errors describe the PR alias, not missing GitHub commits or missing execution recovery heads.

Read-only `gh api repos/brianduff/appoint/pulls/139` and `gh api repos/spinyfin/mono/pulls/3234` calls during this investigation both returned `state: open` and `merged_at: null`. [appoint PR #139](https://github.com/brianduff/appoint/pull/139) had head `5640f808b590aeb15267269289303406f70e274f` on `boss/exec_18e15196c328fa78_463`; [mono PR #3234](https://github.com/spinyfin/mono/pull/3234) had head `15b38100edd7e88112fc63c609ae47a7b42656b2` on `boss/exec_18e092fd7d4c25c8_287`. Thus GitHub still supplied concrete recovery anchors for both reported cases at investigation time; these reads do not establish their historical heads at failure time.

Cube owns creation of `pr/<number>`: `tools/cube/src/app/workspace_ops.rs::workspace_goto` fetches the GitHub remote, resolves the PR head branch, then sets both the branch and PR alias with `--allow-backwards`. Cube's PR publication paths also set the alias. Boss's local and SSH `prepare_recovery_pr` implementations call that checkout before reconciliation and restore the execution's three saved pointers afterward.

That alias is deliberately disposable. `tools/cube/src/app/gc.rs::collect_unreferenced_pr_bookmarks` removes PR aliases no working copy references; it does not consult Boss's recovery records. Shared jj operations can also merge concurrent alias moves into a bookmark conflict. A successful checkout is not a durable guarantee that the alias will still exist or resolve uniquely when a later command reads the store. Per-PR worker sequencing does not serialize global Cube cleanup or every operation on the shared store.

The engine owns the defect: it relied on this temporary Cube alias as a durable recovery prerequisite and classified its loss as a permanent execution-pointer failure. Changing Cube to retain every PR alias indefinitely would not repair an already missing or conflicted alias. The engine must reconstruct the alias from GitHub and retain its existing preserved-work containment checks.

Read-only inspection of the appoint shared store's jj operation history establishes the PR #139 race directly. Operation `b57628bc9e27` set `pr/139` to `55f56cf7902f2eb3d3d5cab07858edd4dd8c90f3` at 2026-10-23 22:47:00.185 CDT. Operation `f04d3e7ef77e` deleted it at 22:47:01.050 CDT; its recorded arguments are `jj bookmark forget pr/139`. Thus checkout did write the alias, but a separate command removed it 865 milliseconds later, during the reported failing dispatch. This command matches Cube's alias cleanup mechanism. The operation records do not identify the caller process, so they establish the deletion without proving which Cube invocation initiated it.

The production coordinator database was not read. The exact concurrent operations behind mono PR #3234's conflict and the Cube binary versions used by these dispatches remain unverified. The regression fixtures reproduce both missing and conflicted aliases in a real jj store.

## Why another execution was created

The coordinator supplied a second completed-then-redispatched incident on [appoint PR #143](https://github.com/brianduff/appoint/pull/143), dated 2026-10-24: fix commit `70e8bdd0` pushed at 04:02:15Z, implementation completed at 04:04:14Z, review of the new head started at 04:04:21Z, and another implementation execution started at 04:06:24Z while that review was still running. The owner stopped it around 04:08Z. These timestamps are coordinator-provided evidence, not reconstructed from the local database.

Before this fix, `core/src/work/dispatch_helpers.rs::reconcile_revision_execution` handled ready/waiting dependency rows and live/claimed rows explicitly. A completed revision implementation fell through to the terminal fallback, which inserted another implementation execution if the work item was still eligible. A later completed PR-review execution also shadowed the completed implementation: the cross-kind guard suppressed reminting only while the review was live. Completion and work-item transition are separate observations, so reconciliation could encounter either case while the task remained active. This is an automatic redispatch bug, not a deliberate pointer-reconciliation-only pass: the fallback creates a new worker execution.

The guard now checks the latest implementation specifically, preserving its completed result even after a review finishes. Explicit restart still creates a new ready implementation through its existing path. The regression leaves the revision active, marks its implementation completed, and runs product reconciliation three times without minting another execution, then adds a newer completed review and repeats three more times.

There were additional routes around the product reconciler: `reconcile_active_dispatch` treats a terminal execution or an execution absent from its live-slot oracle as needing dispatch, and `rescan_active_dispatch` remints terminal executions on active autostart rows. Their primary-PR review guard explicitly excludes revisions, which do not own the PR URL. A review attached to the chain root therefore did not protect the completed revision from these scans. The shared completed-revision admission predicate now protects both scans and the dispatch-failure recovery candidate list as well as product reconciliation. The PR #143 regression keeps the review running, covers reviews attached to either the root or revision, and invokes all three reconciliation/scan entry points three times. The original implementation stays completed, the review stays running, and no replacement implementation is created.

## Repair and human visibility

After bound-PR checkout, both host adapters verify the alias. Missing or conflicted aliases trigger a fresh GitHub head lookup using the existing `git_utils::gh_cli` helpers, a fetch of the head branch, and an explicit alias update to the returned full SHA. Fetch preserves and restores the execution's baseline, recovery, and publication pointers. Reconciliation retains its content-equivalence checks: repairing a PR alias is never permission to discard unpublished work. A second check at reconciliation handles alias loss between preparation and consumption with one repair attempt, not an unbounded retry loop.

GitHub metadata lookup has a bound, and jj commands retain their existing bounds. Failure to import the authoritative head becomes an explicit recovery error. In addition to the execution failure signal, an unrecoverable revision raises a deduplicated attention item on the root PR card, explaining that later revisions remain gated and that the blocked revision needs repair and explicit restart. This signal requires human acknowledgement; unrelated review activity on the root must not clear it. No revision is marked done or failed merely to release its dependents.

Schema migration 51 rearms only the two exact legacy PR-alias error messages once. It preserves the failed attempt and creates a dependency-gated recovery attempt through the existing migration helper. Other pointer failures and new repair-failure messages are not rearmed.

## Regression coverage

- `execution_bookmark::pr_anchor_tests`: missing and conflicted PR aliases repaired from a fetched head; execution pointers unchanged by the repair; normal containment reconciliation succeeds afterward; strict recognition of legacy blocker text.
- `work::tests::revision_recovery`: completed implementation not reminted across three reconcile passes; root PR attention deduplicated without changing revision status; upgrade rearm runs once and preserves the original failed execution.

These use temporary stores and databases, not the reported live PRs. Validation commands and their observed results are recorded in the PR evidence section.
