# Boss: External Issue Tracker Sync (GitHub Projects)

## Overview

Boss owns a private taxonomy of work — `products`, `projects`, `tasks`, `chores`
— stored in SQLite. Teams using Boss in shared repos already track work in an
external tracker (GitHub Projects today, plausibly Jira or Linear tomorrow).
The two systems drift: an issue gets filed upstream and a human re-types it into
Boss as a chore, or a Boss task is marked `done` while the upstream issue
stays open. This is the "two pieces of paper" problem.

This design proposes a one-way ingestion layer that pulls upstream tracker
state into Boss's existing taxonomy. The initial backend is
**GitHub Projects + Issues** (against `spinyfin/mono`'s "Boss" project). The
seam is an internal `ExternalTracker` trait so that a Jira or Linear
implementation can land later without re-architecting the engine's reconciler.
Sync is **product-scoped**: every product can be bound to at most one upstream
tracker, and every work item under that product inherits the binding.

The artefact this design produces is the doc itself plus a list of follow-up
implementation chores. No code is written here.

---

## Goals

- A single source of truth for the *existence* and *status* of a Boss work
  item that has an upstream issue: the upstream issue. Boss reads, Boss does
  not duplicate-write.
- A product-level binding so all work items under a product share the same
  upstream surface. No per-task wiring.
- A stable internal pointer (`work_item.external_ref`) that survives renames,
  re-titles, and column moves upstream — keyed by `(kind, repo, issue_number)`,
  not by title.
- A periodic reconciler that runs per-product, is **idempotent** (re-running it
  has no side effects unless upstream changed), and degrades gracefully on
  rate-limit / network failure.
- Auto-import of new upstream issues as Boss chores so a human filing an issue
  in GitHub doesn't have to mirror-type it.
- Auto-close-mirror: when the upstream issue closes, the Boss work item moves
  to `done` (or `archived` for `not planned` closures).
- Automatic PR association: when a PR mentions an upstream issue
  (`Fixes #N` / `Closes #N`), and that issue is bound to a Boss work item, the
  PR URL lands on the Boss row's `pr_url` column.
- An `ExternalTracker` trait surface narrow enough that the engine's
  reconciler loop has no GitHub-specific code paths.

## Non-Goals

- **Bidirectional field sync.** Assignees, labels, comments, milestones,
  custom fields beyond status — none of these mirror in v1. The body of an
  upstream issue is the description on the Boss row at *create* time; later
  edits do not re-sync.
- **Boss → upstream creation.** A Boss chore created locally does *not* spawn
  an upstream GitHub issue. v1 is one-way (upstream → Boss) with one narrow
  exception: reverse-close (closing the upstream issue when its Boss work item
  finishes), gated behind a product-config flag and disabled by default.
- **Multiple trackers per product.** One product → one tracker. A monorepo
  spanning two tracking surfaces (say, an open-source repo + an internal
  Jira) gets modelled as two products.
- **Non-GitHub backends in v1.** The trait exists so Jira / Linear can land
  later, but only `github` ships.
- **Webhooks / push-based delivery.** v1 is polling. The design notes a
  webhook path as a future extension because the trait surface accommodates
  it, but no webhook receiver is shipped.
- **Bidirectional field sync of `priority`.** Priorities are a Boss-only
  concept for now; nothing reads or writes GitHub priority fields.
- **Backfill of pre-existing Boss work items into upstream.** Items that
  exist in Boss *before* the tracker binding is set do not get a GitHub issue
  auto-created for them. They are simply un-bound (`external_ref IS NULL`)
  forever, unless a human explicitly links them via CLI (Q9 covers this).
- **Comment mirroring** in either direction.
- **GitHub Issues *outside* the configured Project.** Issues live in repos
  but appear in a Project; v1 ingests only issues that appear in the bound
  Project. Repo-only issues are out of scope.

---

## Alternatives Considered

Before settling on the chosen approach, three other shapes were on the table.

### Alternative A — Bidirectional sync with conflict resolution

A symmetric sync: edits in either system propagate to the other, with a
conflict resolution layer (last-writer-wins by timestamp, or field-level
merges). This is what most "real" trackers do (Linear's Slack integration,
Jira's GitHub bridge).

**Why not.** Two reasons. First, the engineering cost is roughly 5× one-way:
every field needs its own propagation path, conflict logic, and a way to
detect "is this edit ours or theirs?" (otherwise sync ping-pongs). Second,
the v1 acceptance criteria explicitly only need status + PR. Anything else is
out of scope. Bidirectional is the natural place to land if v1 succeeds and
the team starts wanting labels and assignees — the trait surface here is
shaped so a bidirectional implementation can extend it without re-laying
foundations.

### Alternative B — Manual link, no auto-import

The user filing an issue in GitHub also runs `boss chore create --link
github:spinyfin/mono#560`. Boss never polls upstream; it just remembers the
link and lets the user manually flip status when the upstream issue closes.

**Why not.** Defeats the point. The single biggest pain is "we filed it
upstream and forgot to mirror it into Boss." A manual link command doesn't
fix that — it just adds another step. The reconciler is precisely the value-
add: it sees the upstream state and reflects it in Boss without the user
remembering to do anything.

### Alternative C — Per-task `external_ref`, no product binding

Skip the product-level config entirely. Each Boss work item knows its own
upstream issue, set via a free-form `external_ref` field
(`github:spinyfin/mono#560`). The reconciler walks every work item with a
non-null `external_ref` and probes upstream individually.

**Why not.** Looks simpler but isn't, for three reasons:

1. **Auto-import is hard without a product binding.** If new GitHub issues
   should auto-appear in Boss, the reconciler needs to know *which Boss
   product* receives the new chore. Without a product-level binding, every
   new GitHub issue requires the user to first create a Boss work item and
   link it — which is the same problem as Alternative B.
2. **Tracker config bloat.** Per-task `external_ref` means each row needs to
   carry org / repo / project info. Most teams have one tracker per product;
   per-task storage is redundant for the common case and the absent shared
   identity makes batch operations (rate-limited probes!) hard to coalesce.
3. **No natural place for tracker-wide settings** (label filter, status
   mapping). Per-task means scattering them; product-level gives a single
   place to put them.

A per-task `external_ref` *does* still exist in the chosen design (Q4) — but
it stores only `{kind, issue_number, project_item_id}`, and inherits org /
repo / project from the product binding. Best of both shapes.

### Alternative D — Use the existing PR-detection pipeline only

Boss already detects PRs via the `pr_url_capture` / `merge_poller` pipeline.
One could argue: tasks already get PR-linked via `Fixes #N` footers, and
when a PR merges, the upstream issue closes "naturally" via GitHub itself —
no Boss-side sync needed.

**Why not.** This handles *PR-merge → issue close → Boss task closes*, but
only if (a) the Boss task was created in Boss first and (b) a worker pushed
a PR mentioning the issue. It does nothing for issues filed upstream that
need to *appear* in Boss, and nothing for issues closed manually upstream
(not via a PR). The merge poller is necessary but not sufficient.

---

## Chosen Approach

A periodic, product-scoped, one-way ingestion reconciler with a narrow
optional write-back path for reverse-close. The trait surface is structured
so the engine's reconciler has no GitHub-specific code, and a Jira
implementation could land as a second `ExternalTracker` impl plus config
schema.

The rest of this section answers each design question and its open question
in turn.

---

## Design Question 1 — Where Does the Binding Live?

### Options

- **(a) Three new columns on `products`.** Mirrors how `repo_remote_url`
  lives on `products`. Cheap; native filtering.
- **(b) One JSON column on `products`.** `external_tracker TEXT NULL`
  carrying a serialised `{kind, config}`. Cheaper migration; harder native
  SQL filtering.
- **(c) New `product_external_trackers` table** keyed by `product_id`.
  Future-proofs "multiple trackers per product."
- **(d) A row in the existing `metadata` key/value table.** Path of least
  resistance for migration; awful query shape.

### Discussion

(c) is overengineering for v1: non-goals explicitly rule out multiple
trackers per product. If that ever becomes a goal, a one-shot migration
promotes (a) or (b) into a table.

(d) makes every read hit a generic key/value table and every CLI path
re-parse strings; rejected.

The choice is between (a) and (b). The two upstream-tracker kinds we
anticipate (`github`, `jira`) have meaningfully different config payloads
(`org`/`repo`/`project_number` for GitHub; `host`/`project_key` for Jira) —
trying to fit them into discrete typed columns means each new backend churns
the schema. JSON is the right shape for config; only the *kind* discriminator
is worth promoting to a typed column for efficient filtering.

### Recommendation

A hybrid of (a) and (b): one typed column for the discriminator, one JSON
column for the kind-specific config.

```sql
ALTER TABLE products ADD COLUMN external_tracker_kind   TEXT;
ALTER TABLE products ADD COLUMN external_tracker_config TEXT;  -- JSON, kind-specific
```

`external_tracker_kind` is the load-bearing field. When `NULL`, the product
has no upstream binding and the reconciler skips it. When set
(`'github'` for v1), `external_tracker_config` carries a JSON payload whose
shape is validated against a kind-specific schema at write time:

```jsonc
// for kind = 'github'
{
  "org": "spinyfin",
  "repo": "mono",
  "project_number": 1,
  "label_filter": null,          // optional: array of labels; null = all
  "status_field_mapping": null   // optional: map of project status → boss status
}
```

The PAT / installation credential is **not** stored here. Resolved out of
band (Q11).

---

## Design Question 2 — The `ExternalTracker` Trait

### Surface

```rust
#[async_trait::async_trait]
pub trait ExternalTracker: Send + Sync {
    /// Identifier (`"github"`, eventually `"jira"`, etc.). Must match the
    /// `external_tracker_kind` column.
    fn kind(&self) -> &'static str;

    /// Validate a kind-specific config JSON at write time. Called by the
    /// CLI / RPC when the user binds a tracker.
    fn validate_config(&self, config: &serde_json::Value) -> Result<(), TrackerConfigError>;

    /// Fetch the current state of every upstream item in this product's
    /// configured scope. Returns a flat list — pagination is the impl's
    /// problem. Idempotent. Read-only.
    async fn fetch_items(&self, ctx: &TrackerContext) -> Result<Vec<UpstreamItem>>;

    /// Fetch a single upstream item by its stable id (used when the
    /// reconciler probes a single known issue rather than the whole list).
    async fn fetch_item(&self, ctx: &TrackerContext, ref_: &UpstreamRef) -> Result<Option<UpstreamItem>>;

    /// Optional write-back: close an upstream issue. Returns `Unsupported`
    /// for trackers that don't implement this (or have it disabled by
    /// config). Called only when reverse-close is gated on for the product.
    async fn close_item(&self, ctx: &TrackerContext, ref_: &UpstreamRef, reason: CloseReason) -> Result<()>;
}

pub struct TrackerContext {
    pub product_id: String,
    pub config: serde_json::Value,   // raw, per-kind
    pub credential: TrackerCredential, // resolved out-of-band
}

/// A stable upstream identifier. The fields are normalised across trackers
/// so the reconciler can treat them opaquely.
pub struct UpstreamRef {
    pub kind: String,           // "github" | "jira" | ...
    pub canonical_id: String,   // tracker-specific; for github: "spinyfin/mono#560"
    pub raw: serde_json::Value, // tracker-specific blob: { issue_number, project_item_id } for github
}

pub struct UpstreamItem {
    pub upstream_ref: UpstreamRef,
    pub title: String,
    pub body: String,
    pub status: UpstreamStatus,   // Open | Closed { reason: completed | not_planned }
    pub upstream_url: String,     // canonical web URL
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub pr_associations: Vec<UpstreamPrAssociation>,
    pub updated_at: i64,          // unix seconds
}

pub enum UpstreamStatus {
    Open,
    Closed { reason: ClosedReason },
}

pub enum ClosedReason { Completed, NotPlanned, Unknown }

pub struct UpstreamPrAssociation {
    pub pr_url: String,
    pub merged: bool,
    pub merged_at: Option<i64>,
}
```

`fetch_items` does pagination internally and is *bounded* by the product's
config (`label_filter`, `project_number`). The reconciler loop has no idea
whether the underlying call was one or fifty HTTP requests.

`UpstreamRef.canonical_id` is what gets stored in `work_items.external_ref`
(Q4) — opaque to the engine, parseable only by the tracker that produced
it.

### Why a flat list instead of streaming?

For v1, a typical Boss product has tens of upstream items, not thousands.
A flat `Vec<UpstreamItem>` is the simplest shape. If a tracker ever needs to
stream incremental updates (e.g. `since = last_sync_at`), the trait can grow
a `fetch_items_since` variant without breaking existing call sites.

### Why an async trait?

The reconciler runs inside Tokio. `gh` invocations are subprocess-bound but
cheap to schedule async. The trait does not constrain the impl to use HTTP
directly — the v1 `GitHubTracker` shells out to `gh` (Q3), but a future Jira
impl could use `reqwest`.

### Recommendation

The trait above. One file (`engine/src/external_tracker/mod.rs`); the
GitHub impl lives at `engine/src/external_tracker/github.rs`. No
GitHub-specific types leak into the reconciler.

---

## Design Question 3 — GitHub-Specific Implementation

### Choosing the GitHub interface

GitHub Projects has two query surfaces: the v3 REST API (per-issue) and the
v2 GraphQL API (per-project, with all custom fields). For "give me every
issue in project N with its current status field," GraphQL is the right
shape. For "fetch one issue by `(owner, repo, number)`," REST is fine.

Boss already shells out to `gh` heavily (`gh pr view`, `gh pr list`,
`gh api`). `gh` handles auth transparently via the user's GitHub login.
Standardising on `gh` (not raw `reqwest`) keeps auth out of Boss's
problem domain and inherits the user's existing `gh auth status`.

### Concrete `gh` invocations

```sh
# List all items in a project (v2):
gh api graphql -F org=spinyfin -F number=1 -f query='
  query($org: String!, $number: Int!) {
    organization(login: $org) {
      projectV2(number: $number) {
        items(first: 100) {
          pageInfo { hasNextPage endCursor }
          nodes {
            id
            content {
              __typename
              ... on Issue {
                number
                title
                body
                state
                stateReason
                url
                repository { nameWithOwner }
                labels(first: 20) { nodes { name } }
                assignees(first: 10) { nodes { login } }
                closedByPullRequestsReferences(first: 5) {
                  nodes { url merged mergedAt }
                }
                updatedAt
              }
            }
            fieldValues(first: 20) { ... }   # status field
          }
        }
      }
    }
  }'

# Fetch one issue (for single-item probes):
gh api repos/spinyfin/mono/issues/560
```

`closedByPullRequestsReferences` is GitHub's first-class field for "PRs that
will close this issue" — populated whenever the PR body contains
`Fixes #N` / `Closes #N` / `Resolves #N`. This is what powers behaviour (4),
PR association.

### Pagination

`items(first: 100)` plus a `pageInfo` loop. The impl pages until
`hasNextPage = false`. For products with <100 items (the common case), one
request suffices.

### Rate limits and backoff

GitHub's GraphQL rate limit is points-based (5000/hour for users). A
`fetch_items` for a 100-item project costs ~1 point. Even at a 1-minute
cadence per product, this is well under budget for ~10 products. The impl
records the `X-RateLimit-Remaining` header that `gh` exposes and trips an
exponential backoff if it drops below a threshold (say 100 remaining).

### Failure modes

- **Network failure / `gh` unavailable.** Return `Err(TrackerError::Transient)`.
  Reconciler logs, increments a `external_tracker.fetch_failed` counter,
  retries on the next tick. No state changes in Boss.
- **Project not found / 404.** `Err(TrackerError::ConfigInvalid)`. Surface
  as an attention item on the product: *"External tracker binding points
  at `spinyfin/mono` project #1 which does not exist or is not visible."*
- **Auth failure.** `Err(TrackerError::Auth)`. Same attention-item shape.

### Recommendation

`GitHubTracker` is a struct that owns a `gh` invocation helper (similar to
the existing `MergeProbe`). All `gh` calls are `tokio::process::Command`
shellouts. One GraphQL query for list, one REST call for single-item fetch.

---

## Design Question 4 — `work_items.external_ref` Storage and Lookup

### Where the per-row pointer lives

The reconciler needs two cheap lookups every tick:

1. Given an upstream item, find the Boss work item that mirrors it (so
   updates flow to the right row).
2. Given a Boss work item, find its upstream item (for reverse-close, for
   showing a "↗ #560" affordance on the kanban card).

Both are answered by a small typed pointer on the work item.

The existing `tasks` table is shared between `kind = 'project_task'` and
`kind = 'chore'`. Both kinds get the pointer; tasks rarely have upstream
issues today but might in the future, and adding the column to `tasks` is
cheaper than splitting storage.

### Schema

```sql
ALTER TABLE tasks ADD COLUMN external_ref_kind          TEXT;  -- 'github' | ...
ALTER TABLE tasks ADD COLUMN external_ref_canonical_id  TEXT;  -- 'spinyfin/mono#560'
ALTER TABLE tasks ADD COLUMN external_ref_raw           TEXT;  -- JSON, tracker-specific
ALTER TABLE tasks ADD COLUMN external_ref_synced_at     TEXT;  -- unix seconds, last upstream→boss reconcile

CREATE INDEX tasks_external_ref_idx
    ON tasks (external_ref_kind, external_ref_canonical_id)
 WHERE external_ref_canonical_id IS NOT NULL;
```

Three typed columns, not a single JSON blob:

- `external_ref_kind` lets the reconciler filter "all rows bound to GitHub"
  in one indexable predicate.
- `external_ref_canonical_id` is the lookup key for "is this upstream item
  already mirrored?" — checked once per upstream item per reconcile tick.
- `external_ref_raw` holds the tracker-specific extras (for GitHub: the
  `project_item_id`, which is needed for status field reads/writes).
- `external_ref_synced_at` is the last successful reconcile timestamp; used
  to surface stale rows.

The index is partial (`WHERE … IS NOT NULL`), so rows without an upstream
binding don't bloat it.

### Lookup methods on `WorkDb`

```rust
impl WorkDb {
    fn find_by_external_ref(&self, kind: &str, canonical_id: &str) -> Result<Option<WorkItem>>;
    fn set_external_ref(&self, work_item_id: &str, ref_: &UpstreamRef) -> Result<()>;
    fn clear_external_ref(&self, work_item_id: &str) -> Result<()>;
    fn list_external_refs_for_product(&self, product_id: &str) -> Result<Vec<(String, UpstreamRef)>>;
    fn touch_external_ref_synced_at(&self, work_item_id: &str, now: i64) -> Result<()>;
}
```

### Why not a separate `external_refs` table?

A side table keyed by `(work_item_id, kind)` would future-proof "multiple
upstream refs per work item" (e.g. an item mirrored in both Jira *and*
GitHub). v1 explicitly forbids that (non-goal: multiple trackers per
product), and a side table adds a join on every kanban render. Inline
columns are cheaper and we have the conversion path if the constraint ever
relaxes.

### Why include `external_ref_raw` as JSON?

Tracker-specific extras change. For GitHub today: the `project_item_id`
(needed for status field reads). For Jira tomorrow: the issue's project key
and version. Keeping these in a JSON blob, opaque to the engine, means
adding a new tracker doesn't require a schema migration to introduce its
extras.

---

## Design Question 5 — The Reconciler Loop

### Shape

A single `tokio::task` per engine, sweeping every bound product every
`reconcile_interval`. Per-product processing is *sequential* within the
sweep (one product's network calls don't block another's, but they don't
run in parallel either — pragmatism: v1 doesn't need parallelism for
~10 products and we avoid juggling `JoinSet` semantics).

```rust
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    registry: Arc<TrackerRegistry>,
    interval: Duration,
    metrics: Arc<Registry>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let outcome = run_one_pass(&work_db, &registry).await;
            // … emit metrics
            tokio::time::sleep(interval).await;
        }
    })
}
```

This mirrors `merge_poller::spawn_loop` exactly — same task structure, same
metrics shape, same logging convention.

### Cadence

**Default 120 seconds** (2 minutes). Configurable via engine settings
(`reconcile_external_trackers_interval_seconds`). Why 2 min:

- The lower bound is ~60s (any faster wastes API budget for no perceptual
  benefit; humans don't refresh GitHub more often).
- The upper bound is ~5min before users start typing "is sync stuck?"
- 120s is in the middle, leaves API headroom, and matches the cadence
  ballpark used by `merge_poller`.

An on-demand `boss product sync-external-tracker <product>` CLI verb (Q8)
runs `run_one_pass` against a single product immediately, for users who
don't want to wait.

### Per-product processing (`process_product`)

For each product with `external_tracker_kind IS NOT NULL`:

1. Resolve the credential (Q11). If unresolvable, log + skip; emit
   `external_tracker.skip_no_credential`.
2. Call `tracker.fetch_items(ctx)` → `Vec<UpstreamItem>`. On error:
   classify; emit metric; skip.
3. Open a SQL transaction. Within it:
    a. Build a `HashMap<canonical_id, &UpstreamItem>` from the fetched list.
    b. `list_external_refs_for_product(product_id)` → existing bindings.
    c. For each upstream item:
       - If `find_by_external_ref(kind, canonical_id)` returns `Some(row)`:
         **reconcile_existing** (Q6).
       - Else: **import_new** (Q7).
    d. For each existing binding whose canonical_id is *not* in the fetched
       map: it's been removed from the project upstream. Apply
       **handle_removed_upstream** (Q12).
4. Commit. Emit per-tick metrics
   (`external_tracker.imported`, `.closed`, `.pr_attached`, etc.).

### Idempotency

Re-running `process_product` with no upstream changes produces zero writes.
This is the key correctness property — the reconciler runs every 2 minutes
forever, and we cannot afford a "diff" implementation that flip-flops.
Guarantees:

- **Import** is keyed by `find_by_external_ref`. If a Boss row already
  exists for the upstream issue, we never create a second one.
- **Close-mirror** is conditional on the Boss row's status *currently
  differing* from the desired status — `UPDATE … WHERE status != ?`. No
  status change → no SQL write.
- **PR-association** is conditional on `pr_url IS NULL` or `pr_url != ?`.
  No new PR → no SQL write.
- **`external_ref_synced_at`** is updated every successful tick; this is
  the one column that legitimately moves every cycle.

### Scheduling boundaries

- One reconciler task per engine, not per product. Per-product loops would
  multiply the number of long-lived tasks for no benefit.
- A reconcile pass takes <500ms wall-clock for a typical product (one
  GraphQL request + a handful of SQL writes). At 10 products and 120s
  cadence the loop is idle >99% of the time.
- The reconciler does *not* hold the SQL connection between passes. Each
  pass opens and commits its own transaction.

### Recommendation

The single-task, 120s-interval loop above. Per-product sequential within
a pass. The metrics shape mirrors `merge_poller` so dashboards and
diagnostics reuse the same lens.

---

## Design Question 6 — Reconciling an Existing Work Item

This is the hot path: a Boss row already exists for the upstream issue, and
we want to mirror the latest upstream state into Boss without overwriting
local edits.

### Status mirroring

Five upstream states map to four Boss statuses:

| Upstream                              | Boss `tasks.status`        |
|---------------------------------------|----------------------------|
| Open (no PR associated, no Boss work) | `todo`                     |
| Open + Boss work in-flight            | (unchanged; Boss owns it)  |
| Open + associated PR exists           | `in_review`                |
| Closed `completed`                    | `done`                     |
| Closed `not_planned`                  | `done` *(see below)*       |

The reconciler **never overwrites a Boss-side status transition that the
upstream wouldn't reach on its own.** Concrete rules:

- If `upstream_status = Open` and `boss_status ∈ {active, blocked,
  in_review, done}` — leave Boss alone. The user has progressed the work
  locally; upstream is just catching up.
- If `upstream_status = Open` and `boss_status = todo` — leave Boss
  alone (nothing to change).
- If `upstream_status = Closed{Completed}` and `boss_status != done` —
  set Boss to `done`. The upstream is the source of truth on completion.
- If `upstream_status = Closed{NotPlanned}` and `boss_status != done` —
  set Boss to `done` (with a `last_status_actor = 'external_tracker'`
  marker so the kanban can render a subtle "not planned" indicator).
  Open Q below.

**Open question for review:** should `not_planned` map to `done` or to a
new `archived` / `cancelled` status? `tasks.status` doesn't have an
`archived` variant today (the schema uses `deleted_at` for soft-delete).
The chosen v1 mapping is `done` to avoid schema churn; the kanban surface
shows the close-reason in the card tooltip via a new `closed_reason` column
on the work item *if* the value adds enough to justify the extra column.
**Recommendation:** ship without the column; if users miss the
distinction, add `closed_reason TEXT` in a follow-up.

### Title and body

- **Title.** Mirrored on *create only*. Subsequent upstream title edits
  do not overwrite Boss's `name` — users rename freely in Boss.
- **Body.** Same: mirrored on create, not re-synced. The non-goal
  ("bidirectional field sync") covers this. An optional follow-up could
  add a `description_synced INTEGER NOT NULL DEFAULT 0` flag where
  `0` means "Boss user edited it, do not re-sync." Out of v1.

### `external_ref_synced_at`

Bumped on every successful reconcile, regardless of whether other columns
changed. Used by the kanban to render "last synced 30s ago" / "synced 4
days ago — possibly stale" on the upstream-ref affordance.

### Recommendation

`reconcile_existing(boss_row, upstream_item)` does status mirroring with
the table above, never touches title/body, bumps `synced_at`. Conflict
policy: **upstream wins on close, Boss wins on everything else.**

---

## Design Question 7 — Importing a New Upstream Item

When `find_by_external_ref` returns `None`, the upstream item has not yet
been mirrored. We create a Boss row.

### Defaults at create

- **Kind:** `chore`. Chores are the right default because the user filed an
  issue without context about whether it's part of a planned project. The
  reconciler is not in a position to assign it under a project. A follow-up
  CLI verb `boss work move-to-project <selector> <project>` lets a human
  re-classify chore → project task later.
- **Product:** the product whose binding produced this fetch.
- **Status:** `todo`. (Or `in_review` if `upstream_item.pr_associations`
  shows an open PR; rare for fresh issues, but covered.)
- **Name:** upstream title.
- **Description:** upstream body, prefixed with a one-line
  `> Imported from <upstream_url>` so users can chase the origin.
- **Priority:** `medium` (the schema default).
- **`pr_url`:** if a merged PR is already associated upstream, use it.
- **`external_ref_*`:** populated from `upstream_item.upstream_ref`.
- **`created_via`:** new value `'external_tracker_sync'` (existing column
  already has `'unknown' | 'cli' | 'app' | ...` precedent).

### Edge case: an issue closed *before* it was ever imported

Suppose the reconciler runs for the first time against a product whose
upstream project has 50 historic closed issues. Do we import all 50 as
`done` chores?

**No.** The default `label_filter` on a fresh binding excludes already-closed
issues. Specifically: on import, if `upstream_status = Closed{*}` *and* the
Boss DB has no `external_ref` for it yet, **skip**. The reconciler logs
`external_tracker.skipped_closed_at_first_sight` and moves on.

Once an item has been imported, future closures *do* mirror — the skip rule
applies only to "never seen before, already closed."

This handles the bootstrap case: turning on the binding doesn't dump
hundreds of historical closed issues into Boss as `done` chores. Only
forward-going state mirrors.

### Recommendation

Import as `chore` / `todo` with the upstream title / body. Skip
already-closed items at first-sight. Stamp `created_via`.

---

## Design Question 8 — PR Association

GitHub maintains `closedByPullRequestsReferences` automatically when a PR
body matches `Fixes #N` / `Closes #N` / `Resolves #N`. The reconciler
reads this field and propagates it to the Boss row.

### Rules

1. On reconcile, if `upstream_item.pr_associations` is non-empty:
   - Pick the most recent association (sorted by `merged_at` desc, then
     by `pr_url`).
   - If `boss_row.pr_url IS NULL` or `boss_row.pr_url != association.pr_url`,
     update it. Emit `external_tracker.pr_attached`.
2. If the chosen association has `merged = true` and the upstream issue
   has not yet closed (race condition: PR merged, issue not yet auto-closed
   by GitHub), the reconciler does *not* short-circuit and close the
   issue itself. We trust GitHub's auto-close. On the next tick (≤2 min)
   the issue will be `Closed{Completed}` and the standard close-mirror
   path runs.
3. Multiple PR associations are rare but legal (one issue, multiple PRs).
   v1 picks the most recent; future work could surface a list.

### Behaviour (5) — PR merge propagation

The acceptance criterion lists behaviour 5 as "when the associated PR
merges, transition the GitHub issue to closed." This is **GitHub's job,
not Boss's** when the PR body contains `Fixes #N`. Boss does not need to
explicitly close the issue — GitHub does it. The Boss reconciler then
picks up the closure on its next tick.

The only case where Boss might need to write back is when a PR merges
*without* a `Fixes #N` footer but is nonetheless associated with the Boss
work item. v1's policy: Boss does not write back in this case either. The
human can either edit the PR body to add the footer, or close the issue
manually. Auto-closing the upstream issue from Boss requires the
reverse-close path (Q9) and is off by default.

### Failure mode: PR-association points at a PR not owned by this product

Possible if a fork or external PR mentions the issue. Boss still writes
the PR URL to `pr_url` — `pr_url` is a URL, not a foreign key. The
merge poller skips PRs whose host repo doesn't match the product's
`repo_remote_url` (existing behaviour); the Boss side is harmless.

### Recommendation

Read `closedByPullRequestsReferences`; update `pr_url` when it changes;
trust GitHub for auto-close.

---

## Design Question 9 — Reverse-Close (Behaviour 3)

The acceptance criterion lists behaviour 3 as "boss work item marked `done`
→ close the upstream GitHub issue," gated behind a config flag.

### Why off by default

Closing a public GitHub issue is **visible to other humans**. A Boss user
marking a task `done` locally might mean "I shipped this," or it might
mean "I'm done dealing with this and reclassifying it." Closing upstream
in the latter case is rude. Default off; users who run a tight upstream =
local mapping can opt in per-product.

### Configuration

```jsonc
{
  "org": "...",
  "repo": "...",
  "project_number": 1,
  "reverse_close": false   // optional; default false
}
```

When `true`, the reconciler examines every product-bound work item whose
status flipped to `done` since `external_ref_synced_at` and whose
upstream issue is still `Open`, and calls `tracker.close_item(ref,
CloseReason::Completed)`.

### Idempotency

`close_item` is idempotent on the GitHub side: closing an already-closed
issue is a no-op. The reconciler still gates on "current upstream status
is `Open`" to avoid pointless API calls.

### Failure modes

- **Permission denied** (closing an issue requires write access). Surface
  as an attention item on the product; users with read-only `gh auth`
  cannot use reverse-close. Log + emit
  `external_tracker.reverse_close_failed`.
- **Race condition** (issue closed upstream between fetch and close).
  GitHub returns 200; harmless.

### Recommendation

Ship the `reverse_close` flag, gated off by default. Surface as an
attention item if the flag is on but the credential lacks write scope.

---

## Design Question 10 — Source-of-Truth Policy

The brief asks for justification + failure modes. The chosen mode:
**one-way ingestion with zero writeback to GitHub Projects in v1.** The
narrow exception is reverse-close (Q9), which is opt-in and orthogonal.

### Justification

Three reasons:

1. **Writeback is a permission ladder.** Reading from GitHub is a low
   threshold (any authenticated `gh` works). Writing to GitHub requires
   the credential to have `issues:write` and/or
   `projects:write`. Many users authenticated for `gh pr view` are not
   authenticated for `gh issue edit`. Opting writeback off by default
   means Boss works for the most users with the least setup.
2. **Writeback risks cycles.** Once Boss writes back, the next reconcile
   tick reads Boss's own write — and if there's any timestamp skew or
   field-mapping bug, the reconciler can ping-pong. The simplest defence
   is "don't write."
3. **`external_ref_*` is enough for stable identity.** The premise of
   "writeback custom field" is "without it, Boss can't re-identify the
   issue after a rename." But identity is by issue number, not title.
   Issue numbers never change; renames don't break us.

### Failure modes of the chosen mode

- **Upstream issue deleted.** The reconciler's next `fetch_items` doesn't
  include it. **handle_removed_upstream** kicks in (Q12).
- **Upstream issue moved to a different project.** The reconciler's
  `fetch_items` for *this* product doesn't include it anymore — same as
  deletion from this product's POV. Q12 handles it.
- **Upstream issue's number changes.** Can't happen on GitHub; issue
  numbers are immutable.
- **Upstream repo renamed (org or repo).** Breaks the binding — the
  product config still says `spinyfin/old-name` but the API returns
  `spinyfin/new-name`. The reconciler gets 404s and surfaces a
  config-invalid attention item. The user fixes the binding manually.

### Recommendation

One-way ingestion. Reverse-close opt-in. No custom-field writebacks. If
behavioural needs in v2 demand more, the trait's `close_item` shape
generalises naturally to `update_status` / `set_field`.

---

## Design Question 11 — Credentials

GitHub access goes through `gh`. v1 does not store any credential in
`external_tracker_config` (or anywhere else in `state.db`).

### Resolution path

`TrackerContext.credential` is constructed by a `TrackerCredentialResolver`
trait whose default impl simply confirms `gh auth status` succeeds for the
target host (`github.com` for v1). The credential itself is implicit: any
`gh api` call inherits the user's `gh` login.

```rust
pub trait TrackerCredentialResolver: Send + Sync {
    async fn resolve(&self, kind: &str, config: &serde_json::Value)
        -> Result<TrackerCredential, TrackerCredentialError>;
}
```

For GitHub, the default impl just runs `gh auth status` once at engine
startup (per host) and caches the result. If auth is missing, the
reconciler skips all GitHub-bound products and surfaces an attention item
on each.

### Future PAT support

If users want to bind a product to a tracker that the local `gh` is not
logged into (e.g. an organisation account different from their personal
login), a future extension lets the binding reference a credential by
*name*: `"credential_ref": "spinyfin-bot-pat"`. The actual PAT lives in
the OS keychain (macOS: Keychain Services) and is resolved by a different
`TrackerCredentialResolver` impl. **Out of v1.** The bare default
(`gh auth status`) is enough for the Boss-on-mono target.

### Recommendation

Credential plumbing is a trait. v1 ships the `gh auth status` impl;
nothing is stored in Boss state. Future PAT-in-keychain extensions slot in
without schema changes.

---

## Design Question 12 — Handling Removed Upstream Items

An upstream item that was previously mirrored disappears from
`fetch_items` results. Options:

- **(α) Soft-delete the Boss row** (set `deleted_at`).
- **(β) Mark the Boss row's `external_ref_*` columns NULL but leave the
  row otherwise intact.** "Unbind" rather than delete.
- **(γ) Do nothing.** The Boss row's `external_ref_synced_at` quietly
  ages out.

### Discussion

(α) is wrong: an issue moved to a different project (or otherwise removed
from this product's scope) doesn't mean the work is moot. Soft-deleting
Boss state because GitHub's slicing of upstream issues changed is
overreach.

(γ) leaves a stale binding: the Boss row still claims to be bound to an
upstream issue that no longer matches the product's tracker. If the user
looks at the kanban card and clicks "↗ #560", they'd land on an issue that
isn't actually in this product's project. Confusing.

(β) is the principled middle. The Boss row stays — work is work — but its
binding clears. The kanban card no longer shows the upstream affordance.
A `WorkAttentionItem` surfaces: *"`<work_item_name>` was bound to
upstream `<canonical_id>` which is no longer in the configured project.
The link has been cleared; re-bind manually if this was unintended."*

### Recommendation

**Pick (β).** Clear `external_ref_*`, leave the row otherwise alone,
surface an attention item once.

### Re-discovery

If the upstream item later reappears in the project (e.g. it was moved out
and back in), the next reconcile imports it as a *new* item, which would
create a duplicate chore. To prevent this, the reconciler maintains a
shadow lookup against `external_ref_canonical_id` *including* unbound
rows — i.e. `find_by_external_ref` checks both `external_ref_canonical_id
IS NOT NULL` rows *and* a side table `unbound_external_refs` that retains
the last canonical_id of every unbound row for re-binding.

**Update:** in the spirit of YAGNI, v1 ships *without* the side table.
The reconciler clears `external_ref_canonical_id` but *not* the column
itself — instead, it sets `external_ref_synced_at = NULL` and adds a
`external_ref_unbound_at TEXT NULL` column to record the unbind time. When
the upstream item reappears, the reconciler matches on the still-present
canonical_id and re-binds (resets `external_ref_unbound_at` to NULL,
sets `external_ref_synced_at` to now). Simpler shape, no side table.

```sql
ALTER TABLE tasks ADD COLUMN external_ref_unbound_at TEXT;
```

Recommendation revised: clear `external_ref_synced_at`, set
`external_ref_unbound_at` to now, leave `external_ref_canonical_id`
populated so re-binding is automatic.

---

## Design Question 13 — Backfill of Pre-Existing Boss Work Items

When a product binding is freshly set, the product may already have local
chores filed manually that *correspond* to existing upstream issues. The
reconciler can't auto-link them — title-matching is brittle.

### Options

- **(p) Title-match heuristic at bind time.** Fuzzy match Boss work-item
  names to upstream titles; surface a "review and accept" list.
- **(q) No auto-link.** Local Boss work items stay unbound; users link
  manually with `boss task link-external <selector> <canonical_id>`.
- **(r) Mark all existing items as "orphan, may have an upstream
  twin"** and let the user resolve in the kanban.

### Recommendation

**Pick (q).** Heuristic auto-linking (p) is a precision/recall trap;
false matches are worse than no matches. v1 ships an explicit CLI verb:

```sh
boss task link-external <selector> --github spinyfin/mono#560
boss task unlink-external <selector>
```

The reconciler then treats the manually-linked row as if the upstream
binding had existed all along: status mirroring, PR-association, the
works.

If users repeatedly request bulk-linking, a follow-up `boss product
link-external-bulk` could ship the title-match heuristic with an
interactive accept step. Out of v1.

---

## Design Question 14 — UI / Affordance

Out of scope for the *engine* design but worth sketching so the schema
supports it.

- **Kanban card.** When `external_ref_canonical_id IS NOT NULL`, render
  a small "↗ #560" link in the card footer. Click opens
  `https://github.com/<repo>/issues/<n>`. Tooltip shows last sync time.
- **Card detail (when shipped).** A "Linked external tracker" row with
  the canonical_id, sync time, web URL, and a "Refresh now" button that
  triggers `boss product sync-external-tracker <product>` for just this
  row.
- **Attention items.** A new `kind = 'external_tracker'` carrying the
  product_id, the upstream binding's status (`ok`, `config_invalid`,
  `auth_failed`, `reverse_close_blocked`), and a remediation hint.

The macOS app side is a follow-up chore once the engine RPC surface is in
place.

---

## Design Question 15 — Open Questions (from the brief)

The brief lists four open questions. Sketched answers:

1. **Work items created in Boss before the tracker binding exists —
   backfill or only sync forward?**
   Sync forward only. v1 ships `boss task link-external` for manual
   bulk-link. See Q13.

2. **Resolution policy when Boss and tracker disagree on status (Boss says
   `done`, GitHub issue still open)?**
   Upstream wins on `Closed`; Boss wins on everything else. Specifically:
   if upstream is `Open` and Boss is `done`, Boss's status holds (a user
   actively closing the local work is more recent than the upstream
   state, which hasn't caught up yet). If reverse-close is enabled, the
   reverse-close handler propagates Boss → upstream; otherwise the issue
   stays open upstream until a human closes it. No "tug of war." See Q6
   and Q9.

3. **Granularity of the reconcile loop — global, per-product, or
   event-driven via webhooks?**
   v1: global single task, per-product sequential within each pass. See
   Q5. Webhooks are not shipped; the trait's `fetch_items` shape doesn't
   preclude a future webhook receiver that delivers
   `Vec<UpstreamItem>`-equivalent payloads.

4. **Should the Boss work item mirror the GitHub project's column / status
   field, or only its open/closed state?**
   v1 mirrors only open/closed. The project column → Boss status mapping
   is a richer feature (it would let "In Progress" upstream put Boss to
   `active`, etc.). The non-goals leave it out for now; the
   `status_field_mapping` config field is reserved so a v2 can light it up
   without a schema migration. See Q1's config sketch.

---

## Schema and Wire Summary

### Column adds

```sql
-- product-level binding
ALTER TABLE products ADD COLUMN external_tracker_kind   TEXT;
ALTER TABLE products ADD COLUMN external_tracker_config TEXT;

-- per-work-item upstream pointer
ALTER TABLE tasks ADD COLUMN external_ref_kind          TEXT;
ALTER TABLE tasks ADD COLUMN external_ref_canonical_id  TEXT;
ALTER TABLE tasks ADD COLUMN external_ref_raw           TEXT;
ALTER TABLE tasks ADD COLUMN external_ref_synced_at     TEXT;
ALTER TABLE tasks ADD COLUMN external_ref_unbound_at    TEXT;

CREATE INDEX tasks_external_ref_idx
    ON tasks (external_ref_kind, external_ref_canonical_id)
 WHERE external_ref_canonical_id IS NOT NULL;
```

No backfill required for the new columns; all default to `NULL`.

### Protocol additions

```rust
// types.rs
pub struct Product {
    /* … existing fields … */
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_tracker_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_tracker_config: Option<serde_json::Value>,
}

pub struct WorkItemExternalRef {
    pub kind: String,
    pub canonical_id: String,
    pub raw: serde_json::Value,
    pub web_url: String,
    pub synced_at: Option<String>,    // unix seconds string
    pub unbound_at: Option<String>,
}

pub struct WorkItem { /* …existing fields… */
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<WorkItemExternalRef>,
}

pub struct SetProductExternalTrackerInput {
    pub product_id: String,
    pub kind: Option<String>,
    pub config: Option<serde_json::Value>,
    #[serde(default)] pub unset: bool,
}

pub struct LinkExternalRefInput {
    pub work_item_id: String,
    pub kind: String,
    pub canonical_id: String,
}

// wire.rs
SetProductExternalTracker      { request_id: String, input: SetProductExternalTrackerInput }
SyncProductExternalTracker     { request_id: String, product_id: String }
LinkWorkItemExternalRef        { request_id: String, input: LinkExternalRefInput }
UnlinkWorkItemExternalRef      { request_id: String, work_item_id: String }
```

### CLI

```sh
# Product binding
boss product set-external-tracker <selector> --kind github --org spinyfin --repo mono --project 1
boss product set-external-tracker <selector> --kind github --reverse-close
boss product set-external-tracker <selector> --unset
boss product show <selector>            # gains an "External tracker:" block
boss product sync-external-tracker <selector>   # on-demand reconcile pass

# Per-row manual link
boss task link-external <selector> --github <owner>/<repo>#<number>
boss task unlink-external <selector>
boss chore link-external <selector> --github <owner>/<repo>#<number>
boss chore unlink-external <selector>
```

### Engine module split

- `engine/src/external_tracker/mod.rs` — trait, `TrackerRegistry`,
  `TrackerContext`, error types.
- `engine/src/external_tracker/github.rs` — `GitHubTracker` impl with
  `gh` shellouts and GraphQL query construction.
- `engine/src/external_tracker/reconcile.rs` — the periodic loop and
  per-product pass.
- `engine/src/external_tracker/credentials.rs` — `TrackerCredentialResolver`
  trait and `gh auth status` default impl.
- `engine/src/work.rs` — `set_external_ref`, `clear_external_ref`,
  `find_by_external_ref`, `list_external_refs_for_product`.
- `engine/src/protocol.rs` — RPC additions.
- `cli/src/main.rs` — new `product set-external-tracker`,
  `product sync-external-tracker`, `task/chore link-external`,
  `task/chore unlink-external` verbs.

### Metrics

Counters, registered on the existing `Registry`:

- `external_tracker.fetch_succeeded`
- `external_tracker.fetch_failed`
- `external_tracker.imported`
- `external_tracker.closed`
- `external_tracker.pr_attached`
- `external_tracker.unbound`
- `external_tracker.reverse_close_succeeded`
- `external_tracker.reverse_close_failed`
- `external_tracker.skipped_closed_at_first_sight`
- `external_tracker.skip_no_credential`

Cardinality is bounded by product count; no per-item labels.

---

## Sequence Diagram — Reconcile Pass for One Product

```
┌────────────┐ ┌──────────────┐ ┌──────────────┐ ┌──────────┐ ┌────────┐
│ reconciler │ │ GitHubTracker│ │   gh / api   │ │ work_db  │ │ sqlite │
└─────┬──────┘ └──────┬───────┘ └──────┬───────┘ └────┬─────┘ └───┬────┘
      │ process_product(p)             │              │            │
      │ ───────────► fetch_items(ctx)  │              │            │
      │              │ gh api graphql …│              │            │
      │              │ ───────────────►│              │            │
      │              │  Vec<UpstreamItem>             │            │
      │              │ ◄───────────────│              │            │
      │ ◄──────────  │                 │              │            │
      │ BEGIN TX     │                 │              │            │
      │ ──────────────────────────────────────────────│───────────►│
      │ list_external_refs_for_product(p)             │            │
      │ ──────────────────────────────────────────────►            │
      │ for each upstream item:                       │            │
      │   find_by_external_ref(kind, cid)             │            │
      │   ─────────────────────────────────────────────────────────►
      │   ↓ Some(row): reconcile_existing             │            │
      │     UPDATE tasks SET status, pr_url WHERE …   │            │
      │     UPDATE external_ref_synced_at             │            │
      │   ↓ None: import_new                          │            │
      │     INSERT INTO tasks (...)                    │            │
      │ for each existing binding not in fetch:       │            │
      │   handle_removed_upstream (clear + unbound_at)│            │
      │ COMMIT                                         │            │
      │ ──────────────────────────────────────────────│───────────►│
      │ emit metrics                                  │            │
      │ sleep(interval)                                │            │
```

---

## Risks and Open Questions

**R1 — Auth fragility.** v1 inherits `gh auth status`. If the user is
logged out, every bound product silently produces zero-update reconciles
until a human notices. Mitigation: attention items on the product when
credential resolution fails. *Open Q for review:* should the engine
surface a top-level "external tracker auth missing" banner in the macOS
app, or is the per-product attention item enough?

**R2 — Rate limits at scale.** A user with 50 bound products at 120s
cadence makes ~25 GraphQL requests/min ≈ 25 points/min, well under
GitHub's 5000/hr GraphQL budget. But the design assumes one engine
instance per user; a shared engine serving many users would multiply.
Mitigation: v1 documents the assumption ("single-user engine"); a future
fan-out engine would need shared rate-limit accounting.

**R3 — Two truths on title/body.** v1 mirrors only on create; later
upstream edits don't propagate. Users may complain "I fixed the issue
title upstream and Boss still shows the old one." Mitigation: ship as
documented; if it bites, add a `boss task resync-fields` verb later that
re-pulls title/body on demand. Auto-resync is bidirectional-sync
territory and out of scope.

**R4 — Reconciler silently breaks on schema changes.** GitHub's GraphQL
projectV2 surface is in active development and has changed shape before.
A breaking change to a queried field would cause every reconcile to fail.
Mitigation: a deserialiser unit test pinned to a snapshot response; a
metrics-based alert if `fetch_failed` ratio passes a threshold.

**R5 — Reverse-close gone wrong.** A user enables reverse-close, marks
ten Boss chores `done` to clean up local mess, and accidentally closes
ten public GitHub issues. Mitigation: reverse-close stays off by default;
the CLI prompt on enable surfaces the implication
("This will close upstream GitHub issues when matching Boss work items
are marked done. Proceed? [y/N]"); the engine logs every reverse-close
with the actor.

**R6 — Re-bind ambiguity.** An upstream item that reappears after
unbinding rebinds via the still-present `external_ref_canonical_id`. If
the user *manually* re-imports the same upstream issue (e.g. with
`task link-external`) while it's unbound, two rows could end up with the
same canonical_id. Mitigation: enforce a unique index over
`(external_ref_kind, external_ref_canonical_id)` filtered on
`external_ref_unbound_at IS NULL`. Two simultaneously-bound rows for the
same upstream item is rejected at SQL level.

```sql
CREATE UNIQUE INDEX tasks_external_ref_bound_uniq
    ON tasks (external_ref_kind, external_ref_canonical_id)
 WHERE external_ref_canonical_id IS NOT NULL
   AND external_ref_unbound_at  IS NULL
   AND deleted_at               IS NULL;
```

**R7 — `gh` version drift.** Boss assumes a `gh` recent enough to support
GraphQL on Projects v2. If a user has `gh < 2.20`, `gh api graphql`
works but specific project-v2 fields may be missing. Mitigation: doc
minimum `gh` version (2.40+); attention item on
`fetch_failed: schema_mismatch`.

**R8 — Reverse-close + skip-closed-at-first-sight interaction.** If a
user (a) enables reverse-close, (b) imports a project for the first time,
(c) marks an unrelated *local* chore as `done`, no upstream issue exists
to close — fine. But: what if a freshly-bound product has a Boss work
item that was imported, then immediately marked `done` by the user
*before* the next reconcile tick? The reverse-close handler will try to
close upstream on the next tick. Correct behaviour, but the user might
not realise their click closes a GitHub issue 90 seconds later.
Mitigation: the CLI / UI on `boss task move <selector> --status done`
notes "this product has reverse-close enabled — upstream issue
`<canonical_id>` will be closed on the next sync."

**R9 — `created_via = 'external_tracker_sync'` accounting.** Chores
created by the reconciler should be obviously machine-imported in the
UI. Mitigation: the existing `created_via` column already supports this
shape; the kanban renders a tiny "📡" badge on cards whose
`created_via = 'external_tracker_sync'`.

**Open Q1.** Should the reconciler emit an event on every reconcile pass
even when nothing changed, so the kanban can show a "last refreshed"
timestamp on the product? Tradeoff: per-tick chatter on a topic vs.
on-demand RPC. v1 leans toward on-demand
(`GetProductSyncStatus(product_id)`) plus an event only when something
changes — matches the existing `work_item_changed` convention. Confirm
with reviewer.

**Open Q2.** The unique partial index on
`(external_ref_kind, external_ref_canonical_id) WHERE unbound_at IS NULL`
prevents double-binding. SQLite supports partial indices since 3.8.0
(2013), so this is safe. Confirm: any reason to prefer a CHECK constraint
or application-level guard instead?

**Open Q3.** `external_tracker_config` is JSON. The CLI presents typed
flags (`--org`, `--repo`, `--project`); these get serialised to the JSON
shape. Should we also support `--config-json '<raw json>'` for unusual
configurations? Inclines toward yes (matches the precedent of
`product set-default-model` which accepts the slug verbatim) but adds
surface area for invalid configs. Default: no; add later if asked.

---

## Follow-up Implementation Chores (to enqueue once approved)

Bite-sized; each fits one worker session. Ordered roughly by dependency.

1. **Schema migration** — add the seven new columns
   (`products.external_tracker_*`, `tasks.external_ref_*`) and the partial
   indices. Idempotent. Acceptance: fresh init and migration both yield
   the new schema; existing tests pass.

2. **Protocol types** — extend `Product`, `WorkItem`,
   `SetProductExternalTrackerInput`, `LinkExternalRefInput`, plus the
   four new RPC variants. Mirror in `Models.swift` (read-only renderer
   on the macOS side initially). Acceptance: serde / Codable round-trip
   green.

3. **`ExternalTracker` trait + `TrackerRegistry`** — trait definition,
   error types, in-process registry keyed by `kind`. No GitHub-specific
   logic. Acceptance: a fake `EchoTracker` impl can register and the
   registry serves it; unit tests cover registration and lookup.

4. **GitHub impl: `fetch_items`** — `gh api graphql` query
   construction, pagination, deserialisation to `Vec<UpstreamItem>`.
   Acceptance: integration test against a fixture JSON file pinned in
   testdata; unit tests cover pagination, label filter, empty project.

5. **GitHub impl: `fetch_item`** — single-issue `gh api repos/...`
   fetch. Acceptance: unit tests for 200 / 404 / 500 responses.

6. **GitHub impl: `close_item`** — `gh issue close <url> --reason
   completed`. Acceptance: unit tests for success / permission-denied /
   already-closed.

7. **Credential resolver** — `gh auth status` default impl; attention
   item emission on failure. Acceptance: unit tests with a fake
   `gh` mock.

8. **`WorkDb` external-ref methods** — `set_external_ref`,
   `clear_external_ref`, `find_by_external_ref`,
   `list_external_refs_for_product`. Acceptance: SQL-level unit tests
   covering insert, update, partial-index uniqueness, rebind from
   unbound state.

9. **Reconciler core: `run_one_pass`** — per-product processing without
   the spawn loop. Acceptance: integration test feeds a synthetic
   `EchoTracker` and asserts the SQL state matches expectations across
   create / close / pr-attach / unbind cases.

10. **Reconciler spawn loop** — `tokio::spawn` with the configured
    interval, mirroring `merge_poller::spawn_loop`. Acceptance: smoke
    test that one tick fires; metrics emit; sleep honors interval.

11. **CLI: `boss product set-external-tracker`** — flags for `--kind`,
    `--org`, `--repo`, `--project`, `--reverse-close`, `--unset`.
    Validates config via the trait's `validate_config`. Acceptance:
    `--help` covers each verb; integration test covers bind/unbind.

12. **CLI: `boss product sync-external-tracker`** — on-demand
    reconcile of a single product. Acceptance: smoke test that one
    invocation runs one pass.

13. **CLI: `boss task link-external` / `boss task unlink-external` /
    chore equivalents** — manual binding. Acceptance: integration test
    for link → reconcile → mirror.

14. **`product show` extension** — render the external tracker block
    in `boss product show`. Acceptance: snapshot test.

15. **Kanban: ↗ #N affordance on cards** — read `WorkItem.external_ref`
    and render the link. Acceptance: SwiftUI snapshot for bound /
    unbound / unbound-with-stale-ref states.

16. **Attention items: external-tracker kind** — `kind =
    'external_tracker'`, surfaced on the macOS attention list.
    Acceptance: integration test for each of the four reasons.

17. **Reverse-close handler** — engine path that closes upstream when
    Boss flips a row to `done` and reverse-close is on. Acceptance:
    integration test with a faked `close_item`.

18. **Documentation** — runbook for binding a tracker, troubleshooting
    auth, interpreting attention items. One markdown file under
    `tools/boss/docs/runbooks/`.

19. **(Optional follow-up) Webhook receiver** — listen for GitHub
    webhook events and trigger an immediate reconcile of the affected
    product. Out of v1.

20. **(Optional follow-up) Bulk link** — `boss product
    link-external-bulk` with title-match heuristic and interactive
    accept. Out of v1.

21. **(Optional follow-up) Jira tracker impl** — a second
    `ExternalTracker` implementation, vetting that the trait surface
    actually accommodates a non-GitHub backend without churning the
    engine.

---

## Out of Scope

- Bidirectional field sync (assignees, labels, comments, milestones,
  body re-sync).
- Boss → GitHub issue creation. Boss work items created locally do not
  auto-spawn upstream issues.
- Multiple trackers per product.
- Webhook-driven push delivery.
- Title-match heuristic for bulk-linking existing work items.
- Stored PATs / credential management beyond `gh auth status`.
- Cross-product upstream sharing (one upstream issue mirrored to two
  Boss products).
- Boss-side support for trackers that don't expose a stable issue number
  (none of the candidates suffer this; called out for trait-design
  rigour).
- A reverse index over upstream-ref state for analytics ("how many
  bound items per product?") beyond what the reconciler logs.
