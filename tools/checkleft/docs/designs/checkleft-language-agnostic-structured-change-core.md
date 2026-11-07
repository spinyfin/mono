# Checkleft: language-agnostic "structured change" core

Status: design (no implementation in this change)
Project: `checkleft: language-agnostic structured-change core`

## Overview

A checkleft check today receives a `ChangeSet` (which files changed, plus per-file
line hunks) and a `SourceTree` (raw byte access to files), and returns `Finding`s.
Anything richer than "these bytes changed" — "this proto field's type changed", "this
Java method's signature is no longer source-compatible", "this TOML key was removed" —
each check must reconstruct from raw bytes on its own, at both the base and current
revision, every time. There is no shared notion of a _parsed model_, a _structured
delta between two revisions of that model_, or _typed data flowing from a finding into
its fix_.

Avesta's Starlark-checks spec (uhvesta/mono#2) introduced exactly that missing layer —
but bound it to Starlark as the authoring language and to a bespoke per-format Rust
adapter for each format. This document hoists the **substrate** — parsed model at two
revisions → structured deltas → findings → typed fix data → file edits — into
checkleft's **core**, decoupled from any single authoring surface, and designs it to
scale across ~14 languages/formats without building 14 bespoke type systems.

The central design bet is a **three-tier adapter model** over one shared
structured-change contract: a **generic syntactic tier** (tree-sitter → a uniform
`kind`/children/span node tree, so a new language is "register a grammar + selectors",
near-zero code), a **typed-projection tier** (hand-written typed models for the few
high-value formats/concerns where the payoff justifies the cost), and a **text/line
tier** (the grammarless universal fallback). All three produce the same core
`Delta`/`Finding`/`FileEdit` types; authoring surfaces (Rust-native, Starlark, config)
are _consumers_ of that core, not definers of it.

This is a design only. No feature code is included; the final section is a
dependency-ordered, PR-sized implementation breakdown.

## Goals

- **Define the format-agnostic structured-change contract in checkleft core.** A single
  set of core traits/types — parsed model at two revisions → structured deltas →
  findings → typed fix data → file edits — that lives independent of Starlark, Rust, or
  any config format, and that every check kind and adapter builds on.
- **Make polyglot support cheap by default.** Adding a new language should not require a
  bespoke type system. A generic syntactic tier backed by a shared parsing substrate
  (tree-sitter) must make "add a language" cost approximately "register a grammar +
  declare selectors."
- **Allow richness where it pays.** High-value formats/concerns (proto evolution,
  Java/Kotlin API surface, structured-config schema) can graduate to a typed-projection
  tier layered on the generic tree, opt-in and incremental.
- **Provide a useful default structured delta for _all_ formats.** A generic keyed
  tree-diff (add / remove / modify / rename / move over stably-identified nodes) should
  give every format a meaningful `Delta` with zero per-format delta code, with typed
  deltas as specializations.
- **Make `Finding` + typed fix data + `FileEdit` first-class core types** (they partly
  exist already; this formalizes and extends them) and make **change-region restriction
  (T2060)** — limiting detection and fixes to PR-changed hunks — a natural, built-in
  property of the model rather than each check's problem.
- **Reuse checkleft's existing change-detection / base-ref machinery** (`Scenario`,
  base-ref resolution, `ChangeSet`, `SourceTree::read_file_versioned`) as the source of
  "base vs current"; do not build a parallel mechanism.
- **Keep authoring surfaces as thin consumers.** Rust-native checks, a future Starlark
  surface (avesta's model re-expressed on top), and config-driven checks all sit on the
  same substrate, selectors, findings, and fix model.

## Non-goals

- **Shipping a Starlark authoring surface.** checkleft has no Starlark check-authoring
  tier today (the `starlark`/`bazel` code in-tree _checks_ Starlark files; it does not
  _author_ checks in Starlark). This design shows Starlark as _one future consumer_ of
  the core and keeps the contract Starlark-compatible, but building that surface is
  separate downstream work.
- **Building all 14 typed projections.** The typed-projection tier is opt-in and
  demand-driven. v1 delivers the generic tier + text tier + the core contract, and _one_
  reference typed projection (proto) as proof. The other 13 formats are served by the
  generic/text tiers until a concrete check justifies a projection.
- **Replacing the existing check/finding/fix pipeline.** The runner, scheduler,
  exclusion model, `fix` subcommand design, and the three execution tiers (built-in,
  declarative, wasm-component) are reused. This adds a layer they can consume; it does
  not rewrite them.
- **A general-purpose semantic/AST refactoring engine.** The core produces structured
  _deltas_ and _edits_; it is not a language server, a type checker, or a
  cross-file program analyzer. Whole-program semantic analysis stays a per-check concern.
- **Replacing tree-sitter with a custom parser framework**, or committing to
  tree-sitter as the _only_ possible generic backend. The generic tier's contract (the
  uniform node tree) is defined so the backend is swappable; tree-sitter is the chosen
  first implementation, not a permanent coupling.
- **Wiring every existing check onto the new model in v1.** Existing checks keep working
  unchanged (they consume `ChangeSet`/`SourceTree` directly). Migration to the structured
  layer is incremental and per-check.

## Current-state grounding: checkleft today

Source: `tools/checkleft/src/{check,input,output,runner,source_tree,vcs}.rs`,
`src/change_detection/`, `src/fix/`, `src/external/`, `wit/check.wit`, `sdk/src/lib.rs`.

### What a check sees and returns

A check is the `Check` / `ConfiguredCheck` trait pair (`src/check.rs`). The runtime
surface is:

