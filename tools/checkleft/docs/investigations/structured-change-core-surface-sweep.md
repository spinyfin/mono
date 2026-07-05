# Surface sweep: checkleft language-agnostic structured-change core

**Date:** 2026-07-05
**Project:** `checkleft: language-agnostic structured-change core` (P2070, `proj_18be4f34ac75dc10_9aa`)
**Design doc:** `tools/checkleft/docs/designs/checkleft-language-agnostic-structured-change-core.md` — **not on `main`; exists only on open PR [#1685](https://github.com/spinyfin/mono/pull/1685)** (branch `boss/exec_18be4f4280f0be80_9ae`)
**Kind:** incident-002 P6 backstop — verify every design-specified user-facing surface still exists in shipped code; catch silent regressions.

## Bottom line

**No regressed or absent-by-regression surfaces found. This project has shipped nothing yet, so there is nothing that could have regressed.**

The design is explicitly design-only (line 3: _"Status: design (no implementation in this change)"_; line 33: _"This is a design only. No feature code is included"_) and its design doc has **not merged** — it lives solely on open PR #1685. Consequently:

- Every **foundation surface** the design says _already exists in checkleft today_ (§Current-state grounding) is **PRESENT** on `main` and matches the design's description.
- Every **new surface** the design _proposes to build_ (§Proposed implementation task breakdown) is **NOT-YET-BUILT** — absent because no implementation task has landed, **not** because a shipped surface was deleted or downgraded. This is the expected state for an unmerged design, not a regression.

The incident-002 failure mode (a _merged_ surface silently deleted during a forward-port) **cannot apply here**: nothing from this project has merged to begin with.

One genuine, actionable defect surfaced incidentally: the project's **design-doc pointer is broken on `main`** (it resolves to a file that does not exist on `main`). Details and a proposed follow-up are in [Incidental finding](#incidental-finding-broken-design-doc-pointer) below.

## Method & scope note

The task template asks for the design's **§Surfacing** section — the heading that enumerates concrete shipped surfaces (pages, components, endpoints, badges, flags). **This design has no such section, and no such surfaces.** It is a _core-library_ design: its deliverables are Rust traits, types, and modules inside `tools/checkleft`, plus WIT-contract records — not UI/endpoint/flag surfaces. The template's surface vocabulary does not map onto it.

To perform a faithful sweep anyway, the two enumerations that stand in for "surfaces" were both checked against `main`:

1. **§Current-state grounding: checkleft today** (design lines 90–220) — the surfaces the design _asserts already exist_ and builds upon. A regression here (one of these gone/downgraded) _would_ be a real incident-002-class finding.
2. **§Proposed implementation task breakdown** (design lines 666–795) — the surfaces the design _proposes to create_. These are the closest thing to a "surfaces this design ships" list.

Verification was against the working tree at `main@origin` (commit `tvyzrnurpkoo…`, _"test(boss-client): cover engine-command resolution gaps (#1810)"_); the checkleft source was untouched. Symbols were located with `grep`/`find` under `tools/checkleft/{src,sdk,checks,wit}`.

## Table 1 — Foundation surfaces (design §Current-state grounding): must be PRESENT

These are things the design claims exist today. All were verified present with the described shape.

| Surface                                                                                                                    | Design §ref                                             | Status      | Evidence (path on `main`)                                                                                                                        |
| -------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------- | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ConfiguredCheck` trait + `run(&ChangeSet, &dyn SourceTree) -> CheckResult`                                                | §What a check sees and returns (L95–111)                | **PRESENT** | `tools/checkleft/src/check.rs:13` (trait), `:14` (`run`)                                                                                         |
| `ChangeSet` / `ChangedFile` / `ChangeKind` / `FileDiff` / `DiffHunk`                                                       | §The change model (L113–125)                            | **PRESENT** | `tools/checkleft/src/input.rs:10, 115, 123, 88, 105`                                                                                             |
| `FileDiff::line_delta` (hunk data consumed only for line-delta accounting today)                                           | §The change model, fact 1 (L129–132)                    | **PRESENT** | `tools/checkleft/src/input.rs:94`                                                                                                                |
| `SourceTree::read_file_versioned` + `enum TreeVersion { Current, Base }`                                                   | §The change model, fact 2 (L133–145)                    | **PRESENT** | `tools/checkleft/src/source_tree.rs:136` (`read_file_versioned`), `:138–139` (`Current`/`Base` arms)                                             |
| `Finding` / `Severity` / `Location`                                                                                        | §The finding + fix model (L150–156)                     | **PRESENT** | `tools/checkleft/src/output.rs:12, 29, 46`                                                                                                       |
| `SuggestedFix { description, edits }`                                                                                      | §The finding + fix model (L157)                         | **PRESENT** | `tools/checkleft/src/output.rs:54`                                                                                                               |
| `FileEdit { path, old_text, new_text }` (content-based, no span today)                                                     | §The finding + fix model (L158, L172–173)               | **PRESENT** | `tools/checkleft/src/output.rs:60`                                                                                                               |
| WIT records: `change-kind`, `diff-hunk`, `file-diff`, `change-set` (`base-files`), `file-edit`, `suggested-fix`, `finding` | §The finding + fix model (L161–164), §fact 2 (L143–145) | **PRESENT** | `tools/checkleft/wit/check.wit:37, 62, 73, 97, 109, 152, 160, 167`                                                                               |
| SDK `ChangeSet::base_file_content` + `base_files` (base content crosses to wasm guests)                                    | §The change model, fact 2 (L143–145)                    | **PRESENT** | `tools/checkleft/sdk/src/lib.rs:112` (`base_file_content`), `:107` (`base_files`)                                                                |
| File-level `scope_findings_to_changeset` (the pre-T2060 restriction the design generalizes)                                | §Change-region restriction today (L177–185)             | **PRESENT** | `tools/checkleft/src/runner.rs:1322` (design cited `runner.rs:1284`; function moved but is present)                                              |
| `ifchange` in-tree hunk-overlap precedent (`hunk_touches_range`)                                                           | §Change-region restriction today, T2060 gap (L198–204)  | **PRESENT** | `tools/checkleft/checks/file/ifchange/src/lib.rs:453` (plus `_new`/`_old` variants at `:445`/`:449`)                                             |
| Three execution/authoring tiers (built-in, declarative-v1, wasm-component)                                                 | §The three execution/authoring tiers today (L206–220)   | **PRESENT** | `src/checks/**` (built-in); `src/external/declarative/` + `checks/{format,lint}/*.yaml` (declarative); `wit/check.wit` + `sdk/` (wasm-component) |

**Two descriptive nuances noted (not regressions):**

- The design (L189–190) says `scope_findings_to_changeset` is at `runner.rs:1284`; on `main` it is at `runner.rs:1322`. The function exists and behaves as described — the line number is merely stale in the design draft.
- The design (L133–139) describes `read_file_versioned`'s **trait default** as `bail!`ing on `TreeVersion::Base`. The concrete production impl in `source_tree.rs:136` already routes `TreeVersion::Base => self.read_base_file(path)`. This is consistent with the design's own §base-reads task, which frames "make `TreeVersion::Base` real in the production `SourceTree`" as remaining work; the current tree shows base reads at least partially wired. Not a regression — if anything, foundation is slightly _ahead_ of the design's "today" snapshot. Whether `read_base_file` is fully production-ready is out of scope for this sweep.

## Table 2 — Proposed new surfaces (design §Proposed implementation task breakdown): expected NOT-YET-BUILT

These are the surfaces the design proposes to create. All are absent from `main`. **Absent here = not-yet-implemented (design unmerged), not deleted/downgraded** — so none is an incident-002-class regression. Listed for completeness and as a shipped-state baseline for a future post-merge sweep.

| Proposed surface (symbol/artifact)                                                                                                                                                                    | Design §ref                                                                                    | Status                         | Evidence                                                                                                                                      |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- | ------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `core-types`: `trait Adapter`, `trait ParsedModel`, `trait StructuredDelta`, `struct GenericTree`/`Node`/`Span`, `enum GenericChange`/`StructuredDeltaGeneric`, `struct Selectors`, `AdapterRegistry` | §Core traits and types (L308–355), §Structured deltas (L357–398), task `core-types` (L673–679) | **ABSENT (not built)**         | 0 matches for any of these symbols under `src/`,`sdk/`,`checks/`                                                                              |
| `region-gate`: `fn gate_to_changed_regions`, `fn gate_edits_to_changed_regions`, `RegionMode` (T2060 hunk-level restriction)                                                                          | §…Finding/fix_data/FileEdit and T2060 (L538–561), task `region-gate` (L688–695)                | **ABSENT (not built)**         | 0 matches; only the file-level precedent (Table 1) exists                                                                                     |
| `base-reads`: production `TreeVersion::Base` support replacing the `bail!`                                                                                                                            | task `base-reads` (L681–686)                                                                   | **PARTIAL / see nuance**       | `source_tree.rs:139` routes `Base` to `read_base_file`; completeness not assessed here                                                        |
| `generic-tier`: tree-sitter → `GenericTree` substrate + grammar/selector/identity registration                                                                                                        | task `generic-tier` (L699–704)                                                                 | **ABSENT (not built)**         | no `structured`/`GenericTree` module; no tree-sitter adapter substrate in `src/`                                                              |
| `generic-diff`: `fn generic_tree_diff` (shared keyed tree-diff, default `Adapter::diff`)                                                                                                              | §Structured deltas (L340–343), task `generic-diff` (L706–712)                                  | **ABSENT (not built)**         | 0 matches for `generic_tree_diff`                                                                                                             |
| `text-tier`: grammarless Myers line-diff fallback adapter                                                                                                                                             | task `text-tier` (L714–718)                                                                    | **ABSENT (not built)**         | no such adapter                                                                                                                               |
| `finding-fix-model`: `Location.span`, `FileEdit.range`, `struct FixHandle`, `trait Fixer`, `FixerId`, `FixerRegistry`                                                                                 | §Core Finding/fix_data/FileEdit model (L495–536), task `finding-fix-model` (L720–726)          | **ABSENT (not built)**         | `output.rs` `Finding` has no `fix: Option<FixHandle>`; `Location` has no `span`; `FileEdit` has no `range`; 0 matches for `FixHandle`/`Fixer` |
| `rust-consumer`: `trait StructuredCheck` + bridge to `ConfiguredCheck`                                                                                                                                | §How authoring surfaces consume the core (L563–578), task `rust-consumer` (L730–737)           | **ABSENT (not built)**         | 0 matches for `StructuredCheck`                                                                                                               |
| `proto-projection`: `ProtoModel` + `ProtoChange` reference typed adapter                                                                                                                              | §Structured deltas (L404–414), task `proto-projection` (L739–744)                              | **ABSENT (not built)**         | no proto structured-change adapter                                                                                                            |
| `format-registration`, `typed-fix-data`, `config-consumer`, `docs-migration-guide`                                                                                                                    | tasks L746–776                                                                                 | **ABSENT (not built)**         | downstream of the above; none present                                                                                                         |
| Deferred (`starlark-consumer`, typed projections, move-detection, guest-side SDK, cross-file checks, model caching)                                                                                   | Deferred/future (L778–795)                                                                     | **ABSENT (by design; not v1)** | explicitly out of v1 scope                                                                                                                    |

## Incidental finding: broken design-doc pointer

Independent of the surface sweep, the project's design-doc pointer is **broken on `main`**:

- `boss project show proj_18be4f34ac75dc10_9aa` reports `Design doc: tools/checkleft/docs/designs/checkleft-language-agnostic-structured-change-core.md`.
- That path **does not exist on `main`** (`gh api repos/spinyfin/mono/contents/...` → 404; no local file; no code-search hits).
- The rendered GitHub URL in `project show` points at branch `boss/exec_18be4f4280f0be80_9ae` (the still-open design PR #1685), where the file _does_ exist — so the pointer only resolves while that PR is open, and only via that branch, not via `main`.

This is the same class of issue `boss project lint-design-docs` is built to surface (a pointer that resolves cleanly to a branch but whose file is missing on the canonical branch). It is benign _today_ only because PR #1685 is still open; if #1685 is closed-without-merge, or if the file is renamed on merge, the pointer breaks entirely. It is a documentation/metadata defect, **not** a product-surface regression, and is recorded here for the operator rather than fixed in this investigation PR.

## Conclusion

- **Regressed surfaces:** none.
- **Absent-by-regression surfaces:** none.
- **Foundation surfaces (must exist today):** all **PRESENT** and matching the design.
- **Proposed new surfaces:** all **NOT-YET-BUILT**, consistent with an unmerged, design-only project — no shipped surface was deleted or downgraded.

There is no regression to restore. The correct next action for the project is ordinary forward implementation (starting with the Depth-0 tasks `core-types` / `base-reads` / `region-gate`) once design PR #1685 merges — plus the metadata fix for the design-doc pointer noted above.

## Follow-up (for the operator to file separately)

1. **Fix the broken design-doc pointer for P2070.** After PR #1685 merges, confirm `boss project show proj_18be4f34ac75dc10_9aa` resolves the doc on `main`; if the file lands at a different path/name, run `boss project set-design-doc` to update the pointer. Optionally run `boss project lint-design-docs` to catch this and any sibling stale pointers. (Metadata-only; no product code.)
2. **Re-run this surface sweep _after_ the structured-change core actually ships.** Once the Depth-0/1 implementation tasks merge, Table 2's "ABSENT (not built)" rows become the checklist of surfaces that _must then be PRESENT_ — at which point a future forward-port could regress one, and this sweep becomes meaningful as a real incident-002 backstop.
