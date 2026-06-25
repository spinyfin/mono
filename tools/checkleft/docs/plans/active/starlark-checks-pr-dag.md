# Starlark Checks PR DAG

Each PR in this DAG must be independently compilable and testable. A child PR may depend on its parent, but no PR should contain code that only compiles after a later PR lands.

## PR 1: Core Evaluator + Text Context

Depends on: none

Scope:
- Add the Meta `starlark` Rust dependency and Bazel lockfile updates.
- Add `src/starlark/` with an isolated `StarlarkCheckRunner`.
- Evaluate one Starlark check source against a text evolution context.
- Provide hermetic globals for `check_meta`, `finding`, `fail`, `fail_but_overridable`, `Severity`, `regex_match`, `regex_find_all`, and `glob_match`.
- Map Starlark findings to existing `crate::output::Finding`.
- Keep discovery, package manifests, load paths, fixes, and non-text adapters out of scope.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

## PR 2: Package Manifest + Directory Discovery

Depends on: PR 1

Scope:
- Parse `checkleft/package.toml`.
- Discover local checks from `checkleft/<adapter>/<public|private>/<name>/check.checkleft`.
- Validate unknown adapters, invalid visibility directories, missing package manifests, and invalid `.checkleft` placement.
- Add changeset-scoped ancestor discovery without full-repo walking.
- Return discovered checks without yet wiring automatic runner execution.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

## PR 3: Load Resolution

Depends on: PR 2

Scope:
- Implement `load("//lib/name", ...)` and `load(":helper", ...)`.
- Enforce package-local and check-local load boundaries.
- Reject dependency-style `@dep//` imports.
- Add tests for successful loads and boundary violations.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

## PR 4: Runner Integration for Local Text Checks

Depends on: PR 3

Scope:
- Wire discovered local Starlark checks into the existing runner path.
- Filter checks by `applies_to` before evaluation.
- Preserve existing Rust, declarative, and WASM check behavior.
- Emit configuration/runtime failures as checkleft findings or errors according to the spec.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

## PR 5: Adapter Registry + Shared Adapter Output

Depends on: PR 4

Scope:
- Introduce `FormatAdapter` and `AdapterRegistry`.
- Move the text context builder behind the `text` adapter.
- Group applicable checks by adapter and share parsed output for each adapter/file-set pair.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

## PR 6A: module_json Adapter

Depends on: PR 5

Scope:
- Add typed `module_json` parsing, diffing, and Starlark context values.
- Add focused adapter and Starlark policy tests.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

## PR 6B: Java Adapter

Depends on: PR 5

Scope:
- Add Java public API extraction using `tree-sitter-java`.
- Add Java delta modeling and Starlark context values.
- Add focused adapter and Starlark policy tests.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

## PR 6C: Proto Adapter

Depends on: PR 5

Scope:
- Invoke `protoc` for base/current descriptor sets.
- Add descriptor wrappers, schema deltas, and the proto Starlark context.
- Add focused adapter and Starlark policy tests.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

## PR 7: Fix Evaluation

Depends on: PR 4 and adapter PRs as needed by tests

Scope:
- Evaluate `fix.checkleft`.
- Map Starlark `FileEdit` values to existing fix scheduler types.
- Carry typed `fix_data` from check findings into fix evaluation.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

## PR 8: Versioned Distribution

Depends on: PR 4

Scope:
- Resolve `path://`, `git://`, and `registry://` dependencies.
- Generate and verify `PACKAGE.lock`.
- Enforce public/private visibility for consumed packages.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

## PR 9: Functional Testing CLI

Depends on: PR 4

Scope:
- Add `checkleft test [check_id] [--update]`.
- Run fixture-based `testdata/<case>/` tests with `before/`, current files, `expected.toml`, and optional expected fixes.
- Keep tests hermetic and independent.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`
