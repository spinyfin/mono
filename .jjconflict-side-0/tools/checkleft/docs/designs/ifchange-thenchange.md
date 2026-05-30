# Checkleft: IfChange / ThenChange

## Overview

Checkleft should support a lightweight way to declare "manual sync" contracts
between code fragments that live in different files, often in different
languages. The intended model is equivalent to Google's `LINT.IfChange` /
`LINT.ThenChange` convention: if a marked block changes, the linked file or
linked block must also change in the same changeset.

This fits Checkleft well because the framework already evaluates
change-scoped, repository-local policy. The feature should be implemented as a
built-in check with a small framework extension for richer diff access.

## Goals

- Support inline, language-agnostic change-coupling markers in ordinary source
  files.
- Catch "changed source block without corresponding target change" in local and
  CI runs.
- Work across file types without requiring AST support.
- Preserve Checkleft's change-scoped, deterministic execution model.
- Reuse Checkleft's existing severity and bypass policy instead of inventing a
  special exemption channel.

## Non-Goals

- Proving semantic equivalence between the source and target blocks.
- Cross-repository or network-resolved targets.
- Auto-fixing or synchronizing target files.
- General graph scheduling for arbitrary inter-check dependencies.
- A first pass that supports every edge case of Google's internal tooling.

## Proposed User-Facing Model

### Syntax

Adopt the same directive spelling described in Filip Hracek's article and in
Chromium's public docs:

```text
LINT.IfChange
LINT.IfChange(label)
LINT.ThenChange(path)
LINT.ThenChange(path:label)
```

Usage rules:

- Directives live on their own lines inside normal file comments.
- A block starts at `LINT.IfChange...` and ends at the matching
  `LINT.ThenChange(...)`.
- `path` is repository-relative.
- `label` is file-local and names a target block in the referenced file.
- `ThenChange(path)` points at the whole target file rather than a specific
  labeled block.
- Reciprocal annotations are recommended but not required.

Examples:

```text
// LINT.IfChange
const VERSION = "v2";
// LINT.ThenChange(tools/release/version.txt)
```

```text
# LINT.IfChange(schema)
message User {
  string id = 1;
}
# LINT.ThenChange(frontend/src/types.ts:user-schema)
```

### Check Configuration

Add a new built-in check implementation:

```yaml
checks:
  - id: ifchange-thenchange
    policy:
      severity: error
      allow_bypass: true
```

The initial version should not require check-specific config. If later needed,
we can add optional settings such as ignored path globs or stricter reciprocity
validation.

### Enforcement Semantics

For each changed file, the check should:

1. Parse all `IfChange` / `ThenChange` blocks in the current file.
2. Parse the same file from the base revision when the file existed before the
   change.
3. Determine which contracts were touched by the diff, including:
   - edits inside a block body,
   - edits to directive lines,
   - additions of a new contract,
   - removals of an existing contract,
   - renames that move the annotated file.
4. For every touched contract, verify that the declared target also changed in
   the same changeset.

Target satisfaction rules:

- `ThenChange(path)` is satisfied when the target file has any diff in the same
  changeset.
- `ThenChange(path:label)` is satisfied when the target block with that label is
  also touched by the same changeset.
- A rename of the target file counts as a target change.
- A missing target file or missing target label is a configuration-style error
  finding, not a silent pass.

Reciprocity rules:

- The check should not require the target to point back to the source.
- If both sides declare contracts, each side is enforced independently when it
  changes.
- Teams that want symmetric protection should annotate both sides.

Bypass rules:

- Use normal Checkleft policy bypasses (`allow_bypass`, `bypass_name`).
- Do not add a separate `NO_IFTTT`-style escape hatch.

## Why A Framework Extension Is Needed

The current check API is close, but not sufficient for a faithful
implementation:

- `ChangeSet` only exposes per-file added/removed line counts, not hunk ranges.
- `SourceTree` only exposes the current tree, not base-revision file contents.
- The check therefore cannot reliably answer "did this exact block change?" for
  additions, deletions, directive edits, or label retargeting.

