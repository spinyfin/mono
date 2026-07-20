# Checkleft: `fix` subcommand (auto-apply fixes)

## Status

**Shipped.** The v1 feature landed across twelve PRs (#1621–#1632); this document describes the design **as built**, and the task breakdown at the end records which PR delivered each piece.

Two v1 gaps remain open and are called out where they arise: the WASM `fix-check` host method exists but is not yet reachable from the `checkleft fix` CLI (§D), and the `fix` subcommand is not yet documented in the user-facing CLI reference (§E).

## Overview

`checkleft run` reports check failures; it never edits the tree. This design adds a companion `checkleft fix` subcommand that **automatically applies fixes** to the files a check is failing on. `fix` reuses `run`'s machinery to discover which files fail which checks, then drives each check's declared fix mechanism over its own failing-file set — a formatter's `--write`, a linter's `--fix`, a WASM check's fix entry point — writing the corrected bytes back to the real working tree.

The hard part is not invoking `prettier --write`; it is doing so **safely**, so that a fixer can only ever modify the files it was told to fix, never produces a partial write on error, and leaves the tree untouched when the fix fails. The chosen mechanism is a per-check **writable copy sandbox** with **atomic copy-back of only the changed files** — an airlock that makes "touch nothing outside the fixable set" a structural guarantee rather than a post-hoc check.

The safety core, the declarative fix tier, and the built-in `suggested_fix` tier are all live and wired into the CLI. The WASM/external tier's host and guest surfaces are complete but not yet connected to the subcommand.

## Goals

- Add `checkleft fix [PATHS…]` that applies fixes to every file failing a fixable check, as the write-side companion to `run`.
- Honor `--all` exactly like `run`: default change-scoped file set vs. full-repo scan.
- Add `--allow-dirty` (default **true**): whether it is acceptable to fix files that already have uncommitted modifications in the working tree. When false, do not fix already-dirty files (never clobber uncommitted work).
- Express the fix mechanism declaratively per check (a fix command in the check YAML) for the declarative tier, and as an SDK entry point for the WASM/external tier. Both are optional per check.
- Support batch and single-file fix modes, mirroring `run`'s `per_file`/`batch`.
- **Safety is the headline property:** a fix only ever writes files in its own fixable set; failures leave originals untouched; no partial writes.
- Provide a fix for as many pre-bundled checks as is feasible (all formatters, most linters).
- Deterministic, convergent results when multiple checks fix the same file.
- Re-verify after fixing and report what still fails (unfixable, or only partially fixed).
- Progress UI parity with `run` (fixing can be slow).

## Non-goals

- **Changing what `run` reports.** `run`'s output, exit semantics, and failing-file computation are unchanged; `fix` consumes them.
- **Interactive or partial fixing.** `fix` fixes _all_ failing fixable files by default. No per-hunk prompts, no "fix only finding #3". (Listed as `future / not a v1 blocker` in the task breakdown.)
- **Inventing fixes for checks that have none.** A failing check with no declared fix is a no-op for `fix` (reported as "no fix available"), never an error.
- **A general refactoring engine.** `fix` delegates to the check's own tool/logic; it does not implement fixes itself beyond applying the edits/writes those mechanisms produce.

## Starting-state audit

This section records the codebase **as it was before this work**, since the design's shape is largely a consequence of what already existed. Statements here about what "does not exist yet" describe the pre-`fix` tree; the Chosen approach sections below describe what replaced them.

Source: `tools/checkleft/src/main.rs`, `runner.rs`, `check.rs`, `output.rs`, `vcs.rs`, `config.rs`, `external/declarative/{mod,executor,resolve,selector}.rs`, `external/sandbox.rs`, `external/runtime.rs`, `external/component_bindings.rs`, `wit/check.wit`, `sdk/src/lib.rs`, `sdk-macro/src/lib.rs`, `bundled.rs`, and the `checks/{format,lint}/*.yaml` definitions.

### CLI / `run` dispatch (`main.rs`)

- Subcommands today: `Run(RunArgs)`, `List`, `ShowPlan` (temporary), `Install`, `Uninstall`. A bare `checkleft` is `run`.
- `RunArgs`: `--all: bool`, `--base_ref: Option<String>`, `--default_branch: Option<String>`, `--format human|json`, `--show_progress: Option<bool>`, plus `ConfigArgs { external_checks_file, external_checks_url }`. There is **no positional path arg** and **no `--allow-dirty`** today.
- `dispatch_run` resolves a `ChangePlan` from CI-env + overrides (`resolve_change_plan`), builds a `Runner`, resolves the `ChangeSet`, optionally wires a `LiveProgress` reporter, calls `runner.run_changeset_with_progress`, sorts results, renders human/JSON, and exits `1` iff any finding is `Severity::Error`.

### Runner orchestration (`runner.rs`)

- `Runner { registry, resolver, source_tree, external_package_provider, external_executor }`.
- `run_changeset_with_progress(changeset, reporter)` calls `schedule_runs(changeset)` to dedupe checks into `ScheduledCheckRun { configured_check_id, source_path, execution, policy, config, changeset }`, where `changeset` is **already filtered to the files that check applies to**.
- `ScheduledExecution` is one of `BuiltInConfigured { check }`, `BuiltInMissing`, `ExternalResolved { package }` (declarative **and** WASM both flow through `external_executor`), `Invalid`.
- Checks run concurrently in a `JoinSet`; built-ins via `ConfiguredCheck::run_with_progress`, externals via `spawn_blocking` → `ExternalCheckExecutor::execute_with_progress`.
- Each produces `CheckResult { check_id, findings: Vec<Finding> }`. `scope_findings_to_changeset` drops findings on files outside the changed set (a no-op under `--all`).
- **The failing-file set of a check is exactly the set of distinct `finding.location.path` values in its `CheckResult`** (filtered to error/warning as appropriate). This is the join point `fix` reuses — no new "which files fail" logic is needed.

### Check + finding model (`check.rs`, `output.rs`, `sdk/src/lib.rs`)

- `Finding { severity, message, location: Option<Location>, remediations: Vec<String>, suggested_fix: Option<SuggestedFix> }`.
- `SuggestedFix { description, edits: Vec<FileEdit> }`, `FileEdit { path: PathBuf, old_text: String, new_text: String }`.
- **These already exist** and are surfaced in the human renderer (`= fix: …`), but are currently a _latent, unapplied_ channel — no code writes them back. `fix` can adopt them as a fix source for built-in checks (see Chosen approach §F).
- The WIT contract (`wit/check.wit`) already defines `file-edit` and `suggested-fix` records and carries `suggested-fix: option<suggested-fix>` on `finding`. The guest SDK exposes `FileEdit`/`SuggestedFix` types. There is **no fix entry point** today and the guest FS preopen is **read-only**.

### Declarative executor (`external/declarative/`)

- A check is `ExternalCheckDeclarativePackage { applies_to, needs, invocations, skip_symlinks }`.
- `Invocation { id, kind }`; `InvocationKind::Tool(ToolInvocation { run, mode: InvocationMode (Batch|PerFile), args })` or `BazelAspect(...)`.
- Args are templated: `{{files}}` (batch → matched file list), `{{file}}` (per-file), `{{repo_root}}`, `{{config.KEY}}`. Argv is chunked under a 128 KiB threshold.
- `ExitSemantics { codes: BTreeMap<i32, ExitOutcome>, default }` with `ExitOutcome::{Ok, Findings, Error}`. `Error` must never be masked as clean.
- Invocations run with **cwd = repo root**, directly on real files; **declarative checks are not sandboxed today** (sandboxing "deferred by design"). The tool's stdout is parsed by a `transform` (`linelist | json | passthrough`) into findings. The tool is run in **check mode** (`--list-different`, `--check`, `-mode=check`) and never writes.
- The bundled snapshot (`bundled.rs`) embeds each YAML via `include_str!`, so adding fields to a YAML automatically flows into the compiled-in defaults.

### Sandbox (`external/sandbox.rs`)

- `create_sandbox(changeset, scope: AccessScope, source_tree, ceiling: &HostCeiling) -> Result<SandboxResult>`; `SandboxResult { root: TempDir, allowed_paths: Vec<PathBuf> }`.
- `AccessScope::{ModifiedOnly, WholeRepo, Globs(Vec<String>), ExplicitFiles(Vec<PathBuf>)}`.
- Populates the temp dir at repo-relative paths, **preferring `fs::hard_link` from the ceiling** (zero-copy) and falling back to `source_tree.read_file` (copies) for cross-filesystem / virtual-tree / symlink entries. Path normalization rejects `..` escapes; symlinks are always materialized via `SourceTree` (containment-checked).
- Today the sandbox is consumed **read-only** by the WASM runtime (`WasiCtxBuilder…preopened_dir(root, "/")`, read-only). **The hardlink fast path is the critical hazard for fix:** a hardlink shares an inode with the real file, so an in-place truncating write inside the sandbox would silently mutate the real file outside any copy-back control.

### WASM runtime + SDK (`external/runtime.rs`, `component_bindings.rs`, `wit/check.wit`, `sdk*`)

- Component Model is landed: `wasmtime::component::bindgen!` over `wit/check.wit`; world `check` exports `list-checks`, `run-check(name, input) -> result<list<finding>, check-error>`, plus optional `declare-required-files` / `declared-exclusions` / `evaluate-exclusion`.
- Epoch-based timeouts (base 5 s, +100 ms/file, host ceiling 5 min), `MemoryLimiter` (default 256 MiB, ceiling 512 MiB), AOT `.cwasm` cache. Phase-1 store (no preopen) does discovery; phase-2 store preopens the sandbox root read-only.
- Guest SDK: `#[check(name, description?, severity?, access_scope?, declared_exclusions?, evaluate_exclusion?, required_files?)]` + `export_checks!`. Author writes `fn(CheckInput) -> Vec<Finding>`. The `CheckEntry` trait (`__private`) is the host-facing dispatch surface; **it has no `fix` method**.

### VCS (`vcs.rs`)

- `Vcs { root, kind: Git|Jujutsu }`. `current_changeset` (working-tree vs HEAD), `changeset_since(base)` (merge-base diff), `all_files_changeset` (tracked files, for `--all`).
- **No explicit working-tree dirty query exists yet** — `fix` adds one (`git status --porcelain` / `jj` equivalent) for `--allow-dirty`.

## Alternatives considered (safety mechanism)

The contract proposes sandbox-copy-back and asks us to evaluate alternatives. Three were weighed.

### A. In-place fix + post-verify that only allowed files changed (rejected)

Run the fixer directly on the real tree (as `run` invokes tools today), snapshot the fixable files' pre-content first, then after the run diff the tree and assert nothing outside the fixable set changed; roll back from the snapshot on violation.

Rejected:

- **Rollback is best-effort, not atomic.** To roll back you must have snapshotted _every_ file the tool _might_ touch — but the whole risk is that the tool touches files you did not anticipate. You cannot snapshot the complement of a set you do not know.
- **A crash mid-write leaves real damage.** If the process dies after the tool has rewritten three files and before verification, the tree is in a half-fixed state with no airlock to discard.
- **Detection ≠ prevention.** "Assert afterward that nothing escaped" still _let the escape happen_; for a buggy formatter that rewrites a sibling import, the damage is already on disk.

### B. Per-check writable copy sandbox + atomic copy-back of changed files (CHOSEN)

Stage only the fixable files into a fresh temp dir (forced copies, never hardlinks), run the fixer with cwd = sandbox, detect which staged files changed, and atomically copy _only those_ back to the real tree. Detailed in Chosen approach.

Chosen because the safety property is **structural**: the fixer can write anywhere it likes _inside the sandbox_, but the host only ever copies back paths that were in the fixable set to begin with, and discards the entire sandbox on any error. "Touch nothing outside the fixable set" is then a property of the copy-back loop's domain, not of the tool's good behavior. It also reuses the existing `create_sandbox` infrastructure (`AccessScope::ExplicitFiles`, path-containment, `SourceTree` materialization) almost verbatim — the only new primitive is "force copy, no hardlink" plus the copy-back.

### C. One shared sandbox for the whole run (rejected as the default)

Stage every fixable file for every check into a single sandbox, run all fixers, copy back once.

Rejected as the unit of isolation:

- **Cross-check interference.** Two checks fixing the same file in one shared dir race or clobber each other; ordering (lint-before-format, §6) can no longer be enforced by sequencing copy-backs.
- **Coarse failure blast radius.** If one fixer errors, you must decide the fate of every other check's staged edits at once.
- A per-check (more precisely, per-fix-invocation) sandbox keeps each fixer's blast radius to its own files and lets the scheduler order and serialize overlapping fixers cleanly. (We _do_ reuse one sandbox across the files of a single check's batch invocation — that is the batch, not the cross-check, case.)

