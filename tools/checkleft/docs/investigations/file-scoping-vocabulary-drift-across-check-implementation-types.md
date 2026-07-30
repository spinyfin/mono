# Checkleft file-scoping vocabulary drift across check implementation types

- **Date:** 2026-07-30
- **Work item:** Investigation — drift and variance in checkleft's file-scoping vocabulary across check implementation types
- **Trigger:** [mono#2554](https://github.com/spinyfin/mono/pull/2554), which scopes `boss/no-legacy-filehandle-write-api` to Swift files by adding a per-pattern `paths` key inside the wasm `text/forbidden-pattern` check's own config
- **Scope:** research only. No production code, build file, or `CHECKS.yaml` was changed. All measurements were taken with an unmodified `checkleft` binary built from HEAD against a throwaway fixture repo outside the workspace.
- **Prior art:** [`../designs/checkleft-unified-file-exclusion-mechanism-across-checks.md`](../designs/checkleft-unified-file-exclusion-mechanism-across-checks.md)

Checkleft answers "which files does this check apply to?" through at least twenty distinct mechanisms, and the vocabulary has drifted far enough that the same intent is spelled five different ways depending on how a check happens to be implemented. This document catalogues the drift with citations, distinguishes the naming drift from the behavioural drift, explains why the last unification effort did not prevent this, and proposes one vocabulary that works identically across all three implementation types.

## Verdict

**The `paths` key in mono#2554 is not a mistake by its author — it is a symptom of a framework gap.** Framework-level include-side scoping — spelled `applies_to` today, and proposed below as `include` so that it pairs with `exclude` (§5.1) — is genuinely unreachable from a wasm Component-Model check, in both of the ways an author would try it. `applies_to` is rejected outright in a `mode = "component"` manifest (`src/external/mod.rs:444-449`), and an `applies_to` key placed in a component check's `CHECKS.yaml` `config:` block is **silently ignored** — measured below. There was no framework mechanism the PR could have used instead.

The deeper finding is that the include side of file scoping was never unified at all. The previous unification effort deliberately scoped itself to the _exclude_ side and named leaving `applies_to` alone as an explicit non-goal. It succeeded completely at what it set out to do — framework `exclude` works identically across all three implementation types today, and this was measured. Drift reappeared on the untouched half.

The most dangerous property found is not naming: **a scoping glob that _cannot_ match — a leading `./`, a bare directory name, a `!` prefix — is accepted without a diagnostic and silently selects zero files**, turning a check into a green no-op. `applies_to: ["./src/*.rs"]`, `applies_to: ["!src/**"]`, and `applies_to: ["src"]` each select zero files and report success. A glob that _can_ match but happens to match nothing in this changeset is a different thing entirely — that is the ordinary case on every diff run, and staying silent and green is correct. The defect is that the two are observationally identical. A framework `exclude` list containing an _invalid_ glob is worse in the other direction: it is discarded wholesale with only a tracing warning, so the check runs on everything.

## Method

Every load-bearing claim below is either **measured** or **read**, and each is labelled. Measurements used `bazel-bin/tools/checkleft/checkleft` built from HEAD (`e5b4e11d`) run against a git fixture repo containing files at several depths, a mixed-case filename, a symlink, and a subdirectory `CHECKS.yaml`. The probe check is a synthetic declarative package whose tool emits one finding per file it was handed, so the finding set _is_ the selected file set. A second probe deliberately over-reports on files it was never given, to exercise the framework's finding-stage filters.

Reads cite `file:line` at HEAD. Where a grep returned nothing, the absence was confirmed by locating the symbol that _does_ exist and following it — for example, `effective_matcher_for` (`src/config.rs:227`) is never called by the runner; the runner builds its matcher inline at `src/runner.rs:1276`, which is where the fail-open behaviour lives.

The fixture, the probe manifests, and the exact invocations are reproducible from the tables below; nothing in them depends on the workspace.

---

## 1. Complete inventory of scoping mechanisms

Mechanisms are grouped by the stage at which they narrow the file set. "Coordinate" is the path shape a pattern is matched against.

### Stage A — changeset computation (what is in scope at all)

| #   | Name                                        | Declared in                                    | Shape   | Coordinate     | Notes                                                                                                                                                                  |
| --- | ------------------------------------------- | ---------------------------------------------- | ------- | -------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A1  | `--all` / `--base-ref` / `--default-branch` | CLI flags (`src/main.rs:45-49`)                | include | n/a            | `ChangePlan` (`src/change_detection/mod.rs:31-40`); `--all` → `Vcs::all_files_changeset()` (`src/vcs.rs:121-128`), which shells out to `jj file list` / `git ls-files` |
| A2  | `settings.include_config_files`             | `CHECKS` file, top level (`src/config.rs:541`) | exclude | exact filename | Drops `CHECKS.yaml` / `CHECKS.toml` from scheduling (`src/runner.rs:1401-1402`, `:1440-1443`)                                                                          |
| A3  | VCS tracking / ignore                       | not configurable                               | exclude | n/a            | Untracked and gitignored paths never enter the changeset at all — a _tracked_ vendored tree still does. This is why A3 cannot substitute for an exclude                |

### Stage B — check scheduling and file selection

| #   | Name                                                                                 | Declared in                                                 | Impl types                      | Shape                                                                          | Coordinate                                                                 | Citation                                                                                                                                                                                                                                                                                                                                                                                                                       |
| --- | ------------------------------------------------------------------------------------ | ----------------------------------------------------------- | ------------------------------- | ------------------------------------------------------------------------------ | -------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| B1  | **`CHECKS` file directory placement**                                                | the location of the `CHECKS` file itself                    | all three                       | include, directory-granularity, no glob                                        | repo-relative prefix                                                       | `src/config.rs:300-331` (`resolve_for_file` → `resolve_for_dir` walks up), `src/runner.rs:1145-1152`                                                                                                                                                                                                                                                                                                                           |
| B2  | `scope: changeset`                                                                   | check entry (`src/config.rs:85`, `:98-111`)                 | all three                       | opts out of file scoping entirely                                              | n/a                                                                        | `src/runner.rs:1223-1250` — resolved once at repo root with an **empty** changed-file set                                                                                                                                                                                                                                                                                                                                      |
| B3  | **Global `exclude`** (aliases `exclude_files`, `exclude_globs`)                      | `CHECKS` file, top level (`src/config.rs:530-537`)          | all three                       | exclude                                                                        | config-dir-relative, normalised to repo-relative (`src/config.rs:618-624`) | Unions down the hierarchy (`src/config.rs:408-412`, `:216-218`)                                                                                                                                                                                                                                                                                                                                                                |
| B4  | **Per-check `exclude`** (same aliases)                                               | check entry, sibling of `config:` (`src/config.rs:575-583`) | all three                       | exclude                                                                        | as B3                                                                      | `src/config.rs:688-716`                                                                                                                                                                                                                                                                                                                                                                                                        |
| B5  | Legacy in-`config` `exclude_files` / `exclude_globs`                                 | inside a check's `config:` blob                             | all three                       | exclude                                                                        | as B3                                                                      | `src/config.rs:635-659`. **Only these two names** are read here — canonical `exclude` is not                                                                                                                                                                                                                                                                                                                                   |
| B6  | `ExclusionMatcher`                                                                   | — (the single matcher core for B3–B5)                       | all three                       | exclude                                                                        | repo-relative                                                              | `src/exclusion_matcher.rs:33-53`; changeset subtraction `:70-83`; applied for built-ins `src/runner.rs:268`, for components `src/external/runtime.rs:602`, `:646`, for declarative `src/external/declarative/executor.rs:298`                                                                                                                                                                                                  |
| B7  | **`applies_to`** in a check-definition manifest                                      | definition YAML (`src/external/mod.rs:248`, `:321`)         | **declarative only**            | include                                                                        | repo-relative                                                              | `select_files` `src/external/declarative/executor.rs:280-313`. Rejected in component mode: `src/external/mod.rs:444-449`                                                                                                                                                                                                                                                                                                       |
| B8  | **`config.applies_to`** per-repo override                                            | check entry, **inside** `config:`                           | **declarative only**            | include, _replaces_ B7                                                         | repo-relative                                                              | `src/external/declarative/resolve.rs:280-313`; consumed `executor.rs:110-114`                                                                                                                                                                                                                                                                                                                                                  |
| B9  | `skip_symlinks`                                                                      | definition manifest, boolean (`src/external/mod.rs:254`)    | **declarative only**            | exclude, non-glob                                                              | n/a                                                                        | `executor.rs:299-307`. No per-repo override exists                                                                                                                                                                                                                                                                                                                                                                             |
| B10 | `access_scope` (`modified_only` \| `whole_repo` \| `globs([…])` \| `declared_files`) | Rust `#[check(...)]` attribute                              | **wasm only**                   | include (sandbox read set, _not_ check targets)                                | repo-relative                                                              | `sdk-macro/src/lib.rs:146-174`; `src/external/sandbox.rs:95-107`, `:270-330`                                                                                                                                                                                                                                                                                                                                                   |
| B11 | `declare-required-files`                                                             | wasm guest export                                           | **wasm only**                   | include, per-invocation, concrete paths only                                   | repo-relative                                                              | `wit/check.wit` (`export declare-required-files`)                                                                                                                                                                                                                                                                                                                                                                              |
| B12 | **Hardcoded Rust path predicates**                                                   | check source                                                | **built-in only**               | include                                                                        | repo-relative                                                              | `src/check.rs:75-81` (`count_applicable`), `:90-96` (`run_per_text_file`); e.g. `.github/workflows/*.y{a,}ml` at `src/checks/workflow_yaml.rs:16-24`, `frontend/src/**/*.ts{,x}` at `src/checks/frontend_no_legacy_api.rs:128-133`, `BUILD`/`BUILD.bazel` at `src/checks/repo_visibility.rs:76-79`, `.bazelversion` at `src/checks/bazel/bazelversion_policies.rs:65-67`, Starlark at `src/checks/bazel/bazel_policies.rs:165` |
| B13 | `code-patterns` `lang`                                                               | in-`config` string                                          | built-in                        | include, _language name_ not a glob                                            | extension set                                                              | `src/checks/code_patterns/config.rs:8-10`; `lang: "java"` → `*.java` at `mod.rs:64-71`                                                                                                                                                                                                                                                                                                                                         |
| B14 | `md/doc-structure` `include_globs` + `exclude_globs`                                 | in-`config`                                                 | built-in                        | include + exclude                                                              | **repo-relative**                                                          | `src/checks/doc_structure.rs:71-76`, `:101-111`, `:287-296`. Also gated by a hardcoded `.md` extension test (`:102`)                                                                                                                                                                                                                                                                                                           |
| B15 | `forbidden-imports-deps` **per-rule** `include_globs`                                | in-`config`, per rule                                       | built-in                        | include                                                                        | **repo-relative**                                                          | `src/checks/forbidden_imports_deps.rs:107`, `:144-146`                                                                                                                                                                                                                                                                                                                                                                         |
| B16 | `forbidden-imports-deps` **per-rule** `exclude_files` (alias `exclude_globs`)        | in-`config`, per rule                                       | built-in                        | exclude                                                                        | **config-dir-relative** (`strip_prefix`)                                   | `src/checks/forbidden_imports_deps.rs:109-110`, `:184-189`                                                                                                                                                                                                                                                                                                                                                                     |
| B17 | `text/forbidden-pattern` `surfaces`                                                  | in-`config`                                                 | wasm                            | selects `files` vs `changeset` text                                            | n/a                                                                        | `checks/text/forbidden-pattern/src/lib.rs:106-110`                                                                                                                                                                                                                                                                                                                                                                             |
| B18 | `text/forbidden-pattern` **per-pattern `paths`**                                     | in-`config`, per pattern                                    | wasm                            | include                                                                        | repo-relative                                                              | **mono#2554 only — not at HEAD**                                                                                                                                                                                                                                                                                                                                                                                               |
| B19 | `file/forbidden-path` `rules[].patterns` + `when`                                    | in-`config`, per rule                                       | wasm                            | subject matter, not scoping                                                    | repo-relative                                                              | `checks/file/forbidden-path/src/lib.rs:49-55`                                                                                                                                                                                                                                                                                                                                                                                  |
| B20 | `file/ifchange` `trigger_globs` / `required_globs`                                   | in-`config`, per coupling                                   | wasm                            | subject matter, but `trigger_globs` also gates which files the check reacts to | repo-relative                                                              | `checks/file/ifchange/src/lib.rs:689-712`                                                                                                                                                                                                                                                                                                                                                                                      |
| B21 | `giant-structs` `exclude_structs`                                                    | in-`config`                                                 | wasm                            | exclude on a **symbol** axis, not a file axis                                  | `path.rs::Name` or bare `Name`                                             | `checks/rust/giant-structs-create/src/lib.rs:53`; host-applied `src/external/runtime.rs:1242`                                                                                                                                                                                                                                                                                                                                  |
| B22 | Deleted-file filtering                                                               | not configurable                                            | declarative + built-in **only** | exclude                                                                        | n/a                                                                        | `executor.rs:296`, `src/check.rs:79`, `:101`. Component checks **do** see deleted files — `file/forbidden-path` depends on that                                                                                                                                                                                                                                                                                                |

### Stage C — finding filtering

| #   | Name                          | Impl types           | Citation                                                                                                                                               | No-op under `--all`? |
| --- | ----------------------------- | -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------- |
| C1  | `scope_findings_to_changeset` | all three            | `src/runner.rs:1453-1459`, called first in `apply_policy_to_result` (`:1519-1529`) from both the built-in path (`:295`) and the external path (`:403`) | **yes** (measured)   |
| C2  | `drop_excluded_findings`      | all three            | `src/runner.rs:1472-1477`                                                                                                                              | **no** (measured)    |
| C3  | `policy.changed_lines_only`   | all three            | `src/runner.rs:1495-1517`; config `src/config.rs:132`, `:601`                                                                                          | **yes** (measured)   |
| C4  | `normalize_finding_paths`     | **declarative only** | `executor.rs:206-219` — rewrites absolute and `bazel-out/<cfg>/bin/` paths to repo-relative _so that_ C1 can match them                                | n/a                  |

### Stage D — fix selection

| #   | Name                                    | Shape   | Coordinate                                        | Citation                                                     |
| --- | --------------------------------------- | ------- | ------------------------------------------------- | ------------------------------------------------------------ |
| D1  | `checkleft fix [PATHS…]`                | include | **prefix match, not globs** (`Path::starts_with`) | `src/main.rs:113-114`, `:820-827`                            |
| D2  | `--allow-dirty=false`                   | exclude | exact paths                                       | `src/main.rs:836-841`; `Vcs::dirty_paths` `src/vcs.rs:134`   |
| D3  | `applies_to` re-applied on the fix path | include | repo-relative                                     | `executor.rs:1041-1044`, `filter_by_applies_to` `:1304-1313` |
| D4  | Exclusion subtraction on the fix path   | exclude | repo-relative                                     | `executor.rs:981-991`                                        |

### Mechanisms deliberately excluded from the inventory

- `bazelversion-policies` `patterns` (`src/checks/bazel/bazelversion_policies.rs:227-235`) uses `globset::Glob` but matches **Bazel version strings**, not paths. It is a naming collision, not a scoping mechanism.
- `source_tree.rs:166-230`'s `ignore::WalkBuilder` glob governs sandbox _materialisation_, not check targets. The previous design already ruled this a different axis, and that ruling still holds.
- `bypass` (`src/bypass.rs`) is a logged, one-off exception on a path that is normally in scope. Not scoping.

---

## 2. The per-implementation-type matrix

Rows are mechanisms; columns are the three implementation types.

| Mechanism                                        | Legacy built-in (Rust)                      | Declarative (YAML)                        | Wasm (Component Model)                                                                                               |
| ------------------------------------------------ | ------------------------------------------- | ----------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `CHECKS` file directory placement (B1)           | **available** `src/config.rs:300-331`       | **available** (same)                      | **available** (same)                                                                                                 |
| Global `exclude` (B3)                            | **available** `src/runner.rs:268`           | **available** `executor.rs:298`           | **available** `runtime.rs:646`                                                                                       |
| Per-check `exclude` (B4)                         | **available**                               | **available**                             | **available**                                                                                                        |
| Legacy in-`config` `exclude_files` (B5)          | **available** `src/config.rs:635-659`       | **available** (same)                      | **available** (same)                                                                                                 |
| `exclude` written _inside_ `config:`             | **unavailable — silent no-op** (measured)   | **unavailable — silent no-op** (measured) | **unavailable — silent no-op** (measured)                                                                            |
| `applies_to` in the definition manifest (B7)     | **unavailable** (no manifest exists)        | **available** `src/external/mod.rs:248`   | **unavailable — hard error** `src/external/mod.rs:444-449`                                                           |
| `config.applies_to` override (B8)                | **unavailable — silent no-op**              | **available** `resolve.rs:280-313`        | **unavailable — silent no-op** (measured)                                                                            |
| `applies_to` written as a _sibling_ of `config:` | **unavailable — silent no-op** (measured)   | **unavailable — silent no-op** (measured) | **unavailable — silent no-op** (measured)                                                                            |
| `skip_symlinks` (B9)                             | unavailable                                 | **available** `executor.rs:299-307`       | unavailable                                                                                                          |
| `access_scope` globs (B10)                       | unavailable                                 | unavailable                               | **available but different** — it grows the sandbox read set, it does not narrow check targets (`sandbox.rs:270-311`) |
| Hardcoded Rust predicate (B12)                   | **available** (recompile required)          | unavailable                               | **available** (recompile of the guest required) — e.g. `is_markdown_file` in `checks/md/link-integrity/src/lib.rs`   |
| Bespoke in-`config` include globs                | **available but per-check** (B13, B14, B15) | unavailable                               | **available but per-check** (B18, B20)                                                                               |
| `policy.changed_lines_only` (C3)                 | **available**                               | **available**                             | **available**                                                                                                        |
| Deleted files reach the check (B22)              | **no** `src/check.rs:79`                    | **no** `executor.rs:296`                  | **yes**                                                                                                              |
| `normalize_finding_paths` (C4)                   | unavailable                                 | **available** `executor.rs:206-219`       | unavailable                                                                                                          |

### "Run this check only on Swift files" — the answer, per implementation type

| Implementation type | What the author must do today                                                                                                                                                                                         | Number of distinct answers                                                                                |
| ------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| **Declarative**     | `config: { applies_to: ["**/*.swift"] }` in `CHECKS.yaml`, or `applies_to:` in the definition manifest                                                                                                                | **2** (definition and per-repo override; the override _replaces_ the definition)                          |
| **Legacy built-in** | Edit the check's Rust predicate and recompile — unless the check already happens to expose a bespoke include key (`include_globs` on `md/doc-structure` or `forbidden-imports-deps`, `lang` on `code-patterns`)       | **4** (recompile; `include_globs` repo-relative; `lang`; or enumerate every other file type in `exclude`) |
| **Wasm**            | Nothing framework-level exists. Either the guest already ships a bespoke key (`paths` — only after mono#2554), or recompile the guest's predicate, or enumerate every _other_ file type in a framework `exclude` list | **3**, none of them the framework's own vocabulary                                                        |

The framework `exclude` route is technically available to all three and was measured to work — `exclude: ["*.rs"]` on the wasm probe removed exactly the Rust files. But expressing "only Swift" as "not Rust, not TypeScript, not YAML, not Markdown, …" is unbounded and **fails open**: the first `.kt` file added to the repo silently re-enters the check's scope. That is not a substitute for an include side.

---

## 3. Semantic divergence

Naming drift is visible. Behavioural drift is what changes coverage without anyone noticing.

### 3.1 Glob dialect — one engine, one set of surprises

All glob-shaped mechanisms compile through `globset::Glob::new` with **default builder options**: `applies_to` at `executor.rs:289`, `filter_by_applies_to` at `executor.rs:1309`, `ExclusionMatcher` at `exclusion_matcher.rs:39`, `doc_structure` at `:290`, `forbidden-imports-deps` at `:197`, `SourceTree::glob` at `source_tree.rs:173`, and the `paths` key mono#2554 adds. The dialect is therefore _consistent_, but it is not the dialect authors expect. Measured against the fixture:

| Pattern             | Selected                                                          | Consequence                                                                                                                                |
| ------------------- | ----------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `**/*.rs`           | `src/lib.rs, src/main.rs, src/q1b.rs, top.rs`                     | baseline                                                                                                                                   |
| `*.rs`              | the same four files — identical to the row above                  | `literal_separator` is off, so `*` crosses `/`. `*.rs` is not "top level only"                                                             |
| `sub/*.txt`         | `sub/deep/note.txt`                                               | same cause — one `*` spans two components                                                                                                  |
| `sub?deep/note.txt` | `sub/deep/note.txt`                                               | `?` matches `/` as well                                                                                                                    |
| `src/*.rs`          | `src/lib.rs, src/main.rs, src/q1b.rs`                             |                                                                                                                                            |
| `src/**/*.rs`       | identical to `src/*.rs`                                           | `**` may match zero components                                                                                                             |
| `**/*.{rs,ts}`      | `.rs` and `.ts` files                                             | brace expansion works                                                                                                                      |
| `**/q?b.rs`         | `src/q1b.rs`                                                      |                                                                                                                                            |
| `./src/*.rs`        | **(none)**                                                        | a leading `./` silently matches nothing                                                                                                    |
| `!src/**`           | **(none)**                                                        | `!` is a literal character, not negation — a gitignore habit that silently selects nothing                                                 |
| `src`               | **(none)**                                                        | bare directory names never match; the changeset holds files only                                                                           |
| `src/`              | **(none)**                                                        | trailing slash likewise                                                                                                                    |
| `SRC/**`            | **(none)**                                                        | matching is case-sensitive…                                                                                                                |
| `**/*.RS`           | `src/Upper.RS`                                                    | …even though the fixture sits on a case-insensitive APFS volume where `open("src/upper.rs")` succeeds. `**/*.rs` **misses** `src/Upper.RS` |
| `[invalid`          | **loud error**: `check execution failed: invalid applies_to glob` | invalid _syntax_ is caught                                                                                                                 |
| `srcc/**`           | **(none)**                                                        | a typo'd but syntactically valid glob is silent                                                                                            |

_(measured, `checkleft run --all`, probe declarative check)_

The identical battery against per-check `exclude` and global `exclude` produced the **same dialect in every row** — `./`, `!`, bare directories and case all behave the same way. That is the one piece of good news in this section: there is exactly one glob dialect, so a future unification does not have to reconcile two.

### 3.2 Coordinate systems — the same YAML block, two different meanings

`forbidden-imports-deps` matches `include_globs` against the **repo-relative** path (`:144-146`) and `exclude_files` against the **config-dir-relative** path (`:184-189`, `path.strip_prefix(config_dir)`). Measured with the check declared in `pkg/CHECKS.yaml` over `pkg/b.rs` and `pkg/inner/a.rs`:

| Rule config                       | Files flagged                                            |
| --------------------------------- | -------------------------------------------------------- |
| _(no selectors)_                  | `pkg/b.rs`, `pkg/inner/a.rs`                             |
| `include_globs: ["inner/**"]`     | **(none)** — silently selects zero                       |
| `include_globs: ["pkg/inner/**"]` | `pkg/inner/a.rs`                                         |
| `exclude_files: ["inner/**"]`     | `pkg/b.rs` — works                                       |
| `exclude_files: ["pkg/inner/**"]` | `pkg/b.rs`, `pkg/inner/a.rs` — silently excludes nothing |

_(measured)_

Two keys, three lines apart in the same rule, with exactly inverted coordinate conventions, and both wrong spellings fail silently. The framework `exclude` keys (B3–B5) are config-dir-relative and normalised to repo-relative up front (`src/config.rs:618-624`), agreeing with `exclude_files` and disagreeing with `include_globs`.

Component checks receive changeset paths and finding locations in repo-relative coordinates by contract; `wit/check.wit` states the CHECKS-file directory is "deliberately never exposed to the sandboxed guest", so a wasm check has no way to implement config-dir-relative matching even if it wanted to. Declarative checks additionally get `normalize_finding_paths` (`executor.rs:206-219`) rewriting absolute and `bazel-out/<cfg>/bin/…` paths back to repo-relative — a normalisation the component and built-in paths do **not** get.

### 3.3 Key position — canonical names work in one place, aliases in another

Measured, using the declarative probe with a `**/*.rs` baseline:

| Where the key is written                                           | Result                              |
| ------------------------------------------------------------------ | ----------------------------------- |
| `applies_to:` **inside** `config:`                                 | works — narrows to the listed files |
| `applies_to:` as a **sibling** of `config:` (mirroring `exclude:`) | **silently ignored**                |
| `exclude:` as a **sibling** of `config:`                           | works                               |
| `exclude:` **inside** `config:`                                    | **silently ignored**                |
| `exclude_files:` **inside** `config:`                              | works                               |

The two framework-level scoping vocabularies live in opposite positions, and each is a silent no-op in the other's position. `ParsedCheckConfig` (`src/config.rs:562-585`) carries no `deny_unknown_fields`, so an unrecognised key on a check entry is dropped without a diagnostic; the same is true of `excludes:` (measured — the plural typo is accepted and does nothing). The user docs state that `exclude`, `exclude_files` and `exclude_globs` "are all equivalent… both at the top level and inside a check entry" (`userdoc/docs/checks-config.md:188`); for the in-`config` position that is inaccurate, because `extract_legacy_config_excludes` reads only the two legacy names (`src/config.rs:645`).

### 3.4 Precedence and composition

Composition is well-defined where it exists, and it composes as AND across stages (read, with the exclude leg measured):

```
scheduled(C, f) := f's directory resolves to C            (B1)
positive(C, f)  := applies_to / bespoke include / intrinsic set   (B7, B8, B12–B20)
excluded(C, f)  := global exclude ∪ per-check exclude ∪ legacy in-config   (B3–B5)
effective(C, f) := scheduled ∧ positive ∧ ¬excluded
```

- `exclude` beats `applies_to`: subtraction happens after the positive filter inside `select_files` (`executor.rs:293-309`).
- `config.applies_to` **replaces** the definition's `applies_to` rather than intersecting it (`executor.rs:113`, `resolve.rs:280-313`). This is deliberate and documented, but it means a per-repo retarget silently discards whatever the definition author had carefully authored.
- A check-internal filter (`paths`, `include_globs`, the guest's own predicate) **composes** with the framework filter — it never shadows it, because the framework has already removed excluded paths from the changeset before the check runs (`runtime.rs:646`, `runner.rs:268`) and then drops any surviving finding on an excluded path (C2).
- Two files that resolve to the same check id but a different effective exclude set are scheduled as **separate runs**, keyed on the exclude fingerprint (`src/runner.rs:96-116`, `:1162-1173`).

### 3.5 Interaction with `--all` / whole-tree mode

| Mechanism                                                       | Under `--all`                                                                                                                                                                                                  |
| --------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `applies_to`, `exclude`, `skip_symlinks`, bespoke check filters | still apply (measured — every table in §3.1 was produced under `--all`)                                                                                                                                        |
| `scope_findings_to_changeset` (C1)                              | **no-op** — the changeset _is_ every tracked file (measured: the over-reporting probe's finding on an unchanged `top.rs` survives under `--all` and is dropped in a diff run)                                  |
| `drop_excluded_findings` (C2)                                   | **still applies** (measured — the over-reporting probe's finding on the excluded `src/main.rs` is dropped in both modes)                                                                                       |
| `changed_lines_only` (C3)                                       | **no-op** — `--all` carries no diff hunks, so `changed_lines` is `None` and everything is kept (measured: a line-1 finding survives under `--all` and is dropped in a diff run whose only change is on line 3) |

The `--all` behaviour of C1 is a real hazard for declarative checks: `normalize_finding_paths`'s own doc comment (`executor.rs:203-205`) records that an un-normalised `bazel-out/…` path "NEVER matches a real repo-relative path in the changeset, so the change-scope filter… silently drops every such finding" — a check that finds real warnings and reports none.

### 3.6 Directories, symlinks and generated trees

- **Directories** are never matchable: the changeset contains files only, so `exclude: ["vendor"]` silently excludes nothing while `exclude: ["vendor/**"]` works (measured).
- **Symlinks**: `skip_symlinks` is declarative-only. Measured — with the fixture's tracked `link.rs → src/lib.rs`, `skip_symlinks: false` selects `link.rs` and `skip_symlinks: true` does not. There is no equivalent for built-in or wasm checks; they see the symlink as an ordinary changed file.
- **Generated Bazel trees**: `bazel-out/` and the `bazel-*` convenience symlinks are handled in three unrelated places — `SourceTree::glob` skips escaping symlinks during sandbox materialisation (`source_tree.rs:211-230`), `normalize_finding_paths` strips `bazel-out/<cfg>/bin/` from _declarative_ finding paths (`executor.rs:226-240`), and mono's root `CHECKS.yaml` lists `bazel-*` / `bazel-*/**` as `file/forbidden-path` _subject matter_ patterns. None of the three is the scoping vocabulary.

### 3.7 Failure modes — the finding that matters most

**A misspelled or structurally-empty scope glob never fails loudly** — and it is indistinguishable from a scope that legitimately matched nothing today. Confirmed for every mechanism tested:

| Failure                                                                                                                       | Behaviour                                                                                      | Evidence                                                                                                     |
| ----------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `applies_to` glob with invalid _syntax_ (`[invalid`)                                                                          | **loud** — the check errors out                                                                | measured                                                                                                     |
| `applies_to` pattern that is structurally incapable of matching (leading `./`, trailing `/`, bare directory name, `!` prefix) | **silent — defect.** Check runs, selects zero files, reports success                           | measured (§3.1)                                                                                              |
| `applies_to` valid and structurally matchable, but zero files in this changeset                                               | **silent — correct.** Normal for a diff run; listed for contrast, not as a defect              | measured                                                                                                     |
| `exclude` glob with invalid _syntax_                                                                                          | **silent** — the _entire_ exclude list is discarded and the check runs on everything           | measured; `src/runner.rs:1273-1278` falls back to `ExclusionMatcher::default()` with only a `tracing::warn!` |
| `exclude` glob valid and structurally matchable, but matching nothing                                                         | **silent — correct.** Same reasoning as the row above: an exclude that has nothing to subtract | measured                                                                                                     |
| Framework key in the wrong position (`applies_to` sibling, `exclude` in-`config`)                                             | **silent**                                                                                     | measured                                                                                                     |
| Misspelled framework key (`excludes:`)                                                                                        | **silent**                                                                                     | measured; `src/config.rs:562` has no `deny_unknown_fields`                                                   |
| Unknown key in a _definition manifest_                                                                                        | **loud**                                                                                       | read — `RawDeclarativeCheckManifest` has `deny_unknown_fields` (`src/external/mod.rs:242`)                   |
| `applies_to` in a `mode = "component"` manifest                                                                               | **loud**                                                                                       | read — `src/external/mod.rs:444-449`                                                                         |

A pattern that is structurally matchable but matches nothing in the whole _tree_ (`srcc/**`, `SRC/**`, a path renamed out from under the config) sits between rows 2 and 3: undetectable at config-resolution time, and legitimately empty in some repos, so it warrants a warning under `--all` at most and never an error. §5.5 item 1 treats all three cases.

The reason the split matters is that rows 2 and 3 are **observationally identical**. `run_declarative_check` returns an empty `CheckResult` the moment `select_files` comes back empty (`executor.rs:115-120`), so an author who typed `./src/*.rs` gets byte-identical feedback to an author whose scope legitimately did not fire today. A diagnostic for row 2 closes that gap without touching row 3 — which is the whole reason §5.5 draws its line at _structurally_ empty rather than at zero matches.

The invalid-`exclude` fail-open is a known defect with an **open, unmerged** fix in [mono#1648](https://github.com/spinyfin/mono/pull/1648), which moves validation into config resolution and emits a `ConfigDiagnostic`. It is still live at HEAD; the code comment at `src/runner.rs:1275` ("config validation surfaces it elsewhere") describes that unlanded state, not the current one.

### 3.8 One more divergence: progress counts

`eligible_file_count` returns the full changeset size for component checks (`src/external/runtime.rs:259-266`) and applies the `applies_to` filter for declarative ones (`executor.rs:244-269`). A wasm check that filters internally — as `text/forbidden-pattern` will with `paths` — will report "N files" in the progress UI while scanning a subset. Read, not measured.

---

## 4. Why the previous unification did not hold

The archived project is **"checkleft: unified file-exclusion mechanism across checks"**. Its design doc is [`../designs/checkleft-unified-file-exclusion-mechanism-across-checks.md`](../designs/checkleft-unified-file-exclusion-mechanism-across-checks.md), landed 2026-06-24 in mono#1638. It shipped in two PRs: mono#1640 (matcher core plus config resolution, 2026-06-24) and mono#1641 (enforcement across all check kinds plus removal of the guest-side `exclude_files` code, 2026-06-25).

**What it unified — completely.** One `exclude` vocabulary with two layers (global and per-check), one matcher core, one precedence rule, one inheritance story, and two enforcement points (selection-time subtraction plus a finding-location backstop). All of that is intact at HEAD and was measured working for all three implementation types (§2). The cleanup it filed as future work also landed: no guest still parses `exclude_files` (`checks/file/size/src/lib.rs:11-13`, `checks/file/forbidden-path/src/lib.rs:12-14`, `checks/rust/giant-structs-create/src/lib.rs:28-30` all now carry comments saying the host owns it).

**What it deliberately left out.** The include side, explicitly, in its own Non-goals section:

> **Not** a change to `applies_to`'s positive-selection semantics or its replace-override behavior. Excludes compose _with_ `applies_to`; they do not alter it.

and, when rejecting negation-inside-`applies_to` as an alternative:

> **Doesn't unify.** `applies_to` is declarative-only. Programmatic and component checks have no `applies_to`, so negation-in-`applies_to` would still leave them needing a separate mechanism — the exact fragmentation we are removing.

That second passage is the crux: the design **identified** the include-side gap precisely, named it, and then scoped it out. It also scoped out `skip_symlinks` and the `forbidden-imports-deps` per-rule selectors, filing them as future work that has not been done.

**Did the wasm runtime postdate it? No — it predated it.** The `checkleft:check@0.1.0` WIT contract landed 2026-06-06 (mono#1413), `file/size` moved to wasm on 2026-06-14 (mono#1497), `file/ifchange` on 2026-06-15 (mono#1538), and `declared-files` access scope on 2026-06-16 (mono#1579) — all _before_ the exclusion work on 2026-06-24/25. The exclusion unification explicitly covered the wasm path (its Task 4) and did so successfully. Wasm's arrival is not the explanation.

**So why has drift reappeared?** Three reasons, in order of weight:

1. **The include side was never given a framework home, and new wasm checks keep needing one.** `text/forbidden-pattern` was created 2026-07-24 (mono#2286), a month after the unification. It is a _generic_ check — a regex plus a message, configured entirely from `CHECKS.yaml` — so unlike `file/size` or `md/link-integrity` it has no intrinsic file type. Generic checks are exactly the ones that need include-side scoping, and there is nowhere to put it but the guest's own config.
2. **Nothing forbids a check from shipping its own scoping key.** The framework rejects `applies_to` in a component _manifest_ (`src/external/mod.rs:444-449`) but says nothing about a guest-internal key, and the config blob is passed through verbatim (`src/external/runtime.rs:1443-1456`). There is no lint, no test, and no reviewer-visible signal that a new per-check path key is re-fragmenting the vocabulary.
3. **The silent-no-op failure mode hides the cost.** Because a wrongly-placed or wrongly-spelled framework key does nothing rather than erroring (§3.7), an author who _tried_ `applies_to` on a wasm check would see no error — just a check that kept matching everything. That reads as "the framework does not support this", which is correct, but it arrives as silence rather than as a diagnostic that would have surfaced the gap much earlier.

**A unification proposal that does not close (2) will regress again**, for the same reason: the next generic wasm check will want file scoping, and adding a key to its own config will remain the path of least resistance.

---

## 5. Proposed common vocabulary

One include key and one exclude key, both framework-level, both siblings of `config:`, both applying identically to all three implementation types.

### 5.1 The keys

| Key       | Semantics                                                                                                                                                                                                                                                       | Subsumes / renames                                                                                                                                                                                                    |
| --------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `include` | Non-empty list of globset patterns. When present, the check runs only on changeset files matching at least one pattern. Absent means "every file the check would otherwise see". Authored relative to the declaring `CHECKS` file, normalised to repo-relative. | B7 and B8 (`applies_to` — retained as a permanent alias; only the check-entry _position_ moves, out of `config:`), B13 `lang`, B14 `include_globs`, B18 `paths`, and the check-level role of B12 hardcoded predicates |
| `exclude` | Unchanged from today: subtractive, always wins, aliases `exclude_files` / `exclude_globs` retained.                                                                                                                                                             | B3, B4, B5, B16                                                                                                                                                                                                       |

**On the name.** The include key is spelled `include`, not `applies_to`, so that the pair reads `include` / `exclude` — one word per direction, each the obvious opposite of the other. `applies_to` is the declarative path's historical spelling and is retained as an alias for `include`, exactly the way `exclude_files` and `exclude_globs` are retained as aliases for `exclude`. No existing config has to change; §5.4 has the compatibility detail.

Both keys accept exactly the dialect measured in §3.1. `include` in a check-_definition_ manifest (today's `applies_to`, B7) keeps its current meaning as the definition's default; the check-entry key overrides it, preserving today's replace semantics.

Deliberately **not** subsumed in this proposal:

- `skip_symlinks` (B9) — a file-_type_ predicate, not a path glob. Fold it in only via a separate design, as the previous effort already recommended.
- `access_scope` (B10) and `declare-required-files` (B11) — these grow a sandbox's read set. They are the opposite direction from scoping and must not be conflated.
- `exclude_structs` (B21) — a symbol axis.
- `file/forbidden-path`'s `patterns` (B19) and `file/ifchange`'s `*_globs` (B20) — these are the checks' subject matter. A `file/forbidden-path` with no `patterns` is not a check.

### 5.2 The single enforcement path

One `PathScope` value per check instance, built once during scheduling next to today's `ExclusionMatcher`, holding an optional positive `GlobSet` and the existing negative one. `Runner::build_scheduled_check_run` (`src/runner.rs:1258-1298`) constructs it; the scheduling loop already folds the effective exclude patterns into the run-group key (`:1162-1173`) and would fold the `include` patterns in the same way.

Enforcement lands in exactly the places exclusion already lands, so no new stage is introduced:

1. **Selection-time.** Extend `ExclusionMatcher::filter_changeset` (`src/exclusion_matcher.rs:70-83`) into `PathScope::filter_changeset`, applying the positive filter before the negative one. Its three existing call sites — `runner.rs:268` (built-in), `runtime.rs:602`/`:646` (component), and `executor.rs:298` via `select_files` — then get include-side scoping for free.
2. **Finding-stage.** `drop_excluded_findings` (`runner.rs:1472-1477`) gains the symmetric positive test. `scope_findings_to_changeset` (`runner.rs:1453-1459`) is **untouched** — it is load-bearing and stays exactly as it is.
3. **Fix-stage.** `run_declarative_fix`'s subtraction (`executor.rs:986-991`) and `filter_by_applies_to` (`executor.rs:1304-1313`) collapse into one `PathScope` call.

### 5.3 How a wasm check receives it — it does not, and that is the point

This is the crux question, and the answer is unusually clean: **the WIT contract does not change at all.**

A component check today receives `check-input { changeset, config-json }` (`wit/check.wit`). The host already lowers an _exclusion-filtered_ changeset — `let filtered = exclusion.filter_changeset(changeset)` at `src/external/runtime.rs:646`, with the guest-facing comment "the host lowers an exclusion-filtered changeset so the guest never sees an excluded path and cannot target it". Applying the positive filter at the same call site means a scoped wasm check simply receives a smaller `changed-files` list. It needs no new import, no new export, and no awareness that scoping happened.

Consequences worth stating explicitly:

- **No breaking change for external check authors.** `checkleft:check@0.1.0` stays at `0.1.0`; existing `.wasm` artifacts keep working with unchanged sha256 pins.
- The sandbox is populated from the same filtered changeset (`execute_component_check` → `run_component_check`, `runtime.rs:441-452`), so a scoped check also stops _materialising_ files it will not look at — a small hermeticity and speed win.
- A `whole_repo` / `globs` access-scope check (`md/link-integrity`) still reads the whole tree; only its _targets_ narrow. That is the correct and existing behaviour for `exclude`.
- A `scope: changeset` check (`src/runner.rs:1223-1250`) runs with an empty changed-file set by construction, so `include` on such an entry is meaningless. It should be rejected as a config diagnostic, not silently accepted.

### 5.4 Migration path — every check that uses a bespoke mechanism

Enumerated exhaustively; there are seven, plus one PR in flight.

| #   | Check                                                                                                                                                                                                    | Bespoke mechanism today                             | Migration                                                                                                                                                                                                                             | Coverage-change risk                                                                      |
| --- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| 1   | `text/forbidden-pattern` (mono#2554)                                                                                                                                                                     | per-pattern `paths` (B18)                           | For the `boss/no-legacy-filehandle-write-api` instance all three patterns share `["**/*.swift"]`, so a single check-entry `include` is exact. Delete the guest-side key and its `globset` dependency.                                 | **none** if the instance's patterns share one scope — verify per instance before deleting |
| 2   | `md/doc-structure`                                                                                                                                                                                       | in-`config` `include_globs` + `exclude_globs` (B14) | `include_globs` → check-entry `include`; `exclude_globs` → `exclude`. Both are already repo-relative and already `globset` — a pure rename. The hardcoded `.md` gate (`doc_structure.rs:102`) stays as the check's intrinsic subject. | **low** — same coordinate, same dialect. mono's root `CHECKS.yaml` has one instance       |
| 3   | `forbidden-imports-deps` per-rule `include_globs` (B15)                                                                                                                                                  | repo-relative                                       | Rules that share one scope hoist to check-entry `include`. Rules with genuinely different scopes need per-rule selection to survive — see §5.6.                                                                                       | **low** for the hoist                                                                     |
| 4   | `forbidden-imports-deps` per-rule `exclude_files` (B16)                                                                                                                                                  | **config-dir-relative**                             | Hoist to check-entry `exclude`, **rewriting each pattern to repo-relative**. This is the one migration that changes the pattern text.                                                                                                 | **high** — this is the coordinate flip measured in §3.2                                   |
| 5   | `code-patterns` `lang` (B13)                                                                                                                                                                             | language name → extension set                       | `lang` stays (it selects the _parser_), but its implicit file gate becomes an explicit default `include` on the definition.                                                                                                           | **none** if the default matches `matches_language_path` exactly                           |
| 6   | Built-in hardcoded predicates (B12) — `workflow-*` (three checks), `repo-visibility`, `bazel-policies`, `bazelrc-policies`, `bazelversion-policies`, `frontend-no-legacy-api`, `rust-test-rule-coverage` | Rust `fn is_*` predicates                           | These are the checks' _intrinsic_ subject, not repo policy. Leave them; they become the "would otherwise see" set that `include` narrows. Optionally express each as a definition-level default `include` in a later pass.            | **none** (no change)                                                                      |
| 7   | `md/link-integrity` `is_markdown_file`                                                                                                                                                                   | guest predicate                                     | Same as 6 — intrinsic. Leave it.                                                                                                                                                                                                      | **none**                                                                                  |
| 8   | `file/size`, `file/forbidden-path`, `change/file-count`, `giant-structs*`, `file/ifchange`                                                                                                               | none (already framework-only for exclusion)         | Nothing to migrate.                                                                                                                                                                                                                   | **none**                                                                                  |

**Compatibility cost of the renames.** `CHECKS.yaml` files exist in mono (3), flunge, and checkleft-sandbox. The proposal adds a _new name_ in a _new position_ rather than repurposing an existing key, so nothing breaks on day one:

- `applies_to` is retained as an alias for `include` in both positions it works in today — the check-_definition_ manifest (B7) and inside `config:` (B8). Every existing `applies_to` keeps working with unchanged semantics. This is the same treatment `exclude_files` and `exclude_globs` already get, and it costs one `#[serde(alias)]`.
- `exclude` / `exclude_files` / `exclude_globs` are untouched.
- Only the seven bespoke keys in the table above are candidates for removal, and each is in-tree in mono.

**A deprecation window is needed for exactly one thing, and it is a _position_, not a spelling: `config.applies_to`.** Moving the check-entry key out of `config:` to sit beside `exclude` is the only change that could break an out-of-tree `CHECKS.yaml`. Recommended shape: accept both positions for one release, emit a `ConfigDiagnostic` at `warning` when the in-`config` position is used, then make it an error.

The `applies_to` **spelling** needs no window at all, and should not get one: keeping it as a permanent alias is what makes the rename free, exactly as `exclude_files` is a permanent alias today. The cost of that choice is honest and worth stating — for as long as both spellings are accepted, a reader of an unfamiliar `CHECKS.yaml` may meet either word. The alternative, forcing every `applies_to` to `include`, buys one word at the price of a breaking change to configs the framework cannot see. Not worth it; documentation should present `include` as the name and `applies_to` as the legacy alias, and the guard-rail check of §5.6 should be written against the canonical name.

### 5.5 What becomes an error rather than a silent no-op

This is the half of the proposal that has more safety value than the renaming. Each of these is a silent no-op today (all measured in §3.7):

1. **`include` (or `exclude`) matching zero files in the current changeset** — _not_ an error. This is normal for a diff run. But an `include` that cannot match anything **structurally** should be: a pattern with a leading `./`, a pattern ending in a path separator, and a pattern that is a bare directory name with no wildcard are all guaranteed-empty in any repo and any changeset, decidable without touching the tree, and should be `ConfigDiagnostic` errors at config-resolution time. That is the same judgement `override_applies_to` already makes for an _empty_ `applies_to` list (`resolve.rs:290-295`), which errors with "use `enabled: false` to disable the check instead" — an empty list is the degenerate structurally-empty pattern, so this generalises existing precedent rather than inventing a new severity.

   A pattern that is structurally matchable but matches nothing in the whole _tree_ (`srcc/**`, `SRC/**`) is a **third** case, and it is neither of the above: not decidable at config-resolution time, and not always wrong — `include: ["**/*.kt"]` on a guard check in a repo that has no Kotlin yet is deliberate and must stay green. So at most a **warning**, and only under `--all`, where zero-of-every-tracked-file is a meaningful signal. Never an error. The plumbing already exists: `eligible_file_count` is computed per check before dispatch and handed to the progress reporter (`src/runner.rs:358-370`, `src/progress/state.rs:108`), so "selected 0 of N files" has a natural home with no new machinery.

2. **An unrecognised key on a check entry** — add `deny_unknown_fields` to `ParsedCheckConfig` (`src/config.rs:562`), which turns `excludes:`, `includes:`, and a misplaced `paths:` into diagnostics. This is a breaking change for any `CHECKS.yaml` carrying a stray key and needs the same deprecation window.
3. **An invalid glob in an `exclude` list** — already fixed in the open mono#1648; land it.
4. **`include` on a `scope: changeset` check** — meaningless, so reject.
5. **A `!`-prefixed pattern** — reject with a diagnostic pointing at `exclude`, rather than compiling it as a literal that matches nothing.

### 5.6 Should the framework forbid per-check scoping config?

**Yes, for the check-level axis; no, for genuinely finer axes.**

The distinction that makes this tractable: a check may not ship config that answers _"is this file a target of this check at all"_ — that is the framework's question. It may ship config on a strictly finer axis the framework cannot express, such as "which of my rules applies to this file" (`forbidden-imports-deps` per-rule) or "which of my patterns applies to this file" (`text/forbidden-pattern` per-pattern). The previous design drew exactly this line for `forbidden-imports-deps` and it held; the failure was that nothing _enforced_ it.

Enforcement, in increasing order of strength:

- **Naming rule.** A finer-axis selector must use the framework's word (`include` / `exclude`) nested under the finer construct — `patterns[].include`, `rules[].include` — never a new word like `paths`. One vocabulary, several scopes, the way `exclude` already works at global and per-check levels.
- **A check that ships a _check-level_ path key anyway** should be caught by review, and by a checkleft check over `checks/**/src/lib.rs` and `src/checks/**` looking for top-level config fields named from a denylist (`paths`, `include_globs`, `file_globs`, `only`, `scope`, `applies_to`). That check would have flagged mono#2554 and would flag the next one. Note that `applies_to` belongs on that denylist for _guest-side_ config even though it stays a valid framework alias: a guest that parses it is re-implementing the framework's job under the framework's own word, which is harder to spot in review than a novel word like `paths`, not easier.
- **Not proposed:** a runtime rejection. The host cannot tell a scoping key from a subject-matter key by inspecting `config-json`, and guessing would break `file/forbidden-path`.

---

## 6. Recommendation on the triggering PR (mono#2554)

**Land it. Rename `paths` to `include`. Do not block it on the framework work.**

Rationale, in the order a reviewer needs it:

1. **The operator's premise — that `paths` sits "alongside a standard `applies_to`" — does not hold for this check.** There is no reachable `applies_to` for a wasm check. Measured: an `applies_to` key in a component check's `config:` block is passed to the guest verbatim (`runtime.rs:1443-1456`), lands in a `serde` struct with no such field, and is discarded without a diagnostic; the check keeps matching every file. The manifest position is a hard error (`src/external/mod.rs:444-449`). The author had no framework option.
2. **The false positive it fixes is real and live.** A raw `\.write\(\s*[A-Za-z_]…\)` regex matching a Rust string literal that embeds a Python `sys.stdout.write(...)` call, and then pointing the author at Swift `DiagnosticWrite` remediation text, is a genuine defect. The alternative framework-level workaround — enumerating every non-Swift extension in `exclude` — fails open on the next new language in the repo.
3. **The rename is the whole of the change needed.** `paths` has identical semantics to the framework's include side: it compiles through `globset::Glob::new` with default options and matches the repo-relative changeset path. Renaming costs one key name plus its doc comment and three lines of `tools/boss/CHECKS.yaml`. Rename it to `include` — the word §5.1 proposes — rather than to today's `applies_to`, so the guest key is already spelled the way the framework will spell it and the phase-3 hoist is a pure deletion instead of a second rename. Stated honestly: until phase 1 lands, this leaves the repo with two spellings of one concept (`applies_to` on the declarative path, `include` on this guest) where renaming to `applies_to` would collapse them today. That is the better trade, because the guest key is deleted at phase 3 either way, and if phase 3 slips the repo is left holding the end-state word rather than the legacy one.
4. **Per-pattern granularity is legitimate and should survive.** `text/forbidden-pattern` hosts N independent patterns under one check id; a check-level scope cannot express "pattern A is Swift-only, pattern B is universal". Under §5.6's rule, `patterns[].include` is the correct shape. (In _this_ instance all three patterns share `["**/*.swift"]`, so once the framework key exists the per-pattern keys can be hoisted and deleted — but that is a follow-up, not a precondition.)
5. **Add one line to the PR that the current key does not have:** a comment in `checks/text/forbidden-pattern/src/lib.rs` stating that this key exists only because framework-level include-side scoping is not reachable from a component check today (§5.1 proposes it as `include`), and that it is to be hoisted and deleted once it is. Without that, the next author reads it as precedent.

If the coordinator prefers to hold the PR: the blocking dependency is §7's Phase 1 (roughly one PR), after which mono#2554 reduces to three lines of `CHECKS.yaml` and zero guest changes. That is the cleaner end state; it is not worth leaving the false positive in place for.

---

## 7. Sizing the unification

**Verdict: a small stack — four PRs — not a single chore and not a project.**

| Phase                                         | Scope                                                                                                                                                                                                                                                                   | Files                                                                                                                                                             | Effort |
| --------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------ |
| **1. `PathScope` core + framework `include`** | Parse and normalise a check-entry `include` (accepting `applies_to` as an alias); generalise `ExclusionMatcher` into `PathScope` with an optional positive set; fold into the run-group key; apply at the three existing selection sites and the finding-stage backstop | `src/config.rs`, `src/exclusion_matcher.rs`, `src/runner.rs`, `src/external/runtime.rs`, `src/external/declarative/executor.rs`                                   | medium |
| **2. Loud failures**                          | `deny_unknown_fields` on `ParsedCheckConfig`; structurally-empty-glob diagnostics; reject `!` patterns; reject `include` on `scope: changeset`. Land mono#1648 first or fold it in                                                                                      | `src/config.rs`, `src/runner.rs`                                                                                                                                  | small  |
| **3. Migrate the bespoke keys**               | The seven rows of §5.4 (items 1–5), plus the three mono `CHECKS.yaml` files                                                                                                                                                                                             | `src/checks/doc_structure.rs`, `src/checks/forbidden_imports_deps.rs`, `src/checks/code_patterns/`, `checks/text/forbidden-pattern/src/lib.rs`, 3 × `CHECKS.yaml` | medium |
| **4. Guard rail + docs**                      | The denylist check from §5.6; `userdoc/docs/checks-config.md` (including the in-`config`-`exclude` inaccuracy at `:188`); `check-author-api.md`                                                                                                                         | docs + one check definition                                                                                                                                       | small  |

**Checks that migrate:** 5 with config changes (`md/doc-structure`, `forbidden-imports-deps`, `code-patterns`, `text/forbidden-pattern`, plus the `boss/no-legacy-filehandle-write-api` instance). 9 built-ins and 2 wasm checks keep their intrinsic Rust predicates unchanged.

**Does the wasm interface change?** **No.** `wit/check.wit` is untouched; filtering happens host-side before lowering (§5.3). This is **not** a breaking change for external check authors — no artifact rebuild, no sha256 re-pin, no version bump.

**Risk that a migration silently changes coverage — and how a reviewer detects it.** The risk is concentrated in exactly one place: `forbidden-imports-deps`'s `exclude_files`, which is config-dir-relative (§3.2) and must have every pattern rewritten. A reviewer cannot see that from the diff, because the _wrong_ rewrite also looks plausible.

The detection method that actually works, and should be a required step in the phase-3 PR description:

```sh
# For each migrated check, before and after, on a whole-tree run:
checkleft run --all --format json > before.json      # at the base commit
checkleft run --all --format json > after.json       # with the migration applied
# The set of (check_id, location.path) pairs must be identical.
diff <(jq -r '.[]|.check_id as $c|.findings[]|select(.location)|"\($c)\t\(.location.path)"' before.json | sort -u) \
     <(jq -r '.[]|.check_id as $c|.findings[]|select(.location)|"\($c)\t\(.location.path)"' after.json | sort -u)
```

`--all` is essential here: a diff-based run only exercises the files that happen to have changed, so it cannot distinguish "scope preserved" from "scope silently narrowed to zero". This is one of the few legitimate uses of `--all` outside CI's integrity pipeline, and it should be stated as such in the PR.

A weaker but cheaper signal: after phase 2, a migration that narrows a scope to nothing structurally becomes a config diagnostic rather than a silent pass — which is most of the reason phase 2 should land before phase 3.

---

## Open questions

1. **Should a check-entry `include` intersect the definition's `include`, or replace it?** Today's `config.applies_to` replaces (`executor.rs:113`). Replace is simpler and matches the word "override"; intersect is safer, since a repo retargeting `format/rust` to `tools/**` cannot accidentally pull in non-Rust files. The two differ observably. Not resolved here.
2. **Is `forbidden-imports-deps`'s config-dir-relative `exclude_files` (B16) intentional, or a bug?** The previous design recorded it as "a different coordinate convention" without ruling on it. Given that its sibling `include_globs` is repo-relative and the framework `exclude` is repo-relative-after-normalisation, it looks like an accident — but it has been load-bearing since before the unification and changing it is a behaviour change for `tools/boss/CHECKS.yaml`. Needs an owner's call.
3. **Should the check-entry `include` be authored config-dir-relative (like `exclude`) or repo-relative (like today's `config.applies_to`)?** These disagree. Config-dir-relative is more consistent with `exclude` and more useful in a subdirectory `CHECKS.yaml`; repo-relative preserves every existing `config.applies_to` in mono, flunge, and checkleft-sandbox verbatim. Making them disagree _within the proposal_ would be the worst outcome.
4. **What is the right granularity guard?** §5.6 proposes a denylist check over check sources. Whether a static name denylist is strong enough, or whether the SDK should expose a typed "finer-axis scope" helper that the framework can recognise, is unresolved.
5. **How many out-of-tree `CHECKS.yaml` files carry a stray key today?** `deny_unknown_fields` on `ParsedCheckConfig` (§5.5 item 2) is the single highest-value safety change here and the single most likely to break someone. This was not measurable from mono; it needs a survey of flunge and checkleft-sandbox before phase 2 is scheduled.
6. **Does `?` matching `/` (§3.1) ever produce a wrong match in a real config?** Measured as true in the fixture; no real-world instance was found in mono's three `CHECKS.yaml` files. Whether to set `literal_separator` and take the compatibility hit is a separate question from this proposal and should not be bundled into it.
7. **Case sensitivity on a case-insensitive filesystem** (§3.1): `**/*.rs` does not match `src/Upper.RS`, but the file opens as `src/upper.rs`. Whether checkleft should case-fold on such volumes, warn, or keep byte-exact matching is undecided. Byte-exact matching at least matches what CI on Linux does, which is an argument for leaving it alone.

## Follow-up code changes noted for separate filing

None of these were made; each is a separate change from the unification itself.

1. **Land mono#1648** — invalid `exclude` globs currently fail open silently (§3.7). The fix exists and is unmerged.
2. **Fix the stale comment at `src/runner.rs:1275`** — "config validation surfaces it elsewhere" describes the state after mono#1648, not HEAD.
3. **Fix `userdoc/docs/checks-config.md:188`** — the claim that `exclude`, `exclude_files` and `exclude_globs` are equivalent "both at the top level and inside a check entry" is false for the in-`config` position (§3.3).
4. **Component `eligible_file_count`** (`src/external/runtime.rs:259-266`) reports the whole changeset for every wasm check, so the progress UI over-reports for any check that filters internally (§3.8).
