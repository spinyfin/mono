# Build file for the prebuilt wasm-tools release archives downloaded via
# http_archive in //:MODULE.bazel. Each archive contains a single `wasm-tools`
# binary for one host platform; the wasm_tools_toolchain targets in
# //tools/checkleft/wasm select the matching one per exec platform.

filegroup(
    name = "wasm_tools",
    srcs = ["wasm-tools"],
    visibility = ["@mono//tools/checkleft/wasm:__pkg__"],
)