## Alternatives considered (WASM fix shape)

### W1. Guest returns edits; host applies them (CHOSEN)

Add a `fix-check(name, input) -> result<list<file-edit>, fix-error>` export. The guest reads its (read-only) sandbox, computes `file-edit` records (the record type **already exists** in the WIT), and returns them. The host validates every `edit.path ∈ fixable set` and applies edits to real files through the same atomic write path as the declarative tier.

Chosen because: it needs **no new write capability** in the WASI sandbox (guest FS stays read-only — a smaller trust surface); it reuses the `file-edit`/`suggested-fix` types already in `wit/check.wit` and the SDK; "only touch fixable files" is enforced by the host before a single byte is written; and it matches the ergonomic the SDK already implies (pure function over `CheckInput`). It is also testable without a filesystem.

### W2. Writable sandbox preopen; guest writes files; host copies back (alternative)

Give the fix invocation a **writable** WASI preopen (the forced-copy sandbox from approach B) and let the guest mutate files with `std::fs::write`; the host then copy-backs changed files exactly like the declarative tier.

This is the literal reading of the contract's "capability for the wasm check to write the (sandboxed) file(s)." It is fully supported by the same safety core (B) and is the right escape hatch for a guest that wraps native formatting logic which only knows how to rewrite a file in place. It is not the v1 shape because it widens the guest capability (WASI write) for no benefit over W1 in the typed-Rust-check sweet spot, and edits are easier to validate, log, and unit-test than opaque file writes.

