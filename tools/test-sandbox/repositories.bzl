"""Host runtime inputs audited for repository-owned test actions."""

_RUNTIME_TOOLS = [
    "awk",
    "basename",
    "bash",
    "cat",
    "chmod",
    "cp",
    "cut",
    "date",
    "dirname",
    "echo",
    "env",
    "false",
    "find",
    "git",
    "git-receive-pack",
    "git-upload-pack",
    "grep",
    "head",
    "kill",
    "ln",
    "mkdir",
    "mkfifo",
    "mktemp",
    "mv",
    "od",
    "perl",
    "printf",
    "pwd",
    "python3",
    "rm",
    "sed",
    "sh",
    "shasum",
    "sleep",
    "sort",
    "tail",
    "tee",
    "touch",
    "tr",
    "true",
    "uname",
    "unzip",
    "wc",
]

def _test_runtime_repository_impl(repository_ctx):
    resolved = {}
    for tool in _RUNTIME_TOOLS:
        candidates = ["/usr/bin/" + tool, "/bin/" + tool]
        if tool == "git":
            candidates = [
                "/Applications/Xcode.app/Contents/Developer/usr/bin/git",
                "/Library/Developer/CommandLineTools/usr/bin/git",
            ] + candidates
        if tool in ["git-receive-pack", "git-upload-pack"]:
            candidates = [
                "/Applications/Xcode.app/Contents/Developer/usr/libexec/git-core/" + tool,
                "/Library/Developer/CommandLineTools/usr/libexec/git-core/" + tool,
            ] + candidates
        if tool == "python3":
            candidates = ["/opt/homebrew/bin/python3"] + candidates
        for candidate in candidates:
            path = repository_ctx.path(candidate)
            if path.exists:
                path = path.realpath
                resolved[tool] = str(path)
                repository_ctx.symlink(path, "bin/" + tool)
                if tool == "git":
                    git_runtime = path.dirname.dirname.get_child("libexec").get_child("git-core")
                    if git_runtime.exists:
                        resolved["git-tree"] = str(git_runtime)
                if tool == "python3" and "/opt/homebrew/" in str(path):
                    python_runtime = path.dirname.dirname
                    resolved["python3-tree"] = str(python_runtime)
                break

    required = [
        "bash",
        "env",
        "git",
        "git-receive-pack",
        "git-upload-pack",
        "kill",
        "mkdir",
        "mktemp",
        "perl",
        "rm",
        "sed",
        "sh",
        "sleep",
    ]
    missing = [tool for tool in required if tool not in resolved]
    if missing:
        fail("required audited test runtime tools are missing: {}".format(", ".join(missing)))

    repository_ctx.file(
        "manifest",
        content = "".join([
            "{}={}\n".format(tool, resolved[tool])
            for tool in sorted(resolved.keys())
        ]),
    )
    repository_ctx.file(
        "BUILD.bazel",
        content = """\
filegroup(
    name = "runtime",
    srcs = glob(["bin/*"]) + ["manifest"],
    visibility = ["//visibility:public"],
)
""",
    )

test_runtime_repository = repository_rule(
    implementation = _test_runtime_repository_impl,
    configure = True,
    local = True,
)
