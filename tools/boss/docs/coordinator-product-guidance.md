# Per-product coordinator guidance (`BOSS_COORDINATOR.md`)

The coordinator system prompt is generic across products, but product-specific coordinator rules kept accumulating in it, in private coordinator memory, or in the session handoff. None of those is the right home: the prompt is shared by every product, memory does not survive or version, and the handoff is for volatile facts. This document describes the mechanism that replaces them: a `BOSS_COORDINATOR.md` at the root of each product repo, read by the engine from GitHub and handed only to the coordinator.

## What the file is

`BOSS_COORDINATOR.md` at the repo root holds coordinator-only operating rules for that product: its build and validation gates, deliberate design choices that must not be filed as defects, where its debugging docs live, how to investigate its own state. It binds the coordinator for work on that product, alongside the generic prompt.

It is deliberately distinct from `AGENTS.md` / `CLAUDE.md`, which every worker reads. Coordinator vocabulary (work items, probes, engine state, dispatch) does not belong in a worker's context, so coordinator rules never go in those files, and worker rules never go here.

The seed for `spinyfin/mono` is the repo's own [`BOSS_COORDINATOR.md`](../../../BOSS_COORDINATOR.md).

## How it is loaded

The coordinator does not lease repos, so the file is read the way design docs are: GitHub is the source of truth, Boss stores `(repo, path, ref)`, and the content is fetched at read time. There is no Boss-internal mirror.

A read, implemented in `boss_engine_design_docs::DesignDocsService::fetch_coordinator_guidance` (`tools/boss/engine/design-docs/src/coordinator_guidance.rs`):

1. parses the product's `repo_remote_url` into `owner/repo` (not a GitHub URL → `NotGitHub`; no URL → `NoRepoConfigured`);
2. resolves the default branch (memoised from the Designs-tab listing cache when present);
3. probes the default branch's HEAD sha — one tiny request;
4. fetches `BOSS_COORDINATOR.md` **at that sha**, through the same sha-keyed body cache design docs use. A sha is immutable, so a cache hit needs no revalidation, and a push is picked up by the very next read because the sha probe precedes the lookup.

### Versioning

The version the coordinator is acting on is always the commit sha HEAD resolved to, and it is shown everywhere the content is: in the session-start brief, in `boss guidance show`, and in the audit event. There is no branch-named "latest" copy anywhere; a body is only ever associated with a sha.

### When it is fetched: both, by design

- **At coordinator session start**, for every non-archived product. `start_new` (`tools/boss/engine/core/src/coordinator_tmux.rs`) reads them concurrently, each bounded by `SESSION_START_FETCH_BUDGET` (15 s), and `compose_start_brief` injects the result as a "Product coordinator guidance" section of the session-start brief, after the handoff and before the consumption steps. A fresh session is therefore bound by product rules on its first turn with no action on its part. A product whose read overruns the budget is reported as `Failed` naming the budget; the launch is never blocked past it.
- **On demand**, via `boss guidance show [--product <id>]` (`FrontendRequest::ListCoordinatorGuidance`). This always re-probes HEAD, so it is the way to see a change that landed mid-session, retry a read that failed at launch, or confirm the sha in context. The generic prompt tells the coordinator to run it after a chore that edits a product's file merges, when a product is added mid-session, and when the brief reported a fetch failure.

Lazy-only loading was rejected because it makes being bound by the rules depend on the model choosing to run a command. Start-only loading was rejected because a coordinator session can live for days.

### How the coordinator learns the file changed mid-session

Recommendation, in order of preference:

1. **Coordinator-driven re-read on the known trigger (implemented).** The dominant way a `BOSS_COORDINATOR.md` changes is a chore the coordinator itself filed; it knows when that PR merges and re-reads with `boss guidance show --product <id>`. This costs nothing until something changes.
2. **Engine-driven nudge on merge (recommended follow-up, not implemented here).** The merge poller already observes every Boss-tracked PR reaching `merged`. When a merged PR's file list contains `BOSS_COORDINATOR.md`, the engine would re-read the product's guidance and deliver the new sha and text to the coordinator pane through the same send-keys channel the prompt-change nudge uses. This covers the case where the coordinator forgets, with no standing GitHub traffic, and is the mechanism to build next.
3. **Polling with conditional requests — rejected.** The design-doc body cache's ETag revalidation fits a document the UI is actively showing, not a file that changes a few times a month: it would add a standing request per product per interval for almost no hit rate.
4. **Re-read on every product-scoped action — rejected.** The coordinator's actions are CLI calls, most of which do not pass through a product; threading a GitHub round trip into each would add latency everywhere for a change that is rare.