**Resolved: W1 only.** The design originally proposed shipping W1 as the default _and_ W2 as an opt-in. In the end W2 was not implemented at all — every check that wanted a fix was served by W1, and a second write path with no consumer is surface without a user. W2 stays available as a future addition (D4) since it needs no change to the safety core.

## Chosen approach

### A. CLI surface and relationship to `run`

```
checkleft fix [PATHS…] [--all] [--allow-dirty[=true|false]]
              [--base_ref <ref>] [--default_branch <name>]
              [--format human|json] [--show_progress[=BOOL]]
              [--verify[=BOOL]] [--max-passes <n>]
```

The new flags ship in clap's kebab-case (`--allow-dirty`, `--max-passes`); the underscore spellings used in earlier drafts of this document were never the real surface. `FixArgs` flattens `RunArgs`, so every `run` flag is accepted unchanged.

- **Discovery shares `run`.** `fix` resolves the same `ChangePlan` (honoring `--all`, `--base_ref`, `--default_branch`, and `PATHS…`), builds the same `Runner`, and calls the existing run path to obtain `Vec<CheckResult>`. For each check, the **failing-file set** is the distinct `finding.location.path` values whose severity is `Error` or `Warning` (info findings are advisory and not fixed). No new per-check applicability/failure logic is introduced.
- **`PATHS…`** further intersect the candidate set with the given paths (a convenience for "just fix this dir"); absent, behavior matches `run`.
- **Apply phase.** For each check with a declared fix and a non-empty failing set, schedule a _fix run_ (§B). Checks with no declared fix are recorded as "no fix available" and skipped.
- **Output** (`--format human` default). The design sketched three result buckets; implementation found a fourth worth separating, so the human renderer reports five states per check:
  - **Fixed:** the files written (count + list).
  - **Still failing:** `Error`-severity findings that survived fix + re-verify (unfixable check, or partially fixed).
  - **Warnings remain (non-blocking):** warning-only residue, split out from "still failing" because it does not affect the exit code and reads very differently to a user.
  - **No fix available:** checks that failed but declare no fix, listing their files under "needs manual fix".
  - **Skipped (dirty):** files left alone under `--allow-dirty=false` (§E), surfaced as a footer count.
  - A summary footer — `N file(s) fixed, M still failing, K error(s), J check(s) with no fix available[, D skipped (dirty)] (in Xs)` — matching `run`'s footer style, plus an explicit `(--verify=false; post-fix state unknown)` marker when verification was skipped.
