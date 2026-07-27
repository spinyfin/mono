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
