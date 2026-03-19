# Plan: Migrate Checkleft From Flunge

## Goal

Import the existing `checkleft` framework from `flunge` into
`mono/tools/checkleft` while preserving source history where practical,
leaving behavior intact, and creating a clean base for later consolidation
work.

## Current State

The current `checkleft` implementation in `flunge` contains:

- a Rust crate with a binary and library target,
- Bazel targets for the binary, library, and tests,
- built-in checks under `src/checks`,
- external-check runtime support under `src/external`,
- configuration, VCS, and source-tree plumbing,
- user documentation under `userdoc/docs`.

`mono` already has the core infrastructure needed for a future move:

- a Rust workspace rooted at [Cargo.toml](../../../../../Cargo.toml),
- Bazel crate-universe wiring in [MODULE.bazel](../../../../../MODULE.bazel),
- an existing `tools/` layout with tool-local docs patterns.

## Preferred Import Strategy

Prefer a history-preserving import over a filesystem copy.

The recommended implementation path is:

1. derive a branch from `flunge` history containing only `cli/checkleft`,
2. rewrite that branch so the tree lands at `tools/checkleft`,
3. merge that branch into `mono`,
4. layer `mono`-specific integration commits on top.

Concretely, a subtree-style import is the best fit here because it preserves
useful commit history without forcing the entire `flunge` history into the
normal `mono` development flow.

## What "Done" Looks Like

1. `tools/checkleft` exists in `mono` with code, build metadata, and docs.
2. The imported code retains traceable source history from `flunge`.
3. The imported package builds in `mono` via Cargo and Bazel.
4. Existing `checkleft` tests pass in `mono` or have explicitly documented
   follow-up gaps.
5. User-facing docs needed for local development move with the code.
6. No `flunge` callers are updated yet.

## Work Breakdown

### Phase 1: Duplicate The Package

1. Create a history-preserving import of `flunge/cli/checkleft` rather than a
   plain file copy.
2. Use a subtree-style split/import flow so the resulting `mono` path is
   `tools/checkleft` and the old commit history remains inspectable.
3. Drop repo-local build artifacts and generated directories during the import
   (`target/`, transient caches, editor output).
4. Translate the Bazel package naming from `flunge` conventions to `mono`
   conventions, keeping visibility as narrow as possible.

### Phase 2: Integrate With Mono Build Systems

5. Add `tools/checkleft` to the `mono` Cargo workspace members.
6. Merge any missing Rust dependencies into `mono`'s workspace dependency set
   and refresh `Cargo.lock`.
7. Add Bazel targets under `tools/checkleft/BUILD.bazel` using
   `@mono_crates//:defs.bzl`.
8. Verify that no new targets default to public visibility unless a concrete
   consumer requires it.

### Phase 3: Repair Mono-Specific Breakage

9. Fix path assumptions, workspace-relative lookups, and test fixtures that
   currently depend on the `flunge` repo layout.
10. Re-run the `checkleft` test suite under Cargo and Bazel.
11. Document any intentionally deferred incompatibilities.

### Phase 4: Rehome Docs

12. Move or rewrite the `userdoc` content into a durable `mono` doc location.
13. Decide whether operator docs should remain inside `tools/checkleft` or move
    into a broader repo docs site later.
14. Keep check-author and external-check contract docs versioned with the code.

### Phase 5: Follow-On Adoption Work

15. Update `flunge` to consume the `mono` version of `checkleft`.
16. Remove the duplicated implementation from `flunge` once consumers are cut
   over.
17. Add any packaging or distribution workflow needed for cross-repo use.

## Recommended Order

Do the migration in two PRs:

1. This prep PR: establish `tools/checkleft` and planning docs.
2. A follow-up implementation PR: import the actual framework with preserved
   history and wire it into Cargo and Bazel.

The later adoption work for `flunge` should remain a separate task and PR.

## Risks

- Cross-repo history import is more operationally complex than a plain copy,
  especially if we need to rewrite the path before merging into `mono`.
- `checkleft` currently depends on crates that `mono` does not yet declare in
  its shared workspace dependency set.
- Bazel target names and visibility may need adjustment to fit `mono`'s
  stricter package hygiene.
- Some tests may encode the old `flunge/cli/checkleft` path layout.
- The current `userdoc` structure may not map directly onto `mono`'s lighter
  docs layout without either duplication or a docs-site decision.

## Open Questions

- Should `checkleft` stay as a standalone Rust crate under `tools/checkleft`,
  or should some framework pieces eventually move into shared libraries under
  `lib/rust/`?
- Do we want the first implementation PR to carry the existing `userdoc`
  structure verbatim, or only the minimum docs needed to keep developers
  unblocked?
