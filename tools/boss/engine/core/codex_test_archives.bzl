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
        sha256 = "8cf911ea676523bfb2121ec561848d2aba564890ad536db4d8a3353f2b9850b1",
    ),
    "codex_test_x86_64_macos": struct(
        asset = "codex-x86_64-apple-darwin",
        sha256 = "d69200f0bf841b1d1a07f80b80cf742a2e4fc2bab91ae8a44b1042f8e8ca9fa4",
    ),
    "codex_test_x86_64_linux": struct(
        asset = "codex-x86_64-unknown-linux-musl",
        sha256 = "f479424eca092484dc40d87ae28c44f4cc40234a60045d6131e493800d814a30",
    ),
    "codex_test_aarch64_linux": struct(
        asset = "codex-aarch64-unknown-linux-musl",
        sha256 = "5cda6182bd94c3a30f2eb63a495489ebf7f691fddb14d70f48c6c1a5071b6cde",
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
