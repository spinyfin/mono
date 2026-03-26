# Boss: Work Tab Kanban Design

## Overview

The Work tab should move from a tree-first navigator to a board-first view.
The primary purpose of the tab is to help the human boss understand the state
of work at a glance and move work forward. A kanban board does that better than
the current hierarchy-focused presentation.

The board should use four fixed columns:

- **Backlog**
- **Doing**
- **Review**
- **Done**

Cards on the board should be only **tasks** and **chores**. Projects remain
important, but they should organize cards through filtering and grouping rather
than appearing as peer work items in the main board.

This design builds on the existing work taxonomy. It does not replace the
current product/project/task/chore model; it changes how that model is exposed
in the Work tab.

## Goals

- Make planned, active, review-ready, and completed work obvious at a glance.
- Treat tasks and chores as the main units of day-to-day tracking.
- Let the user filter or group work by project without losing a product-level
  view.
- Preserve the existing storage model and canonical status values where
  possible.
- Keep the first version lightweight enough for the current macOS PoC.

## Non-Goals

- Making projects into board cards.
- Designing full agent assignment or automation from the board.
- Adding a fifth permanent `Blocked` column.
- Replacing product/project CRUD with a new planning system.
- Building dependency graphs, estimates, or roadmap tooling.

## Why Kanban Here

The current work taxonomy is good at modeling structure, but the current UI
leans too hard on hierarchy. The Work tab should optimize for answering
questions like:

- What is not started yet?
- What is currently in progress?
- What is waiting on review?
- What has finished recently?

Those are workflow questions, not hierarchy questions. The hierarchy still
matters, but it should be supporting context around the board instead of the
main event.

## Primary Model

The Work tab should be scoped to a single **product** at a time. Within that
product:

- **Projects** are organizational containers.
- **Tasks** are project-scoped board cards.
- **Chores** are product-scoped board cards.

The board should never render products or projects as cards. The user manages
projects through filters, grouping controls, and inspectors.

## Status Mapping

The existing backend status model is already close to what the kanban board
needs. The frontend should project statuses into board columns like this:

- `todo` -> `Backlog`
- `active` -> `Doing`
- `blocked` -> `Doing`
- `in_review` -> `Review`
- `done` -> `Done`

`blocked` should stay a first-class status in storage, but it should not create
a separate permanent column. Instead, blocked cards should render as a special
kind of `Doing` card:

- blocked badge or accent color,
- sorted to the top of the `Doing` column by default,
- included by a quick `Blocked only` filter.

This keeps the board aligned with the requested four-column workflow while
preserving operational signal.

## Information Architecture

Recommended layout:

```text
┌────────────────────┬──────────────────────────────────────────────────────┐
│ Products / Filters │ Work Board                                           │
│────────────────────│──────────────────────────────────────────────────────│
│ Boss               │ Product: Boss                                        │
│                    │ Filters: All projects | Search | Blocked only        │
│ Projects           │ Group by: None / Project                             │
│ [x] Work taxonomy  │                                                      │
│ [x] Agent polish   │ Backlog   Doing   Review   Done                      │
│ [ ] Infra cleanup  │ ┌──────┐  ┌──────┐ ┌──────┐ ┌──────┐                 │
│                    │ │card  │  │card  │ │card  │ │card  │                 │
│ Options            │ │card  │  │card  │ │card  │ │card  │                 │
│ [x] Show chores    │ └──────┘  └──────┘ └──────┘ └──────┘                 │
│ [ ] Blocked only   │                                                      │
└────────────────────┴──────────────────────────────────────────────────────┘
```

### Left Sidebar

The left sidebar should stop behaving like a deep tree of products, projects,
tasks, and chores. It should instead hold board context:

- product selection,
- project filter checkboxes or pills,
- quick toggles such as `Show chores` and `Blocked only`,
- optional saved filter state across launches.

This keeps the board wide and readable while still giving the user structural
control.

### Main Board

The main area should be a four-column kanban board. Each column shows cards for
the currently selected product after project/filter rules are applied.

Each column should show:

- a title,
- a count badge,
- cards ordered with the most urgent/active items first.

The first version can use vertical scrolling inside the full board scroll view.
Independent horizontal board virtualization is unnecessary at PoC scale.

## Cards

Cards should represent either:

- a task that belongs to a project, or
- a chore that belongs directly to the product.

Each card should show, at minimum:

- title,
- kind icon (`task` vs `chore`),
- project label when applicable,
- blocked state when applicable,
- PR link when present,
- updated-at or created-at secondary metadata.

