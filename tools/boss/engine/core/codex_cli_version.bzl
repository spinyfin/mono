"""Single source of truth for the Codex CLI version pinned for compatibility tests.

Consumed by codex_test_archives.bzl's module extension (to select the
checksum-pinned release binaries the driver's config-compatibility test runs
against — MODULE.bazel cannot `load()` this file directly) and, via
`rustc_env` + `env!("CODEX_CLI_VERSION")`, by the Rust code (driver's
config_compatibility_tests and engine/core's PINNED_CODEX_CLI_VERSION) that
asserts the installed CLI matches it.

Bump procedure:
1. Update CODEX_CLI_VERSION below.
2. Update the sha256 for each platform entry in codex_test_archives.bzl to the
   new release's published checksums.
3. Re-run the driver's config_compatibility_tests and engine/core's
   conformance version_pin test against the new binary; update fixtures if
   behavior changed.
Do not bump only one consumer — a version pin that isn't reflected everywhere
silently reintroduces the test/production divergence this pin exists to
prevent.
"""

CODEX_CLI_VERSION = "0.153.4"
