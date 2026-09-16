"""Pinned jj executable for real, sandboxed recovery tests."""

_ARCHIVES = {
    "aarch64-apple-darwin": "464243d23f511e626228afd4ca01d83c5fec93ea2358312ab9c59d3e1d5cd50d",
    "x86_64-apple-darwin": "259344bce7bd0bea889543fbd372055f2de43f79057f0d3947b815eec7166c32",
    "aarch64-unknown-linux-musl": "a67aa1bde26375a2b542b908a6dd567826b0c32fd07e2073f220beb75453f489",
    "x86_64-unknown-linux-musl": "9f0be0f1348a2372b7c08d0130cae994ee9061f9a6c2eebe458f9266cd1e0faa",
}

def _jj_test_repository_impl(ctx):
    arch = "aarch64" if ctx.os.arch in ["aarch64", "arm64"] else "x86_64"
    os = "apple-darwin" if ctx.os.name == "mac os x" else "unknown-linux-musl"
    platform = arch + "-" + os
    ctx.download_and_extract(
        url = "https://github.com/jj-vcs/jj/releases/download/v0.26.0/jj-v0.26.0-" + platform + ".tar.gz",
        sha256 = _ARCHIVES[platform],
    )
    ctx.file("BUILD.bazel", 'exports_files(["jj"], visibility = ["@@//tools/boss/engine/recovery:__pkg__", "@@//tools/boss/engine/core:__pkg__"])\n')

jj_test_repository = repository_rule(implementation = _jj_test_repository_impl)
