# Checkleft: unify the include side of file scoping across check implementation types

- **Date:** 2026-07-30 (design); 2026-09-02 (as-built)
- **Status:** shipped — one framework `include` key, intersect composition, config-dir-relative authoring, doorway-anchored guard
- **Design PR:** [mono#2582](https://github.com/spinyfin/mono/pull/2582)
- **Shipped in:** [mono#2618](https://github.com/spinyfin/mono/pull/2618) (enforcement path), [mono#2704](https://github.com/spinyfin/mono/pull/2704) (per-check migrations), [mono#2761](https://github.com/spinyfin/mono/pull/2761) (guard check)
- **Related, after the framework landed:** [mono#2554](https://github.com/spinyfin/mono/pull/2554) reduced to the `tools/boss/CHECKS.yaml` `include` lines (no third include implementation)
- **Input investigation:** [`../investigations/file-scoping-vocabulary-drift-across-check-implementation-types.md`](../investigations/file-scoping-vocabulary-drift-across-check-implementation-types.md) ([mono#2559](https://github.com/spinyfin/mono/pull/2559), merged 2026-07-30)
- **Prior art (exclude side):** [`checkleft-unified-file-exclusion-mechanism-across-checks.md`](./checkleft-unified-file-exclusion-mechanism-across-checks.md) ([mono#1638](https://github.com/spinyfin/mono/pull/1638) / [mono#1640](https://github.com/spinyfin/mono/pull/1640) / [mono#1641](https://github.com/spinyfin/mono/pull/1641))

Checkleft's exclude side was already unified; its include side was not. The load-bearing property this project shipped is that **one framework key answers "which files is this check a target of" identically for built-in Rust checks, declarative checks, and wasm component checks**, and that the check-entry list **intersects** the check's definition scope rather than replacing it.

## Verdict

Adopt **`include`** as the single framework include key, paired with `exclude`. Make it **intersect** the check's definition scope rather than replace it, author it **config-dir-relative** exactly like `exclude`, and guard the boundary with a **name denylist anchored on the config-deserialisation doorway**, not an unanchored name denylist.

That is what shipped. Two of those four reversed a recommendation in the input investigation; both reversals were driven by measurements taken for the original design and are argued in place below. A fifth reversal happened during implementation: the original design ([mono#2582](https://github.com/spinyfin/mono/pull/2582)) chose `applies_to` as the canonical word; [mono#2618](https://github.com/spinyfin/mono/pull/2618) renamed to `include` and kept `applies_to` as a permanent serde alias on declarative manifests. The as-built public surface is `include` / `exclude`.

## Goals — delivered

Collapse the include side of file scoping to one framework-level key that behaves identically across all three check implementation types, and extend it to the types that had no framework include side at all.

Concretely, as of the three shipping PRs:

- One key name (`include`), one glob dialect (`globset` defaults), one coordinate system (config-dir-relative, normalised to repo-relative), one composition rule (intersect), reachable from a `CHECKS` file for a built-in, a declarative, and a component check alike.
- No check ships its own check-level answer to "is this file a target of this check", and `checkleft/no-check-level-file-scoping` enforces that so the drift cannot silently recur.
- The settled zero-match **error** case (structurally-empty patterns) is preserved and applied to both keys. The `--all` zero-match **warning** did not ship; see Remaining gaps.

The original goal that "a migration that changes a check's effective file set is detectable by a mechanical before/after procedure" did not ship. The migrations in [mono#2704](https://github.com/spinyfin/mono/pull/2704) landed without `explain-scope`. That is a standing gap, not a recorded decision to drop the protocol.

### Binding zero-match semantics

These were settled before this design and were **not** reopened. Everything below is designed around them.

1. A **structurally-empty** pattern is an error at config-resolution time. Shipped in [mono#2618](https://github.com/spinyfin/mono/pull/2618) via `glob_scope::structurally_empty_reason`, covering the three textually decidable forms: a leading `./`, a trailing `/`, and a leading `!`. A bare name with no separator and no wildcard is **not** covered: `src` and `pnpm-lock.yaml` are the same shape, and `pnpm-lock.yaml` is a live exclude entry.
2. A **structurally-matchable** pattern that matches nothing in the repo is at most a _warning_, and only under `--all` — never an error. **Not shipped.** No selected-file count surface exists to consume.
3. A pattern that matches files in the repo but none in _this changeset_ is silent and green. Held: `select_files` still early-returns on an empty selection.

## Non-goals — held

Each of the following was deliberately out of scope. They stayed out.

- **`skip_symlinks`**. A file-_type_ predicate, not a path glob. Still declarative-only.
- **`access_scope`** and **`declare-required-files`**. Both _grow_ a sandbox's read set — the opposite direction from scoping.
- **`exclude_structs`**. A symbol axis, not a file axis.
- **`file/forbidden-path`'s `patterns`** and **`file/ifchange`'s `trigger_globs` / `required_globs`**. These are the checks' subject matter, not their scoping.
- **Whether `forbidden-imports-deps`'s per-rule `exclude_files` should stop being config-dir-relative.** Recorded, not decided, not migrated. The per-rule `include` / `exclude_files` coordinate mismatch is still load-bearing for existing configs (`src/checks/forbidden_imports_deps.rs`).
- **Setting `literal_separator` on the glob dialect**, and case-folding on case-insensitive volumes. Dialect changes affecting every glob-shaped key, including ones this project does not touch.

## As-built surface

### Config

One framework key, `include`, as a sibling of `config:` on a check entry — the same position `exclude` occupies:

```yaml
checks:
  - id: boss/no-legacy-filehandle-write-api
    check: text/forbidden-pattern
    include:
      - "**/*.swift"
    exclude:
      - "vendor/**"
    config:
      patterns: [...]
```

Live instances in this repo: `md/doc-structure` and `checkleft/no-check-level-file-scoping` in root `CHECKS.yaml`; `boss/no-legacy-filehandle-write-api` in `tools/boss/CHECKS.yaml` ([mono#2554](https://github.com/spinyfin/mono/pull/2554)).

- **Position.** The sibling position is canonical. The legacy in-`config` position (`config.include`) still works and emits a warning-severity `ConfigDiagnostic` pointing at the sibling key. Empty lists are rejected ("use `enabled: false`").
- **Coordinate.** Config-dir-relative in both positions, normalised through `normalize_check_entry_patterns` — the same function `exclude` uses (renamed from `normalize_exclude_patterns` in [mono#2618](https://github.com/spinyfin/mono/pull/2618)).
- **Dialect.** Unchanged — `globset::Glob::new` with default builder options.
- **Absence.** No `include` means the definition scope, unnarrowed.
- **Check-entry aliases.** The sibling key accepts only `include`. It does not alias `applies_to`. An `applies_to:` sibling is an unknown check-entry field.
- **Manifest aliases.** Declarative manifests accept `include` with a permanent `#[serde(alias = "applies_to")]`. Both present is a duplicate-field error under `deny_unknown_fields`. Component manifests still reject `include` / `applies_to` outright (`reject_declarative_fields`).
- **Internal names.** Several Rust identifiers still say `applies_to` (`CheckConfig.applies_to_patterns`, `package.applies_to`, the run-group fingerprint prefix, `PathScope` glob-error labels). The public YAML and user-facing key is `include`. That residue is not a second public spelling.

### Enforcement — one type, the sites exclusion already owned

`ExclusionMatcher` was deleted. `PathScope` is the single matcher:

```rust
pub struct PathScope {
    include: Option<GlobSet>,   // None ⇒ universal
    exclude: Option<GlobSet>,   // None ⇒ excludes nothing
}
```

`PathScope::filter_changeset` applies the positive set first and subtracts the negative set second — excludes always win. Construction is `PathScope::new` at `build_scheduled_check_run`, after config-resolution has already validated the globs; an invalid pattern `expect`s rather than failing open. `PathScope::new_lenient` exists for tests and is unused in production.

| Stage                  | Site                                                                 | As-built                                                                                          |
| ---------------------- | -------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| Selection, built-in    | `src/runner.rs` (`filter_changeset` before the check runs)           | built-in receives the scope-filtered changeset; its intrinsic predicate still ANDs on top         |
| Selection, component   | `src/external/runtime.rs`                                            | host lowers the filtered changeset; WIT contract untouched                                        |
| Selection, declarative | `select_files` in `src/external/declarative/executor.rs`             | positive set is `definition ∩ entry`; excludes subtracted last                                    |
| Finding backstop       | `drop_excluded_findings`                                             | drops findings whose path is out of the instance's `PathScope`; location-less findings kept       |
| Fix path               | `run_declarative_fix`                                                | same `definition ∩ entry` composition as the run path                                             |
| Run grouping           | `run_group_key`                                                      | normalised include patterns join exclude in one scope fingerprint                                 |
| Progress count         | `eligible_file_count` seeded with `path_scope.filter_changeset(...)` | component default (changeset size) is therefore already include-filtered; the cosmetic gap closed |

`scope_findings_to_changeset` is **untouched**. It is load-bearing and orthogonal.

The WIT contract stayed at `checkleft:check@0.1.0`. Guests never received a scope field; they receive a shorter changeset.

## Decision 1 — naming: use `include`, paired with `exclude`

**As-built: `include` is the canonical unified word.** It pairs directly with the existing singular `exclude`.

The original design ([mono#2582](https://github.com/spinyfin/mono/pull/2582)) chose `applies_to` and argued against renaming: the declarative manifest field is required under `deny_unknown_fields`, so renaming only the check-entry key would end at two permanent spellings rather than one. [mono#2618](https://github.com/spinyfin/mono/pull/2618) reversed that during implementation: the manifest field moved to `include` with `applies_to` as a permanent serde alias, and the unshipped check-entry key accepted only `include`. Duplicate canonical-and-alias fields on a manifest are rejected. The exclude-side spelling and its `exclude_files` / `exclude_globs` aliases were unchanged.

There is no recorded design-time argument in [mono#2582](https://github.com/spinyfin/mono/pull/2582) for this reversal. The implementation PR's commit message names it as a coordinated manifest rename ("splitting would leave canonical and released spellings inconsistent"). The as-built public pair is `include` / `exclude`; `applies_to` survives only as the released-manifest alias.

## Decision 2 — composition: a check-entry `include` INTERSECTS the definition scope

**As-built: intersect. It does not replace.** This reversed then-HEAD behaviour (`override.unwrap_or(definition)`) and still contradicts a section of the shipped userdoc (see Remaining gaps).

The decisive argument is not safety. It is that **replace is unimplementable for two of the three implementation types**, so choosing it guarantees the per-type divergence this project exists to remove.

For a built-in check, the framework hands over a changeset and the check then applies its own Rust predicate. The framework has no way to make `workflow-yaml` ignore `is_workflow_file`. The same is true of a wasm guest with an intrinsic predicate. So for those checks the observable composition of a framework filter with the check's own scope is **always AND** — structurally, unavoidably.

Choosing "replace" therefore yields replace-for-declarative and intersect-for-the-other-two. Choosing intersect yields one rule everywhere. That is what `select_files` and the built-in/component `filter_changeset` sites now do.

### The uniform formalisation

Give every check a **definition scope** — the positive set it selects when the check entry carries no `include`:

| Implementation type | Definition scope                     | Where it lives                                |
| ------------------- | ------------------------------------ | --------------------------------------------- |
| Declarative         | the manifest's `include` list        | required on the declarative manifest          |
| Component           | universal (`**`)                     | a component manifest may not declare one      |
| Built-in            | the check's intrinsic Rust predicate | opaque to the framework, applied by the check |

Then, for every implementation type:

```
effective(check, file) = scheduled(check, file)          # CHECKS-file placement
                       ∧ definition_scope(check, file)
                       ∧ entry_include(check, file)   # absent ⇒ universal
                       ∧ ¬excluded(check, file)
```

For a component check, `definition_scope` is universal, so the intersection degenerates to exactly the entry list. For a built-in, the framework applies the entry list and the check applies its own predicate; the AND is automatic. Only the declarative path changed behaviour, and only for an entry that would have _widened_ beyond the definition.

**Live breakage at ship time: zero.** The design-time census found no `config.include` overrides in mono, flunge, checkleft-sandbox, or appoint. Widening from a repo config is now impossible; that is a genuine capability loss and the intended default. An explicit widening escape hatch was deferred and remains unbuilt.

## Decision 3 — coordinate system: config-dir-relative, exactly like `exclude`

**As-built: the check-entry `include` is authored relative to the directory of the `CHECKS` file that declares it, and normalised to repo-relative at config-resolution time — through the same code path `exclude` already uses.** This applies to both entry positions.

The constraint is that the include and exclude sides must not disagree. `exclude` is config-dir-relative and has live instances. Moving `exclude` to repo-relative would have been a far larger break than moving include to config-dir-relative. So include moved.

The design-time census made this free: every live include-side selector sat in a root `CHECKS` file, where `config_dir` is empty and normalisation is the identity. The one forthcoming non-root instance — [mono#2554](https://github.com/spinyfin/mono/pull/2554)'s `include: ["**/*.swift"]` in `tools/boss/CHECKS.yaml` — is coordinate-agnostic: a check declared there only ever receives files under `tools/boss/`, and `**` may match zero components, so `**/*.swift` and `tools/boss/**/*.swift` select the same set from that changeset.

**The definition-manifest `include` stays repo-relative, and that is not a disagreement.** A check definition is authored with no consuming repo in view and has no config directory to be relative to. The no-disagreement constraint is about the two keys an author writes side by side in one file — check-entry `include` and check-entry `exclude` — and those agree exactly.

## Decision 4 — guard mechanism: a name denylist anchored on the config doorway

**The rule to enforce:** a check may not ship config that answers "is this file a target of this check at all" — that is the framework's question. It may ship config on a strictly finer axis ("which of my rules/patterns applies to this file"), and such a selector must use the framework's word nested under the finer construct — `patterns[].include`, `rules[].include` — never a new word.

**As-built: `checkleft/no-check-level-file-scoping` ([mono#2761](https://github.com/spinyfin/mono/pull/2761)).** A static name denylist, anchored on the config-deserialisation doorway. An unanchored name denylist cannot tell top-level `include` from `patterns[].include`; nesting is the whole distinction, and nesting is structural.

Doorway anchoring (receiver-based, not bare method name):

- **Wasm guests:** `.config()` whose receiver is a `CheckInput` / `&CheckInput` parameter (`CheckInput::config<T>()`).
- **Built-in Rust checks:** `try_into()` whose receiver chain references a `&toml::Value` parameter of the enclosing function (`configure` / `configure_scoped` / a `parse_config` helper). Benign numeric `x.try_into()` is ignored.

Type extraction covers typed `let cfg: T = …` bindings and turbofish. When a doorway call is present but no config type can be resolved, the guard emits an inspection-failure finding rather than silently passing. Unreadable, non-UTF-8, or unparseable source also fails loudly.

The denylist is matched against **effective serde config keys** — the field's Rust ident, `#[serde(rename)]`, every `#[serde(alias)]`, and container `rename_all` — so `#[serde(rename = "paths")] my_files: …` is still caught.

**Denylist (bespoke, both surfaces):** `paths`, `path`, `path_globs`, `include_globs`, `file_globs`, `files`, `only`, `targets`, `scope`, `globs`.

**Built-in vs guest asymmetry**, which the original design did not spell out and [mono#2761](https://github.com/spinyfin/mono/pull/2761) had to decide: framework spellings `include` / `applies_to` are denylisted only on the **guest** surface. On built-ins they are legal by rule definition as deliberate framework-key pass-throughs for `deny_unknown_fields` (see `DocStructureConfig`) — not via a per-check exemption. A guest that parses the framework's own word is re-implementing the framework's job under a name that reads as legitimate.

Fields on any other struct in the crate — `ForbiddenImportsDepsRuleConfig.include` nested under `rules[]` — are out of scope by construction. Field-line reporting is scoped to the doorway struct's own text span. `#[cfg(test)]` (including `cfg(all(test, …))`) is skipped; sibling `tests.rs` modules under the built-in tree are skipped because the `#[cfg(test)]` sits on the parent `mod`.

**Scope of the check:** `tools/checkleft/checks/**/src/lib.rs` and `tools/checkleft/src/checks/**/*.rs`, declared with the framework's own `include` in root `CHECKS.yaml` at `severity: error` with no `allow_bypass`. The path predicate also requires `.rs`.

**Limits, held.** A static denylist has a false negative for a novel word (`swift_only`, `subjects`). The denylist is a rail against the known drift shapes, not a proof. Two stronger mechanisms were considered and remain rejected for v1:

- **Host-side runtime rejection** — impossible: the host cannot distinguish a scoping key from a subject-matter key by inspecting `config-json`; guessing would break `file/forbidden-path`'s `patterns`.
- **A typed SDK finer-axis scope helper as the sole mechanism** — insufficient, not wrong. It reaches only SDK-using wasm guests. Still deferred.

The `syn` parse did not have to escalate to a marker attribute. The receiver-anchored doorway plus turbofish extraction was enough.

## Decision 5 — migration verification protocol

The investigation's §7 protocol compares the set of `(check_id, location.path)` pairs from `checkleft run --all --format json` before and after. `--all` on both sides is correct and essential, and this design keeps it. But the protocol as written **cannot detect the exact failure it exists to catch**, and the reason is measurable.

`CheckResult` carries only `check_id` and `findings` (read — `src/output.rs:6-9`), and `print_json_results` serialises exactly that (read — `src/main.rs:2107-2110`). There is no per-check selected-file count in any machine-readable output: the eligible-file count is computed and handed to the progress reporter, but progress is enabled only under `OutputFormat::Human` (read — `src/main.rs:624-625`), and the `show-plan` subcommand prints the change plan, not the per-check selection (read — `src/main.rs:191-196`).

Consequently, for a check that is **green on both sides** — the normal state of every guard check in a healthy repo — a migration that silently narrows its scope to zero files produces an _empty finding set before and an empty finding set after_. The diff is empty and the PR looks verified. That is precisely the silent coverage change the protocol was written to catch.

### The protocol

A prerequisite: `checkleft explain-scope --all --format json`, a new read-only subcommand that emits, per configured check id, the resolved definition scope, the normalised entry `include`, the effective exclude patterns, and **the concrete sorted list of selected files**. It is specified as its own task and gates every migration task.

Every migration PR must run all four steps and paste the output of steps 2 and 3 into its description.

1. **Build both sides with bazel** — `bazel build //tools/checkleft:checkleft` at the base commit and with the migration applied, keeping the two binaries. (Per repo rule, bazel is the only sanctioned local build path; bare `cargo` is forbidden.)

2. **Scope diff — the primary signal.** Run `checkleft explain-scope --all --format json` with each binary and diff the per-check selected-file sets:

   ```sh
   base/checkleft explain-scope --all --format json > scope-before.json
   head/checkleft explain-scope --all --format json > scope-after.json
   diff <(jq -r '.[] | .check_id as $c | .selected_files[] | "\($c)\t\(.)"' scope-before.json | sort -u) \
        <(jq -r '.[] | .check_id as $c | .selected_files[] | "\($c)\t\(.)"' scope-after.json | sort -u)
   ```

   This must be **empty**, or every line of difference must be enumerated and justified in the PR body. This is the step that catches zero-to-zero narrowing, because it compares selection directly rather than inferring it from findings.

3. **Finding diff — the secondary signal.** Run `checkleft run --all --format json` with each binary and diff the `(check_id, location.path)` pairs:

   ```sh
   base/checkleft run --all --format json > before.json
   head/checkleft run --all --format json > after.json
   diff <(jq -r '.results[] | .check_id as $c | .findings[] | select(.location) | "\($c)\t\(.location.path)"' before.json | sort -u) \
        <(jq -r '.results[] | .check_id as $c | .findings[] | select(.location) | "\($c)\t\(.location.path)"' after.json | sort -u)
   ```

   This must also be empty. It catches what step 2 cannot: a change in _check-internal_ behaviour on an unchanged selection — for example, hoisting `md/doc-structure`'s `include_globs` to the framework widens what the framework selects while the check's own `.md` gate (read — `src/checks/doc_structure.rs:101-103`) still applies, so the selection legitimately changes while findings must not.

4. **State the `--all` justification.** The repo rule reserves `--all` for CI's integrity pipeline or a case with a strong stated justification. This is that case, and the PR must say so: a diff-scoped run only exercises files that happen to have changed, so it cannot distinguish "scope preserved" from "scope silently narrowed to zero".

Neither step 2 nor step 3 is sufficient alone. Step 2 proves the framework's selection is unchanged; step 3 proves the check's behaviour on that selection is unchanged. Both are required on every migration PR.

**As-built: the subcommand does not exist.** [mono#2704](https://github.com/spinyfin/mono/pull/2704) migrated `md/doc-structure`, `forbidden-imports-deps`, and `code-patterns` without it. There is no recorded decision to drop the protocol; the migrations simply did not run it. `CheckResult` still carries only `check_id` and `findings`. Future scope changes are still undetectable when both sides are green.

## Diagnostics, as shipped

- **Structurally-empty pattern → error at config resolution**, on both `include` and `exclude`, for the three textually decidable forms. The `!` case points the author at `exclude`. Bare names are not covered.
- **Matches nothing in the whole tree → warning, only under `--all`.** Not shipped.
- **Matches nothing in this changeset → silent and green.** Held.
- **`include` on a `scope: changeset` check.** Specified as a rejection of the check-entry `include` key. As-built, the resolver only rejects leftover `config.applies_to` on a changeset-scope entry (`src/config.rs`). Sibling `include` and documented legacy `config.include` are accepted and then do nothing, because a changeset-scope run is seeded with an empty changed-file set. See Remaining gaps.
- **Unknown check-entry keys.** The design asked for `#[serde(deny_unknown_fields)]` on `ParsedCheckConfig`, gated on an out-of-tree survey. As-built is stronger on the diagnostic and softer on the cutover: `#[serde(flatten)] unknown_fields` captures stray keys, `diagnose_unknown_check_fields` names the check id and suggests a correction (edit distance ≤ 2, or "belongs inside `policy:`"), and severity is versioned — warning through `0.1.0-alpha.9`, error after (`UNKNOWN_CHECK_FIELD_WARN_THROUGH_VERSION`). Current `Cargo.toml` is `0.1.0-alpha.8`, so this is presently a warning. Keys inside `config:` stay untouched. No separate survey PR ran; the flatten approach does not abort the rest of the file, which is why the survey was skippable.

Invalid glob syntax is a config-resolution diagnostic (`PathScope::new` during parse). The runner then `expect`s. The fail-open that [mono#1648](https://github.com/spinyfin/mono/pull/1648) was opened to fix — an invalid exclude glob discarding the entire list — was absorbed by [mono#2618](https://github.com/spinyfin/mono/pull/2618). [mono#1648](https://github.com/spinyfin/mono/pull/1648) is still open and is now stale relative to `PathScope`; it is not outstanding work for this project.

## Per-check migrations ([mono#2704](https://github.com/spinyfin/mono/pull/2704))

### `md/doc-structure`

`include_globs` / `exclude_globs` came off `DocStructureConfig`. Repo policy lives on the check-entry `include` / `exclude` in root `CHECKS.yaml`. The check's intrinsic `.md` gate stayed as subject matter.

`include_globs` was **deleted**, not made optional. A config carrying the legacy key now hard-errors via `deny_unknown_fields`. `DocStructureConfig` keeps framework-key pass-through fields (`include`, `applies_to`, `exclude_files`, `exclude_globs`) so a leftover framework key still in the config blob is consumed rather than silently ignored, and a forgotten check-specific key (or a bare `config.exclude`, which the legacy extractors never read) becomes a hard error.

### `forbidden-imports-deps`

The design asked to hoist per-rule `include_globs` to the check-entry `include` where every rule in an instance shares one scope, keeping per-rule selection only when rules genuinely differ, spelled `rules[].include`.

As-built: the per-rule selector was **renamed** to `include` with `include_globs` as an alias. It was **not hoisted**. The only live instance cited at design time sat in flunge's root `CHECKS.yaml`, which this repo cannot migrate. Per-rule `exclude_files` was not touched, as specified.

### `code-patterns`

The design asked to express `lang`'s implicit file gate as a definition-level default so the framework, not the check, answers "is this file a target", then delete the implicit gate.

As-built, for a built-in, definition scope **is** the check's intrinsic predicate. [mono#2704](https://github.com/spinyfin/mono/pull/2704) replaced `path.extension() == Some("java")` with an explicit glob list `["**/*.java"]` that both documents the default and implements `matches_language_path`. The framework still does not own a `code-patterns` definition include; the check still applies the predicate. That matches how every other built-in definition scope works.

One deliberate widening: `Path::extension()` returns `None` for a dot-leading `.java` filename, so the old gate rejected it; `**/*.java` matches it. Documented in the PR body and pinned by test.

## Relationship to [mono#2554](https://github.com/spinyfin/mono/pull/2554)

The original design had to state an ordering because [mono#2554](https://github.com/spinyfin/mono/pull/2554) was open and contained a third, independent include implementation (`narrow_by_include`).

The framework work landed first ([mono#2618](https://github.com/spinyfin/mono/pull/2618), 2026-08-08). [mono#2554](https://github.com/spinyfin/mono/pull/2554) then merged (2026-08-10) as the three lines of `tools/boss/CHECKS.yaml` only — exactly the "framework lands first" reduction the design specified. `narrow_by_include` never existed on `main`.

## Alternatives considered

These rejections still hold against the as-built system.

### A. Leave the include side per-implementation-type, and only document the divergence

**Rejected.** This is the status quo plus prose. Generic checks (`text/forbidden-pattern`) have no intrinsic file type and are exactly the ones that need include-side scoping, so the next one will again add a key to its own config because that is the only path available. Documentation does not close the gap that produces the drift.

### B. Extend the WIT contract so the guest receives its scope and applies it

**Rejected.** It is a breaking change to `checkleft:check@0.1.0`. It is unnecessary: the host already lowers a filtered changeset. And it is the wrong direction on the guard question — it would hand every guest the scoping vocabulary as a first-class capability at the same moment decision 4 takes it away. The WIT contract stayed at `0.1.0`.

### C. Keep an asymmetric include-side spelling

**Rejected** during implementation, more strongly than the original design. [mono#2582](https://github.com/spinyfin/mono/pull/2582) had accepted `applies_to` / `exclude` never reading as opposites in order to avoid a manifest rename. [mono#2618](https://github.com/spinyfin/mono/pull/2618) paid the rename cost so the public pair is `include` / `exclude`, with `applies_to` only as the released-manifest alias.

### D. Keep replace semantics for the check-entry key

**Rejected** — argued in full in decision 2. Replace is unimplementable for built-in and wasm checks, whose intrinsic predicates the framework cannot reach, so choosing it means shipping replace-for-declarative and intersect-for-everything-else.

## Design-time census (2026-07-30)

The measurements that drove decisions 1–3. They are a snapshot at `eeb6bce6`, not a description of HEAD today.

At that commit the include side covered **one of three** implementation types via **two** mechanisms: declarative checks had a required definition-manifest list plus a per-repo override that **replaced** it; component and built-in checks had no framework include side.

Command: `grep` over every `CHECKS.yaml` / `CHECKS.toml` in mono at `eeb6bce6` and in the cube-managed checkouts of the three sibling repos. (The original grep was for the then-canonical word `applies_to`; the live include-side selectors were the bespoke keys in the last column.)

| Repo                  | `CHECKS` files                                                                                   | `applies_to` / `include` in any config position | Other include-side keys                                                                  |
| --------------------- | ------------------------------------------------------------------------------------------------ | ----------------------------------------------- | ---------------------------------------------------------------------------------------- |
| **mono**              | `CHECKS.yaml`, `tools/boss/CHECKS.yaml`, `tools/checkleft/CHECKS.yaml`, `tools/cube/CHECKS.toml` | **zero**                                        | `md/doc-structure` `include_globs` — **root** file                                       |
| **flunge**            | `CHECKS.yaml`, `docs/CHECKS.yaml`, `mobile/ios/vendor/CHECKS.yaml`                               | **zero**                                        | one `forbidden-imports-deps` rule with `include_globs` + `exclude_globs` — **root** file |
| **checkleft-sandbox** | `CHECKS.yaml`                                                                                    | **zero**                                        | none                                                                                     |
| **appoint**           | none                                                                                             | —                                               | —                                                                                        |

Three consequences, each load-bearing:

1. There was no such thing as "preserving every existing `config.include` verbatim". There were none. Any argument for a decision that rested on back-compatibility of live include overrides was arguing about the empty set.
2. Every live include-side selector sat in a _root_ `CHECKS` file, where `config_dir` is empty and normalisation is the identity. Config-dir-relative and repo-relative were the same function at every live site.
3. The one non-root instance that would exist is the one [mono#2554](https://github.com/spinyfin/mono/pull/2554) added, and it is coordinate-agnostic for the reason in decision 3.

`ParsedCheckConfig` at that HEAD had no include-side field and no `deny_unknown_fields`. An unrecognised key on a check entry was dropped silently.

## Original implementation breakdown — what each entry became

The original design listed 19 in-scope tasks and 5 deferred ones, expecting them to land as separate PRs. Implementation collapsed the in-scope work into three PRs. Depth-1 parallelism was not used; [mono#2618](https://github.com/spinyfin/mono/pull/2618) took the matcher, parse, enforcement, intersect, fix-path, structurally-empty diagnostics, unknown-key diagnostics, and in-`config` deprecation together.

| Original entry                                                                                     | Outcome                                                                                                                                                                                                                                                  |
| -------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1. `explain-scope` subcommand                                                                      | **Not shipped.** See Remaining gaps.                                                                                                                                                                                                                     |
| 2. `PathScope` matcher core                                                                        | [mono#2618](https://github.com/spinyfin/mono/pull/2618). `ExclusionMatcher` deleted, not kept as a wrapper.                                                                                                                                              |
| 3. Parse and normalise check-entry `include`                                                       | [mono#2618](https://github.com/spinyfin/mono/pull/2618).                                                                                                                                                                                                 |
| 4. Enforce `PathScope` on built-in and component paths                                             | [mono#2618](https://github.com/spinyfin/mono/pull/2618).                                                                                                                                                                                                 |
| 5. Switch the declarative path to intersect                                                        | [mono#2618](https://github.com/spinyfin/mono/pull/2618).                                                                                                                                                                                                 |
| 6. Reconcile with [mono#2554](https://github.com/spinyfin/mono/pull/2554)                          | Framework landed first; [mono#2554](https://github.com/spinyfin/mono/pull/2554) reduced to `CHECKS.yaml`.                                                                                                                                                |
| 7. Unify the fix path onto `PathScope`                                                             | [mono#2618](https://github.com/spinyfin/mono/pull/2618).                                                                                                                                                                                                 |
| 8. Structurally-empty pattern diagnostics                                                          | [mono#2618](https://github.com/spinyfin/mono/pull/2618) (`glob_scope`).                                                                                                                                                                                  |
| 9. Zero-match warning under `--all`                                                                | **Not shipped.** Depends on entry 1.                                                                                                                                                                                                                     |
| 10. Survey stray keys in out-of-tree `CHECKS` files                                                | **Not run as a survey.** Superseded by the flatten diagnostic in entry 11.                                                                                                                                                                               |
| 11. `deny_unknown_fields` on `ParsedCheckConfig`                                                   | Shipped as flatten + diagnose + versioned severity, not a hard serde deny. [mono#2618](https://github.com/spinyfin/mono/pull/2618).                                                                                                                      |
| 12. Reject `include` on `scope: changeset`; deprecate in-`config`                                  | Deprecation warning for `config.include` shipped. Changeset rejection shipped only for leftover `config.applies_to`. See Remaining gaps.                                                                                                                 |
| 13. Migrate `md/doc-structure`                                                                     | [mono#2704](https://github.com/spinyfin/mono/pull/2704). `include_globs` deleted.                                                                                                                                                                        |
| 14. Hoist `forbidden-imports-deps` per-rule `include_globs`                                        | [mono#2704](https://github.com/spinyfin/mono/pull/2704) renamed to `rules[].include` with alias; did not hoist.                                                                                                                                          |
| 15. Express `code-patterns`'s `lang` file gate as a definition-level default                       | [mono#2704](https://github.com/spinyfin/mono/pull/2704): explicit `**/*.java` glob-backed predicate, still applied by the check.                                                                                                                         |
| 16. Migration reconciliation sweep                                                                 | **Not run.** The three migrations shipped in one PR, so the cross-PR interaction the sweep existed to catch did not arise.                                                                                                                               |
| 17. Guard check                                                                                    | [mono#2761](https://github.com/spinyfin/mono/pull/2761).                                                                                                                                                                                                 |
| 18. Documentation                                                                                  | Partially shipped. Guard rule, canned-checks entries, external-check-package-contract, and check-author-api best-practice bullet landed. **`checks-config.md` still documents replace semantics** for a per-repo `include` override. See Remaining gaps. |
| 19. Land [mono#1648](https://github.com/spinyfin/mono/pull/1648) (invalid exclude globs fail open) | Intent absorbed by [mono#2618](https://github.com/spinyfin/mono/pull/2618). The original PR is still open and stale.                                                                                                                                     |

### Deferred — still deferred

- **Typed SDK finer-axis scope helper.** Strengthens the denylist against a novel-word false negative. Not a v1 blocker; reaches only SDK-using wasm guests.
- **Explicit widening escape hatch.** No demonstrated demand at design time; still none recorded.
- **`forbidden-imports-deps` per-rule `exclude_files` coordinate.** Explicit non-goal; the disagreement still has its live instance in flunge's root `CHECKS.yaml`, where the two coordinates coincide.
- **`skip_symlinks` unification.** File-type predicate; needs its own design.
- **Component `eligible_file_count` under-reporting.** Closed incidentally: the runner now seeds progress from `path_scope.filter_changeset(...)` before asking the executor for a count, so the component default (changeset size) is already include-filtered.

## Remaining gaps

These are in-scope v1 items the design specified and no PR delivered. They are not deferred-by-design.

1. **Userdoc still documents replace.** `userdoc/docs/checks-config.md` "Overriding `include` for declarative checks" still says the repo's `include` list **replaces** the definition's list entirely, and still shows the legacy in-`config` position as the way to write it. The "Precedence vs `include`" section repeats the replace claim. The code path is intersect (`select_files`, `tests_selection.rs`). Entry 18 required this rewrite; [mono#2618](https://github.com/spinyfin/mono/pull/2618) only mechanically renamed `applies_to` → `include` in that section, which left the wrong composition under the new name. `canned-checks.md` for `code-patterns` already states intersect correctly.

2. **`explain-scope` and the `--all` zero-match warning.** Decision 5's primary signal does not exist. `Commands` in `src/main.rs` is `Run`, `Fix`, `List`, and `ShowPlan` — no `ExplainScope`. Without a selected-file list, a later migration that narrows a green check to zero files is still undetectable from findings, and the binding case-(2) warning has nothing to consume.

3. **`include` on `scope: changeset` is not rejected.** The resolver special-cases leftover `config.applies_to` (structurally-empty patterns and the changeset combination) and does not consult the canonical sibling `include` or the documented legacy `config.include`. `config.applies_to` is also not extracted into `applies_to_patterns` — so those diagnostics fire on a key that is not actually the include list. Residue of the [mono#2618](https://github.com/spinyfin/mono/pull/2618) rename, not a recorded decision to weaken entry 12.
