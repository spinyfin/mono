"""Module extension registering the checksum-pinned Codex CLI test binaries.

MODULE.bazel cannot `load()` an arbitrary `.bzl` file to read
`CODEX_CLI_VERSION` directly (bzlmod module files only support `load()`
indirectly, through `use_extension`/`use_repo_rule`), so the four
`codex_test_*` repos are registered here instead, in lockstep with
codex_cli_version.bzl. Bump procedure: see codex_cli_version.bzl.
"""

load(":codex_cli_version.bzl", "CODEX_CLI_VERSION")

_PLATFORMS = {
    "codex_test_aarch64_macos": struct(
        asset = "codex-aarch64-apple-darwin",
        sha256 = "670af2b049d9c95afb74d7da385f30c5033d13a07175001dd8958c51944984d0",
    ),
    "codex_test_x86_64_macos": struct(
        asset = "codex-x86_64-apple-darwin",
        sha256 = "8d938ddb93c4424b1d45f1606984ed514c5aa70e463302a6a2227fba7af02db7",
    ),
    "codex_test_x86_64_linux": struct(
        asset = "codex-x86_64-unknown-linux-musl",
        sha256 = "9226581be592d18f7e7f740a352fdb63aa61e45e39f7eb9b09d3888c84bba33f",
    ),
    "codex_test_aarch64_linux": struct(
        asset = "codex-aarch64-unknown-linux-musl",
        sha256 = "f54dc5852042445bf41da3aa31156f3cb02f52c5a1a04074de73dc5598f7e1f7",
    ),
}

def _codex_test_repo_impl(repo_ctx):
    tag = "rust-v" + CODEX_CLI_VERSION
    asset = repo_ctx.attr.asset
    repo_ctx.download_and_extract(
        url = "https://github.com/openai/codex/releases/download/" + tag + "/" + asset + ".tar.gz",
        sha256 = repo_ctx.attr.sha256,
    )
    repo_ctx.file(
        "BUILD.bazel",
        'filegroup(name = "codex", srcs = ["{}"], visibility = ["@//tools/boss/engine/driver:__pkg__"])\n'.format(asset),
    )

_codex_test_repo = repository_rule(
    implementation = _codex_test_repo_impl,
    attrs = {
        "asset": attr.string(mandatory = True),
        "sha256": attr.string(mandatory = True),
    },
)

def _codex_test_archives_impl(_module_ctx):
    for repo_name, platform in _PLATFORMS.items():
        _codex_test_repo(name = repo_name, asset = platform.asset, sha256 = platform.sha256)

codex_test_archives = module_extension(implementation = _codex_test_archives_impl)
