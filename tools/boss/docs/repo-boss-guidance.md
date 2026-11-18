# Per-repo worker guidance (`BOSS.md`)

A product repo can carry a `BOSS.md` at its root. The engine reads it from GitHub when it dispatches a coding worker and injects it into that worker's prompt. It is the home for rules that only apply to a worker **running under Boss** — for example "attach screenshots of visual changes with `boss attach`" — and that therefore should not sit in `CLAUDE.md` / `AGENTS.md`, which a worker started outside Boss reads too.

## Which file holds what

| Where                     | Who sees it                                       | Versioned with the repo | Put here                                                                                                                                                                 |
| ------------------------- | ------------------------------------------------- | ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `CLAUDE.md` / `AGENTS.md` | Every agent in the repo, Boss or not              | Yes                     | Rules true for any contributor: build and test commands, conventions, layout.                                                                                            |
| `BOSS.md`                 | Boss-dispatched **coding** workers, for that repo | Yes                     | Worker rules that mention Boss: `boss attach`, how Boss PRs should read, Boss-specific gates.                                                                            |
| `BOSS_COORDINATOR.md`     | The coordinator only                              | Yes                     | Coordinator rules: validation gates it should demand, designs that must not be filed as defects. See [coordinator-product-guidance.md](coordinator-product-guidance.md). |
| Product dispatch preamble | Every worker of the product (all kinds), via Boss | No — a Boss setting     | Short, product-wide dispatch instructions set on the product in Boss (the `--dispatch-preamble` product setting). Not reviewed through a PR.                             |

Rules of thumb:

- If a human or a non-Boss agent would benefit from the rule, it is `CLAUDE.md` / `AGENTS.md`.
- If it names Boss concepts and is for the person doing the work, it is `BOSS.md`.
- If it is about _managing_ work (filing, probing, merging), it is `BOSS_COORDINATOR.md`; coordinator vocabulary never goes in a worker's context.
- Reach for the dispatch preamble only for a setting you want to change without a PR, or one that must also reach non-coding workers. Anything that deserves review and history belongs in a repo file.

Both `BOSS.md` and the preamble can apply to the same worker. The preamble is unchanged by this mechanism.

## Which executions get it

Coding executions only:

- implementation of chores and project tasks (`chore_implementation`, `task_implementation`);
- revisions (`revision_implementation`), which include review-finding fixes; and
- the engine-minted fixes that edit a PR branch: merge-conflict resolution (`conflict_resolution`) and CI remediation (`ci_remediation`).

Not design (`product_design`, `project_design`), investigation (`investigation_implementation`), PR review (`pr_review`, `pr_review_guide`), answer agents, automation triage or progression.

The list is one exhaustive `match` in `receives_repo_boss_guidance` (`tools/boss/engine/execution-runner/src/repo_guidance.rs`), covered by a test. A new `ExecutionKind` does not compile until someone decides which side it is on. An execution that is not on the list does not even cost a GitHub read.

## Where the content comes from

`BOSS.md` on the repo's **default branch, at its HEAD at dispatch time**. It is read exactly the way `BOSS_COORDINATOR.md` is (see [coordinator-product-guidance.md](coordinator-product-guidance.md) for the mechanism): one `commits/HEAD` probe resolves the default branch to a sha, then the file is fetched at that sha through the sha-keyed body cache. A new push is therefore picked up by the next dispatch, and a worker is always given one concrete commit's text. The code is shared: `DesignDocsService::fetch_guidance_file` for the read, `load_product_guidance_file` and `settle_with_cache` (`tools/boss/engine/core/src/coordinator_guidance.rs`) for the budget, state classification and cache, with `GuidanceFile` selecting the file. `RepoBossGuidanceLoader` (`tools/boss/engine/core/src/repo_boss_guidance.rs`) is the adapter the prompt composer calls.

The read is bounded by `DISPATCH_FETCH_BUDGET` (10 s). A read that overruns it is a failed read, handled as below.

Local dispatch (`PaneSpawnRunner`) and remote SSH dispatch (`SshHostAdapter`) both compose the prompt through the same function, so both carry the block.

## What the worker sees

The block sits directly under the `[product-preamble]` block, if there is one, and above the task text:

```
[repo-boss-guidance repo=spinyfin/mono sha=1234567890abcdef1234567890abcdef12345678]
Boss-specific rules for this repo (`BOSS.md` at the default branch's HEAD). They apply to this run alongside the repo's CLAUDE.md / AGENTS.md.
<the file's text>
[/repo-boss-guidance]
```

The markers record the source, so a transcript shows which version a worker was given.

| Situation                                     | What the prompt carries                                                                                                                                                    | Engine log                          |
| --------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------- |
| File present, under the cap                   | The block above, with `repo=` and `sha=`.                                                                                                                                  | `info`: repo, sha, bytes            |
| No `BOSS.md` in the repo                      | Nothing. No block, no note.                                                                                                                                                | `debug`                             |
| Product has no repo / not a GitHub remote     | Nothing.                                                                                                                                                                   | `debug`                             |
| File over 32 KiB                              | A block with a one-line note giving the size and the cap, and saying the guidance was **not** included. The text is withheld, never truncated.                             | `warn`: repo, sha, bytes, cap       |
| GitHub unreachable, earlier file cached       | The cached text, marked `[repo-boss-guidance repo=… sha=… cached read_at=…]`, with a first line saying it is a cached copy, why the fetch failed and that it may be stale. | `warn`: sha, read time, fetch error |
| GitHub unreachable, nothing cached            | A block with a one-line note that the repo's Boss guidance could not be loaded, with the reason. Dispatch proceeds.                                                        | `warn`: repo, reason                |
| GitHub unreachable, cached read found no file | A block with a one-line "could not be loaded" note, the fetch error, and the sha and read time of the earlier absence.                                                     | `warn`                              |

A GitHub problem never blocks dispatch and never silently drops the guidance: it is either served from the cache, or announced in the prompt. In the three note cases the worker is told to mention it in its final summary, so the gap reaches a human.

### Cache

Every successful read (a file, or a confirmed absence) is saved as the product's last-known-good result in the engine's `metadata` table under `repo_boss_guidance_cache.<product_id>`, with its repo, sha, text and read time. It is a separate namespace from the coordinator's `coordinator_guidance_cache.<product_id>`, so one file's result can never be served for the other, and, as for the coordinator file, an entry is only served for the `owner/repo` it came from and never expires. Over-cap reads neither replace nor clear it.

## Size

32 KiB (`MAX_REPO_GUIDANCE_BYTES`, the same value as the coordinator file's cap). The file is injected into every coding worker's prompt on every dispatch, so keep it to rules a worker needs on every run. Link out to a longer doc rather than inlining it.

## Verifying

- Unit and wiring tests: `bazel test //tools/boss/engine/execution-runner:runner_test //tools/boss/engine/core:engine_lib_test //tools/boss/engine/design-docs:design-docs_test //tools/boss/engine/work-db:work_db_test`.
- To see a block in a real prompt, dispatch a chore at a product whose repo has a `BOSS.md` on its default branch and read `.claude/initial-prompt.txt` in the worker's workspace, or the first user turn in the transcript.

## Changing a rule

Land it as a normal PR to the product repo's `BOSS.md`. The next dispatch after it merges carries the new sha; workers already running keep the version they were started with.
