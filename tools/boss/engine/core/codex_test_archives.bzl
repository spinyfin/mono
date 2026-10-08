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
        host_sha256 = "6e502df69d9220fa305b0c3c7c17ba8f31ab1d4591fc140084dbb823e800c7db",
    ),
    "codex_test_x86_64_macos": struct(
        asset = "codex-x86_64-apple-darwin",
        sha256 = "8d938ddb93c4424b1d45f1606984ed514c5aa70e463302a6a2227fba7af02db7",
        host_sha256 = "cd0ae67e1c2c6cab9c065e3287a56f068961c5592e114c1ccd6e9a4909bba147",
    ),
    "codex_test_x86_64_linux": struct(
        asset = "codex-x86_64-unknown-linux-musl",
        sha256 = "9226581be592d18f7e7f740a352fdb63aa61e45e39f7eb9b09d3888c84bba33f",
        host_sha256 = "8a69207d97545ac753b6585974e1e67a4c51ae5deacf06517db512bb25e0e3c2",
    ),
    "codex_test_aarch64_linux": struct(
        asset = "codex-aarch64-unknown-linux-musl",
        sha256 = "f54dc5852042445bf41da3aa31156f3cb02f52c5a1a04074de73dc5598f7e1f7",
        host_sha256 = "e5e027e6689efda2e3570aa600179f0ebb18632803350e152ed6c9b97dcf9741",
    ),
}

def _codex_test_repo_impl(repo_ctx):
    tag = "rust-v" + CODEX_CLI_VERSION
    asset = repo_ctx.attr.asset
    repo_ctx.download_and_extract(
        url = "https://github.com/openai/codex/releases/download/" + tag + "/" + asset + ".tar.gz",
        sha256 = repo_ctx.attr.sha256,
    )
    host_asset = asset.replace("codex-", "codex-code-mode-host-", 1)
    repo_ctx.download_and_extract(
        url = "https://github.com/openai/codex/releases/download/" + tag + "/" + host_asset + ".tar.gz",
        sha256 = repo_ctx.attr.host_sha256,
    )
    repo_ctx.file(
        "BUILD.bazel",
        'filegroup(name = "codex", srcs = ["{}"], visibility = ["@//tools/boss/engine/driver:__pkg__"])\n'.format(asset) +
        'filegroup(name = "code_mode_host", srcs = ["{}"], visibility = ["@//tools/boss/engine/driver:__pkg__"])\n'.format(host_asset),
    )

_codex_test_repo = repository_rule(
    implementation = _codex_test_repo_impl,
    attrs = {
        "asset": attr.string(mandatory = True),
        "sha256": attr.string(mandatory = True),
        "host_sha256": attr.string(mandatory = True),
    },
)

def _codex_test_archives_impl(_module_ctx):
    for repo_name, platform in _PLATFORMS.items():
        _codex_test_repo(name = repo_name, asset = platform.asset, sha256 = platform.sha256, host_sha256 = platform.host_sha256)

codex_test_archives = module_extension(implementation = _codex_test_archives_impl)
