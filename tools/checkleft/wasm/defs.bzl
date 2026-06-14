"""`rust_wasm_component`: build a checkleft check guest crate into a Component
Model component under Bazel, end to end.

Pipeline:
  1. Cross-compile the guest crate to `wasm32-wasip2` via rules_rust. The `crate`
     dependency is built under an outgoing transition onto
     //platforms:wasm32_wasip2, so callers declare an ordinary
     `rust_shared_library` (cdylib) and this rule retargets it.
  2. Componentize / validate with the hermetic `wasm-tools` toolchain
     (//tools/checkleft/wasm:toolchain_type). On `wasm32-wasip2`, rustc links via
     `wasm-component-ld` and already emits a Component Model component, so this
     step validates and passes it through; for a `wasm32-wasip1` core-module
     input it runs `wasm-tools component new` (optionally with a
     wasi_snapshot_preview1 adapter). See componentize.sh.
  3. Emit the component `.wasm` and a `.wasm.sha256` sidecar. The sidecar holds
     the bare hex digest that a CHECKS manifest pins as `artifact_sha256`.

This is the guest-side build infrastructure described in the design doc's
§Build (bazel) / T9. It is deliberately NOT rules_rust's `rust_wasm_bindgen`,
which wraps wasm-bindgen (JS interop, wasm32-unknown-unknown) and has no
connection to WASI or the Component Model.
"""

WasmComponentInfo = provider(
    doc = "A built checkleft check component and its pinned sha256 digest.",
    fields = {
        "component": "File: the Component Model .wasm artifact.",
        "sha256": "File: text sidecar holding the component's bare hex sha256.",
    },
)

# Outgoing transition: build the guest crate for the Component Model guest
# platform regardless of the host configuration the rule is analyzed in.
def _wasip2_transition_impl(_settings, _attr):
    return {"//command_line_option:platforms": str(Label("//platforms:wasm32_wasip2"))}

_wasip2_transition = transition(
    implementation = _wasip2_transition_impl,
    inputs = [],
    outputs = ["//command_line_option:platforms"],
)

def _single_wasm_output(target):
    wasms = [f for f in target[DefaultInfo].files.to_list() if f.extension == "wasm"]
    if len(wasms) != 1:
        fail((
            "rust_wasm_component `crate` ({}) must produce exactly one .wasm " +
            "output (a cdylib built for wasm32-wasip2); found {}"
        ).format(target.label, [f.short_path for f in wasms]))
    return wasms[0]

def _rust_wasm_component_impl(ctx):
    # `crate` carries an outgoing 1:1 transition, so ctx.attr.crate is a list.
    crate_target = ctx.attr.crate[0]
    core_wasm = _single_wasm_output(crate_target)

    toolchain = ctx.toolchains["//tools/checkleft/wasm:toolchain_type"]
    wasm_tools = toolchain.wasm_tools_info.wasm_tools

    component = ctx.actions.declare_file(ctx.label.name + ".wasm")
    sha256 = ctx.actions.declare_file(ctx.label.name + ".wasm.sha256")

    args = ctx.actions.args()
    args.add(wasm_tools)
    args.add(core_wasm)
    args.add(component)
    args.add(sha256)

    inputs = [core_wasm]
    if ctx.file.adapter:
        args.add(ctx.file.adapter)
        inputs.append(ctx.file.adapter)

    ctx.actions.run(
        executable = ctx.executable._componentize,
        arguments = [args],
        inputs = depset(inputs),
        tools = [ctx.attr._componentize[DefaultInfo].files_to_run, wasm_tools],
        outputs = [component, sha256],
        mnemonic = "WasmComponent",
        progress_message = "Componentizing wasm check %{label}",
    )

    outputs = depset([component, sha256])
    return [
        DefaultInfo(files = outputs),
        OutputGroupInfo(component = depset([component]), sha256 = depset([sha256])),
        WasmComponentInfo(component = component, sha256 = sha256),
    ]

rust_wasm_component = rule(
    implementation = _rust_wasm_component_impl,
    doc = "Builds a Rust guest crate into a checkleft check Component Model component.",
    attrs = {
        "crate": attr.label(
            doc = (
                "A rust_shared_library (cdylib) guest crate. Built for " +
                "wasm32-wasip2 via an outgoing transition; the caller declares " +
                "it as an ordinary host-config target."
            ),
            mandatory = True,
            cfg = _wasip2_transition,
        ),
        "adapter": attr.label(
            doc = (
                "Optional wasi_snapshot_preview1 -> preview2 adapter .wasm, used " +
                "only when `crate` produces a core module (wasm32-wasip1). Not " +
                "required for the default wasm32-wasip2 path."
            ),
            allow_single_file = [".wasm"],
        ),
        "_componentize": attr.label(
            default = "//tools/checkleft/wasm:componentize",
            executable = True,
            cfg = "exec",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
    toolchains = ["//tools/checkleft/wasm:toolchain_type"],
)

# ── precompiled_cwasm_dir ─────────────────────────────────────────────────────

def _precompiled_cwasm_dir_impl(ctx):
    """Precompile each input wasm component to an AOT `.cwasm` fixture.

    Runs the host `precompile_cwasm` tool over every `components` entry, writing
    each result into a single output directory under its canonical cache-key
    filename (`{cache_key}.cwasm`). The directory is therefore a ready-to-use
    checkleft `.cwasm` cache: a test pointed at it (via
    `DefaultExternalCheckExecutor::new_with_cache`, or the
    `CHECKLEFT_CWASM_CACHE_DIR` override) deserializes the AOT artifact instead
    of JIT-compiling the component at runtime.

    The filename is content-derived (it folds in the artifact sha256, wasmtime
    version, engine config and host target), so it cannot be declared at analysis
    time — hence a TreeArtifact (`declare_directory`) output whose contents are
    produced by the action.
    """
    out_dir = ctx.actions.declare_directory(ctx.label.name)

    args = ctx.actions.args()
    args.add(out_dir.path)

    inputs = []
    for comp in ctx.attr.components:
        wasm = comp[WasmComponentInfo].component
        args.add(wasm)
        inputs.append(wasm)

    ctx.actions.run(
        executable = ctx.executable._precompile_tool,
        arguments = [args],
        inputs = depset(inputs),
        outputs = [out_dir],
        mnemonic = "PrecompileCwasm",
        progress_message = "Precompiling .cwasm AOT fixtures for %{label}",
    )

    return [DefaultInfo(files = depset([out_dir]))]

# The precompiled `.cwasm` is wasmtime-version + engine-config + host-target
# specific. The `_precompile_tool` runs in the exec configuration, so its target
# (host) must match the configuration the consuming test runs in for the fixture
# to be a cache hit. For ordinary (non-cross-compiled) `bazel test` runs exec ==
# target == host, so they match; a mismatch degrades safely to a cache miss
# (the runtime would JIT) rather than an incorrect deserialize, because the
# cache key already encodes the target triple.
precompiled_cwasm_dir = rule(
    implementation = _precompiled_cwasm_dir_impl,
    doc = (
        "Precompiles `rust_wasm_component` outputs to AOT `.cwasm` fixtures " +
        "keyed identically to checkleft's runtime cache, for use as host-built " +
        "test data."
    ),
    attrs = {
        "components": attr.label_list(
            doc = "rust_wasm_component targets whose .wasm outputs are precompiled to .cwasm.",
            providers = [WasmComponentInfo],
            mandatory = True,
        ),
        "_precompile_tool": attr.label(
            default = "//tools/checkleft:precompile_cwasm",
            executable = True,
            cfg = "exec",
        ),
    },
)
