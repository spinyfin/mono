# Cut Boss file fan-out while keeping every safety decision explicit

- Date: 2026-11-13
- Status: proposed; this document ships no runtime changes
- Source inspected: `e2da58cf` on `main`
- Evidence: [#3589](https://github.com/spinyfin/mono/pull/3589), [#3600](https://github.com/spinyfin/mono/pull/3600), [#3599](https://github.com/spinyfin/mono/pull/3599), [#3549](https://github.com/spinyfin/mono/pull/3549), [#3466](https://github.com/spinyfin/mono/pull/3466), [#3516](https://github.com/spinyfin/mono/pull/3516)

The central bet is that Boss can declare each RPC's independent safety properties once without making any permission implicit. Remove repeated classification and storage-default bookkeeping while retaining the boundaries that determine authorization, transaction behavior, and build incrementality.

## Verdict

Adopt one exhaustive request-contract table, derive event labels and explicitly selected sanitization traversal from event declarations, and consolidate repeated task-row insertion into one storage writer. Keep explicit dispatch routing, independent security expectations, the existing migration registration and upgrade oracle, and the spoke crate boundaries. Defer dependency-generation adoption pending a bounded comparison of actual changed-file savings and build-graph fidelity; reject blanket visibility expansion.

The source-level estimate is **four fewer files for a new read RPC with a simple response**, and **three fewer production writer files when a shared creation default changes**. These savings have different applicability; they are not a claim that every feature will save seven files.

## Goals

- A minimal persisted-and-exposed field or new RPC verb should touch substantially fewer files because repeated bookkeeping disappears.
- Preserve explicit, exhaustive decisions for replay, admission, worker permissions, progression permissions, and outbound sanitization. A missing decision must fail compilation, never silently grant permission or choose a lane.
- Preserve wire compatibility, denial reasons, database atomicity, and one-way crate dependencies.
- Keep every implementation PR comfortably below the unchanged 30-file `change/file-count` limit and the unchanged 3,000-line file-size limit.
- Count all changed files, including generated manifests, lockfiles, tests, and build definitions. Reducing handwritten edits alone does not establish success.

## Non-goals

No new RPC, persisted product feature, schema format, ORM, generic SQL query builder, transport, authorization tier, test framework, or build system. Do not merge the client, protocol, application, and storage layers; consolidate spoke services; enlarge visibility domains; or relax repository checks. Do not reorganize numbered test modules solely to eliminate a `mod` line. Rejected options below have no implementation rows; the dependency-projection comparison is the one genuine deferred decision.

## Method and evidence

This is a **comparison of options using source inspection and historical diffs**, not a prototype validating a preselected implementation. Savings below are structural counterfactuals: substitute the proposed edit locations into a concrete existing change and count distinct paths. No completed implementation, performance measurement, or end-to-end result is claimed.

The initial survey covers 142 PRs created November 6–13: 21 exceeded 30 files and nine reached exactly 30. Its representative minimal persisted/read field costs 15–20 files, or about 25 with a crate. Those are the survey's aggregate observations, not a newly reproduced census. This review fetched the six linked PR file lists with `gh pr view <number> -R spinyfin/mono --json files,title` and inspected the corresponding current code.

The distinction between a whole PR and one change chain matters. For example, #3589 changes 22 work-db files, but implements actor attribution, backfill, triggers, existing writers, a read endpoint, and regression tests. Those 22 files are not 22 copies of a new column declaration.

| Chain                         | Observed bookkeeping                                                                                                                                                                                                      | Estimated removable files per qualifying change                                                                                                                                                                      | Decision and migration cost                                                                                         |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| Request classification        | `client/src/replay.rs`, `engine/app/src/app/read_admission/lane.rs`, and `engine/worker-policy/src/policy.rs` each enumerate requests. The policy file also maintains progression's independent allowlist.                | Three classification files become one request-contract file: **2 net**. Routing and policy expectation tests remain.                                                                                                 | Adopt in two PRs: transport/admission first, authorization second, each roughly 1,200–1,400 changed lines.          |
| Event metadata                | `engine/app/src/app/frontend_event_kind.rs` repeats variant names; `engine/worker-policy/src/sanitize.rs` enumerates pass-through events alongside real transformations.                                                  | For an already-edited `protocol/src/wire/events.rs`, remove edits to those two tables: **2 net** for a pass-through response. A new transformation still needs policy code and tests.                                | Adopt a small derive crate with its first real consumers, about 16 files / 1,400 changed lines.                     |
| Migration registration        | A new module, `lib.rs`, registry entry, and `schema_init.rs` expected-schema recipe can make four files. Reusing an owning module, as #3589 does, avoids a new module registration.                                       | Removing the test recipe could save **1**; production already has **one** registry. **0 accepted savings** in this design.                                                                                           | Retain. Replacing independent registration coverage merely with the registry under test is not an equivalent check. |
| Shared task creation defaults | Production `INSERT INTO tasks` statements appear in four files: `insert_helpers.rs`, `revision_helpers.rs`, `attentions.rs`, and `design_postmortem.rs`. Other apparent sites in the inspected search were test fixtures. | A default requiring edits in all four becomes one writer change: **3 net**. Explicitly new domain behavior still changes its callers.                                                                                | Adopt one typed insertion seam with all four production owners, about 9 files / 1,250 changed lines.                |
| Cargo/Bazel dependencies      | Internal path dependencies are authored in Cargo and Bazel; production, test-support and integration-test Bazel rules can repeat them. Root manifests and locks also change.                                              | Checked-in full-manifest generation: **0 files saved** by generation alone. A generated, untracked dependency projection could save **up to 1 consumer BUILD file per consumer**, only when it needs no other edits. | Defer a comparison; do not promise adoption or root-lockfile savings.                                               |
| Visibility                    | Spoke dependencies use several distinct consumer sets; `spoke-hub` production and test-support targets share app/core consumers, while `spoke-stream` exposes adapter/hub.                                                | A package group over an unchanged set saves **0** files for a normal new edge; several identical groups changed together could save `D - 1` dependency BUILD edits. No such recurring `D` is established here.       | Drop a broad conversion. A union of those sets changes permitted edges and is not deduplication.                    |
| Spoke crate consolidation     | Hub storage/projection, stream transport, outbound delivery, journal, worker API, and preflight have different dependencies and tests.                                                                                    | Eliminating a future crate could avoid the survey's five fixed build files, but only by changing the unit of compilation/ownership. **0 accepted savings**.                                                          | Drop. No measured compile-cost or ownership evidence justifies undoing these seams.                                 |

The checked PR lists confirm substantial build plumbing, but counts depend on the definition. Counting every `BUILD.bazel`, `Cargo.toml`, `Cargo.lock`, and `MODULE.bazel.lock` yields 15 of 48 files in #3549, 12 of 35 in #3466, and 10 of 31 in #3516. This includes release and visibility changes where present; it is not a measurement of removable duplication.

### Concrete RPC edit budget

The `ListTaskFieldAudit` slice of #3589 supplies a reproducible 15-path example:

| Paths, relative to `tools/boss/`                                                                                                               | Before |  After |
| ---------------------------------------------------------------------------------------------------------------------------------------------- | -----: | -----: |
| `protocol/src/types/task.rs`, `protocol/src/wire.rs`, `protocol/src/wire/events.rs`, `protocol/src/wire/frontend_event_wire_tests.rs`          |      4 |      4 |
| `engine/app/src/app/work_items.rs`, `cli/src/work_cmds.rs`, `engine/app/src/app/tests/task_flag_audit.rs`, `engine/app/src/app/tests.rs`       |      4 |      4 |
| `engine/app/src/app/dispatch.rs`, `engine/worker-policy/src/tier_matrix_tests.rs`                                                              |      2 |      2 |
| `client/src/replay.rs`, `engine/app/src/app/read_admission/lane.rs`, `engine/worker-policy/src/policy.rs` → `protocol/src/request_contract.rs` |      3 |      1 |
| `engine/app/src/app/frontend_event_kind.rs`, `engine/worker-policy/src/sanitize.rs` → annotations in the already-counted event declaration     |      2 |      0 |
| **Distinct paths**                                                                                                                             | **15** | **11** |

This excludes the other behavior in that PR and includes its test module registration. The four-file reduction is a target for implementation review, not an observed after-result. A request-only change saves two; an event-only change with a simple payload saves two. Additional policy behavior, projections, migration SQL, or UI work must be counted separately.

## Alternatives considered

### Put all metadata on request attributes and generate dispatch too

This could remove another registry edit and the dispatch edit. It also couples a protocol declaration to application handler bindings, whose routing includes both grouped handlers and application-specific calls. An opaque callback-token macro could avoid a reverse Rust dependency, so reverse dependency is not an unavoidable flaw of this option.

The deciding costs are reviewable: `wire.rs` already has 2,958 lines, its request enum occupies almost the entire file, and annotating every request needs a substantial declaration relocation or a domain-specific syntax. A separate ordinary Rust contract table reduces three inventories to one without transforming the wire enum or its formatting. Keeping `dispatch.rs` costs one explicit binding per verb, and the Rust match remains exhaustive. Handler binding is not inferred from read/write classification.

Macros are not rejected categorically: `work-db-boundary` already generates an error boundary and validates read transactions, `read_admission/lane.rs` uses a callback macro for its Live inventory, and serde/bon are established precedents. The event derive below has a smaller contract and immediate consumers; there is no equivalent need to encode application routing in protocol declarations.

### Infer safety from names or a single read-only bit

Reject. `GetCoordinatorHandoff { redeliver: true }` has different replay behavior from `redeliver: false`; `GetPrStatus { refresh: true }` changes admission; guarded creates depend on `force_duplicate`; idempotent setters can be replayable without being worker-authorized. Progression deliberately receives fewer verbs than an ordinary worker. These are checkable counterexamples to deriving all properties from a `Get` prefix or `is_read` flag.

The equivalence required during migration is equality of each decision **for the same request payload and authenticated tier**, including denial details and timing predicates. It is not equivalence between replayability, read admission, or permission.

### Keep every table separate, or combine them in the application

Keeping them separate preserves current review prompts but pays the recurring three-file cost. Putting all classifications in the app would make the client depend upward on engine policy or require a second client copy. Choose a small protocol-level value contract instead: client and engine consume it independently; execution and enforcement remain in their existing layers.

### Remove the second migration registration

Inspection changes the premise: [`WorkDb::init`](../../engine/work-db/src/schema_init.rs) calls `registry::all()` once, and the runner applies missing ids inside one transaction. The other list is `supported_databases_apply_post_floor_migrations_without_losing_data`'s expected-schema recipe. It repeats migration functions but independently enumerates what should be registered; it is not a second production registration site.

Generating that expected list from the registry would stop detecting a migration omitted from the registry. Replacing it with a separately maintained schema golden still adds a changed file per migration. Per-migration tests exercising genuine `WorkDb::init` could eventually replace the registration oracle, but that requires a coverage reconciliation across historical migrations before claiming equivalence. Saving one fixed file does not justify that separate migration-test redesign here. Retain the list, immutable released baseline golden, unique UTC ids, `legacy_version: None` for new entries, and current upgrade tests.

This is not a claim that the recipe independently verifies every migration's SQL: it calls the same migration functions. Its independence is **registration coverage**, whereas the released-schema golden independently anchors the supported historical baseline.

### Generate complete Cargo manifests from Bazel, or complete BUILD files from Cargo

Reject full-manifest generation for v1. [`MODULE.bazel`](../../../../MODULE.bazel) already uses `crate.from_cargo` and `all_crate_deps` for third-party dependency resolution. Generating its Cargo input from a configured Bazel graph risks a bootstrap cycle. Generating whole BUILD files from Cargo must preserve information Cargo does not express: visibility, test-only targets, data/runfiles, platform constraints, and alternate test-support libraries. `spoke-adapter/BUILD.bazel`'s `aliases` replacing WorkDb and execution contracts are a concrete example.

Generating checked-in files can reduce mistakes, but they still count toward `change/file-count`. Generation as a technique is established in `protocol-spoke`; the objection is to treating unlike graph dimensions as equivalent or counting generated output as absent.

The narrower candidate is an untracked, hermetic projection of **ordinary local Cargo dependency edges** into Bazel, while BUILD files continue to own targets and overrides. No recorded justification for manually duplicating every local edge was found in the inspected build files and build design. That is an unresolved ownership decision, not evidence that a missing historical rationale once justified it. The deferred comparison must establish feasibility and actual file savings before proposing implementation.

### Replace narrow visibility with one Boss-wide group; merge the spoke crates

Reject both as fan-out fixes. The first changes which consumers may depend on a target, even if no current binary uses the new access. Groups are useful when they name an already-approved identical set; they cannot turn a union of different sets into an equivalent policy.

The second runs against the recorded [engine incrementality design](split-engine-core-for-build-incrementality.md) and the repository's preference for distinct crates. `spoke-journal` depends on the spoke protocol, `spoke-outbound` adds delivery/backoff, `spoke-worker-api` adds proposal and attachment capabilities, and `spoke-hub` adds WorkDb. Sharing a prefix does not establish equivalent ownership or dependency reach. This does not prohibit merging a future tiny glue crate after measuring its real consumers; it supplies no consolidation task now.

### Centralize all updates in a generic database patch engine

Reject. Domain writers intentionally carry different predicates, compare-and-set guards, statement ordering, bulk cardinality, and side effects. Replacing them with a universal setter could silently change dispatch and revision semantics.

There is already a shared seam: [`task_status_audit::execute`](../../engine/work-db/src/task_status_audit.rs) supplies actor/basis and savepoint behavior; SQLite triggers capture row values, including raw writes as unattributed. Reuse it. Consolidate the demonstrably repeated insertion column/default layout, and leave domain-specific update SQL in place. Do not forecast that all 22 work-db files from #3589 disappear on a future change.

## Chosen approach

### One exhaustive request contract, with independent dimensions

Add `protocol/src/request_contract.rs` as an ordinary Rust module exported from `protocol/src/lib.rs`. It owns the sole per-request classification match. The payload/serde enum remains in `wire.rs`; this is one declaration of policy per variant, not a promise to reduce payload and policy to one syntactic declaration.

The contract has four required dimensions: replay rule, admission class, worker access decision, and progression access decision. Each is a small closed enum/value, with no default constructor and no wildcard classification. Reject any convenience constructor that fills authorization from replay or read admission. After the transport stage, the authorization stage adds mandatory fields and updates every row in the same PR.

Use exhaustive pattern matching with data-dependent expressions where needed. Guarded-create classification carries whether the duplicate guard is enabled; the client retains the window duration, safety margin, elapsed-time check, and reconnect policy. Admission retains the `GetPrStatus.refresh` branch and current Live inventory. Existing behavior is copied exactly, including apparently surprising combinations; changing those combinations is a separate behavior change.

Worker access carries the current allow/deny category and redirect selector. `UpdateWorkItem` retains its field-dependent redirect. Progression has an independent explicit allow/deny choice for every variant; worker allow never implies progression allow. The worker-policy crate translates these values into its current denial objects. It continues enforcing policy at the authenticated connection gate; the table never trusts a tier supplied in an RPC payload. Row scoping in `app/trust.rs` and authorization inside handlers remain mandatory.

There is no engine dependency from protocol. New semantic types are protocol value types, not references to app services or worker-policy implementations. `dispatch.rs` remains an exhaustive typed handler match. A new verb must compile in both the contract table and dispatcher before it is usable.

Retain the independently authored progression fixtures in `tier_matrix_tests.rs`, including the exact allowed set. Remove `PROGRESSION_ALLOWED_VERBS` from the production API: its inspected callers are the policy implementation, tests, re-export, and documentation, so the contract replaces its runtime use and fixtures supply the independent expected set. Update those references together. Point the existing exhaustive-vocabulary test scan at the contract source instead of the removed worker match, keeping fixture synthesis and the assertion that every classified verb is exercised. Declare that source as a Bazel test input with narrow visibility; do not read an undeclared checkout path. Likewise preserve the Live-lane audit's exact-set coverage against the relocated classification, rather than merely asserting that known Live requests still work. Do not generate expected permissions or the expected Live set from the contract. A security-relevant new verb still warrants a test edit; that edit is intentionally included in the 11-file target.

### Event declarations own labels and traversal, policy owns transformations

Add a focused `protocol-macros` crate rather than expanding the unrelated WorkDb macro. A derive on `FrontendEvent` emits its Rust variant label and an exhaustive worker-sanitization traversal. Labels preserve the existing UpperCamelCase strings used by `frontend_event_kind::kind`; they are not serde wire tags. Wire tags, field names, defaults, and serialization shape remain unchanged.

Every event explicitly selects either reviewed pass-through or a named transformation hook. Missing, duplicate, or unrecognized selections fail compilation. A pass-through annotation means its present payload was reviewed for worker exposure; it is not a permanent authorization for all future fields. Payload changes must still update exposure tests when they change what can be disclosed.

For transformed events, generate a visitor method with the exact payload types and no default implementation; a typed tuple can carry a large variant's fields without introducing a long argument list. Worker-policy implements those methods using the existing `sanitize_run`, execution handling, and work-tree filtering. Preserve the current pass-through decisions too, including `WorkRuntimePatch` and `TopicEvent`. The derive only generates destructure/reconstruct traversal; it does not decide which fields are secret. A new hook cannot compile until worker-policy implements it. Preserve every existing transformation and the single outbound choke point for both responses and topic pushes.

Simple new response variants then change only their event declaration and relevant wire/behavior tests. Changes to a real sanitization rule still touch policy. Generate neither permissions nor expected redacted JSON. Keep the forbidden-key assertions and authenticated-socket coverage independent of the derive.

Use the normal manifest/lockfile registration path for this crate, with visibility only to protocol. Reuse the existing syn/quote dependency versions and repository Bazel patterns; do not introduce a generator service, checked-in Rust expansion, or new compilation path. The crate and its event consumers land together.

### One typed task-row insertion writer

Add a crate-private storage module, `work-db/src/task_insert.rs`, consumed by the four existing production insertion owners. It is small storage glue inside WorkDb, not a separately compiled subsystem. A builder-equipped insertion value owns common columns and defaults; required identity fields remain required. Optional/defaulted fields must not require every construction site to change when a new defaulted storage field is added.

The writer accepts the caller's existing connection/transaction, row values, and `WriteAttribution` plus operation basis. It executes the one column/parameter layout through `task_status_audit::execute`. It must not open a new connection, commit the caller's transaction, allocate identities twice, or publish events. Existing callers continue to own product/project validation, duplicate suppression, priority authorization, short-id allocation, revision-parent checks, dependency creation, and the choice of domain-specific values.

Preserve the distinctions between project tasks, chores, investigations, revisions, and seeded design tasks. Before replacing each INSERT, record its effective defaults, including values currently supplied by SQLite when columns are omitted. The shared writer must reproduce those values, actor/basis strings, and row counts exactly. Use `Option`/builder defaults only where omission has a defined meaning; do not convert the production row mappers to builders.

All four production owners migrate in the same PR, with their real public creation paths exercised. Existing low-level SQL fixtures that deliberately construct old, partial, or invalid rows remain fixtures. Ordinary updates keep their existing transaction predicates and continue to use the audit helper; this design does not add another wrapper around that helper for no benefit.

### Load-bearing invariants

- **Decision completeness:** every request payload selects every contract dimension, and every event explicitly selects a traversal; omission is a compile error.
- **Authority preservation:** for identical payloads and authenticated identities, allowed/denied results, denial categories, redirects, row scope, and emitted worker-visible fields are unchanged.
- **Replay preservation:** guard enablement, horizon, margin, and time-of-resend checks are unchanged, not merely the name of a replay category.
- **Wire preservation:** serialized tags, field names, defaults, response envelopes, and event ordering are unchanged. Equal Rust labels alone do not prove equal wire behavior.
- **Storage preservation:** committed row values, audit provenance, trigger effects, failure rollback, and transaction membership are unchanged across every migrated creation path.
- **Build preservation:** configured dependency edges, proc-macro host edges, aliases, test-only access, runfiles, and visibility are independently preserved; an equal list of package names is insufficient.

### Rollout and acceptance

Each implementation row replaces its production consumers in its own PR. There is no default-off flag, dormant runtime service, or follow-up activation step. Land the transport table first, then its authorization extension. Land event metadata after authorization because both touch protocol integration and worker-policy review surfaces. Storage can proceed independently.

For transport/authorization changes, run the existing client, protocol, worker-policy and app Bazel tests, adding assertions for both values of each payload-dependent branch. For events, exercise the genuine authenticated app socket and outbound writer, including subscriptions; a hand-built enum-to-JSON demonstration is not integration evidence. Keep wire round trips and forbidden-key assertions. Compile-fail tests for the derive must be Bazel test targets, with compiler inputs/toolchain declared, not direct execution of a built test binary.

For insertion, run the existing WorkDb and app creation/audit tests against real `WorkDb::init`, then the engine suite. Check project-task, chore, investigation, revision, attention-seeded design, and postmortem creation; verify trigger attribution and rollback when a later operation in the same transaction fails. Use existing upgrade tests to prove that no schema change was introduced.

Canonical affected targets include:

```sh
bazel test //tools/boss/client:client_test //tools/boss/protocol:protocol_test //tools/boss/engine/worker-policy:worker-policy_test //tools/boss/engine/app:app_test
bazel test //tools/boss/engine/work-db:work_db_test //tools/boss/engine/work-db:storage_behavior_test
bazel test //tools/boss/engine/...
checkleft run
```

These are implementation acceptance commands, not tests run for this document. Keep normal Bazel caching and the test sandbox. No direct Cargo build/test/fmt, direct test-binary execution, cache clearing, or timeout wrapper is part of the plan.

Each implementation PR reports the actual diff footprint and the old-to-new path substitution from the concrete RPC table or insertion inventory. Validation in that PR gates its own replacement before publication; a later audit cannot authorize an earlier unsafe migration. No synthetic product field or RPC should ship merely to demonstrate a file count. If real implementation requires an additional permanent edit site, update these estimates and explain it in the same PR rather than maintaining a test that asserts the obsolete number.

## Risks / open questions

- **A shared contract makes wrong classifications easier to repeat consistently.** Compilation establishes completeness, not correctness. Preserve independently authored permission tests and review every current branch during conversion. Do not derive expectations from the registry.
- **Payload evolution can evade a pass-through decision.** Retain forbidden-key tests and real socket coverage, and require an exposure review when a pass-through event gains fields. This is also a limitation of today's variant-level sanitization match; generation does not solve field-level policy automatically.
- **The event visitor may grow too broad.** Limit its vocabulary to existing transformations and keep implementations in worker-policy. No handler dispatch, runtime callbacks, or arbitrary string-to-function lookup belongs in it.
- **Insertion defaults differ by creator.** SQLite defaults and deliberately omitted columns are part of behavior. Characterize them before replacement, and preserve per-domain overrides. A writer that merely accepts the same struct shape is insufficient evidence of equivalence.
- **Migration fan-out remains.** The recommendation intentionally accepts the one-file expected-schema cost instead of weakening coverage. This is a rejected optimization, not an implicit promise of later migration work.
- **Manifest projection remains undecided.** The deferred comparison below must choose among existing crate-universe support, a narrow declared-input projection, and retaining explicit local edges. It must be able to recommend keeping the current system; no generator implementation is authorized by that row.
- **Footprint estimates are perishable.** The inspected source is already close to size limits in places. The proposed new modules keep request metadata out of `wire.rs`; re-estimate changed lines and files against the latest base before implementation. No exception to a check is implied.

No separate human decision is required to review this proposal. The open engineering uncertainty is explicitly bounded to the deferred dependency comparison; v1 does not depend on its outcome.

## Proposed implementation task breakdown

Breakdown size: 5 entries (4 in-scope, 1 deferred) — request transport and authorization are separate reviewable replacements, event traversal has its own derive and consumers, task insertion is one storage seam, and dependency projection needs a separate comparative investigation.

The two request entries are ordered because they substantially co-edit the contract table. Event metadata follows authorization to avoid simultaneous changes to protocol exports/build integration and worker-policy tests; forward-port earlier changes preservingly. Task insertion can run in parallel with the protocol chain: it does not edit those shared files. The deferred comparison has no implementation dependency and may run independently if scheduled. Tests live with each capability; there is no separate validation campaign or unused-abstraction PR.

### Centralize request replay and admission contracts

Scope description: Introduce the protocol value contract and replace the two existing replay/admission inventories with consumers of one exhaustive match. Include every current request and all payload-dependent branches. Keep dispatch and authorization behavior unchanged in this first capability; it ships with both real consumers, not an unused registry.

Major deliverables:

1. One required replay/admission classification per request, with no wildcard/default and no upward dependency.
2. Client and admission integration preserving replay windows, refresh branches, and the Live inventory, validated by their existing and extended behavior tests.

Likely changed files:

- `tools/boss/protocol/src/request_contract.rs` (new)
- `tools/boss/protocol/src/request_contract_tests.rs` (new)
- `tools/boss/protocol/src/lib.rs`
- `tools/boss/protocol/BUILD.bazel`
- `tools/boss/client/src/replay.rs`
- `tools/boss/engine/app/BUILD.bazel`
- `tools/boss/engine/app/src/app/read_admission/lane.rs`
- `tools/boss/engine/app/src/app/read_admission/tests.rs`

Estimated size: ~8 files / ~1300 changed lines

Effort: medium

Dependencies: none

Scope: in-scope

### Make worker authorization consume the request contract

Scope description: Extend every existing contract row with explicit worker and progression decisions, then replace worker-policy's production request inventory and allowlist bookkeeping. Retain independent security fixtures, current denial details, handler authorization, and connection row scoping. This is an authorization-preserving conversion, not a permission expansion.

Major deliverables:

1. Mandatory independent worker/progression classifications, including existing redirect semantics, replacing the production allowlist and its API references.
2. Worker-policy integration and independent permission/real-connection regression coverage demonstrating unchanged authority for allowed and denied requests.

Likely changed files:

- `tools/boss/protocol/src/request_contract.rs`
- `tools/boss/protocol/src/request_contract_tests.rs`
- `tools/boss/protocol/src/lib.rs`
- `tools/boss/protocol/BUILD.bazel`
- `tools/boss/engine/worker-policy/BUILD.bazel`
- `tools/boss/engine/worker-policy/src/lib.rs`
- `tools/boss/engine/worker-policy/src/tiers.rs`
- `tools/boss/engine/worker-policy/src/policy.rs`
- `tools/boss/engine/worker-policy/src/tests.rs`
- `tools/boss/engine/worker-policy/src/tier_matrix_tests.rs`
- `tools/boss/engine/app/src/app/tests/trust_authorization.rs`

Estimated size: ~11 files / ~1400 changed lines

Effort: large

Dependencies: Centralize request replay and admission contracts

Scope: in-scope

### Derive event labels and explicit sanitization traversal

Scope description: Add the focused protocol derive and convert all current events plus both real consumers in the same PR. Each event explicitly chooses pass-through or an existing typed transformation hook. Keep wire serialization and policy transformations intact; retain independent redaction expectations and exercise the actual outbound path.

Major deliverables:

1. A minimally visible derive crate emitting stable variant labels and exhaustive typed traversal, with Bazel-run rejection tests for incomplete or invalid declarations.
2. Event annotations, application label integration, and worker-policy visitor implementation preserving every current transformation and verified on response and push paths.

Likely changed files:

- `Cargo.toml`
- `Cargo.lock`
- `MODULE.bazel.lock`
- `tools/boss/protocol-macros/Cargo.toml` (new)
- `tools/boss/protocol-macros/BUILD.bazel` (new)
- `tools/boss/protocol-macros/src/lib.rs` (new)
- `tools/boss/protocol-macros/src/tests.rs` (new)
- `tools/boss/protocol-macros/tests/invalid_annotations.rs` (new)
- `tools/boss/protocol/Cargo.toml`
- `tools/boss/protocol/BUILD.bazel`
- `tools/boss/protocol/src/wire/events.rs`
- `tools/boss/protocol/src/wire/frontend_event_wire_tests.rs`
- `tools/boss/engine/app/src/app/frontend_event_kind.rs`
- `tools/boss/engine/worker-policy/src/sanitize.rs`
- `tools/boss/engine/worker-policy/src/tests.rs`
- `tools/boss/engine/app/src/app/tests/trust_authorization.rs`

Estimated size: ~16 files / ~1400 changed lines

Effort: large

Dependencies: Make worker authorization consume the request contract

Scope: in-scope

### Unify task-row insertion defaults across production creators

Scope description: Implement one crate-private typed task insertion writer and migrate the four production owners together. Reuse the existing audit execute helper and attribution type; leave domain validation, transaction ownership, dependencies, and update predicates with their current callers. Characterize existing creator defaults and retain deliberately low-level test fixtures.

Major deliverables:

1. One builder-backed insertion value and SQL column layout, exercised by task/chore/investigation, revision, attention-seeded design, and postmortem creation.
2. Real WorkDb/app creation-path tests covering effective defaults, actor/basis preservation, trigger rows, and rollback within the enclosing transaction.

Likely changed files:

- `tools/boss/engine/work-db/src/task_insert.rs` (new; includes focused writer tests)
- `tools/boss/engine/work-db/src/lib.rs`
- `tools/boss/engine/work-db/src/insert_helpers.rs`
- `tools/boss/engine/work-db/src/revision_helpers.rs`
- `tools/boss/engine/work-db/src/attentions.rs`
- `tools/boss/engine/work-db/src/design_postmortem.rs`
- `tools/boss/engine/work-db/src/tests/task_status_audit_tests.rs`
- `tools/boss/engine/app/src/app/tests/create_revision_actor.rs`
- `tools/boss/engine/app/src/app/tests/task_flag_audit.rs`

Estimated size: ~9 files / ~1250 changed lines

Effort: large

Dependencies: none

Scope: in-scope

### Compare local dependency projection with explicit Bazel edges

Scope description: Conduct a bounded option-selection investigation before proposing any build generator. Inspect the actual crate-universe outputs and configured graphs for spoke-adapter, spoke-hub, and spoke-worker-api, including their test-support aliases. Compare using existing generated workspace mappings, a narrow declared-input dependency projection, and keeping explicit local edges. Do not migrate manifests or author a generator in this investigation.

Major deliverables:

1. A reproducible comparison recording configured production/test/proc-macro edges, aliases, platform behavior, visibility, bootstrap requirements, and genuinely removable changed paths for #3600 and #3549.
2. A go/no-go recommendation that can choose the current approach, with measured file arithmetic and a bounded subsequent design only if projection preserves the required graph dimensions. No implied adoption or task creation.

Likely changed files:

- `tools/boss/docs/investigations/local-dependency-projection.md` (new)

Estimated size: ~1 files / ~250 changed lines

Effort: medium

Dependencies: none

Scope: deferred (future / not a v1 blocker) — graph fidelity and actual file savings must be established before dependency-generation adoption.
