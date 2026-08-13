# Boss postmortems

Postmortems for production-affecting incidents in Boss and its surrounding tooling (engine, macOS app, cube, dispatcher).

## Convention

- One file per incident, named `incident-NNN-<slug>.md`. `NNN` is a zero-padded sequence starting at `001`; the slug is a short kebab-case description.
- Each postmortem covers, at minimum: summary, timeline, observed effects, investigation and root cause, action items, what went well, what went badly, lessons.
- Action items are reproduced verbatim from the incident's investigation notes when those notes are the authoritative source. The postmortem may _recommend_ a specific fix for an item and justify the recommendation, but it does not silently rewrite the item.
- Postmortems are doc-only — they describe and recommend; they do not change code. Implementation work derived from a postmortem's action items is filed as separate chores or tasks against the postmortem's parent project.

## Index

- [`incident-001-pr-fan-out.md`](incident-001-pr-fan-out.md) — 2026-05-14. The engine's PR-detection fallback misattributed PRs across cube workspaces via shared `.jj/repo/store/git` bookmark visibility, closing the wrong chores as done and killing running workers mid-turn.
- [`incident-002-merge-conflict-deletion-blessed-by-review.md`](incident-002-merge-conflict-deletion-blessed-by-review.md) — 2026-07-03. A merge-conflict revision worker deleted a just-merged feature (the TRE planner badge) to resolve a forward-port conflict, rationalized it as "supersedes" with no design basis, and the automated `pr_review` pass examined the deletion and blessed it — flagging only tidiness. A recurrence of the T793/#1043 class, this time bypassing the control built for it.
- [`incident-003-engine-startup-schema-ordering.md`](incident-003-engine-startup-schema-ordering.md) — 2026-08-06. A `CREATE UNIQUE INDEX` placed in the engine's bootstrap statement batch ran ~400 lines before the guarded migration that supplies its column, so every database with history failed to open and the engine died before binding its socket. The migration existed and was correct; only its ordering was wrong. Second occurrence in 26 hours of "schema DDL correct against a blank database and fatal against one with history."
- [`incident-004-live-revision-workers-reaped-mid-turn.md`](incident-004-live-revision-workers-reaped-mid-turn.md) — 2026-08-08. The merge-poller's staged-PR-URL fast path (`pr_recheck_staged`) finalized and reaped live revision workers mid-turn, discarding post-push work or entire unpushed diffs, while board status could still report review success. The path short-circuits protections written after a prior race of the same class. Measured over the retained trace window: 63 of 64 staged-path finalizations reaped a mid-turn worker (98.4%), 6 total losses, and 5 work items left claiming review status having contributed nothing. That measurement was subsequently reproduced exactly from an independent log surface and extended 4.4x, to 224 mid-turn reaps across 127 PRs over nine days.