```rust
// src/check.rs
#[async_trait]
pub trait ConfiguredCheck: Send + Sync {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult>;
    // + run_with_progress, applicable_file_count, declared_exclusions, evaluate_exclusion
}
```

A check receives **`&ChangeSet`** (what changed) and **`&dyn SourceTree`** (how to read
bytes) and returns a **`CheckResult { check_id, findings: Vec<Finding> }`**. There is no
parsed-model or delta abstraction between "raw bytes" and "check logic."

### The change model (`src/input.rs`)

```rust
pub struct ChangeSet {
    pub changed_files: Vec<ChangedFile>,          // path + kind + old_path
    pub file_diffs: HashMap<PathBuf, FileDiff>,   // per-file line hunks
    // + commit_description, pr_description, change_id, repository
}
pub struct ChangedFile { pub path: PathBuf, pub kind: ChangeKind, pub old_path: Option<PathBuf> }
pub enum ChangeKind { Added, Modified, Deleted, Renamed }
pub struct FileDiff { pub hunks: Vec<DiffHunk> }
pub struct DiffHunk { pub old_start, old_lines, new_start, new_lines, added_lines, removed_lines: usize }
```

Two facts matter for this design:

1. **Hunk-level change data already exists** (`file_diffs` → `DiffHunk` with
   `new_start`/`new_lines`), but is currently consumed only for line-delta accounting
   (`FileDiff::line_delta`), not for restricting where findings may land. That's the
   T2060 gap (below).
2. **The two-revision read substrate already exists.** `SourceTree` exposes

   ```rust
   fn read_file_versioned(&self, path: &Path, version: TreeVersion) -> Result<Vec<u8>>;
   pub enum TreeVersion { Current, Base }
   ```

   The default impl `bail!`s on `TreeVersion::Base`, but the _concept_ — "read this file
   as it was at the base revision" — is already the trait's job. The WIT contract goes
   further: `change-set` already carries `base-files` and the SDK exposes
   `ChangeSet::base_file_content`, so base-revision _content_ already crosses to wasm
   guests. A structured-change core that needs "parse the file at base and at current"
   plugs directly into this substrate; it does not invent its own revision access.

### The finding + fix model (`src/output.rs`, already partly what we want)

```rust
pub struct Finding {
    pub severity: Severity,                 // Error | Warning | Info
    pub message: String,
    pub location: Option<Location>,         // { path, line: Option<u32>, column: Option<u32> }
    pub remediations: Vec<String>,
    pub suggested_fix: Option<SuggestedFix>,
}
pub struct SuggestedFix { pub description: String, pub edits: Vec<FileEdit> }
pub struct FileEdit { pub path: PathBuf, pub old_text: String, pub new_text: String }
```

`Finding`, `SuggestedFix`, and `FileEdit` **already exist** and are mirrored in the WIT
contract (`wit/check.wit` defines `finding`, `suggested-fix`, `file-edit`, plus
`change-kind`, `diff-hunk`, `file-diff`). The `fix` subcommand design
(`checkleft-fix-subcommand-auto-apply-fixes.md`) already adopts `SuggestedFix.edits` as
one of its fix sources. Two gaps relative to avesta's model:

- **There is no _typed fix data_ channel.** avesta's model routes a strongly-typed
  `fix_data` struct from a check to its fixer as the _only_ channel between them; the
  fixer, not the check, computes edits. checkleft today has only the already-computed
  `edits: Vec<FileEdit>` — the check must produce final edits inline.
- **`FileEdit` is content-based, not span-based** (`old_text`/`new_text`, no line/byte
  range). That is fine for whole-file formatters but makes hunk-restriction (T2060) a
  text search rather than a range intersection.

### Change-region restriction today, and the T2060 gap

The only restriction that exists is **file-level**:

```rust
// src/runner.rs:1284
fn scope_findings_to_changeset(result: &mut CheckResult, changeset: &ChangeSet) {
    let changed: HashSet<&Path> = changeset.changed_files.iter().map(|f| f.path.as_path()).collect();
    result.findings.retain(|f| match &f.location { None => true, Some(l) => changed.contains(l.path.as_path()) });
}
```

A finding survives iff its `location.path` is a changed file. Nothing intersects the
finding's _line_ with the file's changed _hunks_. **T2060** (referenced by this project's
brief as the tracked "restrict detection/fixes to PR-changed hunks" investigation — note
there is no T2060 doc or code in-tree today; this design is where that mechanism would
land) is the move from file-level to hunk/line-level restriction: on a PR, a check should
(by default) only flag and only fix regions the PR actually touched, so that a formatting
or lint check does not light up thousands of pre-existing violations in a file the author
merely brushed.

The data to do this is already present (`file_diffs[path].hunks[*].new_start/new_lines`,
computed by `vcs.rs` and threaded to every check, `runner.rs`), and there is a working
**in-tree precedent**: the `ifchange` check already restricts its own findings to changed
regions via per-hunk overlap tests (`hunk_touches_range` in
`checks/file/ifchange/src/lib.rs`). What is missing is a _host-level_ mechanism so every
check gets this uniformly rather than re-implementing it: (a) a place for the _decision_
to live and (b) findings/edits that carry enough span information to intersect against
hunks. The structured-change core is the natural home for both — it turns the `ifchange`
one-off into a framework property.

### The three execution/authoring tiers today

checkleft already runs checks in three kinds (see
`checkleft-component-model-wasm-external-checks-capability-fs-ergonomic-typed-api.md`):