- **JSON output** is a distinct schema rather than a literal mirror of the human buckets, because the per-invocation detail has no good human rendering:

  ```json
  {
    "verify_ran": true,
    "checks": [
      {
        "check_id": "format/oxc",
        "failing_files": [],
        "dirty_skipped": [],
        "fix_status": "executed",
        "invocations": [
          { "invocation_id": "format", "applied": [], "per_file_errors": [], "error": null }
        ],
        "distinct_applied_files": [],
        "still_failing_after_verify": []
      }
    ]
  }
  ```

  `fix_status` is `executed` or `no_fix_available`. A check that only surfaces during verification (a _different_ check now failing on a file we just fixed) is appended with `fix_status: "no_fix_available"` and an empty `failing_files`. Note that `error` can co-exist with a non-empty `applied` list: that is the copy-back first-error-stop case (§B step 6), where some files were validly written before the failure.

- **Exit code.** `0` when, after fixing and re-verifying, **no `Error`-severity finding remains**; `1` otherwise. Three sources feed that decision: residual errors from the verify pass over applied files, original errors on files that were never applied (nothing fixed them, so their findings stand), and fixer operational errors. This resolved the open question in favour of `run`-consistency — the "auto-fix bot exits 0 whenever it applied something" alternative was not taken, because a green exit on a tree that still has errors is a worse default for the CI use case that motivated the flag.

### B. Safety core — writable copy sandbox + atomic copy-back

This is the load-bearing mechanism; every fix (declarative tool, WASM W2, and even the apply step of W1/§F) funnels through it.

For each fix invocation over a fixable set `F` (repo-relative paths):

1. **Compute `F`** = (failing files of the check) ∩ (`applies_to`) ∩ (`--allow-dirty` filter, §E) ∩ (`PATHS…`). If `F` is empty, the invocation is a no-op.
2. **Stage a writable sandbox** containing exactly `F`, via `AccessScope::ExplicitFiles(F)` and a `CopyMode` selector so files are always `fs::copy`'d, **never hardlinked** (hardlinks share inodes — an in-place write would escape copy-back; see audit). `CopyMode { PreferHardlink, ForceCopy }` threads through a new `create_sandbox_with_mode`; the original `create_sandbox` keeps its signature and delegates with `PreferHardlink`, so the read-only check path is untouched. Record, for each staged file, a **pre-fix content hash**. The hash is **SHA-256**, not blake3 as earlier drafts suggested — `sha2` is already a workspace dependency and the hash is only ever compared for equality, so there was no reason to add a crate for it. File mode is _not_ captured at stage time; it is read from the real target at copy-back, which is equivalent here because copy-back only ever overwrites a still-original file in place.
3. **Run the fixer** with cwd = sandbox root:
   - _Declarative:_ the fix invocation's tool + args (§C), batch or per-file, files passed as sandbox-relative paths via `{{files}}`/`{{file}}`.
   - _WASM W2:_ preopen the sandbox **writable**, call `fix-check`.
   - _WASM W1 / suggested_fix (§F):_ the "fixer" produces `file-edit`s; apply them to the staged sandbox copies.
4. **Classify the result** via the fix's exit/`fix-error` semantics (§C/§D). On `error` → **abort: drop the sandbox, real tree untouched.** Report an `Error` finding for the check.
5. **Detect changed files**: re-hash every staged file; the changed set `C` = files whose hash differs. **Enforce the airlock:** `C ⊆ F` by construction (only `F` was staged and only staged paths are walked); any file the fixer _created_ in the sandbox outside `F` is simply never enumerated for copy-back and dies with the temp dir. Copy-back additionally refuses a non-staged path defensively, so the invariant does not rest on the enumeration alone. Newly-created paths and deletions inside the sandbox are logged but not propagated (a fixer must not create/delete files; doing so is reported, not applied).
6. **Atomic copy-back** of each `c ∈ C`: write the new bytes to a temp file in the **same directory** as the real target (same filesystem → atomic `rename`), preserving mode, then `rename` over the target. Per-file atomicity is guaranteed by POSIX `rename`. Across multiple files there is no kernel multi-file transaction; we copy back in a deterministic order and, **on the first copy-back I/O error, stop and report exactly which files were applied** (the already-renamed ones are valid, complete files — never half-written). The sandbox is dropped last.

**Failure handling guarantees:**

- A fixer that exits error → zero writes to the real tree.
- A crash during the fixer run → the real tree is untouched (all work was in the sandbox).
- A crash during copy-back → each individual file is either its original or its fully-fixed version (atomic rename), never a partial mix within a file.

### C. Declarative fix-command schema

The fix is expressed as an **optional `fix` block on an invocation**, backward-compatible (absent ⇒ that invocation has no fix). It mirrors the existing `ToolInvocation` shape so the same binary resolution (`needs`), templating, and chunking apply.

