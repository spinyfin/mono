"""Wraps `rust_test` so every test under `tools/boss/**` links the
`boss-test-isolation` guard crate.

See `tools/boss/test-isolation/src/lib.rs` for what the guard actually does
(installs a private, isolated state root before `main` runs) and
`tools/boss/log-files/src/paths.rs` for the resolver side that refuses to
fall back to production state when no root was installed. The
`boss/raw-rust-test-forbidden` checkleft check (`tools/boss/CHECKS.yaml`)
fails the push gate on a raw `rust_test(...)` anywhere under `tools/boss/**`,
so using this macro is structural, not a convention a BUILD file can quietly
skip.
"""

load("@rules_rust//rust:defs.bzl", "rust_test")

_GUARD_CRATE = "//tools/boss/test-isolation"

def boss_rust_test(name, deps = [], **kwargs):
    """Drop-in replacement for `rust_test` that always links the isolation guard.

    Args:
        name: target name, forwarded to `rust_test` verbatim.
        deps: same as `rust_test`'s `deps` — the guard crate is appended
            automatically; do not list it explicitly.
        **kwargs: everything else forwarded verbatim to `rust_test` (`crate`,
            `srcs`, `crate_root`, `edition`, `env`, `size`, `timeout`,
            `proc_macro_deps`, `shard_count`, `compile_data`, ...).
    """
    rust_test(
        name = name,
        deps = deps + [_GUARD_CRATE],
        **kwargs
    )