- **Built-in** — Rust compiled into the binary (`src/checks/**`).
- **Declarative** (`declarative-v1`) — YAML describing framework-invoked binary calls +
  transforms (`checks/{format,lint}/*.yaml`, `src/external/declarative/`).
- **wasm-component** — typed Rust guest authored via the `#[check]` SDK against the WIT
  contract, run sandboxed in-process.

All three produce `Finding`s and share the runner, scheduler, exclusion, and (planned)
`fix` machinery. **None of them today receives a parsed model or a structured delta** —
that is precisely the layer this design adds _beneath_ the authoring tiers, so that all
of them gain it at once.

## Alternatives considered

### A. Per-format bespoke typed adapters (avesta's shape, ported verbatim) — rejected

Do what avesta's spec does: a hand-written Rust `FormatAdapter` per format, each with its
own `parse → typed descriptor set`, its own `diff → typed delta`, its own typed context
injection, and its own file selectors. `proto`, `java`, `module_json`, `text` as the
built-in set; add one adapter per new format.

Rejected as the _core strategy_ (not as a tier — it survives as the typed-projection
tier):

- **It does not scale to 14+ formats.** Each adapter is a parser binding + a bespoke type
  model + a bespoke diff + tests. Fourteen of these is fourteen mini-projects and a
  perpetual maintenance surface; most would exist only to answer shallow syntactic
  questions a generic tree already answers.
- **It front-loads cost before value is known.** You pay the full adapter cost for
  `html`, `css`, `markdown` before any check needs their typed structure — and most never
  will.
