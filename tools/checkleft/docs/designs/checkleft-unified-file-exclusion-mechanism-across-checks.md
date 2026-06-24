# Checkleft: A unified file-exclusion mechanism across checks

## Status

Design proposal. Project `P1980` (`checkleft: unified file-exclusion mechanism
across checks`). This document is the deliverable for the design phase; it does
not change code. The final section is a machine-consumable implementation task
breakdown.

## Overview

Today, telling checkleft "don't run this check (or any check) on these paths" is
fragmented. Exclusion is implemented independently inside a handful of checks,
with inconsistent surfaces (some checks have it, most do not), inconsistent
granularity (per-rule vs per-check), no repo-wide layer, and — for formatters —
a reliance on the underlying tool happening to read a `.prettierignore`. The
result is that a consuming repo cannot express two extremely common needs in one
coherent way:

1. **Exclude specific files from ONE check.** Example (from flunge): three HTML
   test-reference files must be excluded from `format/oxc` so they are never
   reformatted, while every other check still sees them.
2. **Exclude a subtree from ALL checks.** Example (from flunge): vendored code
   under `mobile/ios/vendor/**` (and, generally, vendored / generated /
   third-party trees) should never be checked by anything.

The current stopgap for the formatter case is a repo-root `.prettierignore`
(oxfmt's / Prettier's native ignore), tracked in flunge chore **T113**. That
only works because oxfmt and Prettier happen to read that file; it does nothing
for the many checks that are not formatters, and it is silent about whether the
ignore is even honored when checkleft passes explicit file arguments.

This design proposes **one** exclusion vocabulary — a dedicated, subtractive
`exclude` key — that works uniformly across **all** check kinds (declarative,
built-in programmatic, and WASM component), in **two layers** (repo-wide global
and per-check), enforced by the **framework** rather than by individual checks
or by whichever tool a check happens to wrap. Exclusion becomes a first-class
checkleft concept with a single matcher, a single precedence rule against
`applies_to`, and a single inheritance story through the `CHECKS` hierarchy.

## Goals

- **One exclusion vocabulary** that works identically across every check kind,
  configured per-repo in `CHECKS.yaml`.
