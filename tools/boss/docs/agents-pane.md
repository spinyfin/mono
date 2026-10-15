# Agents pane: membership, filters, pagination, and epochs

Operator reference for the Agents view. Design rationale lives in [Dynamic Agents pane layout](./designs/dynamic-agents-pane-layout.md); this page is what you need when the pane does something surprising.

## Who is shown

Membership is the engine's live worker snapshot, restricted to local workers (`host_id == "local"`), keyed by run id. A run appears when the engine publishes it and leaves when an accepted snapshot releases it. Remote SSH workers never appear. If the engine feed disconnects, the last accepted snapshot stays on screen with a "Showing last known agents" warning; a disconnect is never treated as "everyone left".

Position (page, cell, order) is local presentation state. It is never identity and never sent to the engine. Agents are ordered by execution start time, then run id, oldest first.

## "Viewer not attached" cards

A viewer (the terminal) is separate from membership. If a run is in the engine's snapshot but its terminal is not attached (attach failed, surface lost, or detached), the run keeps its cell and shows a status card instead of a terminal. It still uses capacity, is filtered and counted like any other agent, and fills the same cell once a viewer attaches. An attach that arrives before the engine reports the run does not create a cell. A late detach after release does not bring a run back.

## Filters

The header has two multi-select menus, **Project** and **Type**.

- Nothing checked means All. Checking items narrows to just those; unchecking the last one returns to All. **Clear** resets both.
- Project lists projects that have live agents, plus **Unfiled** (work with no project; never guessed from the pool).
- Type lists Coding, Design, Review, Automation, Answer, and **Unknown** (any missing or unrecognised type, shown rather than dropped).
- The two menus combine with AND.
- Filters reset to All on every app launch and are not saved.

When a filter is active the header shows **N hidden**, plus **M need input** if any hidden agent is waiting, so a filter cannot bury an agent that needs you. The **Needs input** count beside Tidy covers visible agents only. If the filter matches nothing, the pane says "No agents match the filter".

Filtering is display only. Hidden agents keep running and streaming, their terminals stay mounted, and CLI commands (send, interrupt, stop, status) still reach them.

## Pagination

Per-page capacity comes from the window size and the terminal cell size (minimum legible pane 70 columns by 24 rows, at most 16 per page). Page chips appear only when some agent falls past the first page. Each chip shows its agent count and an orange dot if any agent on it is waiting. Pages are computed from the filtered set, so filtering down to one page's worth hides the chips.

## Epochs and Tidy

A **layout epoch** keeps every agent in the cell it has. New agents take the lowest free cell (growing the grid if needed); released agents leave a dim "finished" placeholder. Gaps can therefore accumulate.

A **layout boundary** removes gaps and placeholders and repacks agents in order. Boundaries are: entering the Agents view, changing page, changing a filter, finishing a window resize, pressing **Tidy**, and forced compaction when a release means fewer pages would suffice. Tidy is highlighted when it would change the layout.

Across a boundary the pane you were looking at (the focused agent, else the first on the page) is kept in view and its terminal is not recreated.
