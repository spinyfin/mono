# Capturing a missing board card

When a lane count includes a card that is not drawn, **before changing the project filter**, right-click a lane title and choose **Copy Board Diagnostics**. Paste the JSON into a local file. Take a screenshot of the affected lane. Then toggle the filter and capture again, keeping both files. Tokens remain comparable within this app session.

This is diagnostic instrumentation, not a fix for the intermittent missing-card report. The earlier isolated project fixture rendered the in-review parent alongside active and blocked revisions, including after incremental updates and narrowing a scrolled board. Those passing captures do not explain the reported failure.

The capture reads the existing model stores, cached lists, section values delivered to SwiftUI, and live AppKit card probes without rebuilding lists, forcing layout, fetching engine state, or changing filters. The last 256 model-apply, invalidation, project-filter, and column-delivery events are retained in memory with sequence numbers and process uptime. Older events are evicted; the first retained sequence indicates whether history was lost. Collection starts with the model; no restart or opt-in is needed. Nothing is uploaded or written to disk automatically. Copying explicitly replaces the clipboard.

Names, descriptions, prompts, search text, PR URLs, paths, and raw item/project/product identifiers are excluded. A random per-model salt produces opaque relationship tokens. The export contains status/kind, relationship tokens, counts, filter booleans, and geometry. Review evidence before sharing.

Compare these layers:

- `rows`: the selected product's received model rows, including parent/revision relationships, status, deletion, folding, and the row's own lane mapping. This is app state, not a fresh engine query.
- `cachedVisible` and `cachedSections`: existing derived caches. Null or absent means unpopulated, not an empty computed result. The effective displayed lane is in the cached sections and may differ from a row's own mapping because of optimistic moves.
- `presentedSections`: section membership last delivered through SwiftUI lifecycle callbacks, shared by the lane header and list. `storedExpanded` reflects persisted disclosure state; a temporary reveal can expand a section independently.
- `geometry`: currently registered card probes, including window attachment, hidden state, window-relative frame, clipped rectangle, and visibility within the board. A mounted card can be clipped or offscreen. An absent probe does not prove a filtering error: lazy content and collapsed sections also omit probes. These measurements do not prove pixels were painted.
- `events`: ordered updates and invalidations leading to the capture. Tree applies record counts; item applies record sanitized row state. This is deliberately not a complete socket transcript. A short burst can evict earlier evidence.

A disagreement between rows and caches points toward filtering/invalidation; disagreement between caches and delivered sections points toward view updates; matching membership with clipped or missing geometry points toward disclosure, scrolling, or layout. Capture both sides of the filter toggle before choosing a fix. No periodic redraw, forced rebuild, or cache bypass is installed by these diagnostics.