- **It fragments the model.** N disjoint type systems means N different mental models for
  check authors and no shared delta semantics. A check spanning two formats (e.g. "this
  BUILD target references a deleted proto") straddles two unrelated type worlds.

The right lesson from avesta is the _pipeline shape_ (parse@2revs → delta → finding →
fix*data → edits) and the *`text` escape hatch\_ — not "one bespoke type system per
format."

### B. One universal generic tree only (tree-sitter for everything, no typed models) — rejected

Adopt a single shared parsing substrate (tree-sitter) exposing a uniform node tree
(`kind` string, children, spans) and _stop there_. Every check matches on `kind` strings
against the generic tree; no typed projections at all.

Rejected as the _whole_ strategy (it survives as the generic tier, the default):

- **It pushes untyped `kind`-string matching into every check**, including the
  high-value ones where it hurts most. "Is this proto field's type change wire-compatible?"
  expressed as generic node matching is brittle, verbose, and re-implemented per check.
- **No semantic delta for concerns the tree doesn't model.** Proto/Java compatibility is
  about _meaning_ (field numbers, wire types, method erasure), not syntax node identity;
  a purely syntactic diff misclassifies semantically-equal-but-syntactically-different
  changes and vice versa.
- **Grammars vary in quality and shape.** A single generic tree hides real differences;
  some concerns genuinely need a curated model.

### C. The tiered model over one shared contract — **chosen**

Define the structured-change contract once in core, then serve formats through **three
tiers**: generic syntactic (default, near-zero-code breadth), typed-projection (opt-in
richness where it pays), and text/line (universal fallback). The generic tier gives
breadth and consistency; the typed tier gives ergonomics and safety exactly where a check
justifies it; the text tier guarantees _every_ format is at least minimally served. All
tiers emit the same `Delta`/`Finding`/`FileEdit`. This is the balance the project asks
for — neither one grammar to rule them all nor N disjoint type systems.

## Chosen approach: the structured-change core

### The pipeline, and where each piece lives

```
                 checkleft change-detection (existing: Scenario + base-ref resolution)
                                     │  yields base_ref, ChangeSet, SourceTree
                                     ▼
   ┌──────────────────────────────────────────────────────────────────────────────┐
   │  STRUCTURED-CHANGE CORE  (new, format-agnostic)                                │
   │                                                                                │
   │   Adapter::parse(file, TreeVersion::Base)    ─┐                                │
   │   Adapter::parse(file, TreeVersion::Current) ─┴─►  ParsedModel×2               │
   │                                                        │                       │
   │   Adapter::diff(base_model, current_model) ────────►  Delta  (structured)      │
   │                                                        │                       │
   │        check logic (any authoring tier) consumes Delta + models               │
   │                                                        │                       │
   │                                              Vec<Finding{ fix_data? }>         │
   │                                                        │  (change-region gate) │
   │                       fix:  Fixer::edits(fix_data, models) ──► Vec<FileEdit>   │
   └──────────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
              existing runner / scope_findings / fix-subcommand apply path
```

The core is a **library layer between change-detection and the check**. It does not own
scheduling, base-ref resolution, or edit application — those already exist. It owns
_parse_, _diff_, _the delta/finding/fix_data/edit types_, and _the change-region gate_.

### Core traits and types

Everything below is `tools/checkleft` core Rust (not Starlark, not WIT-first — the WIT
contract _mirrors_ these, as it does today for `Finding`). Illustrative, not final.

```rust
/// A parsed representation of one file at one revision. Opaque to the framework;
/// meaningful only to the adapter that produced it and the checks that consume it.
/// The generic tier's model is a `GenericTree`; a typed tier's is e.g. `ProtoModel`.
pub trait ParsedModel: Send + Sync + Any {
    /// The uniform generic view every model can produce, so generic-tier checks and the
    /// default tree-diff work against ANY adapter's output. Typed models return a
    /// projection of themselves; the generic model returns itself.
    fn as_generic(&self) -> &GenericTree;
}

/// A format/concern adapter: parse at a revision, diff two revisions.
/// One adapter per FORMAT/CONCERN, not per language (avesta's key property).
pub trait Adapter: Send + Sync {
    type Model: ParsedModel;
    type Delta: StructuredDelta;

    /// Which files this adapter claims (by extension and/or file name). At most one
    /// adapter binds a given file (deterministic precedence; see Selectors).
    fn selectors(&self) -> &Selectors;

    /// Parse one file's bytes at one revision into the typed model.
    fn parse(&self, path: &Path, bytes: &[u8], rev: TreeVersion) -> Result<Self::Model>;

    /// Structured delta between two revisions of the model. The DEFAULT impl calls the
    /// generic keyed tree-diff over `as_generic()`; typed adapters override to add
    /// semantic delta kinds (see Structured deltas).
    fn diff(&self, base: Option<&Self::Model>, current: Option<&Self::Model>) -> Result<Self::Delta> {
        Self::Delta::from_generic(generic_tree_diff(base.map(|m| m.as_generic()),
                                                    current.map(|m| m.as_generic())))
    }
}

/// The uniform generic node tree — the shared substrate every adapter can expose.
pub struct GenericTree { pub root: NodeId, pub arena: NodeArena, /* source, language id */ }
pub struct Node {
    pub kind: Kind,                 // grammar node type, e.g. "field", "function_item"
    pub name: Option<SymField>,     // adapter-declared "identity field" if any (see identity)
    pub span: Span,                 // byte + line/col range in this revision
    pub children: Vec<NodeId>,
    // text accessor via arena + source
}
```

### Structured deltas, generically

**Claim: a generic keyed tree-diff is a useful _default_ `Delta` for all formats, with
typed deltas as specializations.** The generic diff produces:

```rust
pub enum GenericChange {
    Added   { node: NodeRef<Current> },
    Removed { node: NodeRef<Base> },
    Modified{ base: NodeRef<Base>, current: NodeRef<Current>, fields: Vec<FieldChange> },
    Renamed { base: NodeRef<Base>, current: NodeRef<Current> },   // identity same, name changed
    Moved   { base: NodeRef<Base>, current: NodeRef<Current> },   // identity same, parent/order changed
}
pub struct StructuredDeltaGeneric { pub changes: Vec<GenericChange>, /* indexed by path & node */ }

pub trait StructuredDelta: Send + Sync {
    fn generic(&self) -> &StructuredDeltaGeneric;              // every delta exposes the generic view
    fn from_generic(g: StructuredDeltaGeneric) -> Self;       // constructible from it
}
```

**Stable node identity across revisions** is the crux (without it, a diff degenerates to
"everything after an insertion changed"). The identity of a node is a _key_ computed by
the adapter, resolved in this precedence:

1. **Declared identity field.** An adapter marks certain kinds as identity-bearing and
   names the child that carries the key — e.g. `function_item` → its `identifier`,
   `field` → its field name (or, for proto, its field _number_). This is one line of
   selector config per identity-bearing kind, not code.
2. **Structural path fallback.** For kinds with no declared identity, the key is
   `(kind, ordinal-among-same-kind-siblings)` under the parent's key — a stable
   positional identity that still survives edits elsewhere in the file.
3. **Content anchor (text tier).** For the text/line tier, "identity" is line content +
   surrounding context (a Myers-style line diff), yielding added/removed lines and
   modified regions — the generic change kinds still apply, at line granularity.

Matching runs parent-first: match roots by identity, then recursively match children by
identity within matched parents. Unmatched base node → `Removed`; unmatched current node
→ `Added`; matched with differing name → `Renamed`; matched with differing
parent/position → `Moved`; matched with differing scalar fields/text → `Modified` (with a
`FieldChange` list). This is a well-trodden keyed-tree-diff; the novelty here is only that
**it is the shared default so no format writes a diff to get a usable delta.**

**Typed deltas are specializations, not replacements.** A typed adapter's `Delta` _embeds_
the generic delta (via `generic()`) and _adds_ semantic change kinds its checks want:

```rust
pub enum ProtoChange {
    FieldTypeChanged { number: i32, from: ProtoType, to: ProtoType, wire_compatible: bool },
    FieldNumberReused { number: i32, /* ... */ },
    FieldRemoved { number: i32, name: String },
    // ... plus generic() for anything not semantically modeled
}
```

A proto check reasons over `wire_compatible` directly; a check that only wants "some field
changed" can still read the generic view. This is the graduation path in action: start on
the generic delta, promote the queries that hurt into typed change kinds.

### The tiered polyglot strategy, with the 14 formats categorized

Three tiers, one contract:

- **Generic syntactic tier (default).** A tree-sitter grammar registered under the shared
  substrate; the adapter is _data_: `selectors` (ext/name), plus identity-field
  declarations for the kinds that have natural keys. `parse` = run the grammar; `diff` =
  the default generic tree-diff. **Cost to add a language: register a grammar + a few
  selector/identity lines. Near-zero bespoke code.**
- **Typed-projection tier (opt-in).** A hand-written `Adapter` whose `Model`/`Delta` are
  curated types, _layered on the generic tree_ (it can call the grammar and project, or
  use a format-specific parser like `prost`/`protobuf` descriptors). Justified only when a
  concern needs semantic accuracy the syntax tree can't cheaply express.
- **Text/line tier (universal fallback).** No grammar. Model = lines; delta = line diff
  (Myers) mapped onto the generic change kinds. Guarantees every file — even an unknown
  extension — has _some_ structured delta and can host line-oriented checks (the avesta
  `text` escape hatch).

Categorization of the 14 named formats (default tier in **bold**; graduation candidates
noted):

| Format     | Default tier         | Graduate to typed when…                                                                                                            |
| ---------- | -------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| **proto**  | **typed-projection** | _v1 reference projection._ Wire/source-compat is inherently semantic (field numbers, wire types) — the flagship case for typed.    |
| java       | **generic**          | API-surface / source-compat checks land → typed "API surface" projection (avesta's `java`).                                        |
| kotlin     | **generic**          | Same trigger as java; ideally shares the "API surface" projection shape.                                                           |
| go         | **generic**          | Exported-API / breaking-change checks justify a typed surface.                                                                     |
| rust       | **generic**          | The existing giant-structs / API checks stay built-in; a typed "pub API surface" projection only if a breaking-API check needs it. |
| python     | **generic**          | Public-API or import-graph checks justify projection; else generic suffices.                                                       |
| typescript | **generic**          | Exported `.d.ts`-style API-surface checks justify projection; formatting/lint stay declarative.                                    |
| yaml       | **generic**          | Schema/key-presence checks want a typed "config record" projection (keys, types, paths).                                           |
| json       | **generic**          | Same as yaml; JSON is the easiest typed "config record" projection (module_json-like).                                             |
| toml       | **generic**          | Same as yaml/json — `Cargo.toml`/manifest policy checks are strong graduation triggers.                                            |
| shell/bash | **generic**          | Rarely; most shell checks are lint-tool (declarative) or text-tier. Grammar exists for structure-aware checks.                     |
| markdown   | **generic**          | Link/heading/section-structure checks use the generic tree; unlikely to need typed.                                                |
| html       | **generic**          | Structural checks (attrs, tags) use the generic tree; typed projection unlikely.                                                   |
| css        | **generic**          | Selector/property checks use the generic tree; typed projection unlikely.                                                          |

Rationale: **exactly one format starts typed (proto)** because its core value _is_
semantic compatibility and it is the clearest, most-bounded proof. Everything else starts
generic (grammars exist for all of them) and graduates only behind a concrete check. The
config family (yaml/json/toml) is called out as the most likely _next_ graduation because
"key removed / type changed / value out of policy" is a small, high-reuse typed model
("config record" ≈ avesta's `module_json`) serving three formats at once. Nothing is
stranded: every format has the text tier beneath it from day one.

### The generic-tree-vs-typed-projection tradeoff, and where the line sits

The tension is real: the uniform tree maximizes breadth/consistency but pushes untyped
`kind`-string matching into checks; bespoke typed models maximize ergonomics/safety but
cost per format. **Where the line sits (the recommendation):**

> **Default to generic. Graduate a format to a typed projection only when _all three_
> hold: (1) a real, wanted check needs a query the generic tree can't express _cheaply or
> safely_ (semantics, not syntax); (2) that query recurs across multiple checks or is
> compatibility-critical (a wrong answer ships a breaking change); (3) the typed model is
> _small_ relative to the format (a curated surface, not a full re-parse of the grammar).**

Concretely:

- **Syntactic questions stay generic.** "Is there a `TODO` without an owner?", "does this
  heading skip a level?", "is this attribute present?" — generic tree, no projection.
- **Semantic/compatibility questions graduate.** "Is this proto change wire-compatible?",
  "did this Java method's erased signature change?", "was this required config key
  removed?" — typed projection.
- **Formatting/whole-file lint doesn't use structured change at all** — it stays in the
  declarative tier (run `oxfmt`/`prettier`), which this design leaves untouched.

The **graduation mechanism** is deliberately cheap and non-breaking: because a typed
`Delta` embeds the generic delta and a typed `Model` exposes `as_generic()`, a format can
be promoted with **no change to existing generic-tier checks** on that format — they keep
reading the generic view; new typed checks read the richer one. Graduation is additive.

To keep the generic tier from being miserable to author against, the generic node API
ships **ergonomic query helpers** (find-by-kind, descendants, named-child, text-of,
span-of) and a small **selector DSL** shared with the declarative tier, so "match `field`
nodes whose `type` child changed" is a few lines, not a manual tree walk. This narrows the
ergonomics gap that would otherwise push formats to graduate prematurely.

### The core Finding / fix_data / FileEdit model, and T2060

This formalizes the fix pipeline as core types and threads typed fix data through it.

**Typed fix data as the only check→fix channel.** Rather than a check computing final
edits inline, a finding may carry an opaque, strongly-typed `fix_data` payload; a
registered _fixer_ for that check turns `fix_data` + the parsed models into `FileEdit`s.
This is avesta's key property, hoisted to core:

```rust
pub struct Finding {
    pub severity: Severity,
    pub message: String,
    pub location: Option<Location>,      // extended: Location gains an optional Span (byte/line range)
    pub remediations: Vec<String>,
    pub fix: Option<FixHandle>,          // NEW: typed fix data + fixer id …
    pub suggested_fix: Option<SuggestedFix>,  // … OR precomputed edits (today's path; kept)
}

/// Typed, opaque-to-framework fix payload + the fixer that interprets it.
pub struct FixHandle { pub fixer_id: FixerId, pub data: TypedFixData /* erased; downcast by the fixer */ }

pub trait Fixer: Send + Sync {
    fn fixer_id(&self) -> FixerId;
    /// Compute edits from typed fix data and the parsed models at both revisions.
    fn edits(&self, data: &TypedFixData, models: &ModelPair) -> Result<Vec<FileEdit>>;
}

/// FileEdit gains an explicit span so edits intersect cleanly with changed hunks.
pub struct FileEdit {
    pub path: PathBuf,
    pub range: Option<Span>,   // NEW: byte/line range replaced; None = content-anchored (today's old_text/new_text)
    pub old_text: String,
    pub new_text: String,
}
```

Both channels coexist: whole-file formatters keep emitting precomputed `SuggestedFix`
edits; structured checks emit typed `fix_data` and let a fixer compute edits. The `fix`
subcommand (already designed) applies either through its existing sandbox/copy-back path —
it just gains "materialize `FixHandle` → edits via the registered `Fixer`" as a third fix
source alongside the declarative and wasm sources it already lists.

**T2060 (change-region restriction) becomes a property of the core, not each check.**
Because (a) findings carry a `Span` and (b) `FileEdit` carries a `range`, and (c)
`ChangeSet.file_diffs[path].hunks` gives the PR-changed line ranges, the core provides a
single **change-region gate**:

```rust
/// Keep a finding only if its span intersects a changed hunk of its file (PR scenarios);
/// pass-through under --all / non-PR scenarios. Applied centrally, after checks run.
fn gate_to_changed_regions(findings: &mut Vec<Finding>, changeset: &ChangeSet, mode: RegionMode) { … }
/// Symmetrically clip/verify fix edits so a fixer can only rewrite changed regions.
fn gate_edits_to_changed_regions(edits: &mut Vec<FileEdit>, changeset: &ChangeSet, mode: RegionMode) { … }
```

This _generalizes_ today's file-level `scope_findings_to_changeset` (runner.rs:1284) to
hunk-level and _lifts the `ifchange` check's one-off `hunk_touches_range` logic into the
framework_, driven by the same `Scenario` the change-detection layer already computes:
restrict in the change-scoped scenarios (`PullRequest`, `MergeQueue`, `PushToBranch`),
pass through in `PushToDefault` / `--all` / `Local` full scans. Because the gate reads spans, it works uniformly for every tier
(generic, typed, text) and every authoring surface — a check author gets change-region
restriction _for free_ by producing spanned findings, which the structured core does by
construction (every `Node` and every line carries a `Span`). `RegionMode` (strict =
finding must be _inside_ a hunk; touching = finding's span _overlaps_ a hunk) is the one
policy knob, defaulting per scenario. This directly answers the project's requirement that
"the core edit model should make that filtering natural."

### How authoring surfaces consume the core

The core is deliberately _below_ the authoring tier, so each surface is a thin consumer:

- **Rust-native checks (exists today).** A built-in check declares which adapter/tier it
  wants and receives `ModelPair` + `Delta` instead of raw `ChangeSet`/`SourceTree`. A thin
  `StructuredCheck` adapter trait bridges to today's `ConfiguredCheck` so the runner is
  unchanged:

  ```rust
  #[async_trait]
  pub trait StructuredCheck: Send + Sync {
      fn adapter(&self) -> AdapterId;                        // which format/tier
      async fn inspect(&self, delta: &dyn StructuredDelta, models: &ModelPair) -> Result<Vec<Finding>>;
  }
  ```

- **Starlark checks (future consumer — avesta's model re-expressed).** avesta's spec
  becomes _one binding_ over this core: the Starlark host injects the same `Delta`/`Model`
  as Starlark values, a Starlark check returns `Finding`s with typed `fix_data`, and a
  Starlark fixer returns `FileEdit`s — but the parse/diff/adapter substrate, the selectors,
  and the finding/fix/edit types are checkleft's, not Starlark's. Starlark authors get the
  full tiered adapter set (proto typed model, generic tree, text) that Rust authors get,
  because they share the substrate. **This is the decoupling the project asks for:
  Starlark stops _owning_ the structured-change model and starts _consuming_ it.**
- **Config-driven checks (declarative tier, extended).** The existing declarative YAML
  gains the ability to express _structured_ checks — "on adapter=generic(java), find nodes
  of kind X added under Y, emit a finding" — via the shared selector DSL, for the large
  class of checks that are pure structural pattern-matches needing no code. The
  wasm-component tier (typed Rust guest) consumes the same core through the WIT contract,
  which already mirrors the finding/edit/hunk records and would gain the model/delta
  records.

All three surfaces share: the adapter registry, selectors, the generic tree + typed
projections, the delta model, `Finding`/`fix_data`/`FileEdit`, and the change-region gate.
None of them re-implements parse or diff.

### Extensibility, registration, versioning, testing

- **Registration.** An `AdapterRegistry` maps `AdapterId → Adapter` and resolves a file to
  at most one adapter via `Selectors` (ext/name, deterministic precedence: name > ext,
  typed > generic > text; ties are a build-time error, mirroring avesta's "at most one
  adapter per file"). Built-in adapters register in-tree; generic-tier grammars register
  by data (grammar handle + selectors + identity declarations); wasm/out-of-tree adapters
  register through the existing external-provider path.
- **Cost to add a language, per tier.** Text tier: **zero** (automatic fallback). Generic
  tier: **register a grammar + selectors + identity lines** — a data change, no diff/model
  code, unit-testable with a couple of fixtures. Typed tier: a real adapter (parser
  binding + curated model + typed delta + tests) — sized like one of avesta's adapters,
  paid only when a check justifies it.
- **Versioning.** Grammars and typed models are versioned; a delta's model-version is
  recorded so a cache or a cross-revision parse mismatch is detected, not silently
  misread. tree-sitter grammar upgrades are treated like the wasmtime pin in the
  component design — deliberate, tested bumps. The generic node `kind` strings are a
  grammar-version-coupled surface; checks matching on them are validated against the
  pinned grammar in CI so a grammar bump that renames a node fails loudly.
- **Testing.** Each adapter ships golden tests: parse fixtures → model snapshot; (base,
  current) fixture pairs → delta snapshot; and, for typed adapters, semantic assertions
  (e.g. "field-number reuse is flagged wire-incompatible"). The generic tree-diff has one
  shared property-test suite (identity stability, add/remove/rename/move classification)
  that every generic-tier language inherits — a single diff to test, not fourteen.
- **Reuse of change-detection.** The core never resolves refs itself: it consumes the
  `Scenario` + base-ref that `src/change_detection/` already computes, reads base/current
  bytes via `SourceTree::read_file_versioned`, and restricts via `ChangeSet.file_diffs`.
  The one required upgrade to existing infra is making `TreeVersion::Base` actually
  supported by the production `SourceTree` (today the default impl `bail!`s) so base-revision
  parsing works in CI — a bounded, well-scoped change to the VCS-backed source tree.

## Risks / open questions

- **tree-sitter as the generic backend.** Grammars exist for all 14 formats, but quality,
  error-recovery, and node-shape consistency vary. Risk: a poor grammar makes a format's
  generic tier weak. Mitigation: the text tier is always beneath; a weak grammar can be
  replaced or the format pinned to text until a better grammar exists. The generic-tier
  _contract_ (uniform node tree) is backend-swappable by design.
- **Generic node identity heuristics.** The `(kind, ordinal)` structural fallback can
  misclassify large reorderings as remove+add. Mitigation: declared identity fields for
  the kinds that matter; the fallback is only for kinds no check keys on. Open question:
  do we need a content-similarity tie-breaker (move detection) in v1, or defer it?
- **`TypedFixData` erasure.** Threading an opaque typed payload through core Rust
  (`Any`/downcast) and across the WIT/Starlark boundaries (where it must serialize) is the
  trickiest part. Open question: is a typed `fix_data` worth it in v1, or do we ship only
  the existing precomputed-`edits` channel first and add typed fix data in a second phase?
  (Recommendation: ship precomputed edits + the change-region gate first; add typed
  `fix_data` once the proto projection needs it.)
- **`FileEdit` span vs content.** Adding `range: Option<Span>` to `FileEdit` touches the
  WIT contract and the `fix` subcommand's apply path. Open question: extend `FileEdit`, or
  introduce a parallel `SpannedEdit` and convert? Recommendation: extend, keeping
  `old_text`/`new_text` for content-anchored fallback.
- **Change-region default aggressiveness.** Strict hunk-only restriction can hide a real
  regression whose _cause_ is a changed line but whose _symptom_ lands outside the hunk.
  Open question: default to `touching` (overlap) rather than `strict` (contained), and let
  a check opt into strict? This needs a human call (see attentions).
- **Config-driven structured checks scope.** A selector DSL powerful enough to express
  real structural checks risks becoming a query language. Open question: how much power in
  v1 (v1 = "kind added/removed under parent"; defer richer predicates)?
- **Interaction with the wasm sandbox.** Structured models can be large; passing full
  `GenericTree`s across the WIT boundary per check may be costly. Open question: does the
  guest re-parse inside the sandbox (host provides bytes at both revisions, already
  planned via the FS sandbox + base reads) rather than the host lifting a whole tree
  across the ABI? Recommendation: guest-side parse using a shared SDK, host provides
  base+current bytes.

## Proposed implementation task breakdown

Dependency-ordered, PR-sized. Effort hints ∈ `trivial | small | medium | large`.
Parallelism noted per depth. Task names are stable identifiers for the dependency graph.

**Depth 0 — may run in parallel (no inter-dependencies):**

- **core-types: structured-change core types + traits.**
  Scope: Introduce the core module: `Adapter`, `ParsedModel`, `StructuredDelta`,
  `GenericTree`/`Node`/`Span`, `GenericChange`/`StructuredDeltaGeneric`, `Selectors`, and
  the `AdapterRegistry` skeleton (resolve file → at most one adapter, deterministic
  precedence, build-time tie error). Types + registry only; no parsing backend, no checks
  wired. Unit tests for selector precedence and registry resolution.
  Effort: medium. Dependencies: none.

- **base-reads: make `TreeVersion::Base` real in the production `SourceTree`.**
  Scope: Implement base-revision reads in the VCS-backed `SourceTree` (git/jj) using the
  base ref the change-detection layer already resolves, replacing the default `bail!`.
  Tests that base+current bytes differ correctly across `Scenario`s. Prereq for any
  two-revision parse.
  Effort: small. Dependencies: none.

- **region-gate: hunk-level change-region gate (generalize file-level scoping).**
  Scope: Add `gate_to_changed_regions` for findings and `gate_edits_to_changed_regions`
  for edits, driven by `ChangeSet.file_diffs` hunks and `Scenario`, with a `RegionMode`
  (strict/touching) policy defaulting per scenario. Generalizes
  `scope_findings_to_changeset` (runner.rs:1284) from file-level to line-level; keep the
  file-level path as the `None`-span fallback. This can land and be tested against
  synthetic findings before any adapter exists. Directly realizes T2060.
  Effort: medium. Dependencies: none.

**Depth 1 — may run in parallel once their deps land:**

- **generic-tier: tree-sitter generic-tree adapter substrate.**
  Scope: Wire a tree-sitter backend that turns (bytes, grammar) into a `GenericTree`
  (`kind`/children/`Span`), plus the grammar/selector/identity _registration data_ format.
  Register 2–3 grammars as proof (e.g. yaml + markdown + one of java/go). No diff yet.
  Golden parse tests.
  Effort: large. Dependencies: core-types.

- **generic-diff: the shared keyed generic tree-diff.**
  Scope: Implement `generic_tree_diff` (identity resolution: declared field → structural
  `(kind, ordinal)` fallback; parent-first matching; add/remove/modify/rename/move
  classification) as the default `Adapter::diff`. One shared property-test suite (identity
  stability, classification) that every generic-tier language inherits.
  Effort: large. Dependencies: core-types. (Parallel with generic-tier; needs
  generic-tier only to test end-to-end on a real grammar.)

- **text-tier: text/line fallback adapter.**
  Scope: The grammarless universal fallback: model = lines, delta = Myers line diff mapped
  to `GenericChange` at line granularity, auto-bound to any file no other adapter claims.
  Guarantees every file has a structured delta. Golden tests.
  Effort: small. Dependencies: core-types.

- **finding-fix-model: extend `Finding`/`FileEdit` for spans + typed fix handle.**
  Scope: Add `Location.span`, `FileEdit.range`, and the `FixHandle`/`Fixer`/`FixerId`
  types + a `FixerRegistry`; keep `SuggestedFix` precomputed-edits path intact. Wire the
  WIT contract mirror for the new fields (records only; no guest change forced). No fixer
  logic yet.
  Effort: medium. Dependencies: core-types. (Parallel; region-gate consumes the spans once
  both land.)

**Depth 2 — may run in parallel:**

- **rust-consumer: `StructuredCheck` bridge for built-in Rust checks.**
  Scope: The `StructuredCheck` trait + a bridge that adapts it to today's
  `ConfiguredCheck` so the runner is unchanged; a check declares an `AdapterId`, receives
  `ModelPair` + `Delta`, returns spanned `Finding`s that flow through the region-gate.
  Port ONE simple existing check (or add a trivial generic-tier check) end-to-end as
  proof.
  Effort: medium. Dependencies: generic-tier, generic-diff, base-reads, region-gate,
  finding-fix-model.

- **proto-projection: reference typed-projection tier (proto).**
  Scope: The flagship typed adapter: `ProtoModel` + `ProtoChange` (field number/type/wire
  compatibility) embedding the generic delta, using descriptor parsing. One real proto
  compatibility check on top, proving the graduation path (generic view still available).
  Effort: large. Dependencies: core-types, generic-diff, base-reads. (Parallel with
  rust-consumer.)

- **format-registration: register the remaining generic-tier grammars.**
  Scope: Register grammars + selectors + identity declarations for the rest of the 14
  formats on the generic tier (json, toml, python, typescript, kotlin, go, shell, html,
  css, and any of java/markdown/yaml not done in generic-tier). Pure data + fixtures per
  language; inherits the shared diff test suite.
  Effort: medium. Dependencies: generic-tier, generic-diff.

**Depth 3 — may run in parallel:**

- **typed-fix-data: thread typed `fix_data` through fixers + `fix` subcommand.**
  Scope: Implement `FixHandle` materialization (`Fixer::edits(fix_data, models) →
FileEdit`), register it as a third fix source in the `fix` subcommand's apply path
  alongside declarative/wasm/`suggested_fix`, and run edits through
  `gate_edits_to_changed_regions`. Wire one fixer (proto or a generic-tier check) end to
  end. (Gated on the Depth-based decision to ship typed fix data in v1 — see attentions.)
  Effort: medium. Dependencies: finding-fix-model, proto-projection (or rust-consumer),
  region-gate.

- **config-consumer: config-driven structured checks via the selector DSL.**
  Scope: Extend the declarative YAML tier to express structural checks over an adapter
  ("kind added/removed under parent" in v1) using the shared selector DSL, emitting
  spanned findings — no code per check. Deliberately minimal predicate power in v1.
  Effort: medium. Dependencies: generic-tier, generic-diff, rust-consumer.

**Depth 4:**

- **docs-migration-guide: authoring guide + graduation playbook.**
  Scope: Document how to add a language (each tier's cost), how to author a structured
  check on each surface, and the generic→typed graduation criteria/mechanism. Reference
  doc; no code.
  Effort: small. Dependencies: rust-consumer, proto-projection, config-consumer.

**Deferred / future — not a v1 blocker (recorded so the rejection set is explicit):**

- **starlark-consumer: Starlark authoring surface over the core** (avesta's model
  re-expressed as a binding). The whole point of the decoupling, but a large separate
  project; the core is designed to receive it without change. Effort: large. `future / not a v1 blocker`.
- **java/kotlin/go/config typed projections.** Graduate on demand behind a concrete check;
  the config-record projection (yaml/json/toml) is the most likely next one. Effort:
  large each. `future / not a v1 blocker`.
- **Move detection via content similarity** in the generic diff (v1 uses identity + a
  simple move rule only). Effort: medium. `future / not a v1 blocker`.
- **Guest-side structured parsing SDK** (wasm guest re-parses from host-provided
  base+current bytes rather than lifting a whole tree across the ABI). Effort: medium.
  `future / not a v1 blocker`.
- **Cross-file / multi-format structured checks** (a check spanning two adapters, e.g.
  BUILD target ↔ referenced proto). The shared substrate makes it possible; scope it as
  its own project. Effort: large. `future / not a v1 blocker`.
- **Parsed-model/delta caching** across a run (parse-once, diff-once reuse). Pure
  optimization; measure first. Effort: medium. `future / not a v1 blocker`.
