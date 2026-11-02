# Dispatch focus mode

Focus mode restricts **automatic** dispatch to a chosen set of projects and products. It replaces the manual pattern of pausing global dispatch and force-launching every item in the projects being worked on.

```
bossctl dispatch focus set <project-or-product>...   # replace the set
bossctl dispatch focus add <project-or-product>...
bossctl dispatch focus remove <project-or-product>...
bossctl dispatch focus clear                          # focus mode off
bossctl dispatch focus show                           # the set + how many rows it holds
```

Targets are project or product ids, project short ids (`P12`), or product slugs. Tasks are refused: focus scopes whole projects and products. `bossctl dispatch state` and `bossctl state` print the focus set; `bossctl dispatch diagnose <id>` says whether focus is holding that execution.

Focus mode is on while the set is non-empty. The set is persisted in the engine metadata KV (`dispatch_focus`) and restored before the first scheduler drain, so a restart does not release held rows.

## It is an admission gate and nothing else

Focus never rewrites a work item: no autostart flip, no deferral, no status change. Out-of-scope rows keep their `ready` execution untouched and carry the `focus_mode` dispatch wait reason, exactly as rows carry `dispatch_paused` or `interactive_concurrency_cap`. Clearing focus, or adding a row's project or product, lets them dispatch on the next pass. Rows are held, never dropped.

## What is in scope

A work item is in scope when any of these holds (`tools/boss/engine/work-db/src/dispatch_focus.rs` is the single implementation):

1. its `project_id` is a focused project. A project's design seed task is an ordinary task with `project_id` set, so it counts as the project;
2. its `product_id` is a focused product (any project, or none);
3. its lineage roots in an in-scope item. A `revision` (including engine-minted review, CI-fix and merge-conflict revisions) resolves through its chain root. A `followup` (including a post-merge review followup) resolves through `origin_task_short_id` to the task it followed up. Both steps repeat.

Executions keyed by something other than a task are attributed to the task that owns them:

| Execution           | Attributed to                                                                                                                         |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `pr_review`         | the task whose PR is reviewed (`work_item_id`), then rules 1-3. A review of an out-of-scope PR is held with `focus_mode`, not skipped |
| `pr_review_guide`   | the root task of the comparison's series                                                                                              |
| `answer_agent`      | the item the comment is on                                                                                                            |
| `automation_triage` | the automation's product                                                                                                              |

A row that cannot be attributed to anything is out of scope: focus admits only what it can prove is in scope. If scope cannot be resolved at all (a database error), the pass holds every ready row.

## Precedence

- The **global dispatch pause** overrides focus. The focus gate runs after the pause gate and only ever holds more, so focus never resumes anything while paused, and a paused row shows `dispatch_paused` rather than `focus_mode`. Under the default `new_work` pause, PR reviews stay exempt from the pause, so they follow focus instead.
- `bossctl agents launch` (`force_dispatch`) bypasses focus, as it bypasses the pause. `bossctl work start --force` (the pause-only override) does **not**: the admission preview reports a `focus_mode` blocker and the request is refused with the same guidance.
- The interactive concurrency cap applies as usual to in-scope rows.
- Priority `critical` bypasses the pause and the cap but not focus.
- Answering a worker's blocking question mints a ready execution for the row; under focus mode an in-scope row dispatches without any force-launch.
