"""A hostless macOS XCTest runner that stays inside the test action."""

load("@build_bazel_rules_apple//apple:providers.bzl", "apple_provider")

def _macos_direct_test_runner_impl(ctx):
    ctx.actions.expand_template(
        template = ctx.file._template,
        output = ctx.outputs.test_runner_template,
        substitutions = {},
        is_executable = True,
    )
    return [
        apple_provider.make_apple_test_runner_info(
            test_runner_template = ctx.outputs.test_runner_template,
            execution_requirements = {"requires-darwin": ""},
            execution_environment = {},
        ),
        DefaultInfo(),
    ]

macos_direct_test_runner = rule(
    implementation = _macos_direct_test_runner_impl,
    attrs = {
        "_template": attr.label(
            default = Label("//tools/test-sandbox:macos_direct_test_runner.template.sh"),
            allow_single_file = True,
        ),
    },
    outputs = {
        "test_runner_template": "%{name}.sh",
    },
)