This feature should therefore drive a small, generally useful diff model in the
framework rather than embedding ad hoc VCS shelling inside one check.

## Proposed Framework Changes

### 1. Richer Diff Metadata In `ChangeSet`

Replace the current coarse `file_line_deltas` use with a richer structure:

```rust
pub struct FileDiff {
    pub old_path: Option<PathBuf>,
    pub hunks: Vec<DiffHunk>,
}

pub struct DiffHunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
}
```

`ChangeSet` should expose per-file diff hunks for both Git and Jujutsu-backed
runs. The existing added/removed line counts can either become derived data or
remain as convenience helpers on top of the richer representation.

Why this matters:

- block-change detection should use line-range overlap, not "file changed at
  all",
- file renames already exist in `ChangedFile`, but block-level targeting needs
  the hunk coordinates too,
- other future checks are likely to benefit from block-accurate change ranges.

### 2. Base-Revision File Access

Checks need to inspect both the current file and the pre-change file for
changed files. Add a base-snapshot read path through the source-tree layer.

One acceptable shape is:

```rust
pub enum TreeVersion {
    Current,
    Base,
}

pub trait SourceTree {
    fn read_file(&self, path: &Path) -> Result<Vec<u8>>;
    fn read_file_versioned(&self, path: &Path, version: TreeVersion) -> Result<Vec<u8>>;
    // existing helpers...
}
```

Behavior:

- `Current` keeps today's semantics.
- `Base` is only required for changed files and should read from the parent
  revision used to compute the `ChangeSet`.
- Unchanged files do not need base reads.

This keeps the check interface stable while making historical inspection
available to built-ins. External check APIs should remain unchanged in the
first phase; they can continue using current-tree reads only.

### 3. Reusable Directive Parser

Add a small parser module under `tools/checkleft/src/checks/` or a shared
support module that:

- scans UTF-8 text line-by-line,
- extracts `IfChange` blocks,
- tracks line ranges for the block body and directive lines,
- records optional labels and `ThenChange` targets,
- validates duplicate labels, malformed syntax, and unclosed blocks.

The parser should be intentionally text-based. We do not need language-aware
comment parsing in v1 because the directive strings are distinctive and the
feature is opt-in.

## Check Algorithm

The built-in `ifchange-thenchange` check should use this flow:

1. For each changed file, load current contents.
2. If the file existed previously, load base contents.
3. Parse contracts from both views.
4. Use diff hunks to mark touched contracts in the old view and new view.
5. Build a normalized set of touched contracts keyed by:
   - source path,
   - optional source label,
   - target path,
   - optional target label.
6. For each touched contract, decide whether the target changed:
   - whole-file target: any diff for the target path or rename match,
   - labeled target: parse the target file and test for overlap with one of its
     touched blocks.
7. Emit one finding per unsatisfied contract, de-duplicated across the run.

Suggested finding shape:

- message: "`LINT.ThenChange(frontend/src/types.ts:user-schema)` was not updated
  when `backend/api/user.proto:schema` changed"
- location: source file `IfChange` line
- remediation: "Update the linked target in the same change or bypass this
  check with a documented reason."

## Failure Modes And Diagnostics

The check should report explicit errors for:

- malformed directive syntax,
- duplicate labels within one file,
- unlabeled target reference to a missing file,
- labeled target reference to a file that exists but has no matching label,
- nested `IfChange` blocks,
- an `IfChange` block without a terminating `ThenChange`.

These should be surfaced as ordinary check findings with precise file locations.

## Performance Expectations

The feature should remain cheap enough for local runs:

- only parse files that are changed or referenced by touched contracts,
- cache parsed file contracts by `(path, version)` during a run,
- avoid whole-repo scans,
- avoid shelling out per file after the `ChangeSet` and base snapshots are
  prepared.

## Open Questions

- Should v1 support only one `ThenChange(...)` target per block, or a
  comma-separated target list? One target per block is the safer first cut.
- Should doc-only examples and generated sources need an opt-out path filter, or
  is standard bypass policy enough initially?
- Do we eventually want reciprocity validation as a separate mode
  ("if this points there, require a reverse contract")?