- **Two layers**, both framework-enforced:
  1. **Global excludes** — repo-wide, apply to every check (the
     "vendored/generated, never check this" case), honoring the `CHECKS`
     parent/child hierarchy and inheritance.
  2. **Per-check excludes** — scoped to a single check instance (the "don't
     format these testdata files with oxc" case).
- **Explicit precedence vs `applies_to`.** Define the effective file set
  precisely: positive selection first, then subtract excludes; excludes always
  win.
- **Unify, don't add a sixth mechanism.** Fold the existing per-check
  `exclude_files` / `exclude_globs` implementations into the one model so there
  is a single system, with backward-compatible aliases for configs already
  deployed in real repos.
- **Framework owns exclusion.** A framework-level exclude must make per-tool
  ignore files (the `.prettierignore` stopgap) unnecessary, so behavior is
  consistent regardless of which tool a check wraps.
- **Behavior guarantees.** An excluded path must (a) produce no findings, (b)
  not trigger the check on that path, (c) never be touched on the `fix` path
  (no reformatting), and (d) behave correctly under `CHECKS`-change scheduling.

## Non-goals

- **Not** a content-based or rule-based suppression mechanism. Exclusion is
  path-glob based; it answers "is this path a target of this check?", not "is
  this specific violation acceptable?". The latter is what `bypass` and
  stale-exclusion auditing already do (see _Relationship to adjacent
  mechanisms_).
- **Not** a replacement for `enabled: false`. Disabling a check entirely, or in
  a subtree, remains the job of `enabled` / a child `CHECKS` override. Exclusion
  narrows the _file set_ a still-enabled check runs on.
- **Not** a change to `applies_to`'s positive-selection semantics or its
  replace-override behavior. Excludes compose _with_ `applies_to`; they do not
  alter it.
- **Not** a redefinition of the `source_tree` `ignore`-crate walk. That walk
  governs which files are _materialized into sandboxes_ (a performance/hermeticity
  concern), not which files are _check targets_. Exclusion governs targets. The
  two stay separate (see _Edge cases_).
- **Not** an attempt, in v1, to fold `skip_symlinks` or the per-rule
  `forbidden-imports-deps` selectors into the unified key. Those are noted as
  future work.
- **Not** a new config file. Everything lives in `CHECKS.yaml` so it composes
  with the existing hierarchy; a `.checkleftignore` is explicitly rejected (see
  _Alternatives_).

## Inventory of today's mechanisms (and their limits)

Everything below was confirmed by reading the source. File:line references are
to the tree at design time.

| #   | Mechanism                                             | Where                                                                                                                                        | Scope / granularity       | Path coordinate                                           | Check kinds it covers                                                                             | Limits                                                                                                                                                                                                                                                                      |
| --- | ----------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------- | --------------------------------------------------------- | ------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | `applies_to` positive globs                           | `src/external/declarative/executor.rs:225` (`select_files`); per-repo override `src/external/declarative/resolve.rs` (`override_applies_to`) | per-check                 | repo-root-relative                                        | **declarative only** (enforced: `src/external/mod.rs` rejects the field in non-declarative modes) | Positive-only — cannot _subtract_ a path. Per-repo override **replaces** the list entirely, so you cannot "keep the definition's set but remove three files".                                                                                                               |
| 2   | Per-rule `exclude_files` (alias `exclude_globs`)      | `src/checks/forbidden_imports_deps.rs:132` (config), `:155` (`applies_to`), `:201` (`is_excluded`)                                           | **per-rule**              | **config-dir-relative** (`path.strip_prefix(config_dir)`) | that one built-in check                                                                           | Only this check; only per-rule; a different coordinate convention (strip-prefix) than the component checks.                                                                                                                                                                 |
| 3   | Per-check `exclude_files` (alias `exclude_globs`)     | `checks/file/size/src/lib.rs:36`; `checks/file/forbidden-path/src/lib.rs:48`; `checks/rust/giant-structs-create/src/lib.rs:54`               | per-check                 | repo-root-relative _after_ host rewrite                   | those three WASM component checks                                                                 | Implemented independently in each guest. The host only **normalizes** coordinates (`scope_exclude_globs_to_repo`, `src/external/runtime.rs:1449`) and passes the config to the guest as JSON; it does **not** filter. Divergence risk: each guest re-derives matching.      |
| 4   | `exclude_structs` (struct-level grandfathering)       | `checks/rust/giant-structs/src/lib.rs:37`; host suppression `src/external/runtime.rs:1219` (`apply_struct_exclusions`)                       | per-struct (not per-file) | qualified `path.rs::Name` or bare `Name` within subtree   | giant-structs family                                                                              | Not a file exclusion at all; a different (finer) axis. Out of scope to unify, but noted so the two are not confused.                                                                                                                                                        |
| 5   | Tool-native ignore (`.prettierignore` / `.gitignore`) | `checks/format/oxc.yaml`, `checks/format/prettier.yaml` (tools discover them at repo root); repo-root `.prettierignore`                      | per-tool                  | repo-root-relative (tool's own semantics)                 | only checks whose tool reads such a file (oxfmt, Prettier)                                        | **Leaky and not guaranteed.** checkleft selects files via `applies_to` and passes them **explicitly** as `{{files}}`; whether the tool still honors its ignore for explicitly-passed args is tool-specific. Covers no non-formatter check. This is the flunge T113 stopgap. |
| 6   | `ignore::WalkBuilder` (`.gitignore`-respecting walk)  | `src/source_tree.rs:180`; tracked overlay `:236`                                                                                             | repo-wide                 | repo-root-relative                                        | sandbox materialization for all checks                                                            | **Different axis.** Decides what gets _materialized into a sandbox_, not what is a _check target_. Tracked vendored files still appear in the VCS changeset and are still checked; the walk does not exclude them from checking.                                            |

Two more mechanisms are adjacent but are **not** exclusion and must not be
conflated:

- **`bypass`** (`src/bypass.rs`): a one-off, _logged_ escape hatch. A
  `BYPASS_<CHECK>=<reason>` directive in a PR/commit description lets a check
  that _would have failed_ pass this once, emitting a `Warning` finding that
  records the reason. Boundary: bypass = "run it, it failed, allow it once,
  loudly"; exclude = "this path is permanently out of scope, silently."
- **`exclusion.rs`** (`src/exclusion.rs`): despite the name, this is
  **stale-exclusion auditing** — it flags `CHECKS`-file exclusion entries that
  have gone dead. It is the right home for future "is this unified exclude still
  load-bearing?" auditing, but it is not itself a selection mechanism.

### What the inventory tells us

- **Coverage gap.** The check kind that most needs framework exclusion
  (declarative formatters) has _none_, and is forced onto the leaky tool-native
  path (#5). The checks that _have_ exclusion (#2, #3) are exactly the ones that
  could most easily express it themselves.
- **No global layer.** There is no way to say "no check, ever, on this subtree."
  You must repeat `exclude_files` on every check that supports it — and you
  simply cannot cover the checks that don't.
- **Surface inconsistency.** Per-rule (#2) vs per-check (#3); two coordinate
  implementations (strip-prefix vs host-rewrite); aliasing (`exclude_files` vs
  `exclude_globs`) re-declared per check.
- **Enforcement is per-check, not framework.** Even where the host helps
  (`scope_exclude_globs_to_repo`), it only normalizes coordinates; the actual
  subtraction is re-implemented in each guest. There is no single matcher and no
  single guarantee.

## Alternatives considered

### A. Dedicated subtractive `exclude` key, framework-enforced, two layers — **chosen**

A first-class `exclude` vocabulary, separate from `applies_to`, applied by the
framework uniformly to every check kind, in a global (top-level) and a per-check
layer. Detailed below. Chosen because it is the only option that (a) covers all
check kinds, (b) adds a global layer, (c) keeps positive and negative
vocabularies separate (so they compose predictably and survive an `applies_to`
override), and (d) lets the framework own enforcement so behavior is
tool-independent.

### B. `!negation` patterns inside `applies_to`

Allow `applies_to: ["frontend/**", "!frontend/vendor/**"]`, gitignore-style
last-match-wins.

Rejected:

- **Breaks the override-replace contract.** A per-repo `applies_to` override
  _replaces_ the definition's list (`override_applies_to`). A repo that retargets
  a check would silently drop the definition's carefully-authored negative
  patterns along with the positives. Keeping `exclude` separate means a repo can
  retarget `applies_to` _and_ keep (or add) excludes independently.
- **Matcher mismatch.** `applies_to` compiles to a `globset::GlobSet`, which is
  an unordered _match-any_ set — it cannot express "match A but not B" within one
  set. Ordered negation needs `ignore::Gitignore`-style last-match-wins, i.e. a
  matcher swap and re-implemented ordering. Subtraction as a second stage avoids
  all of that.
- **Doesn't unify.** `applies_to` is declarative-only. Programmatic and component
  checks have no `applies_to`, so negation-in-`applies_to` would still leave them
  needing a separate mechanism — the exact fragmentation we are removing.
- **Conflates concerns.** One vocabulary doing both selection and subtraction is
  harder to reason about than two composable stages with a stated precedence.

### C. Status quo extended: per-check `exclude_files` everywhere + keep `.prettierignore`

Add `exclude_files` to the remaining checks and continue relying on tool-native
ignores for formatters.

Rejected: this _is_ the fragmentation. It still has no global layer, still
requires repeating excludes on every check, still leaves formatter behavior at
the mercy of whether the tool reads an ignore file, and multiplies the
per-check matcher implementations (and their divergence risk).

### D. Honor `.gitignore` for the changeset / rely on VCS ignore

Make checkleft drop `.gitignore`-matched paths from the changeset.

Rejected: vendored and generated code is frequently **tracked deliberately**
(it's committed). `.gitignore` cannot express "tracked, but don't check it."
The changeset is a VCS _diff_; filtering it by `.gitignore` would also drop
legitimately-changed tracked files. This is why mechanism #6 exists as a
materialization concern only and does not filter targets.

### E. A separate `.checkleftignore` file

A repo-root ignore file, gitignore-syntax, parsed by checkleft.

Rejected: it introduces a second configuration surface outside `CHECKS.yaml`,
does not compose with the `CHECKS` parent/child hierarchy and inheritance, and
cannot express per-check scoping (it is inherently global). Everything the
global layer needs is already expressible as a top-level key in the existing
hierarchical config.

### F. Global-only exclude (no per-check layer)

Rejected on its own: it cannot express "exclude these three files from
`format/oxc` only" (motivating case 1). The two layers are both required.

## Chosen approach

### Config schema

Two new framework-level keys, both accepting the canonical name `exclude` and
the backward-compat aliases `exclude_files` and `exclude_globs`. Each is a
non-empty list of glob strings (globset syntax; `**` crosses directory
boundaries), authored **relative to the `CHECKS` file that declares it**
(config-dir-relative), and normalized to repo-root for matching — the same
authoring convention checks #2 and #3 already use.

**Global excludes** — a top-level key, sibling to `checks:`, `settings:`,
`check_definitions:`:

```yaml
# Root CHECKS.yaml
exclude:
  - "mobile/ios/vendor/**" # vendored: never check, by any check
  - "**/*.generated.*"
  - "Cargo.lock"
  - "MODULE.bazel.lock"

checks:
  - id: format/oxc
  - id: file/size
    config:
      max_lines: 3000
```

**Per-check excludes** — a key on the check entry, sibling to `config:` /
`policy:` / `enabled:` (i.e. a _framework_ key, not buried inside the
check-specific `config` blob):

```yaml
checks:
  # Don't format these three reference files with oxc, but still check
  # everything else about them.
  - id: format/oxc
    exclude:
      - "frontend/testdata/report-*.reference.html"

  # Backward-compatible: the established in-config position keeps working.
  - id: file/size
    config:
      max_lines: 3000
      exclude_files: # legacy position + alias, still honored
        - "**/*.md"
        - "**/*.lock"
```

Rationale for `exclude` being a framework key (sibling to `policy`) rather than
a `config` sub-key: it is the only way to apply it _uniformly_ before any check
runs, regardless of check kind. A `config` sub-key requires each check to opt in
to reading it — which is exactly the per-check fragmentation we are removing.

### Effective-file-set semantics (precedence)

For a non-deleted changed file `f` and a check instance `C` resolved for the
directory of `f`:

```
positive(C, f)  := f is selected by C's positive rule
                   (declarative: applies_to / its override globset;
                    programmatic/component: C's intrinsic target set —
                    by default "the non-deleted changed files in scope")

excluded(C, f)  := f matches per-check exclude(C)
                   OR f matches global exclude (union over the hierarchy
                      from repo root down to f's directory)

effective(C, f) := positive(C, f) AND NOT excluded(C, f)
```

Excludes are **subtractive and always win**: a path matched by any applicable
exclude is removed even if `applies_to` (or its override) selected it. Because
exclusion is a _second stage_ applied after positive selection, it composes
cleanly with `applies_to`'s replace-override: whatever positive set the override
produces, excludes subtract from it. The override cannot accidentally erase the
repo's excludes (they live in a separate key), and the excludes cannot be
defeated by a retarget.

### Two enforcement points (the behavior guarantees)

The framework enforces exclusion at **two** points, which together deliver all
four guarantees:

1. **Selection-time subtraction (target filtering).** Before a check runs, the
   framework removes excluded paths from the file set the check will operate on:
   - Declarative checks: subtract the effective exclude set inside `select_files`,
     _after_ the `applies_to` positive filter. Excluded files never reach the
     `{{files}}` argument list — so they are neither checked nor, on `fix`,
     reformatted (`--write` only ever sees the surviving files).
   - Programmatic / component checks: the host lowers a **pre-filtered**
     changeset into the check (built-in Rust checks receive the same filtered
     view). The guest never sees excluded paths, so it cannot target them.

   This is what delivers guarantees (b) "don't trigger the check on that path"
   and (c) "no reformatting on `fix`". It is _required_ — a backstop alone cannot
   stop a formatter from rewriting a file it was handed.

2. **Finding-location post-filter (uniform backstop).** After a check returns,
   the runner drops any finding whose `location.path` is excluded for that check
   instance. This guarantees (a) "no findings on an excluded path" _uniformly_,
   for every check kind, even a check that ignores the filtered changeset or
   derives a path some other way. Framework-meta findings that intentionally land
   on an unchanged `CHECKS` file (config diagnostics, bypass-applied notices,
   stale-exclusion findings) are exempt from this post-filter — they are about
   the config, not about an excluded target.

Guarantee (d), `CHECKS`-change scheduling, falls out of the incremental model:
scheduling is changeset-driven, and excludes only change _which files are
targets_, not _what gets scheduled_. Adding a path to `exclude` simply means
that path stops producing findings on its next change; removing an exclude means
it resumes. No rescan is required (see _Edge cases_).

### One matcher core

A single `ExclusionMatcher` (working name) is built per check instance from
(global excludes accumulated through the hierarchy) ∪ (that instance's per-check
excludes), compiled once to a `globset::GlobSet` over repo-root-relative paths.
Every check kind consults the _same_ matcher via the framework. This replaces
the per-guest implementations (#2 CHECK-level, #3) with one code path and one set
of semantics (case sensitivity, `**` behavior, deleted-file handling).

### Inheritance through the `CHECKS` hierarchy

- **Global excludes accumulate (union) down the hierarchy.** The effective
  global set for a directory is the union of every ancestor `CHECKS` file's
  `exclude` (each authored relative to its own file's directory, normalized to
  repo-root). This differs deliberately from the check-entry `upsert`-replace
  rule, and the asymmetry is the safe choice: excludes are purely subtractive, so
  union can only ever _remove_ coverage. A child cannot accidentally re-enable
  checking of a parent's vendored tree by redefining a key; it can only add more
  excludes. (Whether a child should be able to _narrow_ a parent's global
  exclude is an open question — see _Risks_.)
- **Per-check excludes follow the check entry.** A per-check `exclude` is part of
  a check instance, and check instances are `upsert`-replaced when a child
  `CHECKS` redefines the same `id`. So a child that redefines a check replaces
  its per-check excludes too — consistent with how the rest of a check entry
  already inherits.
- **Remote root config (`external_checks_url`).** Global excludes from a fetched
  root config participate in the same union, applied first, then local root and
  child configs union on top — consistent with how external configs already
  merge.

### Migration / unification plan

- **Backward compatibility (zero-break).** The framework reads per-check excludes
  from the new sibling `exclude` key **and** from the legacy in-`config`
  `exclude_files` / `exclude_globs` position, feeding both into the one matcher.
  Existing real configs — notably the mono root `CHECKS.yaml`'s `file/size`
  `exclude_files` list — keep working unchanged, now enforced by the framework.
- **Guest-side code becomes redundant, then removed.** Once the framework filters
  the changeset before lowering it into a guest, each guest's own `exclude_files`
  matching matches nothing extra (the files are already gone) — a harmless no-op.
  That dead code (#3 in `file/size`, `file/forbidden-path`,
  `giant-structs-create`, and the CHECK-level path in `forbidden-imports-deps`)
  is deleted in a later cleanup task, not in the wiring task, so each step stays
  small and reversible.
- **`forbidden-imports-deps` per-rule selectors stay.** Its per-_rule_
  `include_globs` / `exclude_files` are a _rule-selection_ feature ("which of
  this check's rules apply to this file"), a finer layer than "is this file a
  target of the check at all". v1 folds only the check-level concept into the
  framework; the framework exclude applies _outermost_ (a framework-excluded file
  never reaches any rule). Folding per-rule selection into the unified key is
  future work.
- **Replace the `.prettierignore` stopgap (flunge T113).** The framework now owns
  exclusion, so the formatter ignore file is no longer how checkleft excludes:
  - flunge's `mobile/ios/vendor/**` → a **global** `exclude` in flunge's
    `CHECKS.yaml` (covering all checks, not just formatters).
  - flunge's three HTML reference files → a **per-check** `exclude` on the
    `format/oxc` instance.
  - The `.prettierignore` can then be deleted, _or_ kept purely so a developer
    invoking `oxfmt` / `prettier` directly (outside checkleft) gets the same
    result. checkleft no longer depends on it either way — that is the point.
  - In mono, the load-bearing root `.prettierignore` entries that are _tracked_
    (`Cargo.lock`, `MODULE.bazel.lock`) move to a global `exclude`; the entries
    that are already `.gitignore`d (`target/`, `node_modules/`, `bazel-*`
    symlinks) need no exclude because they are not in the changeset / are skipped
    by `skip_symlinks` already.

### Relationship to adjacent mechanisms

- **vs `bypass`.** Orthogonal. Use `exclude` for permanent, silent
  out-of-scope paths (vendored trees, generated files). Use `bypass` for a
  one-off, logged exception on a path that is _normally_ in scope. They can
  coexist on the same check.
- **vs stale-exclusion auditing (`exclusion.rs`).** Out of scope for v1 to wire
  the new excludes into the audit, but it is the natural future home: a global
  exclude for a vendored tree is rarely stale, but a per-check `exclude` pinning
  three files can outlive its reason. Marked future.
- **vs the `ignore`-crate walk (`source_tree.rs`).** Different axis. Exclusion
  means "not a check _target_"; it does **not** make a file invisible to
  `SourceTree`. A check may still _read_ an excluded file as context for a
  finding on a non-excluded file (the post-filter only drops findings _located
  on_ excluded paths). This is intentional and important for correctness.

## Edge cases

- **`applies_to` override-replace.** Covered by design: excludes are a separate
  key applied as a second stage, so retargeting `applies_to` neither drops nor is
  blocked by excludes.
- **Symlinks / `skip_symlinks`.** Orthogonal and both subtractive. `skip_symlinks`
  (a per-declarative-check flag, `src/external/declarative/mod.rs`) and the
  `source_tree` symlink-escape handling (`:211`) stay as-is in v1; the exclude
  matcher is path-glob based and does not classify by file type. Unifying
  symlink-skipping under the exclusion model is noted as future work.
- **The `fix` path.** The fixable set is derived from _findings_
  (`compute_fix_plan`, `main.rs:527`), and the sandbox stages exactly that set
  (`fix/safety.rs`). With selection-time subtraction, an excluded file produces
  no findings and is never in `{{files}}` for `--write`; it therefore cannot be
  staged or rewritten. Both enforcement points reinforce the guarantee.
- **VCS changeset vs `ignore`-crate walk.** The changeset (`git diff
--name-status`) is the target source-of-truth and _includes_ tracked
  vendored/generated files. `.gitignore` does not filter it. The new framework
  exclude is precisely what removes those tracked-but-unwanted files from check
  targets — closing the gap that #6 cannot. The materialization walk is
  unaffected.
- **Empty effective set.** A check whose entire target set is excluded must be a
  silent no-op (no findings), never an error. Declarative checks already return
  an empty `CheckResult` when the file list is empty; programmatic checks iterate
  and find nothing. Verify in tests.
- **Deleted files.** Already filtered everywhere (`ChangeKind::Deleted`); the
  matcher only concerns non-deleted paths. No change.
- **`CHECKS`-change scheduling.** Editing `exclude` in a `CHECKS` file is itself
  a change; whether that `CHECKS` file is a check target is governed by the
  existing `settings.include_config_files`. Changing excludes does not require a
  full rescan — the incremental model re-evaluates a path the next time it
  appears in a changeset.

## Risks / open questions

- **Global-exclude inheritance direction.** The design proposes union/accumulate
  (a child can only add excludes). Should a child `CHECKS` be able to _narrow_
  (remove) a parent's global exclude, e.g. to re-enable checking of a subtree of
  an otherwise-excluded vendored tree? Union is safer and simpler; a
  narrowing/override syntax is more flexible but reintroduces "a child silently
  weakens a parent" risk. Recommendation: union for v1; revisit if a real need
  appears.
- **Canonical key name.** `exclude` (chosen) vs keeping `exclude_files` as the
  canonical name (it is what is already deployed) vs supporting both first-class.
  The design accepts all three as input but must pick one to _document and emit_
  as canonical.
- **Backstop scope.** Is the finding-location post-filter worth the slight cost
  and the meta-finding exemption, or should v1 rely solely on selection-time
  subtraction? The backstop is what makes the "no findings on excluded path"
  guarantee _uniform_ across check kinds (including future/third-party checks),
  which argues for keeping it.
- **Per-rule vs framework precedence in `forbidden-imports-deps`.** v1 keeps
  per-rule selectors and applies the framework exclude outermost. Confirm this
  layering is the intended long-term model rather than fully folding per-rule
  selection into the unified key.
- **`.prettierignore` disposition.** Delete it once excludes land, or retain it
  solely for direct (non-checkleft) tool invocation? checkleft will not depend on
  it either way; this is a repo-hygiene call.
- **Auditing the new excludes.** Wiring global/per-check excludes into the
  stale-exclusion audit is deferred. Confirm that deferral is acceptable for v1.

## Proposed implementation task breakdown

Dependency-ordered, PR-sized tasks. Depth indicates the dependency layer; tasks
at the same depth with disjoint dependencies may run in parallel (called out
explicitly). Effort hints: `trivial | small | medium | large`.

### Depth 0

**1. Exclusion matcher core + schema parsing**
_Scope:_ Introduce the `ExclusionMatcher` type and the parsing/normalization for
both layers. Parse the top-level `exclude` and per-check `exclude` keys (plus the
`exclude_files` / `exclude_globs` aliases, and the legacy in-`config` per-check
position), reject empty lists, compile to a `globset::GlobSet`, and normalize
config-dir-relative globs to repo-root (unifying the strip-prefix and
host-rewrite conventions into one). Expose `is_excluded(path)` and a
changeset/file-list filter. Pure unit tests; no check wiring yet.
_Effort:_ medium. _Depends on:_ none.

### Depth 1

**2. Wire excludes into config resolution + hierarchy**
_Scope:_ Thread global excludes through `ResolvedChecks` so they **accumulate**
(union) down `resolve_for_dir` from root to leaf; attach per-check excludes to
`CheckConfig` (honoring `upsert`-replace for the check entry). Provide each
resolved check instance with its effective matcher (global ∪ per-check). Tests
for inheritance, union accumulation, and remote-root-config composition.
_Effort:_ medium. _Depends on:_ Task 1.

### Depth 2 — these three may run in parallel

**3. Selection-time subtraction for declarative checks**
_Scope:_ In `select_files`, subtract the effective exclude set after the
`applies_to` positive filter so excluded files never reach `{{files}}` for either
`run` or `fix`. Tests covering both the run and the `--write` fix path, and the
`applies_to`-override interaction.
_Effort:_ medium. _Depends on:_ Tasks 1, 2. _Parallel with:_ Tasks 4, 5.

**4. Selection-time subtraction for programmatic / component checks**
_Scope:_ Have the host lower a pre-filtered changeset into WASM component checks
and present the same filtered view to built-in Rust checks, so guests never see
excluded paths. Subsumes the exclusion role of `scope_exclude_globs_to_repo`
(coordinate normalization stays only where still needed). Legacy guest-side
`exclude_files` matching becomes a redundant no-op (left in place here; removed in
Task 8). Tests for `file/size`, `file/forbidden-path`, `giant-structs-create`,
and `forbidden-imports-deps` (framework exclude applies outermost; per-rule
selectors unchanged).
_Effort:_ medium. _Depends on:_ Tasks 1, 2. _Parallel with:_ Tasks 3, 5.

**5. Finding-location post-filter backstop**
_Scope:_ In the runner, after each `CheckResult`, drop findings whose
`location.path` is excluded for that check instance, exempting framework-meta
findings that intentionally target unchanged `CHECKS` files (config diagnostics,
bypass-applied, stale-exclusion). Tests proving the uniform "no findings on
excluded path" guarantee, including a check that deliberately ignores the
filtered changeset.
_Effort:_ small. _Depends on:_ Tasks 1, 2. _Parallel with:_ Tasks 3, 4.

### Depth 3 — these two may run in parallel

**6. Docs: extend `checks-config.md`**
_Scope:_ Document the `exclude` key (global + per-check), the
canonical-name/alias rules, precedence vs `applies_to`, hierarchy/inheritance
(union for global, replace for per-check), the two behavior guarantees, and
coexistence with `bypass` and tool-native ignores. Update `concepts.md` /
`canned-checks.md` cross-references as needed.
_Effort:_ small. _Depends on:_ Tasks 1–5. _Parallel with:_ Task 7.

**7. Migration: replace the `.prettierignore` stopgap (flunge T113) + mono root**
_Scope:_ In flunge's `CHECKS.yaml`, add the global `exclude` for
`mobile/ios/vendor/**` and the per-check `exclude` for the three HTML reference
files on `format/oxc`; remove checkleft's reliance on `.prettierignore` (decide
delete vs retain-for-direct-use per the open question). In mono, move the tracked
`.prettierignore` entries (`Cargo.lock`, `MODULE.bazel.lock`) to a global
`exclude` if still needed. Cross-repo (flunge + mono); coordinate accordingly.
_Effort:_ small. _Depends on:_ Task 3 (declarative filtering must land so
formatters honor framework excludes). _Parallel with:_ Task 6.

### Future / not a v1 blocker

**8. Remove redundant guest-side `exclude_files` parsing**
_Scope:_ Delete the now-dead per-check matching in `file/size`,
`file/forbidden-path`, `giant-structs-create`, and the CHECK-level path in
`forbidden-imports-deps` (keep its per-rule selectors). Pure cleanup once the
framework is authoritative; the redundancy is harmless until then.
_Effort:_ small. _Depends on:_ Task 4. _Status:_ future / not a v1 blocker.

**9. Stale-exclusion auditing for framework excludes**
_Scope:_ Let global/per-check excludes participate in the `exclusion.rs`
stale-exclusion audit so dead excludes are flagged on the `CHECKS` entry that
declares them, diff-gated on the files they reference.
_Effort:_ medium. _Depends on:_ Tasks 1, 2. _Status:_ future / not a v1 blocker.

**10. Fold `skip_symlinks` (and consider per-rule selectors) into the model**
_Scope:_ Evaluate expressing symlink-skipping, and the `forbidden-imports-deps`
per-rule include/exclude, within the unified exclusion vocabulary rather than as
separate axes. Design spike before any code.
_Effort:_ medium. _Depends on:_ Tasks 1–5. _Status:_ future / not a v1 blocker.

### Parallelism summary

- Depth 0: Task 1.
- Depth 1: Task 2.
- Depth 2 (parallel): Tasks 3, 4, 5.
- Depth 3 (parallel): Tasks 6, 7.
- Future (any time after their deps): Tasks 8 (after 4), 9 (after 2), 10 (after 5).