```yaml
invocations:
  - id: format
    run: oxfmt
    mode: batch
    args: ["--list-different", "{{files}}"] # unchanged CHECK invocation
    exit: { "0": ok, "1": findings, "2": findings, default: error }
    transform: { kind: linelist, message: "file needs oxfmt formatting" }
    fix: # NEW — optional
      # `run` defaults to the invocation's `run` (same binary); override if needed.
      mode: batch # batch | per_file; defaults to the invocation's mode
      args: ["--write", "{{files}}"] # FIX args (write/fix flags)
      # Exit semantics for the FIX run. Outcomes: `ok` (fix applied / nothing to do)
      # or `error` (fix failed → abort, no copy-back). Defaults: 0 => ok, else error.
      exit: { "0": ok, default: error }
```

Schema details:

- **`fix.run`** (optional): binary key into `needs`; defaults to the invocation's `run`. (Almost always the same tool with different args.)
- **`fix.mode`**: `batch` (one process over `{{files}}`) or `per_file` (one process per `{{file}}`); defaults to the invocation's `mode`. Per-file fix isolates a bad file (one file's fix error does not abort the batch), matching `run`'s per-file isolation.
- **`fix.args`**: templated like check args (`{{files}}`, `{{file}}`, `{{repo_root}}`, `{{config.KEY}}`). Convention: the fix args are the check args with the report flag swapped for the write flag (`--list-different`→`--write`, `--check`→(removed), `-mode=check`→`-mode=fix`).
- **`fix.exit`**: `ExitOutcome` reduced to `{Ok, Error}` (a fix has no "findings"). Default `0 ⇒ ok`, else `error`. A formatter's `--write` exits 0 on success; a linter's `--fix` may exit non-zero when _unfixable_ diagnostics remain — that is **not** a fix error (the fixable ones were still applied); model this by mapping the linter's "fixed-but-residual" code to `ok` and letting the post-verify (§G) report the residue. Per-check exit maps make this explicit.
- **What counts as a successful fix:** exit maps to `ok` **and** the sandbox airlock + copy-back complete without I/O error. Whether the file ended fully clean is decided by §G re-verify, not by the fixer's exit alone.
- `bazel_aspect` invocations (clippy) have **no `fix` block** in v1 (see §H, lint/rust).

The bundled YAMLs gain `fix` blocks; because `bundled.rs` embeds them via `include_str!`, the compiled-in defaults update automatically.

**Consequence surfaced during implementation — config discovery under `cwd = sandbox`.** The check path runs tools with `cwd = repo root`, so a tool that discovers its own config (`.prettierrc`, `biome.json`, `.oxfmtrc`) finds it by walking up from the working directory. The fix path deliberately runs with `cwd = sandbox root`, where only the fixable files are staged — no repo-root config is present. A fix block for a config-discovering tool must therefore pass its config explicitly, e.g. `--config {{repo_root}}/…`, or the fixer will silently format against tool defaults. `format/rust` (`--config-path={{repo_root}}`) and `lint/js` (`--no-config-lookup --config {{config.config_file}}`) do this; `format/prettier`, `format/biome`, and `format/oxc` currently do not, which is a live gap for any repo whose config differs from the tool default. The alternative — staging discovered config files into the sandbox alongside `F` — was not pursued in v1 because it widens the staged set beyond the fixable set and so weakens the airlock's simple `C ⊆ F` story.

### D. WASM/external fix entry point

Add to `wit/check.wit` world `check`:

```wit
variant fix-error { unknown-check(string), failed(string), not-fixable }
export fix-check: func(name: string, input: check-input) -> result<list<file-edit>, fix-error>;
```

**W1 shipped; W2 was not built.** The open question resolved to "W1 only for v1" rather than "W1 default with W2 opt-in": once W1 was working, no bundled or example check wanted an in-place write, so shipping a second, wider-capability path with no consumer was unjustified. W2 remains recorded as D4 in the deferred list, and the §B safety core would support it unchanged if a guest ever needs it.

- **SDK:** `#[check(...)]` takes an optional `fix = fn_name` argument. The fixer may be either `fn(CheckInput) -> Vec<FileEdit>` or `fn(CheckInput) -> Result<Vec<FileEdit>, String>`; a `FixOutcome` enum and an `IntoFixOutcome` trait normalize the two shapes, which is a small surface addition the original design did not anticipate but which keeps the ergonomic honest for infallible fixers. `CheckEntry::fix` has a default returning `FixOutcome::NotFixable`, so a check without `fix = …` is a no-op for `fix` and needs no changes.
- **Runtime invocation:** the host reuses the existing phase-2 component instantiation with the sandbox preopen **read-only** — W1 needs no guest write capability at all. The guest returns `file-edit`s; the host validates each `edit.path` through `validate_relative_path` and membership in the staged set, applies them to the staged sandbox copies, then copy-backs (§B).
- **Edit application is strict.** An edit whose path is outside `F` is an **airlock violation and aborts the whole fix** — the tree is left untouched rather than the edit being quietly dropped. Likewise a stale `old_text` (not found in the file) is a hard error, and an empty `old_text` is rejected (pure insertion is unsupported; an edit must anchor to existing text). Matching is first-occurrence `replacen`. This is deliberately less forgiving than the built-in `suggested_fix` path (§F), which filters out-of-set edits silently: a WASM guest is external code whose misbehaviour we want to surface loudly, whereas a built-in check emitting an out-of-scope edit is more likely a scoping artifact than a bug.
- **v1 read-scope narrowing.** The guest sees only the staged fixable set `F`, not its full declared `access-scope`. A fixer that needs broader read context to compute its edits is out of scope for v1.