Optional first-version metadata:

- project color accent,
- assignee placeholder for future agent linkage,
- short description preview on expanded cards.

## Project Grouping and Filtering

Projects should influence the board in two ways.

### 1. Filtering

The user should be able to filter to:

- all projects,
- one project,
- multiple projects,
- chores only,
- one or more projects plus chores.

This makes it possible to answer both product-level and project-level workflow
questions without changing screens.

### 2. Grouping

The user should be able to switch column rendering between:

- `Ungrouped`: one flat list per status column.
- `Group by project`: sections inside each status column.

When grouped by project:

- project-backed tasks appear in per-project sections,
- chores appear in a `Chores` or `No project` section,
- empty project sections remain hidden by default.

Grouping should be a presentation choice only. It should not change storage or
identity.

## Board Interactions

### Create

The Work tab should support quick creation directly into `Backlog`.

Recommended flows:

- `New Task`: requires a selected product and project.
- `New Chore`: requires only a selected product.
- Quick-add affordance at the top of the `Backlog` column.

### Move

Cards should be movable between columns. Drag-and-drop is the ideal interaction
for the macOS app, but the first version can also support:

- move menu in the card,
- keyboard/action menu in the inspector.

Column moves should update canonical item status:

- drop into `Backlog` -> `todo`
- drop into `Doing` -> `active` unless explicitly marked blocked
- drop into `Review` -> `in_review`
- drop into `Done` -> `done`

Blocking and unblocking should be a separate action from column movement.

### Inspect and Edit

Selecting a card should open a detail inspector without navigating away from
the board. The inspector should allow editing:

- name,
- description,
- status,
- project,
- PR URL.

Projects and products can still have detail views, but those should not displace
the board as the default Work experience.

## Sorting

Within each column, recommended default ordering is:

1. blocked cards first inside `Doing`,
2. explicit `ordinal` when present for project tasks,
3. most recently updated items,
4. alphabetical fallback.

This respects existing ordered phases where they exist without making the whole
board feel like a strict sequential plan.

## Relationship to Existing Work Taxonomy

This design intentionally reuses the current domain model:

- `Product` remains the top-level scope.
- `Project` remains the container for meaningful feature work.
- `Task` remains the canonical backend term for project-scoped work.
- `Chore` remains a product-scoped work item.

No new table is required for board columns. The board is a derived view over
the existing `tasks` table plus project metadata.

The main state change is in the frontend: the Work tab should treat the board
as the primary presentation and the hierarchy as supporting context.

## Engine and Protocol Impact

The existing store and `get_work_tree` response are enough for a first board
implementation. The frontend can derive columns locally from:

- product,
- projects,
- tasks,
- chores.

Recommended near-term additions:

- update-work-item support for changing `project_id` on tasks,
- a focused list/query endpoint later if the board grows beyond one product's
  practical size,
- optional persistent UI preferences for project filters and grouping mode.

No schema change is required for the first cut unless we later decide to store
explicit board ordering separate from `ordinal`.

## Frontend State Model

The current work state is detail-first. A board-first UI should add:

- `selectedProductID: String?`
- `selectedProjectFilterIDs: Set<String>`
- `includeChores: Bool`
- `showBlockedOnly: Bool`
- `boardGrouping: .none | .project`
- `selectedCardID: String?`

The existing `selectedWorkNodeID` model can remain for inspector/edit flows,
but it should no longer drive the overall layout of the Work tab.

## Implementation Plan

### Phase 1: Board Projection

1. Keep the current engine APIs and persistence model.
2. Replace the tree-first Work sidebar with product and project filters.
3. Render tasks and chores into the four fixed kanban columns.
4. Add inspector-based editing from a selected card.

### Phase 2: Workflow Actions

5. Add move actions between columns.
6. Add drag-and-drop for cards.
7. Add quick-create in `Backlog`.
8. Add blocked styling and blocked-only filtering.

### Phase 3: Polish

9. Persist selected product, filters, and grouping mode.
10. Add richer project grouping visuals and counts.
11. Add keyboard shortcuts and better empty states.

## Design Decisions

- The Work tab should be board-first, not tree-first.
- Only tasks and chores should appear as board cards.
- Projects should organize the board through filters and grouping.
- The board should use the fixed columns `Backlog`, `Doing`, `Review`, and
  `Done`.
- `blocked` should remain a status but render inside `Doing`.
- The first implementation should derive the board from the existing work tree
  instead of adding new backend concepts.
