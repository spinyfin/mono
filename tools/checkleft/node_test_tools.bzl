"""Declared host utilities for pinned Node archive integration tests."""

def _node_test_tools_impl(repository_ctx):
    tools = ["tar", "gzip"]
    if repository_ctx.os.name == "linux":
        tools.append("getconf")
    for tool in tools:
        found = False
        for directory in ["/usr/bin", "/bin"]:
            path = repository_ctx.path(directory + "/" + tool)
            if path.exists:
                repository_ctx.symlink(path.realpath, tool)
                found = True
                break
        if not found:
            fail("Node archive tests require " + tool)
    repository_ctx.file(
        "BUILD.bazel",
        content = """
filegroup(
    name = "tools",
    srcs = glob(["*"], exclude = ["BUILD.bazel"]),
    visibility = ["@@//tools/checkleft:__pkg__"],
)
""",
    )

node_test_tools = repository_rule(
    implementation = _node_test_tools_impl,
    local = True,
)
