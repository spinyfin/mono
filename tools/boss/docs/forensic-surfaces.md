# Boss forensic surfaces

Two independent surfaces for answering "what happened?" questions
against a live Boss install. Both are currently undocumented elsewhere
at the retention/semantics level this note covers.

**Workers:** paths under `~/Library/Application Support/Boss/` are
coordinator runtime state and off limits to worker sessions. This doc
exists so agents know _what exists_ and _what each surface can answer_
when reading investigation notes or writing recovery procedures — not
so workers open the files themselves.

## 1. `engine-audit.log` — best-retained forensic surface

Path (default):

```text
~/Library/Application Support/Boss/engine-audit.log
```

(Override: `BOSS_ENGINE_AUDIT_PATH`.)

Reach for this **first** for provenance questions ("who deleted this
row", "what surface issued this change").

### Retention

- Small, effectively unrotated for months of typical use.
- Measured 2026-07-26: ~1.3 MB back to 2026-05-08.
- Contrast: `engine-trace.jsonl*` holds ~4 days under normal write
  volume (rotates at 100 MB, keeps 10, purely size-based — a restart no
  longer rotates a file that isn't yet full) and can still have rotated
  past the window you care about during a high-volume incident.

Lifecycle event shapes (`start` / `socket_bound` / `shutdown`) are also
documented in [`tools/boss/app-macos/README.md`](../app-macos/README.md)
under "Forensic / audit log". That README also notes an in-process
~2 MiB half-drop bound; under normal load the file stays well under
that ceiling, which is why multi-month retention is observed in
practice.

### `work_item_deleted` events

Each delete emits one `work_item_deleted` event with:

| Field                 | Notes                                     |
| --------------------- | ----------------------------------------- |
| `work_item_id`        | The deleted item                          |
| `cascade_deleted_ids` | Items removed as a cascade of this delete |
| `actor`               | Coarse only — see below                   |
| `peer_pid`            | Process that issued the change            |
| `reason`              | Why the engine recorded the delete        |
| `ts`                  | Timestamp                                 |

### Reading deletes

**`reason` discriminates the call path:**

- `reason: "delete_work_item request"` → the `DeleteWorkItem` frontend
  RPC (a human / CLI / app delete).
- `reason: "unpopulate_project"` with `actor: "engine"` and no
  `peer_pid` → an engine-internal unpopulate. A different thing
  entirely from a user delete.

**`actor` is coarse:**

- Resolves only to `mac_app` vs `unknown`.
- There is no caller-stamped surface on deletes the way creates have
  `created_via`, so coordinator `boss`, worker `boss`, and any ad-hoc
  client all collapse to `unknown`.
- `unknown` means "not the app" — nothing finer.

**`peer_pid` traps:**

- Near-useless post-hoc (the process is gone by the time you read the
  log).
- **Not** monotonic across batches — never infer ordering from it.
- Distinct `peer_pid` per event + empty `cascade_deleted_ids` = N
  independent CLI calls, not one cascading action.

## 2. Per-task cost — transcripts only, ~29-day cliff

Per-task cost is **not** in `state.db`. No table holds tokens, model,
or turn counts.

### Where cost lives

The only source is the Claude Code session JSONL:

```text
~/.claude/projects/<workspace-slug>/<session-uuid>.jsonl
```

Reached via `work_runs.transcript_path`. The slug is derived from the
cube workspace path, e.g.:

```text
-Users-brianduff--local-share-cube-workspaces-mono-agent-070
```

### Retention cliff

Measured 2026-07-26 against the live state root:

| Age of transcript | Survival |
| ----------------- | -------- |
| ≤ 28 days         | 100%     |
| 28–35 days        | ~9.7%    |
| > 35 days         | 0%       |

This is Claude Code's own ~30-day cleanup (`cleanupPeriodDays` is
unset; default applies), **not** a Boss GC.

Of 12,411 `work_runs` ever recorded (same measurement window):

- 7,348 have a `transcript_path`
- only 4,883 still resolve on disk

Roughly two-thirds of Boss run history is permanently cost-blind.

### What _is_ durable forever in `state.db`

- Wall-clock timing
- Queue wait
- Retry counts
- Execution / run structure
- Review linkage

### Counting traps

- **Rounds = distinct `.message.id`**, not assistant-record count.
  One API response is split across thinking / text / `tool_use`
  records that share an id. Naive counting overstates by ~2×.
- **`worker_stop_received` is not a turn counter.** It is an
  `auto_bind_poller` artifact (three executions carried 760 events
  each in the measurement corpus).
- **Use `started_at → finished_at`, never `created_at → finished_at`.**
  Queue-wait mean is ~370× its median. Inside a started execution,
  agent-active / run-wall median ratio is ~0.97, so run wall is a
  good agent-time proxy.
- **Read the model from `.message.model`; never infer it from
  effort / reasoning policy.** Models drift within a tier over time
  (e.g. both opus-4-8 and opus-5 have served `pr_review`), so a
  policy-derived model is wrong for historical rows.
  `tasks.reasoning` was populated on only 16 / 4,132 rows in the
  measurement corpus.
- **~44% of `pr_review` executions are unchanged-sha re-runs.**
  Group by `(work_item_id, pr_head_before)` to separate real review
  effort from lane flapping; report raw and dedup figures.
- **Output tokens is the best size proxy** — monotonic across all
  four effort levels. Rounds is not (medium 160 vs large 153 at the
  median in the corpus).

### Attribution

Attribution is exact; no PR heuristics required. A `pr_review`
execution carries the **reviewed item's** `work_item_id`. Join:

```text
tasks → work_executions → work_runs → transcript_path
```

Gap: review-findings follow-up work parented elsewhere links back via
`tasks.origin_task_short_id` / `origin_pr_number`, populated on only
27 / 4,132 rows in the measurement corpus.

## Related

- Lifecycle audit events: [`tools/boss/app-macos/README.md`](../app-macos/README.md)
- Transcript viewer design: [`tools/boss/docs/designs/transcript-viewer.md`](designs/transcript-viewer.md)
- Post-crash recovery: [`tools/boss/docs/post-crash-recovery.md`](post-crash-recovery.md)
