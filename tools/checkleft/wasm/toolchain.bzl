"""Hermetic `wasm-tools` toolchain for building checkleft check components.

`wasm-tools` is the Bytecode Alliance CLI used to validate (and, for core-module
inputs, componentize) WebAssembly Component Model artifacts. The
`rust_wasm_component` rule (//tools/checkleft/wasm:defs.bzl) consumes the binary
through this toolchain so the version is pinned and reproducible across CI hosts
rather than relying on a `wasm-tools` that happens to be on `PATH`.

The binary itself is downloaded per host platform via `http_archive` in
//:MODULE.bazel; one `wasm_tools_toolchain` + `toolchain()` pair is declared per
exec platform in this package's BUILD file and registered in MODULE.bazel.
"""

WasmToolsInfo = provider(
    doc = "Locates the hermetic wasm-tools executable for the resolved exec platform.",
    fields = {
        "wasm_tools": "File: the wasm-tools executable.",
    },
)

def _wasm_tools_toolchain_impl(ctx):
    return [
        platform_common.ToolchainInfo(
            wasm_tools_info = WasmToolsInfo(
                wasm_tools = ctx.file.wasm_tools,
            ),
        ),
    ]

wasm_tools_toolchain = rule(
    implementation = _wasm_tools_toolchain_impl,
    doc = "Wraps a prebuilt wasm-tools binary as a Bazel toolchain.",
    attrs = {
        "wasm_tools": attr.label(
            doc = "The wasm-tools executable for this toolchain's exec platform.",
            allow_single_file = True,
            executable = True,
            cfg = "exec",
            mandatory = True,
        ),
    },
)
