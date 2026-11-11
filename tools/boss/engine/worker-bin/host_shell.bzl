"""Declare the macOS shell used by worker environment integration tests."""

def _worker_zsh_repository_impl(ctx):
    # The test wrapper grants execution to declared runfiles. Do not put zsh
    # into the global test runtime: only worker shell tests need it.
    ctx.symlink("/bin/zsh", "zsh")
    ctx.file("BUILD.bazel", 'exports_files(["zsh"], visibility = ["@@//tools/boss/engine/worker-bin:__pkg__", "@@//tools/boss/worker-processes:__pkg__"])\n')

worker_zsh_repository = repository_rule(
    implementation = _worker_zsh_repository_impl,
    local = True,
)
