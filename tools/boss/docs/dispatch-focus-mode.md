# Dispatch focus mode

Focus mode restricts **automatic** dispatch to a chosen set of projects and products. It replaces the manual pattern of pausing global dispatch and force-launching every item in the projects being worked on.

```
bossctl dispatch focus set <project-or-product>...   # replace the set
bossctl dispatch focus add <project-or-product>...
bossctl dispatch focus remove <project-or-product>...
bossctl dispatch focus clear                          # focus mode off
bossctl dispatch focus show                           # the set + how many rows it holds
```

Targets are project or product ids, project short ids (`P<n>`), or product slugs. Tasks are refused: focus scopes whole projects and products. `bossctl dispatch state` and `bossctl state` print the focus set; `bossctl dispatch diagnose <id>` says whether focus is holding that execution.

Focus mode is on while the set is non-empty. The set is persisted in the engine metadata KV (`dispatch_focus`) and restored before the first scheduler drain, so a restart does not release held rows. If the persisted value cannot be read after a few attempts, the engine starts with focus off and `bossctl dispatch focus show` / `bossctl dispatch state` print a warning with the error until the next focus update; re-apply the set with `bossctl dispatch focus set`.

## It is an admission gate and nothing else

Focus never rewrites a work item: no autostart flip, no deferral, no status change. Out-of-scope rows keep their `ready` execution untouched and carry the `focus_mode` dispatch wait reason, exactly as rows carry `dispatch_paused` or `interactive_concurrency_cap`. Clearing focus, or adding a row's project or product, lets them dispatch on the next pass. Rows are held, never dropped.

Pre-merge review **batches** are the one exception, because a batch holds a four-unit review-pool reservation from creation until it completes or fails. While focus is on, an out-of-scope root is not admitted (it goes through the same deferred-admission sweep as a full pool). Setting or narrowing focus releases never-started out-of-scope batches so in-scope reviews can use those reservations; a batch that already has a started member keeps its reservation. An operator all-spawns dispatch pause (reviews held) uses the same rule; the default `new_work` pause does not, because reviews stay exempt and can still start. A pause persisted without a scope resolves from its origin exactly as the coordinator restores it (operator: `new_work`). The spawn-breaker's automatic pause is transient and does not defer admission or release reservations. Explicitly started batches (`bossctl review start`) are never released and are admitted even when focus or an all-spawns pause would hold the PR for automatic admission, because they are requested by name and their members can still be run with `bossctl agents launch`. A batch with a member already claimed for spawn is also kept. The deferred-admission sweep checks the hold locally before fetching PR metadata, so held roots with no review history cost no external fetch per pass; roots with review history still run the immutable-head settlement checks.

## What is in scope

A work item is in scope when any of these holds (`tools/boss/engine/work-db/src/dispatch_focus.rs` is the single implementation):

1. its `project_id` is a focused project. A project's design seed task is an ordinary task with `project_id` set, so it counts as the project;
2. its `product_id` is a focused product (any project, or none);
3. it was explicitly requested with `bossctl agents launch`. This fact is stored on the work item for its lifetime and survives engine restarts;
4. its lineage roots in an in-scope item. A `revision` (including engine-minted review, CI-fix and merge-conflict revisions) resolves through each parent in its chain. A `followup` (including a post-merge review followup) resolves through `origin_task_short_id` to the task it followed up. Both steps repeat.

Executions keyed by something other than a task are attributed to the task that owns them:

| Execution           | Attributed to                                                                                                                         |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `pr_review`         | the task whose PR is reviewed (`work_item_id`), then rules 1-4. A review of an out-of-scope PR is held with `focus_mode`, not skipped |
| `pr_review_guide`   | the root task of the comparison's series                                                                                              |
| `answer_agent`      | the task owning the commented design/investigation doc, or the root task of the commented review guide's series                       |
| `automation_triage` | the automation's product                                                                                                              |

A row that cannot be attributed to anything is out of scope: focus admits only what it can prove is in scope. If scope cannot be resolved at all (a database error), the pass holds every ready row.

## Precedence

- The **global dispatch pause** overrides focus. The focus gate runs after the pause gate and only ever holds more, so focus never resumes anything while paused, and a paused row shows `dispatch_paused` rather than `focus_mode`. Under the default `new_work` pause, PR reviews stay exempt from the pause, so they follow focus instead. An `all_spawns` pause holds reviews too, so pre-merge admission defers those batches the same way focus does rather than letting them occupy reservations they cannot use.
- The spawn-breaker's half-open recovery probe is an automatic path, so it respects focus: under a breaker pause it picks its canary only from ready rows inside the focus set, and admits nothing when every ready row is out of scope or scope cannot be resolved. Explicit launches and review-start requests grant focus admission. A breaker-origin pause is never overridable: persisted operator-forced admission does not dispatch through it.
- `bossctl agents launch` (`force_dispatch`) bypasses focus, as it bypasses an operator-origin pause. Its engine-minted reviews, CI-fixes, merge-conflict revisions and post-merge reviews inherit focus admission for the lifetime of the explicitly launched item. That inherited admission does not bypass the pause or concurrency cap. `bossctl work start --force` (the pause-only override) does **not** bypass focus: the admission preview reports a `focus_mode` blocker and the request is refused with the same guidance.
- The interactive concurrency cap applies as usual to in-scope rows.
- `bossctl review start --pr <n>` records the same durable focus admission as an explicit launch, for every reviewer in the admitted batch and the item's later review follow-ups. Reservation capacity, pauses, and concurrency limits still apply; other out-of-focus items remain held.
- A review guide (`prgc_…`, or its execution id) cannot be launched on its own: `agents launch` refuses with the parent work item's launch command. Both `dispatch diagnose` and focus-held review attention name that command, so starting the parent releases its review pipeline from focus.
- Priority `critical` bypasses the pause and the cap but not focus.
- Answering a worker's blocking question mints a ready execution for the row; under focus mode an in-scope row dispatches without any force-launch.
- Work created by answering an architectural-mismatch question carries a persisted operator-forced admission. Once that work is ready, the drain dispatches it through an operator-origin pause and outside the focus set. Writer exclusion, the claim, and the lease bound still apply. A breaker-origin pause holds the row as `dispatch_paused` and keeps the flag. The admission is consumed when the row is admitted or goes terminal.
