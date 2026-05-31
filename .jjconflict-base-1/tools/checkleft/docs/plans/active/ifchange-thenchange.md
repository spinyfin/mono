# Plan: IfChange / ThenChange Support In Checkleft

## Goal

Add a built-in Checkleft check that enforces `LINT.IfChange` /
`LINT.ThenChange` contracts, plus the minimal framework support needed to make
those checks accurate for changed blocks rather than merely changed files.

## Done Looks Like

1. Checkleft can parse inline `IfChange` / `ThenChange` directives from source
   files.
2. A built-in `ifchange-thenchange` check can fail when a touched source block
   does not have a corresponding target-file or target-block change.
3. The framework exposes enough diff and base-file context to handle edits,
   deletions, additions, and renames deterministically.
4. The feature is covered by unit tests and runner-level integration tests.
5. User docs explain syntax, scope, and bypass behavior.

## Recommended Implementation Order

### Phase 1: Add Diff Primitives To The Framework

1. Extend `ChangeSet` to carry parsed hunk ranges per changed file.
2. Update Git and Jujutsu diff parsing to populate the richer diff model.
3. Preserve existing line-delta behavior as derived data if other checks still
   depend on it.
4. Add tests for:
   - modified files,
   - added files,
   - deleted files,
   - renamed files,
   - multiple hunks in one file.

### Phase 2: Add Base-Revision File Reads

5. Extend the source-tree layer with a base-revision read API for changed
   files.
6. Implement the API for the local VCS-backed tree used by the CLI.
7. Keep external-check host APIs unchanged in this phase.
8. Add tests for:
   - current vs base reads,
   - renamed paths,
   - deleted files,
   - attempts to read paths outside the repo root.

### Phase 3: Implement Directive Parsing

9. Add a parser for `LINT.IfChange` / `LINT.ThenChange`.
10. Represent parsed contracts with:
    - source file path,
    - source label,
    - source line span,
    - target file path,
    - target label,
    - directive line numbers.
11. Reject malformed contracts with actionable diagnostics.
12. Add parser tests for:
    - unlabeled block,
    - labeled block,
    - file target,
    - file-and-label target,
    - duplicate labels,
    - nested blocks,
    - missing `ThenChange`,
    - malformed target syntax.

### Phase 4: Implement The Built-In Check

13. Add `ifchange-thenchange` under `tools/checkleft/src/checks/`.
14. Register it in `tools/checkleft/src/checks/mod.rs`.
15. Detect touched contracts by combining:
    - parsed old-view contracts,
    - parsed new-view contracts,
    - diff hunk overlap.
16. Resolve target satisfaction for:
    - whole-file targets,
    - labeled block targets,
    - renamed target files.
17. De-duplicate findings so one missing counterpart yields one report.
18. Add check tests for:
    - matching source and target edits,
    - source-only edit,
    - target-only edit,
    - contract addition,
    - contract removal,
    - source rename with target update,
    - missing target file,
    - missing target label.

### Phase 5: Wire Docs And Adoption

19. Add user-facing docs under `tools/checkleft/userdoc/docs/`.
20. Document that Checkleft bypass policy is the supported escape hatch.
21. Add a minimal example to the repo-level `CHECKS.yaml` or example config
    docs once the feature is ready to use.
22. Roll the feature out behind normal check configuration rather than making it
    implicit repo-wide behavior.

## Testing Strategy

Use three layers of tests:

1. Pure parser tests for directive syntax and diagnostics.
2. Input/VCS tests for diff hunks and base-file reads.
3. End-to-end runner tests that model realistic changesets with both passing and
   failing linked edits.

Important cases:

- A block changes and the target file changes elsewhere in the file.
- A block changes and the target block changes but line numbers moved.
- A contract is removed from one side only.
- A target file is renamed in the same change.
- A file mentions `LINT.IfChange` in prose rather than as a directive.

## Risks

- The base-file access design can accidentally bleed VCS concerns too far into
  the generic `SourceTree` API if we are not disciplined.
- Hunk-based overlap logic is easy to get subtly wrong around zero-line insert
  and delete hunks.
- Text-only directive parsing may produce false positives in documentation files
  if the directive grammar is too permissive.
- If we later expose the richer diff/base APIs to external checks, that becomes
  a versioned contract change and should be handled separately.

## Suggested PR Breakdown

1. PR 1: framework diff model and tests.
2. PR 2: base-file reads and parser.
3. PR 3: built-in check, user docs, and example configuration.

That split keeps the risky plumbing separate from the policy logic and makes it
easier to review correctness at each layer.
