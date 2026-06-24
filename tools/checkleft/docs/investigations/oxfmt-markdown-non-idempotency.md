# oxfmt markdown formatter is non-idempotent (asterisk-emphasis + underscore-identifier)

**oxfmt version:** 0.55.0 (`npx --yes oxfmt@0.55.0`)
**Date:** 2026-06-24
**Severity:** Bug in oxfmt upstream; checkleft workaround in place (see PR #1633)

## Summary

`oxfmt --write` applied twice to certain markdown files produces a different result on the second application than on the first. The second application is stable (a third run produces no further change). This means `oxfmt(oxfmt(x)) != oxfmt(x)` for these inputs — a violation of formatter idempotency.

The non-idempotency is **in oxfmt itself**, confirmed by running it standalone outside checkleft's pipeline. It is not a checkleft pipeline artifact.

## Triggering construct

A paragraph containing both:

1. An underscore-separated identifier (e.g. `foo_bar`, `project_task`), AND
2. Asterisk-style emphasis (`*word*`) anywhere in the same paragraph.

The two elements do not need to be adjacent; sharing a paragraph is sufficient.

## Minimal reproducer

Input file (`repro.md`, `O0`):

```text
a_b *c*.
```

After one `oxfmt --write` invocation (`O1`):

```text
a_b _c_.
```

After a second `oxfmt --write` invocation (`O2`):

```text
a*b \_c*.
```

After a third invocation (`O3`): identical to `O2` — stable. The formatter converges in exactly two passes.

Note: `text` is used for the code blocks above because oxfmt also formats the content of `markdown`-labeled fenced code blocks (it recurses into embedded markdown), which would corrupt the repro strings in this document. That recursive behaviour is related: the same emphasis-normalisation pass that runs on the file also runs on the embedded markdown snippet.

## Mechanism

Pass 1 converts asterisk-delimited emphasis `*word*` to underscore-delimited `_word_`. This is consistent CommonMark normalisation on its own. However, it creates a paragraph that now has two kinds of underscore: the underscore embedded in the identifier `foo_bar`, and the emphasis delimiters `_c_`. The resulting paragraph `a_b _c_.` is ambiguous: a CommonMark parser following the delimiter-run rules can interpret the `_` in `a_b` as a potential left-flanking delimiter.

Pass 2 encounters this ambiguous state and resolves the emphasis using asterisks to avoid the underscore conflict — converting `a_b` → `a*b` and `_c_` → `\_c*`. This resolution is stable (pass 3 leaves it unchanged), but it represents a different output than pass 1 produced. A formatter that must run twice to reach a fixed point is non-idempotent by definition.

## Standalone proof (real files)

The following six markdown files in the repository triggered the two-pass behaviour during `checkleft fix --all`. All six were tested standalone: `oxfmt --write` was run three times on each, comparing pass 1 → pass 2 (should be equal for an idempotent formatter) and pass 2 → pass 3 (verifying convergence).

| File                                                                          | O1 == O2? | O2 == O3? |
| ----------------------------------------------------------------------------- | --------- | --------- |
| `tools/boss/docs/designs/external-issue-tracker-sync-github-projects.md`      | **NO**    | yes       |
| `tools/boss/docs/designs/merge-conflict-handling-in-review.md`                | **NO**    | yes       |
| `tools/boss/docs/designs/transcript-viewer.md`                                | **NO**    | yes       |
| `tools/boss/docs/designs/unify-work-item-kinds-flavors.md`                    | **NO**    | yes       |
| `tools/boss/docs/designs/worker-live-status.md`                               | **NO**    | yes       |
| `tools/boss/docs/investigations/chore-vs-project-task-collapse-2026-05-30.md` | **NO**    | yes       |

All six: non-idempotent on pass 2, converged by pass 3. No exceptions.

Representative diff (O1 → O2) from `chore-vs-project-task-collapse-2026-05-30.md`:

```diff
-The strongest evidence is historical: the _one_ time the engine ever treated chore and project_task differently in real behavior ... fixed by treating them the _same_.
+The strongest evidence is historical: the _one_ time the engine ever treated chore and project*task differently in real behavior ... fixed by treating them the \_same*.
```

Pass 1 had converted `*one*` → `_one_` and `*same*` → `_same_` (normalising to underscores), then left `project_task` untouched. Pass 2 saw the resulting mixed-underscore paragraph and re-escaped: `project_task` → `project*task`, `_same_` → `\_same*`.

## Control: checkleft pipeline does not contribute the delta

The non-idempotency is reproduced running `npx --yes oxfmt@0.55.0 --write <file>` directly, with no checkleft involvement. This rules out candidate explanations involving checkleft:

- **sandbox copy_back**: checkleft copies the fixer's output back to the working tree via atomic rename. The standalone test bypasses this entirely and still exhibits the bug.
- **trailing-newline / line-ending normalisation**: checkleft does not add or strip newlines between the fixer's output and the working-tree file. The standalone test writes in place and produces the same delta.
- **check-vs-fix config drift**: the standalone invocation uses no config (defaults), matching the behaviour documented in `tools/checkleft/checks/format/oxc.yaml` (checkleft imposes no formatting options of its own; it calls oxfmt with `--write` and the file path, nothing else).

Conclusion: the delta introduced on pass 2 is produced by oxfmt, not by checkleft's pipeline.

## Workaround

`checkleft fix --all` now defaults to `max_passes = 10` (PR #1633, `tools/checkleft/src/runner.rs`). The convergence loop breaks early as soon as no files change in a pass, so the cost is one extra oxfmt invocation per affected file (pass 2 stabilises; pass 3 is a no-op that exits early). The workaround is intentional; it is not a checkleft root-cause fix.

## Filing upstream

This is suitable for filing against oxc/oxfmt. Minimum reproduction:

```bash
printf 'a_b *c*.\n' > repro.md
npx --yes oxfmt@0.55.0 --write repro.md  # O1: a_b _c_.
npx --yes oxfmt@0.55.0 --write repro.md  # O2: a*b \_c*.  -- CHANGED
npx --yes oxfmt@0.55.0 --write repro.md  # O3: identical to O2 -- stable
```

Expected: O1 == O2 (idempotency). Actual: O1 != O2 (two passes required to reach a fixed point).

Confirmed on oxfmt 0.55.0 (the current pin in `tools/checkleft/checks/format/oxc.yaml`). When upgrading oxfmt, re-run this repro to check whether the bug is fixed; if O1 == O2 the workaround in `runner.rs` is safe to remove (the loop exits after one pass anyway, but the constant documents intent).
