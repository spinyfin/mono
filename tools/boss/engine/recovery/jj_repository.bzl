"""Pinned jj executables for real, sandboxed tests.

One parameterized repository rule serves every pinned jj version. MODULE.bazel
instantiates it as `jj_test_tools` (the default version) and as
`cube_jj_newer`, a newer jj that cube's GC tests use to reproduce predecessor
retention across index versions. Each instance declares its own visibility.
"""

# The version used when an instance does not name one. gc_store_archive.py reads
# this and the matching _JJ_RELEASES entry to detect fixture drift, so keep both
# as plain literals.
_DEFAULT_VERSION = "0.26.0"

# Release version -> platform -> archive sha256.
_JJ_RELEASES = {
    "0.26.0": {
        "aarch64-apple-darwin": "464243d23f511e626228afd4ca01d83c5fec93ea2358312ab9c59d3e1d5cd50d",
        "x86_64-apple-darwin": "259344bce7bd0bea889543fbd372055f2de43f79057f0d3947b815eec7166c32",
        "aarch64-unknown-linux-musl": "a67aa1bde26375a2b542b908a6dd567826b0c32fd07e2073f220beb75453f489",
        "x86_64-unknown-linux-musl": "9f0be0f1348a2372b7c08d0130cae994ee9061f9a6c2eebe458f9266cd1e0faa",
    },
    "0.32.0": {
        "aarch64-apple-darwin": "954e094d767491313eb14825104fc941b113b44a2f81451a1e2d2f44af4c2a63",
        "x86_64-apple-darwin": "d7c1a4800ad877219ccb6e0bb7925194f18242bffe4048d6bd834afe7d765773",
        "aarch64-unknown-linux-musl": "1a68cfdbb71e97ed3b417e153701c135d680f07a6f2b6cb6e086008356c650a1",
        "x86_64-unknown-linux-musl": "64470d36bed3f5d04dc7c5d05e47b2052f61cc253977ae3c0b9729937c55a0a3",
    },
}

def _jj_test_repository_impl(ctx):
    version = ctx.attr.version
    arch = "aarch64" if ctx.os.arch in ["aarch64", "arm64"] else "x86_64"
    os = "apple-darwin" if ctx.os.name == "mac os x" else "unknown-linux-musl"
    platform = arch + "-" + os
    ctx.download_and_extract(
        url = "https://github.com/jj-vcs/jj/releases/download/v{v}/jj-v{v}-{p}.tar.gz".format(v = version, p = platform),
        sha256 = _JJ_RELEASES[version][platform],
    )
    ctx.file("BUILD.bazel", 'exports_files(["jj"], visibility = {})\n'.format(repr(ctx.attr.visible_to)))

jj_test_repository = repository_rule(
    implementation = _jj_test_repository_impl,
    attrs = {
        "version": attr.string(default = _DEFAULT_VERSION),
        "visible_to": attr.string_list(mandatory = True),
    },
)