A push straight to the default branch that bypasses Boss (a human editing the file directly) is picked up by the next session start or the next `boss guidance show`; that is acceptable for a file whose edits are supposed to go through chores.

### What the coordinator sees

**Full text, injected,** not a pointer. The point of the mechanism is that the rules bind the session without it having to decide to read them. Each product's entry carries its name and id, `owner/repo`, the sha, the byte size, and the state line, then the body between `--- BOSS_COORDINATOR.md for <product> begins/ends ---` fences.

**Size cap: 32 KiB per file** (`MAX_COORDINATOR_GUIDANCE_BYTES`). The file is a set of rules, not a manual; the cap also bounds the session-start brief when every product's file is inlined. A file over the cap is reported as `OverCap` with both sizes and its text is **withheld** — not truncated, which would silently drop rules from the end — and the coordinator is told to raise it with the operator and file a chore to trim the file.

### Failure behaviour: every state is explicit

Each product gets exactly one entry, whatever happened. The states (`CoordinatorGuidanceState` in `tools/boss/protocol/src/types/coordinator_guidance.rs`):

| State                | Meaning                                                                                                      | What the coordinator is told                                                                                   |
| -------------------- | ------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------- |
| `loaded`             | File read at the sha, under the cap.                                                                         | Full text; sha; size.                                                                                          |
| `missing`            | The sha probe succeeded (repo reachable and visible) and the file 404s at that sha.                          | This product has no guidance file; file a chore to add one when a durable lesson comes up.                     |
| `over_cap`           | File exists at the sha but exceeds 32 KiB.                                                                   | Both sizes; text withheld; tell the operator and file a trim chore.                                            |
| `failed`             | GitHub could not be read: unreachable, not authorized, rate limited, repo 404, or the launch budget ran out. | The classified reason; "nothing is known — do NOT treat this as no guidance"; retry with `boss guidance show`. |
| `no_repo_configured` | The product has no `repo_remote_url`.                                                                        | Nowhere to read from.                                                                                          |
| `not_github`         | The remote is not a github.com URL.                                                                          | Cannot be read through the engine's GitHub path; nothing is known.                                             |

The distinction that matters most is `missing` versus `failed`: a 404 is only read as "no file" when the sha probe has already proven the repo reachable in the same read. A 404 on the probe itself (GitHub's answer for a private repo the token cannot see) is `failed`. The brief's consumption steps require the first reply to list each product's state in one line, so a failure reaches the operator rather than being worked around.

## CLI

```sh
boss guidance show                    # every non-archived product
boss guidance show --product boss     # one product (id, slug, or name)
boss guidance show --json             # { "guidance": [CoordinatorGuidanceView, ...] }
```

Human output per product: a header, the state line (`LOADED from spinyfin/mono @ <sha> (<n> bytes)`, `MISSING: …`, `OVER CAP: …`, `FETCH FAILED: …`), the read time, and the body when loaded. The state line is produced by `CoordinatorGuidanceView::describe_state`, which the session-start brief also uses, so the two surfaces never describe a state differently.

`ListCoordinatorGuidance` is coordinator-only at the worker-tier gate (`boss_worker_policy`): a cube worker calling it gets a `CoordinatorOnly` denial.

## Forensics

`engine-audit.log` records `coordinator_guidance_brief` on every fresh coordinator launch: `start_reason` and, per product, `product_id`, `product_name`, `owner_repo`, `state`, `git_ref`, and `bytes` — never the body. Together with `coordinator_handoff_brief` (see [coordinator-session-handoff.md](coordinator-session-handoff.md)) it answers "which guidance, at which sha, was session X launched with?" without opening the database. On-demand reads log one `coordinator guidance read on demand` line per product at `info`.

## Adding or changing a rule

A durable, product-specific coordinator lesson is landed by filing a chore against that product that quotes the rule verbatim and names `BOSS_COORDINATOR.md`. The generic prompt's "Durable lessons" section routes generic rules to the prompt source and product-specific ones here; neither goes into private memory or `AGENTS.md`.
