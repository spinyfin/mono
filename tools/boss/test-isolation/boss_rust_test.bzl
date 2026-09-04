"""Structural wrapper for Boss Rust test targets.

State-path isolation is enforced by the resolver chokepoint in
`tools/boss/log-files/src/paths.rs`. That resolver treats a Bazel-output
executable as a *test* process only when Bazel's test harness env is
present or the executable's file stem ends in `_test`. This macro
enforces that stem so a direct `bazel-bin/.../<name>` invocation of a
Boss test binary is isolated, while `bazel run` of a production binary
(`engine`, `Boss`, `boss`) keeps production's state root.
"""

load("@rules_rust//rust:defs.bzl", "rust_test")

def boss_rust_test(name, deps = [], **kwargs):
    """Drop-in replacement for `rust_test` used by every Boss test target.

    `name` must end with `_test`. The state-root resolver keys on that
    suffix (alongside `TEST_TMPDIR` / `TEST_SRCDIR` / `BAZEL_TEST`) to
    distinguish test binaries from `bazel run` production binaries that
    also live under `bazel-out`.

    Args:
        name: target name, forwarded to `rust_test`. Must end with `_test`.
        deps: same as `rust_test`'s `deps`.
        **kwargs: everything else forwarded verbatim to `rust_test` (`crate`,
            `srcs`, `crate_root`, `edition`, `env`, `size`, `timeout`,
            `proc_macro_deps`, `shard_count`, `compile_data`, ...).
    """
    if not name.endswith("_test"):
        fail("boss_rust_test name must end with '_test' (got %r); the state-root resolver keys on that suffix to distinguish test binaries from `bazel run` production binaries living under bazel-out." % name)
    rust_test(
        name = name,
        deps = deps,
        **kwargs
    )
