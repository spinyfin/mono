"""Declare the macOS shell used by worker environment integration tests."""

load("//tools/test-sandbox:repositories.bzl", "host_tool_symlink")

def _worker_zsh_repository_impl(ctx):
    # The test wrapper grants execution to declared runfiles. Do not put zsh
    # into the global test runtime: only worker shell tests need it.
    #
    # The tests run /bin/zsh by that path, so the wrapper has to resolve this
    # runfile to it on whichever host runs them. A declared symlink does that;
    # a copy of the client's zsh (what `ctx.symlink` ships to a remote
    # executor) resolves to itself and leaves /bin/zsh denied.
    host_tool_symlink(
        ctx,
        "zsh",
        "/bin/zsh",
        ["@@//tools/boss/engine/worker-bin:__pkg__", "@@//tools/boss/worker-processes:__pkg__"],
    )

worker_zsh_repository = repository_rule(
    implementation = _worker_zsh_repository_impl,
    local = True,
)