**Outstanding: the host method is not wired to the CLI.** `fix_component_check` exists and is tested, but nothing in `dispatch_fix` calls it — only the declarative and `suggested_fix` tiers are wired into the apply phase. A WASM check declaring `fix = fn` therefore compiles, exports `fix-check`, and is never invoked by `checkleft fix`. Closing this is a small piece of orchestration work, not a design question.

### E. `--allow-dirty` (default true)

- **Dirty detection.** `Vcs::dirty_paths() -> Result<HashSet<PathBuf>>`: `git status --porcelain` (paths with worktree modifications, staged or unstaged, plus untracked) for Git; `jj diff --summary` for Jujutsu, reusing the existing summary parser. Repo-relative, normalized to match changeset paths. Both backends insert **both sides of a rename** — a rename dirties its old path as well as its new one, which the original design did not specify but which is the only safe reading of "don't touch uncommitted work".
- **`--allow-dirty=true` (default):** dirty files are eligible to be fixed. This is the common local workflow ("I have uncommitted edits; format them").
- **`--allow-dirty=false`:** dirty files are subtracted from every check's fixable set `F` before staging, so no dirty file is ever staged or written. The partition happens inside `compute_fix_plan`, which keeps a check visible in the output even when _all_ of its files were dirty — otherwise a check would silently vanish and the user would have no idea why nothing happened. Skipped-because-dirty files are reported distinctly from "no fix available".
- **Failure of the dirty query is currently non-fatal:** if `dirty_paths()` errors, the code treats nothing as dirty and proceeds to fix. For a flag whose entire purpose is "don't clobber uncommitted work", failing open is the wrong default; failing closed (or aborting) would better match the flag's intent. Worth revisiting.
- **Interaction with `--all`:** `--all` only widens the _candidate_ set (full repo vs. change-scoped); the dirty filter is applied identically afterward. Note a deliberate consequence: in **local change-scoped mode** the failing set _is_ the working-tree diff, i.e. dirty files, so `--allow-dirty=false` there fixes essentially nothing — which is the point: it is the mode for "only fix already-committed/clean files" (e.g. a CI auto-fix bot on a clean checkout) without touching a human's in-flight edits.

This behaviour — and the `fix` subcommand generally — is **not yet covered in `userdoc/docs/cli.md`**; the user-facing CLI reference still documents only `run`. That documentation is outstanding.

### F. Built-in checks and `Finding.suggested_fix`

Built-in (Rust) checks have neither a declarative `fix` block nor a WASM entry point, but `Finding.suggested_fix: Option<SuggestedFix>` **already exists** as an unapplied edit channel. `fix` adopts it as a third fix source: `Runner::apply_suggested_fixes` collects `FileEdit`s from `Error`/`Warning` findings (Info is advisory and never fixed), restricts them to the check's fixable set, and runs them through the §B apply+copy-back path. It runs after the declarative pass and fills only fix-plan entries the declarative tier left empty, so a check with both sources prefers the declarative one.

Edits outside the fixable set are **silently filtered** here rather than aborting, which is intentionally the opposite of the WASM path's hard failure (§D): a built-in check runs in-process and its out-of-scope edits are far more likely a changeset-scoping artifact than misbehaviour.

**Edits are positionally targeted, not first-occurrence.** The initial implementation used a plain first-occurrence `replacen`, which is wrong whenever `old_text` is a short snippet appearing several times in a file — it would repair the wrong line. Application now sorts a file's edits bottom-up by the finding's line number and applies each at its reported position, so earlier edits do not shift later offsets; an edit whose `old_text` is ambiguous and which carries no position is refused rather than guessed at. A `fixable: bool` field was also added to `Finding` alongside this work, so consumers can tell a fixable finding from an advisory one without inspecting `suggested_fix`.

**No built-in check populates `suggested_fix` yet.** Every check in `src/checks/` still emits `suggested_fix: None`, so this tier is wired but inert — `checkleft fix` applies no built-in fixes today. Populating it per check is incremental follow-up (D2), not framework work.

### G. Ordering, convergence, concurrency, verification

- **Deterministic cross-check order (same file fixed by multiple checks).** When a file is in the fixable set of more than one check, fixes are applied in a fixed category order: **lint-fix before format-fix, format-fix last.** Rationale: a linter's `--fix` can insert or rewrite code (producing unformatted output), so formatting must run last to normalize it; the reverse can leave a formatted file un-formatted again. The shipped ordering is a `FixCategory { Lint, Other, Format }` enum ordered `Lint < Other < Format`, derived from the check id's prefix; the explicit `Other` middle tier gives non-lint, non-format checks a defined slot instead of falling into one of the two named ones by accident. Within a category, order is stable by check id. (Concretely for a `.ts` file: `lint/oxc --fix` → `format/oxc --write`.)
- **Concurrency.** A conflict graph keyed by fixable-file overlap is built with union-find over check indices: any two checks sharing a file land in the same component, and each connected component becomes a `FixGroup` whose checks are ordered by `(category, check_id)`. Groups are pairwise file-disjoint by construction and run **concurrently** via rayon (independent sandboxes, independent copy-backs); checks _within_ a group run **serially**, each re-staging from disk so the second check sandboxes the output of the first. This makes concurrent fixes provably safe (concurrent ⇒ disjoint).
- **Convergence.** `--max-passes <n>` re-runs the ordered fix pipeline, stopping early as soon as a pass writes nothing. **The default is 10, i.e. fixpoint-by-default with a hard cap**, resolving the design's open question against the originally-proposed single pass. The single-pass default actually shipped first and was reverted: a formatter needing two passes left files in an intermediate state that the verify pass then reported as "still failing", which is a confusing failure for something `fix` could simply have finished. A compile-time assertion now guards the default at `>= 2` so it cannot regress to one. The hard cap still prevents oscillation between two non-converging fixers.
- Each pass re-runs the **full original fix plan** rather than narrowing to files that changed in the previous pass. This is more I/O than the design's "re-run on files that still change", but termination is driven by the change detection in §B rather than by the narrowing, so the simpler formulation is correct and was not worth optimizing before there was evidence it mattered.
- **Verification / idempotency (`--verify`, default on).** After fixing, the check logic is re-run and residual findings are reported as "still failing." Verification is scoped to the files that were **actually written**, not the whole originally-failing set: re-checking a file nothing touched can only reproduce the finding we already have. Files that were never applied keep their original findings, and those feed the exit code through a separate path (§A), so nothing is lost by the narrower re-run. `--verify=false` skips it for speed, and the output says so explicitly since the post-fix state is then unknown.

