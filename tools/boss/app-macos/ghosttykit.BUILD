load("@build_bazel_rules_apple//apple:apple.bzl", "apple_static_xcframework_import")

# Exposed as @ghostty_kit//:GhosttyKit — consumed by //tools/boss/app-macos:boss_mac_app_lib.
apple_static_xcframework_import(
    name = "GhosttyKit",
    xcframework_imports = glob(
        ["GhosttyKit.xcframework/**"],
        exclude = ["GhosttyKit.xcframework/**/._*"],
    ),
    visibility = ["//visibility:public"],
)
