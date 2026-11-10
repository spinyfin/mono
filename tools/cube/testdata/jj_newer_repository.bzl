"""A newer jj for testing predecessor retention across index versions."""

_ARCHIVES = {
    "aarch64-apple-darwin": "954e094d767491313eb14825104fc941b113b44a2f81451a1e2d2f44af4c2a63",
    "x86_64-apple-darwin": "d7c1a4800ad877219ccb6e0bb7925194f18242bffe4048d6bd834afe7d765773",
    "aarch64-unknown-linux-musl": "1a68cfdbb71e97ed3b417e153701c135d680f07a6f2b6cb6e086008356c650a1",
    "x86_64-unknown-linux-musl": "64470d36bed3f5d04dc7c5d05e47b2052f61cc253977ae3c0b9729937c55a0a3",
}

def _jj_newer_repository_impl(ctx):
    arch = "aarch64" if ctx.os.arch in ["aarch64", "arm64"] else "x86_64"
    os = "apple-darwin" if ctx.os.name == "mac os x" else "unknown-linux-musl"
    platform = arch + "-" + os
    ctx.download_and_extract(
        url = "https://github.com/jj-vcs/jj/releases/download/v0.32.0/jj-v0.32.0-" + platform + ".tar.gz",
        sha256 = _ARCHIVES[platform],
    )
    ctx.file("BUILD.bazel", 'exports_files(["jj"], visibility = ["@@//tools/cube:__pkg__"])\n')

jj_newer_repository = repository_rule(implementation = _jj_newer_repository_impl)
