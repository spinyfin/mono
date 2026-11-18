"""Partitions the growing app test source set into independent XCTest bundles."""

load("@build_bazel_rules_apple//apple:macos.bzl", "macos_unit_test")
load("@build_bazel_rules_swift//swift:swift.bzl", "swift_library")
load("//tools/test-sandbox:defs.bzl", "XCODE_TEST_ENV")

def app_test_shards(srcs, principal_srcs, linkopts):
    """Includes every catch-all source exactly once, with shared bundle setup.

    Args:
        srcs: Catch-all test sources, excluding the shared principal.
        principal_srcs: Bundle initialization sources compiled into every shard.
        linkopts: Linker options, including the caller's coverage configuration.
    """
    for index in range(4):
        name = "BossTestsShard%d" % (index + 3)
        library = "boss_mac_app_tests_shard%d_lib" % (index + 3)
        sources = sorted(srcs)[index::4]
        data = []
        if "Tests/BossTests/TrekPortraitGridTests.swift" in sources:
            data = ["//tools/boss/app-macos/Sources/Resources/TrekIcons:trek_icons"]
        swift_library(
            name = library,
            testonly = True,
            srcs = sources + principal_srcs + [
                ":generate_real_opener_scan",
                ":generate_report_only_alert_scan",
            ],
            copts = ["-DTEXTUAL_ENABLE_TEXT_SELECTION", "-DBOSS_BAZEL_TEST_BUILD"],
            linkopts = linkopts,
            module_name = name,
            deps = [":boss_mac_app_lib", "//tools/boss/app-macos/Sources/PaneLayout"],
            visibility = ["//visibility:private"],
        )
        macos_unit_test(
            name = name,
            size = "small",
            data = data,
            env = XCODE_TEST_ENV,
            infoplists = ["Tests/BossTests/Info.plist"],
            minimum_os_version = "15.0",
            runner = "//tools/test-sandbox:macos_direct_test_runner",
            deps = [":" + library],
            visibility = ["//visibility:private"],
        )
