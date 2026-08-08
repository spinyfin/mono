# Checkleft: unify the include side of file scoping across check implementation types

- **Date:** 2026-07-30
- **Status:** proposed — design only, no production code changed
- **Input investigation:** [`../investigations/file-scoping-vocabulary-drift-across-check-implementation-types.md`](../investigations/file-scoping-vocabulary-drift-across-check-implementation-types.md) ([mono#2559](https://github.com/spinyfin/mono/pull/2559), merged 2026-07-30)
- **Prior art (exclude side, landed and still holding):** [`checkleft-unified-file-exclusion-mechanism-across-checks.md`](./checkleft-unified-file-exclusion-mechanism-across-checks.md) ([mono#1638](https://github.com/spinyfin/mono/pull/1638) / [mono#1640](https://github.com/spinyfin/mono/pull/1640) / [mono#1641](https://github.com/spinyfin/mono/pull/1641))
- **In-flight dependency:** [mono#2554](https://github.com/spinyfin/mono/pull/2554) — **OPEN, not merged** at the HEAD this design was written against
- **HEAD read:** `eeb6bce6` (`main`, 2026-07-30). Every `file:line` below was read at that commit unless labelled otherwise.

Checkleft's exclude side is unified; its include side is not. This design picks the five decisions that were left open, so that one framework key answers "which files is this check a target of" identically for built-in Rust checks, declarative checks, and wasm component checks.

## Verdict

Adopt **`include`** as the single framework include key, paired with `exclude`. Make it **intersect** the check's definition scope rather than replace it, author it **config-dir-relative** exactly like `exclude`, and guard the boundary with a **name denylist anchored on the config-deserialisation doorway**, not an unanchored name denylist.

Two of those four reverse a recommendation in the input investigation. Both reversals are driven by measurements taken for this design and are argued in place.

## Method, and one correction to the input

Every claim about current behaviour below is labelled **read** (source at `eeb6bce6`) or **measured** (a command run for this design). No production code, build file, or `CHECKS` file was changed.

The task framing for this design stated as verified-at-HEAD that "component (wasm) checks gain `config.include` in mono#2554 via a third, independent implementation, `narrow_by_include`". That is an accurate description of what mono#2554 _contains_, but it is not the state at HEAD: **mono#2554 is OPEN, not merged** (measured — `gh pr view 2554 -R spinyfin/mono`, `state: OPEN`, `mergedAt: null`, branch `boss/exec_18c71263d77d7260_32`, 8 files, +525/−37, last updated 2026-07-30T22:59Z). At `eeb6bce6`, `narrow_by_include` does not exist anywhere in `tools/checkleft/src/` (measured — `grep -rn narrow_by_include tools/checkleft/src/` returns nothing), and `userdoc/docs/checks-config.md` has no component-checks section.

This matters in two directions and both are handled below: the naming decision must weigh mono#2554's userdoc as _pending_ rather than _shipped_, and the implementation ordering must state what happens whichever of the two lands first.

## Goals

Collapse the include side of file scoping to one framework-level key that behaves identically across all three check implementation types, and extend it to the type that has no framework include side at all today.

Concretely, after this project:

- One key name, one glob dialect, one coordinate system, one composition rule, reachable from a `CHECKS` file for a built-in, a declarative, and a component check alike.
- No check ships its own check-level answer to "is this file a target of this check", and a repository check enforces that so the drift cannot silently recur.
- A migration that changes a check's effective file set is detectable by a mechanical before/after procedure, not by reviewer intuition.
- The settled zero-match semantics (below) are preserved exactly.

### Binding zero-match semantics

These were settled before this design and are **not** reopened here. Everything below is designed around them.

1. A **structurally-empty** pattern is an error at config-resolution time.
2. A **structurally-matchable** pattern that matches nothing in the repo is at most a _warning_, and only under `--all` — never an error, because a guard check scoped to a file type the repo does not yet contain is deliberate and must stay green.
3. A pattern that matches files in the repo but none in _this changeset_ is silent and green, and nothing may touch it.

One refinement is needed to make (1) implementable, and it stays inside the semantics rather than relitigating them. The investigation listed "a bare directory name with no wildcard" (e.g. `src`) among the structurally-empty forms. That form is **not textually decidable**: `src` and `pnpm-lock.yaml` are the same shape, and `pnpm-lock.yaml` is a live, load-bearing exclude entry (read — mono root `CHECKS.yaml:39`, inside `file/size`'s `config.exclude_files` declared at `:32`). A bare name is structurally capable of matching a root-level _file_; it is only empty when the repo happens to have a directory there instead, which is case (2), not case (1). So the config-resolution error covers exactly the three forms that can never match any path in any repo: a leading `./`, a trailing `/`, and a leading `!`. A bare name with no separator and no wildcard is demoted to the case-(2) warning.

## Non-goals

Each of the following is deliberately out of scope. They are named here so a later reader can tell "considered and excluded" from "forgotten".

- **`skip_symlinks`** (read — `src/external/mod.rs:253-254`, applied at `src/external/declarative/executor.rs:299-307`). A file-_type_ predicate, not a path glob. Folding it in needs its own design, as the exclusion design already recommended.
- **`access_scope`** (read — `sdk-macro/src/lib.rs:146-174`) and **`declare-required-files`** (read — `wit/check.wit`). Both _grow_ a sandbox's read set. That is the opposite direction from scoping and conflating them would be a security-relevant mistake, not just a vocabulary one.
- **`exclude_structs`** (read — `checks/rust/giant-structs-create/src/lib.rs:53`; host-applied in `src/external/runtime.rs`). A symbol axis, not a file axis.
- **`file/forbidden-path`'s `patterns`** and **`file/ifchange`'s `trigger_globs` / `required_globs`**. These are the checks' subject matter, not their scoping. A `file/forbidden-path` with no `patterns` is not a check.
- **Whether `forbidden-imports-deps`'s per-rule `exclude_files` should stop being config-dir-relative.** Recorded as an open question with evidence below; explicitly **not decided and not migrated** by this project.
- **Setting `literal_separator` on the glob dialect**, and case-folding on case-insensitive volumes. Both are dialect changes affecting every glob-shaped key in the tool, including ones this design does not touch. Separate design.

## Current state at HEAD — what actually exists

### The include side, per implementation type

| Implementation type  | Framework include side at `eeb6bce6`                                                                         | Citation                                                                                                                                                                                                                                   |
| -------------------- | ------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Declarative**      | Two: a required definition-manifest `include`, and a per-repo `config.include` override that **replaces** it | read — `src/external/mod.rs:248`; `src/external/declarative/resolve.rs:280-313`; consumed at `executor.rs:110-113`                                                                                                                         |
| **Component (wasm)** | **None.** The changeset is filtered by exclusion only                                                        | read — `src/external/runtime.rs:602`, `:646`; `include` is rejected outright in a `mode = "component"` manifest at `src/external/mod.rs:444-447`                                                                                           |
| **Built-in (Rust)**  | **None.** `include` appears nowhere in `src/runner.rs` or `src/check.rs`                                     | measured — `grep -n include src/runner.rs src/check.rs` returns nothing. Built-ins receive the exclusion-filtered changeset (`src/runner.rs:268`, `:360`) narrowed by a hardcoded Rust predicate they own (`src/check.rs:75-81`, `:90-96`) |

So at HEAD the include side covers **one of three** implementation types via **two** mechanisms. mono#2554 would take that to two of three via three mechanisms; this design takes it to three of three via one.

### The exclude side, for symmetry

- `ParsedCheckConfig` (read — `src/config.rs:562-586`) carries `exclude` with `#[serde(alias = "exclude_files", alias = "exclude_globs")]` at `:581-582`, and **no include-side field at all**.
- `ParsedCheckConfig` has **no** `deny_unknown_fields` (read — `src/config.rs:561-562`; contrast `src/external/mod.rs:242` on the manifest struct). An unrecognised key on a check entry is dropped silently.
- Framework exclude patterns are authored **config-dir-relative** and normalised to repo-relative at parse time (read — `normalize_exclude_patterns`, `src/config.rs:618-624`; per-check entry point `parse_per_check_exclude_patterns`, `:688-716`; legacy in-`config` position `extract_legacy_config_excludes`, `:635-659`, which normalises the same way).
- `ExclusionMatcher` is `{ inner: Option<GlobSet> }` (read — `src/exclusion_matcher.rs:24-26`), with `is_excluded` `:49`, `filter_paths` `:58`, `filter_changeset` `:70-83`, `is_empty` `:86`. It is a clean base to generalise.
- The effective exclude pattern list is folded into the run-group key so two files with different effective matchers never share a run (read — `run_group_key`, `src/runner.rs:96-116`; grouping loop `:1155-1200`).

### Config census — measured, and it changes the decisions

This is the single most decision-relevant measurement in this design, and it was not available to the input investigation.

Command (2026-07-30): `grep -n include` over every `CHECKS.yaml` / `CHECKS.toml` in mono at `eeb6bce6` and in the cube-managed checkouts of the three sibling repos.

| Repo                  | `CHECKS` files                                                                                   | `include` in any config position | Other include-side keys                                                                                            |
| --------------------- | ------------------------------------------------------------------------------------------------ | -------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| **mono**              | `CHECKS.yaml`, `tools/boss/CHECKS.yaml`, `tools/checkleft/CHECKS.yaml`, `tools/cube/CHECKS.toml` | **zero**                         | `md/doc-structure` `include_globs` — **root** file (`CHECKS.yaml:83-85`)                                           |
| **flunge**            | `CHECKS.yaml`, `docs/CHECKS.yaml`, `mobile/ios/vendor/CHECKS.yaml`                               | **zero**                         | one `forbidden-imports-deps` rule with `include_globs` + `exclude_globs` — **root** file (`CHECKS.yaml:59`, `:62`) |
| **checkleft-sandbox** | `CHECKS.yaml`                                                                                    | **zero**                         | none                                                                                                               |
| **appoint**           | none                                                                                             | —                                | —                                                                                                                  |

Three consequences, each load-bearing below:

1. **There is no such thing as "preserving every existing `config.include` verbatim".** There are none. Any argument for a decision that rests on back-compatibility of live `include` overrides is arguing about the empty set.
2. **Every live include-side selector in every repo sits in a _root_ `CHECKS` file**, where `config_dir` is empty and `normalize_exclude_patterns` returns its input unchanged (read — `src/config.rs:618-621`). Config-dir-relative and repo-relative are _the same function_ at every live site.
3. The one non-root instance that will exist is the one mono#2554 adds — `include: ["**/*.swift"]` in `tools/boss/CHECKS.yaml` (read — mono#2554 diff). It is coordinate-agnostic too; see decision 3.

## Decision 1 — naming: use `include`, paired with `exclude`

**Decision: `include` is the canonical unified word.** It pairs directly with the existing singular `exclude`: one word per direction, each the obvious opposite of the other. This restores the input investigation's naming recommendation.

The declarative manifest field is renamed alongside the check-entry key: `RawDeclarativeCheckManifest.include` is required and remains protected by `#[serde(deny_unknown_fields)]`. The definition surface and the `CHECKS.yaml` configuration surface therefore use the same canonical spelling, removing the split the previous decision sought to avoid. `PathScope` already represents the pair internally as `include` / `exclude`, so the vocabulary is now consistent end to end.

The declarative manifest's previous spelling shipped in a release, so it remains a permanent serde alias for `include`; manifests using either spelling parse identically, while a manifest containing both is rejected as a duplicate field. The new check-entry field has not shipped, so it accepts only `include` and does not manufacture an alias. The existing exclude-side spelling and its `exclude_files` / `exclude_globs` aliases are unchanged.

## Decision 2 — composition: a check-entry `include` INTERSECTS the definition scope

**Decision: intersect. It does not replace.** This reverses today's behaviour (read — `executor.rs:113`, `include_override.as_deref().unwrap_or(&package.include)`) and the shipped documentation (read — `userdoc/docs/checks-config.md:259`, "the repo's `include` list **replaces** the definition's list entirely").

The decisive argument is not safety. It is that **replace is unimplementable for two of the three implementation types**, so choosing it guarantees the per-type divergence this project exists to remove.

For a built-in check, the framework hands over a changeset and the check then applies its own Rust predicate (read — `src/check.rs:75-81` `count_applicable`, `:90-96` `run_per_text_file`; both take a `predicate: impl Fn(&Path) -> bool` owned by the check). The framework has no way to make `workflow-yaml` ignore `is_workflow_file`. The same is true of a wasm guest with an intrinsic predicate such as `md/link-integrity`'s `is_markdown_file`. So for those checks the observable composition of a framework filter with the check's own scope is **always AND** — structurally, unavoidably.

Choosing "replace" therefore yields replace-for-declarative and intersect-for-the-other-two. Choosing intersect yields one rule everywhere.

### The uniform formalisation

Give every check a **definition scope** — the positive set it selects when the check entry carries no `include`:

| Implementation type | Definition scope                     | Where it lives                                                                 |
| ------------------- | ------------------------------------ | ------------------------------------------------------------------------------ |
| Declarative         | the manifest's `include` list        | read — `src/external/mod.rs:248` (required, so always present)                 |
| Component           | universal (`**`)                     | read — a component manifest may not declare one, `src/external/mod.rs:444-447` |
| Built-in            | the check's intrinsic Rust predicate | read — `src/check.rs:75-96`; opaque to the framework, applied by the check     |

Then, for every implementation type:

```
effective(check, file) = scheduled(check, file)          # CHECKS-file placement
                       ∧ definition_scope(check, file)
                       ∧ entry_include(check, file)   # absent ⇒ universal
                       ∧ ¬excluded(check, file)
```

For a component check, `definition_scope` is universal, so the intersection degenerates to exactly the entry list — which is precisely the semantics mono#2554 implements and documents ("for a component check … it is the entire positive selection", read — mono#2554's userdoc addition). **Intersect costs mono#2554 nothing.** For a built-in, the framework applies the entry list and the check applies its own predicate; the AND is automatic and needs no new code inside any check. Only the declarative path changes behaviour, and only for an entry that would have _widened_ beyond the definition.

### Cost, measured and honest

- **Live breakage: zero.** There are no `config.include` overrides in mono, flunge, checkleft-sandbox, or appoint (measured, census above). The behaviour change is real in code and has no live instance to break. Test fixtures under `src/runner/tests_external.rs`, `src/external/declarative/tests_*.rs`, and `src/fix/tests.rs` do exercise `include` (measured — `grep`) and will need review, but those are definition-side lists, not entry-side overrides.
- **Widening becomes impossible from a repo config.** A repo that wants `format/prettier` to also cover a file type the definition omits can no longer say so from its `CHECKS.yaml`; it must change the definition or fork it. This is a genuine capability loss. It is the right default because widening is a statement about the _check_ ("prettier handles `.mjs`"), not about the repo, and the definition is where check-wide statements belong. An explicit escape hatch is specified as a deferred task rather than built speculatively, because with zero overrides in existence there is no evidence anyone wants to widen.
- **A userdoc rewrite is required** — `checks-config.md:257-281` documents replace semantics explicitly. Decision 1 avoided one doc rewrite; this decision incurs one. That is the honest ledger.

## Decision 3 — coordinate system: config-dir-relative, exactly like `exclude`

**Decision: the check-entry `include` is authored relative to the directory of the `CHECKS` file that declares it, and normalised to repo-relative at config-resolution time — through the same code path `exclude` already uses.** This applies to **both** entry positions (the new sibling-of-`config:` position and the legacy in-`config` position), so the include side has exactly one coordinate, mirroring `exclude`.

The constraint from the task framing is that the include and exclude sides must not disagree within the final design. `exclude` is config-dir-relative (read — `src/config.rs:618-624`) and has live instances in mono (`tools/boss/CHECKS.yaml:19`, root `CHECKS.yaml:32` — both read). Moving `exclude` to repo-relative is a far larger break than moving include to config-dir-relative. So include moves.

The reason this is not merely the lesser evil is the census: **it is free.**

- Every live include-side selector sits in a root `CHECKS` file, where `config_dir` is empty and normalisation is the identity (read — `src/config.rs:618-621` returns `patterns.to_vec()` unchanged for an empty `config_dir`). Zero live patterns change meaning.
- The one forthcoming non-root instance — mono#2554's `include: ["**/*.swift"]` in `tools/boss/CHECKS.yaml` — is also unaffected, for a structural reason worth stating because it generalises. Checks are resolved per changed file by walking from the repo root down to the file's directory and accumulating each `CHECKS` file on the way (read — `resolve_for_file` `src/config.rs:300-305` → `resolve_for_dir` `:315-331`, recursive-from-root; scheduling loop `src/runner.rs:1155-1200`). A check declared in `tools/boss/CHECKS.yaml` therefore only ever receives files under `tools/boss/`. Under config-dir-relative, `**/*.swift` normalises to `tools/boss/**/*.swift`; `**` may match zero components (measured in the input investigation §3.1), so both spellings select the identical set from a changeset that only contains `tools/boss/` paths. **This is a verify-on-migration item, not an assumption** — the protocol in decision 5 is what confirms it.
- The ergonomic win is the same one `exclude` already delivers: in a subdirectory `CHECKS` file you write `src/**` and mean _this directory's_ `src/**`, which is what an author expects.

**The definition-manifest `include` stays repo-relative, and that is not a disagreement.** A check definition is authored with no consuming repo in view and has no config directory to be relative to (read — `src/external/mod.rs:248`; the manifest is resolved independently of any `CHECKS` file's location). The no-disagreement constraint is about the two keys an author writes side by side in one file — check-entry `include` and check-entry `exclude` — and those agree exactly. The doc task must state this distinction explicitly, because a reader who meets both surfaces without it will read the manifest as a bug.

## Decision 4 — guard mechanism: a name denylist anchored on the config doorway

**The rule to enforce:** a check may not ship config that answers "is this file a target of this check at all" — that is the framework's question. It may ship config on a strictly finer axis ("which of my rules/patterns applies to this file"), and such a selector must use the framework's word nested under the finer construct — `patterns[].include`, `rules[].include` — never a new word.

**Decision: a static name denylist is strong enough, but only if it is anchored on the config-deserialisation doorway. An unanchored name denylist is not, and this is what the investigation left open.**

The reason is precise. The rule _permits_ `patterns[].include` and _forbids_ top-level `include`. Those two are the same token on the same kind of line. A text- or regex-level denylist over check sources cannot tell them apart, so it must either reject the permitted shape (breaking the rule it is enforcing) or accept the forbidden one (enforcing nothing). Nesting is the whole distinction, and nesting is structural.

There is a structural anchor available, one per implementation type, and both are single doorways:

- **Wasm guests:** `CheckInput::config<T: serde::de::DeserializeOwned>()` (read — `sdk/src/lib.rs:145-147`). Every guest deserialises its config through this one generic. The type argument `T` _is_ the top-level config struct.
- **Built-in Rust checks:** `ConfiguredCheckFactory::configure(&toml::Value)` and `configure_scoped` (read — `src/check.rs:124`, `:129`). Each built-in's implementation deserialises into its own config type there — e.g. `md/doc-structure` at `src/checks/doc_structure.rs:255-256`, `let parsed: DocStructureConfig = config.clone().try_into()`.

So the guard check parses the file with `syn`, resolves the type deserialised at the doorway, and applies the denylist **to that struct's fields only**. Fields on any other struct in the crate — a `PatternConfig` (read — `checks/text/forbidden-pattern/src/lib.rs:144-145`), a `ForbiddenImportsDepsRuleConfig` (read — `src/checks/forbidden_imports_deps.rs:104`) — are out of scope by construction, which is exactly the permitted finer-axis shape.

**Denylist:** `include`, `include`, `paths`, `path`, `path_globs`, `include_globs`, `file_globs`, `files`, `only`, `targets`, `scope`, `globs`.

Both framework spellings belong on it for _guest-side_ config even though `include` is a valid framework key: a guest that parses the framework's own word is re-implementing the framework's job under a name that reads as legitimate, which is harder to catch in review than a novel word like `paths`, not easier.

**Scope of the check:** `tools/checkleft/checks/**/src/lib.rs` (wasm guests) and `tools/checkleft/src/checks/**/*.rs` (built-ins) — declared with the framework's own `include`, dogfooding the key this project ships.

**Limits, stated rather than papered over.** A static denylist has a false negative for a novel word: a check that names its key `swift_only` or `subjects` passes. The denylist is a rail against the _known_ drift shapes and a reviewer-visible signal, not a proof. Two stronger mechanisms were considered and rejected for v1:

- **Host-side runtime rejection** — rejected as impossible, agreeing with the investigation. The host cannot distinguish a scoping key from a subject-matter key by inspecting `config-json`; guessing would break `file/forbidden-path`'s `patterns` (read — `checks/file/forbidden-path/src/lib.rs:49-55`), which is the check's entire reason to exist.
- **A typed SDK finer-axis scope helper as the sole mechanism** — rejected as insufficient, not as wrong. It reaches only SDK-using wasm guests. It does nothing for the built-in checks under `src/checks/**` or for a guest that hand-rolls its serde struct, so it cannot replace the denylist. It is a worthwhile _strengthener_ and is listed as a deferred task.

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
   diff <(jq -r '.[] | .check_id as $c | .findings[] | select(.location) | "\($c)\t\(.location.path)"' before.json | sort -u) \
        <(jq -r '.[] | .check_id as $c | .findings[] | select(.location) | "\($c)\t\(.location.path)"' after.json | sort -u)
   ```

   This must also be empty. It catches what step 2 cannot: a change in _check-internal_ behaviour on an unchanged selection — for example, hoisting `md/doc-structure`'s `include_globs` to the framework widens what the framework selects while the check's own `.md` gate (read — `src/checks/doc_structure.rs:101-103`) still applies, so the selection legitimately changes while findings must not.

4. **State the `--all` justification.** The repo rule reserves `--all` for CI's integrity pipeline or a case with a strong stated justification. This is that case, and the PR must say so: a diff-scoped run only exercises files that happen to have changed, so it cannot distinguish "scope preserved" from "scope silently narrowed to zero".

Neither step 2 nor step 3 is sufficient alone. Step 2 proves the framework's selection is unchanged; step 3 proves the check's behaviour on that selection is unchanged. Both are required on every migration PR.

## Alternatives considered

### A. Leave the include side per-implementation-type, and only document the divergence

Write down that declarative checks scope with `include`, component checks with whatever key their guest ships, and built-ins by recompiling — then invest in documentation rather than framework code.

**Rejected.** This is the status quo plus prose, and the investigation established that the status quo actively regresses: `text/forbidden-pattern` is a _generic_ check with no intrinsic file type, and generic checks are exactly the ones that need include-side scoping, so the next one will again add a key to its own config because that is the only path available (read — the framework passes the config blob through verbatim, `src/external/runtime.rs`). Documentation does not close the gap that produces the drift. It also leaves the built-in path with no framework include side at all, which no amount of prose fixes.

### B. Extend the WIT contract so the guest receives its scope and applies it

Add a scope field to `check-input` in `wit/check.wit`, lower the resolved patterns into the guest, and let each guest filter.

**Rejected**, for three reasons. It is a breaking change to `checkleft:check@0.1.0`, forcing every external `.wasm` artifact to be rebuilt and re-pinned. It is unnecessary: the host already lowers an exclusion-filtered changeset (read — `src/external/runtime.rs:602`, `:646`, with the guest-facing comment "the host lowers an exclusion-filtered changeset so the guest never sees an excluded path and cannot target it"), so applying the positive filter at the same call site makes a scoped guest simply receive a smaller list, needing no new import, export, or awareness. And it is the wrong direction on the guard question — it would hand every guest the scoping vocabulary as a first-class capability at the same moment decision 4 tries to take it away. The chosen design leaves the WIT contract at `0.1.0`, untouched.

### C. Keep an asymmetric include-side spelling

**Rejected** — the symmetry gain is worth the bounded rename cost. The manifest and check-entry fields move together to canonical `include`, so there is no definition-surface/config-surface split. The shipped manifest spelling remains only as its permanent compatibility alias; the unshipped check-entry key accepts no alias. This makes the positive and negative sides read as the pair `include` / `exclude`.

### D. Keep replace semantics for the check-entry key

**Rejected** — argued in full in decision 2. Replace is unimplementable for built-in and wasm checks, whose intrinsic predicates the framework cannot reach, so choosing it means shipping replace-for-declarative and intersect-for-everything-else. That is per-implementation-type divergence in the _composition rule_, which is a worse form of the drift than divergence in the key _name_.

## Chosen approach

### Config surface

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

- **Position.** The sibling position is canonical. The legacy in-`config` position (`config.include`) keeps working and is deprecated on the same shape the exclusion design used for its legacy position: accept both, emit a `ConfigDiagnostic` at `warning` when the in-`config` position is used, make it an error in a later release. With zero live instances, the window is a courtesy to out-of-tree configs, not a migration burden.
- **Coordinate.** Config-dir-relative in both positions, normalised through the same function as `exclude` (read — `src/config.rs:618-624`, to be renamed to a scope-neutral name).
- **Dialect.** Unchanged — `globset::Glob::new` with default builder options, the single dialect every glob-shaped key in the tool already shares.
- **Absence.** No `include` means the definition scope, unchanged. This is what keeps the change inert for every check that does not opt in.
- **Empty list.** Rejected, generalising the precedent already in `override_include` (read — `src/external/declarative/resolve.rs:290-295`, which errors with "use `enabled: false` to disable the check instead").

### Enforcement — one type, the sites exclusion already owns

Generalise `ExclusionMatcher` (read — `src/exclusion_matcher.rs:24-26`) into `PathScope`:

```rust
pub struct PathScope {
    include: Option<GlobSet>,   // None ⇒ universal
    exclude: Option<GlobSet>,   // None ⇒ excludes nothing (today's Default)
}
```

`PathScope::filter_changeset` applies the positive set first and subtracts the negative set second, preserving today's "excludes always win" precedence (read — the same ordering `select_files` already uses, `src/external/declarative/executor.rs:297-298`). No new pipeline stage is introduced; the positive filter lands at the sites the negative filter already occupies:

| Stage                  | Site at HEAD                                                                                    | Change                                                                                     |
| ---------------------- | ----------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| Selection, built-in    | `src/runner.rs:268`, `:360`                                                                     | `exclusion_matcher.filter_changeset` → `path_scope.filter_changeset`                       |
| Selection, component   | `src/external/runtime.rs:602`, `:646`                                                           | same substitution; the WIT contract is untouched, the guest simply receives a shorter list |
| Selection, declarative | `select_files`, `src/external/declarative/executor.rs:280-313`                                  | the positive set becomes `definition ∩ entry` rather than `entry.unwrap_or(definition)`    |
| Finding backstop       | `drop_excluded_findings`, `src/runner.rs:1472-1477`                                             | gains the symmetric positive test                                                          |
| Fix path               | `src/external/declarative/executor.rs` (`run_declarative_fix` subtraction, `filter_by_include`) | collapse into one `PathScope` call                                                         |
| Run grouping           | `run_group_key`, `src/runner.rs:96-116`                                                         | the normalised `include` patterns join `exclude_fingerprint` in the key                    |

`scope_findings_to_changeset` (read — `src/runner.rs:1453-1459`) is **untouched**. It is load-bearing and orthogonal.

The construction site is `build_scheduled_check_run` (read — `src/runner.rs:1258-1298`), which builds today's matcher at `:1275-1279`.

### Diagnostics, within the binding zero-match semantics

- **Structurally-empty pattern → error at config resolution**, on both `include` and `exclude`, for the three textually decidable forms: leading `./`, trailing `/`, leading `!`. The `!` case gets a diagnostic pointing at `exclude` rather than compiling as a literal. Bare names are _not_ covered, for the reason given under the binding semantics.
- **Matches nothing in the whole tree → warning, only under `--all`.** This needs a selected-file count, which `explain-scope` supplies; the warning consumes the same data. Never an error, never in a diff run.
- **Matches nothing in this changeset → silent and green, untouched.** `executor.rs:115-120`'s early return on an empty selection stays exactly as it is.
- **`include` on a `scope: changeset` check → rejected.** Such a check runs with an empty changed-file set by construction (read — `schedule_changeset_scope_runs`, `src/runner.rs:1223-1250`; the empty `changed_files` is built in `build_scheduled_check_run`, `:1258-1298`), so a positive file filter is meaningless there.
- **`deny_unknown_fields` on `ParsedCheckConfig`** (read — absent at `src/config.rs:561-562`), which turns `excludes:`, `includes:`, and a misplaced `paths:` into diagnostics instead of silence. This is the highest-value safety change in the set and the most likely to break an out-of-tree config, so it is gated on a survey task. Note it applies to the check _entry_ only; keys inside `config:` — including the live empty `exclude_files = []` at `tools/cube/CHECKS.toml:10` (read) — are unaffected.

### Relationship to mono#2554

The two overlap and either order works; the ordering must be stated so neither worker is surprised.

- **If mono#2554 lands first** (likely — it is open and near-ready), the component path already has `narrow_by_include` and a shared `resolve::include_globset`. The `PathScope` wiring task then _replaces_ `narrow_by_include` with the unified filter and deletes it. Its semantics are already what this design specifies for a component check (entry list = entire positive selection, which is `universal ∩ entry`), so no behaviour changes and its tests should survive with mechanical edits.
- **If the framework work lands first**, mono#2554 reduces to the three lines of `tools/boss/CHECKS.yaml` and its guest doc comment; its `runtime.rs` and `resolve.rs` changes become unnecessary and should be dropped rather than merged.

Either way the interaction is confined to one task in the breakdown, which names it explicitly.

## Risks / open questions

- **Intersect is a semantic change with zero live instances but non-zero test surface.** The census is decisive about `CHECKS` files but the fixtures in `src/runner/tests_external.rs`, `src/external/declarative/tests_prettier.rs`, `tests_lint_biome.rs`, `tests_execution.rs`, `src/fix/tests.rs`, and `src/runner/tests_fix_multipass.rs` all use `include` (measured — `grep`). A reviewer should expect the declarative-path task to touch several of them and should read each edit as a semantics question, not a mechanical one.
- **`md/doc-structure`'s `include_globs` is a required field** (read — `src/checks/doc_structure.rs:73`, no `#[serde(default)]`). Migrating it to the framework key means making it optional (defaulting to match-all) or deleting it, and either choice changes what a config missing both keys does. The migration task must state which it picked.
- **`forbidden-imports-deps`'s per-rule `exclude_files` coordinate — recorded, not decided.** It is config-dir-relative via `strip_prefix(config_dir)` (read — `src/checks/forbidden_imports_deps.rs:109-110` declaring it with `alias = "exclude_globs"`, `:139-142` applying it, `:183-189` `is_excluded`) while its sibling `include_globs` three lines away is repo-relative (read — `:107`, applied at `:144-146`). New evidence for whoever decides it: **the disagreement has no live instance.** The only configured `forbidden-imports-deps` rule with either key in any of the four repos is in flunge's _root_ `CHECKS.yaml:59,62`, where `config_dir` is empty and `strip_prefix("")` is the identity, so the two coordinates coincide (measured). That makes a later fix cheap, but it is a behaviour change to a live config and is explicitly **out of scope here**.
- **How many out-of-tree `CHECKS` files carry a stray key?** Unmeasurable from mono. This gates `deny_unknown_fields` and is filed as its own survey task.
- **A userdoc inaccuracy, more narrowly than the investigation reported.** The investigation flagged `userdoc/docs/checks-config.md:188` as claiming `exclude` / `exclude_files` / `exclude_globs` are equivalent "both at the top level and inside a check entry". Read at HEAD, that line refers to the two _framework_ positions — top-level and check-entry — and is accurate for both (read — `src/config.rs:530-537`, `:581-582`). The real inaccuracy is one line down: `:190` says the framework "reads from both positions and merges them" without noting that the in-`config` position reads **only** `exclude_files` and `exclude_globs`, never canonical `exclude` (read — `extract_legacy_config_excludes`, `src/config.rs:645`). So `exclude:` written inside `config:` is a silent no-op. The docs task should fix `:190`, not `:188`.
- **The guard check's `syn` parse is the riskiest new component.** It must resolve a type through a doorway across two different call shapes. If it proves brittle, the fallback is to require an explicit marker attribute on each check's top-level config struct and denylist fields only on marked structs — more invasive but trivially robust. The task should be allowed to escalate to that shape without re-opening this design.
- **`--all` runtime on mono is the practical cost of the verification protocol.** Two whole-tree runs of two subcommands, on two binaries, per migration PR. If that proves slow enough to discourage the protocol, the fix is to scope `explain-scope` to a named check id rather than to weaken the comparison.

## Proposed implementation task breakdown

Entries are in dependency order. Depth-1 entries (1, 2, 3) have no dependencies and may run fully in parallel — they touch disjoint files.

### 1. `explain-scope` subcommand — machine-readable per-check file selection

Add a read-only `checkleft explain-scope` subcommand emitting, per configured check id, the resolved definition scope, the normalised entry scope patterns, the effective exclude patterns, and the sorted list of selected files, under both `--format human` and `--format json`. This is the surface the migration protocol's primary signal depends on and the surface the `--all` zero-match warning later consumes; without it, a migration that narrows a green check to zero files is undetectable, because `CheckResult` carries only `check_id` and `findings` (`src/output.rs:6-9`). Touches `src/main.rs` and a small read-only accessor on the runner. Ship it against today's selection logic so it is useful before any semantics change.

- **Effort hint:** medium
- **Dependencies:** none

Scope: in-scope

### 2. `PathScope` matcher core

Generalise `ExclusionMatcher` (`src/exclusion_matcher.rs:24-26`) into a `PathScope` holding an optional positive `GlobSet` alongside the existing negative one, with `filter_changeset` applying the positive set before subtracting the negative one so "excludes always win" is preserved. Confine this entry to the matcher module and its unit tests — keep `ExclusionMatcher` working as a thin wrapper or alias so no call site changes yet, which keeps this PR mechanically reviewable and lets it land in parallel with entries 1 and 3.

- **Effort hint:** small
- **Dependencies:** none

Scope: in-scope

### 3. Parse and normalise the check-entry `include` key

Add `include` to `ParsedCheckConfig` (`src/config.rs:562-586`) as a sibling of `config:`, and read the legacy in-`config` position too. Normalise both through the config-dir-relative path `exclude` already uses (`normalize_exclude_patterns`, `src/config.rs:618-624`; rename it to a scope-neutral name), carry the result on the resolved check config, and fold the normalised patterns into `run_group_key` (`src/runner.rs:96-116`) alongside `exclude_fingerprint`. Reject an empty list, mirroring `override_include` (`resolve.rs:290-295`). Parse and plumb only — no enforcement in this entry, so it is inert on merge.

- **Effort hint:** medium
- **Dependencies:** none

Scope: in-scope

### 4. Enforce `PathScope` on the built-in and component paths

Construct a `PathScope` in `build_scheduled_check_run` (`src/runner.rs:1258-1298`) from the patterns entry 3 plumbed, and substitute it at the built-in selection sites (`src/runner.rs:268`, `:360`) and the component sites (`src/external/runtime.rs:602`, `:646`). Add the symmetric positive test to `drop_excluded_findings` (`src/runner.rs:1472-1477`); leave `scope_findings_to_changeset` (`:1453-1459`) untouched. This is the entry that gives built-in checks a framework include side for the first time and gives component checks one without touching `wit/check.wit`. If mono#2554 has landed, this entry deletes `narrow_by_include` and folds its tests in; if not, coordinate per entry 6.

- **Effort hint:** medium
- **Dependencies:** `PathScope` matcher core; Parse and normalise the check-entry `include` key

Scope: in-scope

### 5. Switch the declarative path to intersect semantics

Change `select_files` (`src/external/declarative/executor.rs:280-313`) and its caller (`:110-113`) so the positive set is the definition manifest's `include` intersected with the entry's, replacing today's `override.unwrap_or(definition)`. Update the fixtures that exercise `include` (`src/runner/tests_external.rs`, `src/external/declarative/tests_prettier.rs`, `tests_lint_biome.rs`, `tests_execution.rs`) and read each edit as a semantics question. This is the only entry that changes observable behaviour for an existing config shape; the census found zero live instances, so the blast radius is tests plus out-of-tree configs.

- **Effort hint:** medium
- **Dependencies:** Enforce `PathScope` on the built-in and component paths

Scope: in-scope

### 6. Reconcile with mono#2554

Land the two efforts in a defined order and remove the duplicate implementation. If mono#2554 merges first, this entry is folded into entry 4 and consists of deleting `narrow_by_include` and re-pointing `resolve::include_globset`'s callers at the unified `PathScope`. If the framework work lands first, this entry reduces mono#2554 to its `tools/boss/CHECKS.yaml` lines and its guest doc comment, dropping its `runtime.rs` and `resolve.rs` changes. Whoever picks this up must check the PR's state first rather than assuming either order.

- **Effort hint:** small
- **Dependencies:** Enforce `PathScope` on the built-in and component paths

Scope: in-scope

### 7. Unify the fix path onto `PathScope`

Collapse `run_declarative_fix`'s exclusion subtraction and `filter_by_include` in `src/external/declarative/executor.rs` into a single `PathScope` call, so the fix path and the run path cannot drift in what they consider in scope. Kept separate from entry 5 because the fix path has its own path-normalisation behaviour and its own tests (`src/fix/tests.rs`, `src/runner/tests_fix_multipass.rs`), and folding it in would make entry 5 span two behaviours.

- **Effort hint:** small
- **Dependencies:** Switch the declarative path to intersect semantics

Scope: in-scope

### 8. Structurally-empty pattern diagnostics

Reject, at config-resolution time and as a `ConfigDiagnostic`, any `include` or `exclude` pattern with a leading `./`, a trailing `/`, or a leading `!`, with the `!` case pointing the author at `exclude`. Deliberately does **not** cover bare names with no separator and no wildcard, which are structurally capable of matching a root-level file — `pnpm-lock.yaml` is a live exclude entry of exactly that shape (mono root `CHECKS.yaml:38`) and must stay green. Symmetric across both keys so the include and exclude sides validate identically.

- **Effort hint:** small
- **Dependencies:** Parse and normalise the check-entry `include` key

Scope: in-scope

### 9. Zero-match warning under `--all`

Emit a warning — never an error, and only under `--all` — when a structurally-matchable pattern selects zero files across the whole tree. Consumes the per-check selected count that entry 1 exposes. Must stay silent in a diff run and must stay silent when the pattern matches tree files but no changeset files, per the binding zero-match semantics.

- **Effort hint:** small
- **Dependencies:** `explain-scope` subcommand; Enforce `PathScope` on the built-in and component paths

Scope: in-scope

### 10. Survey: stray keys in out-of-tree `CHECKS` files

Investigation entry. Enumerate every `CHECKS.yaml` / `CHECKS.toml` reachable across flunge, checkleft-sandbox, and any other consuming repo, and report which carry a key that `ParsedCheckConfig` does not recognise. mono is already clean by inspection; the risk is entirely out-of-tree. Sequenced before entry 11 because `deny_unknown_fields` is the single change most likely to break a config the framework cannot see, and the blast radius is unknown until measured.

- **Effort hint:** trivial
- **Dependencies:** none

Scope: in-scope

### 11. `deny_unknown_fields` on `ParsedCheckConfig`

Add `#[serde(deny_unknown_fields)]` to `ParsedCheckConfig` (`src/config.rs:561-562`), turning `excludes:`, `includes:`, and a misplaced `paths:` from silent no-ops into diagnostics. Applies to the check entry only — keys inside `config:` stay untouched, so the live empty `exclude_files = []` at `tools/cube/CHECKS.toml:10` is unaffected. Land only after the survey has bounded what it breaks.

- **Effort hint:** small
- **Dependencies:** Survey: stray keys in out-of-tree `CHECKS` files; Parse and normalise the check-entry `include` key

Scope: in-scope

### 12. Reject `include` on a `scope: changeset` check, and deprecate the in-`config` position

Two config-resolution diagnostics that belong together because both live in the same validation pass. Reject `include` on a `scope: changeset` entry, which runs against an empty changed-file set by construction and so cannot be file-scoped. Emit a `warning`-severity `ConfigDiagnostic` when `include` appears in the legacy in-`config` position, pointing at the sibling-of-`config:` position; keep accepting it.

- **Effort hint:** small
- **Dependencies:** Parse and normalise the check-entry `include` key

Scope: in-scope

### 13. Migrate `md/doc-structure` to the framework key

Move `include_globs` / `exclude_globs` off `DocStructureConfig` (`src/checks/doc_structure.rs:71-82`) and onto the check entry's `include` / `exclude` in mono's root `CHECKS.yaml:83-85`, leaving the check's intrinsic `.md` gate (`:101-103`) in place as its subject matter. Must state whether `include_globs` becomes optional or is deleted — it is required today, so a config carrying neither key changes meaning either way. Run the full four-step verification protocol and paste steps 2 and 3.

- **Effort hint:** small
- **Dependencies:** Enforce `PathScope` on the built-in and component paths; `explain-scope` subcommand

Scope: in-scope

### 14. Hoist `forbidden-imports-deps` per-rule `include_globs`

Hoist per-rule `include_globs` (`src/checks/forbidden_imports_deps.rs:107`, applied at `:144-146`) to the check entry's `include` where every rule in an instance shares one scope, keeping per-rule selection available for instances whose rules genuinely differ — under the naming rule, a surviving per-rule selector must be spelled `rules[].include`. **Does not touch `exclude_files`**, whose coordinate convention is an explicit non-goal. The only live instance is flunge's root `CHECKS.yaml:59`. Run the verification protocol.

- **Effort hint:** small
- **Dependencies:** Enforce `PathScope` on the built-in and component paths; `explain-scope` subcommand

Scope: in-scope

### 15. Express `code-patterns`'s `lang` file gate as a definition-level default

`lang` selects the _parser_ and must stay, but its implicit file gate (`lang: "java"` → `*.java`) becomes an explicit definition-level default scope so the framework, not the check, answers "is this file a target". Verify the default reproduces the current language-to-path mapping exactly before deleting the implicit gate, and run the verification protocol.

- **Effort hint:** small
- **Dependencies:** Switch the declarative path to intersect semantics; `explain-scope` subcommand

Scope: in-scope

### 16. Migration reconciliation sweep

After entries 13, 14, and 15, run the full four-step protocol once across the whole tree with every migration applied together, and reconcile the result against the sum of the individual PRs' pasted diffs. Separate from the migrating entries because a per-PR check cannot catch an interaction between two migrations — two checks whose scopes newly overlap, or a run-group key collision introduced by the new fingerprint component. Report any discrepancy as a finding rather than silently correcting it.

- **Effort hint:** small
- **Dependencies:** Migrate `md/doc-structure` to the framework key; Hoist `forbidden-imports-deps` per-rule `include_globs`; Express `code-patterns`'s `lang` file gate as a definition-level default

Scope: in-scope

### 17. Guard check: no check-level file scoping

Implement `checkleft/no-check-level-file-scoping`, parsing check sources with `syn`, resolving the type deserialised at the config doorway — `CheckInput::config<T>()` (`sdk/src/lib.rs:145-147`) for wasm guests, `ConfiguredCheckFactory::configure` (`src/check.rs:124`, `:129`) for built-ins — and applying the name denylist to that struct's fields only, so nested finer-axis selectors are permitted by construction. Scope it with the framework's own `include` over `tools/checkleft/checks/**/src/lib.rs` and `tools/checkleft/src/checks/**/*.rs`. If the `syn` doorway resolution proves brittle, escalate to an explicit marker attribute on each top-level config struct rather than weakening the denylist.

- **Effort hint:** medium
- **Dependencies:** Switch the declarative path to intersect semantics

Scope: in-scope

### 18. Documentation

Rewrite `userdoc/docs/checks-config.md` for the unified key: one `include`, intersect semantics replacing the replace semantics documented at `:257-281`, config-dir-relative authoring, and an explicit note that the _definition-manifest_ `include` stays repo-relative because a definition has no config directory. Fix `:190` — the in-`config` exclude position reads only `exclude_files` / `exclude_globs`, never canonical `exclude` (`src/config.rs:645`), so `exclude:` inside `config:` is a silent no-op; `:188` is accurate as written and should not be changed. Update `external-check-package-contract.md:101,108` and `check-author-api.md` with the guard rule and the required nesting for finer-axis selectors.

- **Effort hint:** small
- **Dependencies:** Switch the declarative path to intersect semantics; Guard check: no check-level file scoping

Scope: in-scope

### 19. Land mono#1648 — invalid `exclude` globs currently fail open

An invalid glob in an `exclude` list discards the _entire_ list and runs the check on everything, with only a `tracing::warn!` (`src/runner.rs:1275-1279`). The fix exists as an open, unmerged PR. It is a pre-existing defect rather than something this project introduces, but it should land before the migration entries, because a migration that introduces a malformed pattern would otherwise silently widen a check's scope instead of failing. Also fix the stale comment at `src/runner.rs:1275`, which describes the post-fix state rather than HEAD.

- **Effort hint:** small
- **Dependencies:** none

Scope: in-scope

### 20. Typed SDK finer-axis scope helper

Expose an SDK type a check uses to declare a genuinely finer-axis selector, so the framework can recognise the shape structurally rather than by field name. Strengthens entry 17's denylist against a novel-word false negative. Not a v1 blocker and not a replacement for the denylist: it reaches only SDK-using wasm guests and does nothing for the built-ins under `src/checks/**` or for a guest that hand-rolls its serde struct.

- **Effort hint:** medium
- **Dependencies:** Guard check: no check-level file scoping

Scope: deferred (future / not a v1 blocker) — the denylist covers the known drift shapes; build this only if a novel-word evasion actually occurs.

### 21. Explicit widening escape hatch

Add a way for a repo to _replace_ rather than intersect a definition's scope, for the case where a repo legitimately needs a check to cover a file type its definition omits. Deliberately not built in v1: with zero `include` overrides in any of the four surveyed repos, there is no evidence anyone wants to widen, and shipping an escape hatch before the constraint has bitten anyone re-opens the composition question by the back door.

- **Effort hint:** small
- **Dependencies:** Switch the declarative path to intersect semantics

Scope: deferred (future / not a v1 blocker) — no demonstrated demand; revisit if a repo hits the constraint.

### 22. `forbidden-imports-deps` per-rule `exclude_files` coordinate

Decide whether the per-rule `exclude_files` should stop being config-dir-relative (`src/checks/forbidden_imports_deps.rs:183-189`) and align with its repo-relative sibling `include_globs` (`:144-146`). Explicitly out of scope for this project and listed only so it is not mistaken for an oversight. Evidence for whoever decides it: the disagreement has zero live instances, because the only configured rule with either key sits in flunge's _root_ `CHECKS.yaml` where the two coordinates coincide.

- **Effort hint:** small
- **Dependencies:** none

Scope: deferred (future / not a v1 blocker) — a behaviour change to a live config; needs an owner's call, named as a non-goal here.

### 23. `skip_symlinks` unification

Fold the declarative-only `skip_symlinks` (`src/external/mod.rs:253-254`, applied at `executor.rs:299-307`) into a framework-level mechanism available to all three implementation types. A file-_type_ predicate rather than a path glob, so it does not fit the `include` / `exclude` pair and needs its own design.

- **Effort hint:** medium
- **Dependencies:** none

Scope: deferred (future / not a v1 blocker) — named as a non-goal here; separate design.

### 24. Component `eligible_file_count` under-reporting

The `eligible_file_count` trait default returns the full changeset size, which is what component checks get (`src/external/runtime.rs:259-266`), while declarative checks override it to apply their positive filter. Once entry 4 gives component checks a framework include side, the progress UI will over-report the denominator for any scoped component check. Cosmetic, and confined to the progress display.

- **Effort hint:** trivial
- **Dependencies:** Enforce `PathScope` on the built-in and component paths

Scope: deferred (future / not a v1 blocker) — progress-display cosmetics only; no effect on which files are checked.

### Parallelism and file-overlap notes

- **Depth 1 — fully parallel:** entries 1, 2, 3, 10, 19. Entry 1 touches `src/main.rs`, entry 2 touches `src/exclusion_matcher.rs`, entry 3 touches `src/config.rs`, entry 10 touches nothing, entry 19 touches `src/runner.rs`. Entries 1 and 19 both reach `src/runner.rs` but in unrelated regions (a read-only accessor versus `build_scheduled_check_run`'s matcher construction); the overlap is incidental and they stay parallel.
- **Entries 4 and 5 must be serial, not parallel.** They are functionally separable but both edit `src/external/declarative/executor.rs` and the same test fixtures. Entry 4 lands first; entry 5 forward-ports entry 4's changes preservingly.
- **Entries 8, 11, and 12 all edit `src/config.rs` validation.** They are independent in design and substantially likely to co-edit. Land in the order 8 → 12 → 11 (11 last, since the survey gates it), each forward-porting the previous preservingly rather than reverting.
- **Entries 13, 14, and 15 are parallel** — disjoint check sources and disjoint `CHECKS` files (mono root, flunge root, and the `code-patterns` definition respectively). Each runs the verification protocol independently; entry 16 reconciles them afterwards.
- **Entries 17 and 18 are parallel** in principle, but entry 18 documents the guard rule entry 17 implements, so entry 18 should land second and reflect what actually shipped.