### H. Per-check fix coverage

| Check             | Tier                       | Fix mechanism                                                        | v1        |
| ----------------- | -------------------------- | -------------------------------------------------------------------- | --------- |
| `format/oxc`      | declarative                | `oxfmt --write {{files}}` (batch)                                    | ✅        |
| `format/prettier` | declarative                | `prettier --write --ignore-unknown {{files}}` (batch)                | ✅        |
| `format/biome`    | declarative                | `biome format --files-ignore-unknown=true --write {{files}}` (batch) | ✅        |
| `format/rust`     | declarative                | `rustfmt --config-path={{repo_root}} {{file}}` (per_file)            | ✅        |
| `format/bazel`    | declarative                | `buildifier -mode=fix {{files}}` (batch)                             | ✅        |
| `lint/oxc`        | declarative                | `oxlint --fix --no-error-on-unmatched-pattern {{files}}` (batch)     | ✅        |
| `lint/biome`      | declarative                | `biome lint --files-ignore-unknown=true --write {{files}}` (batch)   | ✅        |
| `lint/js`         | declarative                | `eslint --no-config-lookup --config … --fix {{files}}` (batch)       | ✅        |
| `lint/bazel`      | declarative                | `buildifier -lint=fix -mode=fix {{files}}` (batch)                   | ✅        |
| `lint/rust`       | declarative (bazel_aspect) | `clippy --fix` — **not feasible in v1**                              | ❌ future |

All nine fix blocks shipped as planned. The linters' exit maps encode the §C nuance directly: exit `1` (fixes applied, unfixable diagnostics remain) maps to `ok`, `2` and above to `error`.

