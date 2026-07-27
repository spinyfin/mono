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

def _configured_developer_dir(repository_ctx):
    developer_dir = repository_ctx.os.environ.get("DEVELOPER_DIR", "")
    if not developer_dir:
        xcode_select = repository_ctx.path("/usr/bin/xcode-select")
        if xcode_select.exists:
            result = repository_ctx.execute(
                [str(xcode_select), "-p"],
                quiet = True,
            )
            if result.return_code == 0:
                developer_dir = result.stdout.strip()

    if not developer_dir:
        return ""

    path = repository_ctx.path(developer_dir)
    if not path.exists:
        fail("configured DEVELOPER_DIR does not exist: {}".format(developer_dir))
    return str(path.realpath)

def _test_runtime_repository_impl(repository_ctx):
    developer_dir = _configured_developer_dir(repository_ctx)
    resolved = {}
    for tool in _RUNTIME_TOOLS:
        candidates = ["/usr/bin/" + tool, "/bin/" + tool]
        if tool == "git":
            candidates = ([
                developer_dir + "/usr/bin/git",
            ] if developer_dir else []) + [
                "/Library/Developer/CommandLineTools/usr/bin/git",
            ] + candidates
        if tool in ["git-receive-pack", "git-upload-pack"]:
            candidates = ([
                developer_dir + "/usr/libexec/git-core/" + tool,
            ] if developer_dir else []) + [
                "/Library/Developer/CommandLineTools/usr/libexec/git-core/" + tool,
            ] + candidates
        if tool == "python3":
            candidates = ([
                developer_dir + "/usr/bin/python3",
            ] if developer_dir else []) + ["/opt/homebrew/bin/python3"] + candidates
        for candidate in candidates:
            path = repository_ctx.path(candidate)
            if path.exists:
                path = path.realpath
                resolved[tool] = str(path)
                repository_ctx.symlink(path, "bin/" + tool)
                if tool == "git":
                    git_runtime = path.dirname.dirname.get_child("libexec").get_child("git-core")
                    if git_runtime.exists:
                        resolved["git-tree"] = str(git_runtime.realpath)
                if tool == "python3":
                    if developer_dir and str(path).startswith(developer_dir + "/"):
                        python_runtime = repository_ctx.path(
                            developer_dir + "/Library/Frameworks/Python3.framework",
                        )
                        if python_runtime.exists:
                            resolved["python3-tree"] = str(python_runtime.realpath)
                    elif "/opt/homebrew/" in str(path):
                        resolved["python3-tree"] = str(path.dirname.dirname.realpath)
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
        "developer_dir",
        content = developer_dir + ("\n" if developer_dir else ""),
    )
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
    srcs = glob(["bin/*"]) + [
        "developer_dir",
        "manifest",
    ],
    visibility = ["//visibility:public"],
)
""",
    )

test_runtime_repository = repository_rule(
    implementation = _test_runtime_repository_impl,
    configure = True,
    environ = ["DEVELOPER_DIR"],
    local = True,
)
