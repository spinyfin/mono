<!-- Ground-truth fixture for planner e2e tests (design task 11).
     A pure design-rationale doc with NO implementation task-breakdown section.
     Exercises the clean-no-op path: a real coordinator (and the Planner) would
     return breakdown_found = false and create nothing. -->

# Boss: Rename the widget frobnicator

## Problem

The frobnicator is spelled `frobnikator` in three log lines and one metric
label. This is purely cosmetic and confuses grep.

## Goals

- Consistent spelling across logs and metrics.

## Non-Goals

- No behaviour change, no schema change, no new surface.

## Chosen approach

Fix the spelling in place. This is a one-line-per-site edit with no
dependencies and no meaningful breakdown — there is nothing to enumerate as a
task graph, so a coordinator would file (at most) a single chore by hand rather
than a project of tasks.

## References

- None.