`lint/js` was **kept** rather than dropped, resolving the open question about its impending replacement by `lint/oxc` (#1619): the check is still live, and a fix block for it costs one YAML stanza. It is also the only linter whose fix block passes its config explicitly, so unlike the prettier/biome/oxc formatters it is not exposed to the config-discovery gap described in §C.

`lint/rust` is **not auto-fixed in v1**: clippy is run via the rules*rust \_aspect* (artifact read), not a direct invocation, and `cargo clippy --fix` requires a cargo-driven, writable, single-version workspace and rewrites whole crates (well outside the changeset-scoped fixable set). Deferred (`future / not a v1 blocker`); failing `lint/rust` is reported as "no fix available."

**Not auto-fixable (semantic / intentional no-fix), reported as "no fix available":** `file/size`, `todo-expiry` (`todo_expiry.rs`), `no-usfa-typo` (`typo.rs`)*, `repo_visibility`, `forbidden_imports_deps`, `frontend_no_legacy_api`, `rust_test_rule_coverage`, `workflow_action_version` / `workflow_run_patterns` / `workflow_shell_strict`, `code_patterns`, `api-breaking-surface`, `link-integrity`, `ifchange-thenchange`. (*Several of these — `no-usfa-typo` especially — _could_ emit a `suggested_fix` later via §F; that is future per-check authoring, not a framework change.)

### I. Progress UI integration

`fix` reuses the existing `ProgressReporter` / `LiveProgress` / `TermRenderer`, with a **single reporter instance shared across the discovery and apply phases** so the display is continuous rather than torn down and rebuilt between them. The apply phase registers every check up front (`register(check_id, file_count)`), then per check emits `start` → `record_progress` → `finish(check_id, error_count, elapsed)`. Auto-enabled on an interactive TTY, off for pipes/CI/`--format json`, identical detection to `run`. The final footer is the §A summary.

Tick granularity differs by fix mode, and only per-file mode matches the design's "tick per file fixed": a **batch** invocation ticks once when the whole invocation completes, jumping the counter by the chunk's file count. A batch formatter over many files therefore shows no intra-batch movement. Finer granularity would require the tool to report per-file progress, which none of the bundled formatters do.

The verify pass runs under a no-op reporter so its check runs do not redraw over the apply phase's completed display.

## Decisions taken during implementation

Four questions were left open by the original design. All four are now settled:

- **WASM fix shape → W1 only.** The plan was W1 as default with W2 as an opt-in; W2 was ultimately not built, because no check needed it and an unused write path is pure surface. See §D.
- **Convergence default → fixpoint with a cap of 10.** Single-pass shipped first and was reverted after a two-pass formatter left files half-fixed and reported as failing. See §G.
- **Exit code → `run`-consistent.** `fix` exits 1 if any `Error` survives fix + verify; the friendlier "exit 0 if we applied anything" behaviour was rejected. See §A.
- **`lint/js` → kept.** Its fix block shipped despite the pending `lint/oxc` migration. See §H.

## Known gaps and residual risks

- **WASM fix is unreachable from the CLI.** `fix_component_check` is implemented and tested but has no caller in the apply phase, so `fix = fn` on a WASM check is a functional no-op end to end (§D).
- **`fix` is undocumented for users.** `userdoc/docs/cli.md` covers only `run` (§E).
- **Formatter config discovery under `cwd = sandbox`.** `format/prettier`, `format/biome`, and `format/oxc` fix blocks pass no explicit config, so they may fix against tool defaults rather than repo config (§C).
- **`--allow-dirty=false` has no end-to-end test.** The pure partition function is unit-tested, but nothing verifies that `Vcs::dirty_paths()` actually detects git/jj dirt or that a dirty file is never staged by the real dispatch path — which is the guarantee §E actually makes.
- **`dirty_paths()` fails open**, treating a VCS query error as "nothing is dirty" (§E).
- **Forced-copy cost.** Fix forfeits the hardlink fast path (correctness requires copies). For large `--all` fixes this is more I/O than `run`; still unmeasured, and the full-plan-replay convergence loop (§G) multiplies it by the pass count.
- **`fix.run` override surface.** Defaulting `fix.run` to the check's `run` covered every bundled tool; no bundled check needed a different binary for fix vs. check, so the override remains unexercised.
- **Renames / generated files.** A fixer that wants to _rename_ or _create_ a file is intentionally unsupported by copy-back (only in-place content edits propagate). No bundled check needed it.

## Implementation, as delivered

The work landed in twelve PRs, following the planned dependency order without resequencing. Each task shipped as scoped; the divergences are described in the sections above rather than repeated here.

| Task | Scope                                                                                                                                            | PR                                                  |
| ---- | ------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------- |
| T1   | `Fix(FixArgs)` subcommand, `dispatch_fix`, per-check failing-set computation; apply phase landed as a dry plan so discovery was reviewable alone | [#1621](https://github.com/spinyfin/mono/pull/1621) |
| T2   | Safety core: `CopyMode::ForceCopy` staging, pre-fix hashing, change detection, `C ⊆ F` airlock, atomic same-dir-rename copy-back                 | [#1623](https://github.com/spinyfin/mono/pull/1623) |
| T3   | Declarative `fix` block schema (parse + validate), `FixExitOutcome` reduced to `{Ok, Error}`                                                     | [#1622](https://github.com/spinyfin/mono/pull/1622) |
| T4   | Declarative fix executor; replaced T1's dry plan for declarative checks                                                                          | [#1626](https://github.com/spinyfin/mono/pull/1626) |
| T5   | `fix` blocks for all nine bundled declarative checks                                                                                             | [#1624](https://github.com/spinyfin/mono/pull/1624) |
| T6   | `Vcs::dirty_paths()` (git/jj) and `--allow-dirty` threading                                                                                      | [#1625](https://github.com/spinyfin/mono/pull/1625) |
| T7   | Union-find conflict graph, category ordering, rayon group concurrency, `--max-passes` convergence                                                | [#1629](https://github.com/spinyfin/mono/pull/1629) |
| T8   | `--verify` re-run, human + JSON output, exit code                                                                                                | [#1628](https://github.com/spinyfin/mono/pull/1628) |
| T9   | WIT `fix-check`/`fix-error`, SDK `fix = fn`, host `fix_component_check` (not yet CLI-wired)                                                      | [#1627](https://github.com/spinyfin/mono/pull/1627) |
| T10  | Built-in fix via `Finding.suggested_fix`                                                                                                         | [#1631](https://github.com/spinyfin/mono/pull/1631) |
| T11  | Progress UI for the apply phase                                                                                                                  | [#1630](https://github.com/spinyfin/mono/pull/1630) |
| T12  | Safety + behaviour test suite                                                                                                                    | [#1632](https://github.com/spinyfin/mono/pull/1632) |

T12 covers six of its seven target properties with real tests: sandbox-escape containment, no-fix no-op, abort-leaves-originals-byte-identical, copy-back first-error-stop, deterministic lint→format ordering, and idempotency. The seventh — `--allow-dirty` true vs. false — is only unit-tested at the partition-function level and lacks end-to-end coverage (see Known gaps).

**Deferred / not a v1 blocker (recorded so the rejection set is explicit):**

- **D1. `lint/rust` (clippy) auto-fix.** Needs a cargo-driven `clippy --fix` outside the bazel-aspect model and whole-crate rewrites; out of scope for changeset-scoped fixing. Effort: large.
- **D2. Per-built-in `suggested_fix` authoring** (e.g. `no-usfa-typo` corrected spelling). Framework support shipped in T10, but no built-in check populates the channel yet, so this tier is inert until one does. Effort: small each.
- **D3. Interactive / partial fixing** (per-hunk, per-finding selection). Explicit non-goal for v1. Effort: medium.
- **D4. W2 writable-sandbox WASM fix.** Not built — v1 shipped W1 only (§D). The safety core would support it unchanged if a guest needs in-place writes. Effort: medium.
- **D5. Rename/create-file fixes** via copy-back. Unsupported by design in v1; revisit only if a real check needs it. Effort: medium.
- **D6. Declarative-runtime adoption of the sandbox for _checks_ (not just fix).** Orthogonal hardening, separate project. Effort: large.
