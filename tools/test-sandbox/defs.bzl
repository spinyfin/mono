"""Target-level test sandbox policy helpers."""

load("@rules_rust//rust:defs.bzl", "rust_test")

XCODE_TEST_ENV = {
    "CFFIXED_USER_HOME": "__MONO_TEST_HOME__",
    "HOME": "__MONO_TEST_HOME__",
    "MONO_TEST_HOST_TMPDIR": "__MONO_TEST_HOST_TMPDIR__",
    "MONO_TEST_XCODE_DEVELOPER_DIR": "__MONO_TEST_XCODE_DEVELOPER_DIR__",
    "MONO_TEST_XCODE_TOOLCHAIN": "1",
    "TEMP": "__MONO_TEST_PROCESS_TMPDIR__",
    "TEST_UNDECLARED_OUTPUTS_DIR": "__MONO_TEST_UNDECLARED_OUTPUTS_DIR__",
    "TEST_TMPDIR": "__MONO_TEST_TMPDIR__",
    "TMP": "__MONO_TEST_PROCESS_TMPDIR__",
    "TMPDIR": "__MONO_TEST_PROCESS_TMPDIR__",
}

def network_enabled_rust_test(name, env = None, tags = None, **kwargs):
    """Declares a Rust test with the cross-platform external-network opt-in.

    Args:
      name: Bazel target name.
      env: Additional test environment variables.
      tags: Additional Bazel target tags.
      **kwargs: Remaining arguments forwarded to rust_test.
    """
    network_env = dict(env or {})
    network_env["MONO_TEST_ALLOW_NETWORK"] = "1"
    network_tags = list(tags or [])
    network_tags.append("requires-network")
    rust_test(
        name = name,
        env = network_env,
        tags = network_tags,
        **kwargs
    )

def _test_twin_impl(ctx):
    primary = ctx.attr.binary
    primary_exe = primary[DefaultInfo].files_to_run.executable
    primary_env = primary[RunEnvironmentInfo]

    # The twin runs the primary's own (already compiled) test executable from the
    # runfiles root, so nothing is recompiled; only the runtime env and data differ.
    launcher = ctx.actions.declare_file(ctx.label.name + "_twin_launcher.sh")
    ctx.actions.write(
        output = launcher,
        content = "#!/usr/bin/env bash\nset -euo pipefail\nexec \"./{}\" \"$@\"\n".format(primary_exe.short_path),
        is_executable = True,
    )

    env = dict(primary_env.environment)
    for key, value in ctx.attr.env.items():
        env[key] = ctx.expand_location(value, ctx.attr.data)

    runfiles = primary[DefaultInfo].default_runfiles.merge(
        ctx.runfiles(files = ctx.files.data).merge_all(
            [d[DefaultInfo].default_runfiles for d in ctx.attr.data],
        ),
    )
    return [
        DefaultInfo(executable = launcher, runfiles = runfiles),
        RunEnvironmentInfo(
            environment = env,
            inherited_environment = primary_env.inherited_environment,
        ),
    ]

twin_test = rule(
    implementation = _test_twin_impl,
    attrs = {
        "binary": attr.label(
            mandatory = True,
            providers = [DefaultInfo, RunEnvironmentInfo],
            doc = "The compiled test target to run. It is not recompiled.",
        ),
        "data": attr.label_list(
            allow_files = True,
            doc = "Extra runtime data for this twin, beyond the primary's runfiles.",
        ),
        "env": attr.string_dict(
            doc = "Env overrides on top of the primary's env; $(rootpath) expands over data.",
        ),
    },
    test = True,
    doc = """A test target that reruns another test target's compiled binary.

Used for the jj-version twins: the crate compiles once (the primary rust_test) and
each twin differs only in runtime data and env. Size, timeout, shard_count, tags
and --test_filter are the twin's own and behave as for any test rule; sharding and
filtering are handled by the primary's rust_test wrapper, which the twin execs.
""",
)
