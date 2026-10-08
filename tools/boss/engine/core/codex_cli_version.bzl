"""Single source of truth for the Codex CLI version pinned for compatibility tests.

Consumed by codex_test_archives.bzl's module extension (to select the
checksum-pinned release binaries the driver's config-compatibility test runs
against — MODULE.bazel cannot `load()` this file directly) and, via
`rustc_env` + `env!("CODEX_CLI_VERSION")`, by the Rust code (driver's
config_compatibility_tests and engine/core's PINNED_CODEX_CLI_VERSION) that
asserts the installed CLI matches it.

Bump procedure:
1. Update CODEX_CLI_VERSION below.
2. Update sha256 (CLI) and host_sha256 (code-mode host) for each platform in
   codex_test_archives.bzl to the new release's published checksums.
3. Re-run the driver's config_compatibility_tests and engine/core's
   conformance tests (version_pin, guard_conformance) against the new binary;
   both targets receive the pinned release through `BOSS_TEST_CODEX`, so a
   plain `bazel test //tools/boss/engine/driver:driver_test
   //tools/boss/engine/core:engine_lib_test` exercises it. Update fixtures if
   behavior changed. Also run the opt-in network target
   `bazel test //tools/boss/engine/core:codex_guard_live_test
   --test_env=BOSS_CODEX_AUTH_SOURCE=<auth.json>`; the ordinary test targets
   do not execute its live model probes.
4. Drive the bare TUI live (the driver screen-scrapes it) and re-measure the
   pane-monitor markers, the interrupt path and the hook block path; see
   tools/boss/docs/investigations/codex-0.160.1-qualification-2026-10-06.md
   for the harness and the checklist.
Do not bump only one consumer — a version pin that isn't reflected everywhere
silently reintroduces the test/production divergence this pin exists to
prevent.
"""

CODEX_CLI_VERSION = "0.160.1"
