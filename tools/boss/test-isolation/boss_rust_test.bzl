"""Structural wrapper for Boss Rust test targets.

State-path isolation is enforced by the resolver chokepoint in
`tools/boss/log-files/src/paths.rs`: a Bazel-output executable gets a private
state root at first resolution, without linker-retained constructors.
"""

load("@rules_rust//rust:defs.bzl", "rust_test")

def boss_rust_test(name, deps = [], **kwargs):
    """Drop-in replacement for `rust_test` used by every Boss test target.

    Args:
        name: target name, forwarded to `rust_test` verbatim.
        deps: same as `rust_test`'s `deps`.
        **kwargs: everything else forwarded verbatim to `rust_test` (`crate`,
            `srcs`, `crate_root`, `edition`, `env`, `size`, `timeout`,
            `proc_macro_deps`, `shard_count`, `compile_data`, ...).
    """
    rust_test(
        name = name,
        deps = deps,
        **kwargs
    )
