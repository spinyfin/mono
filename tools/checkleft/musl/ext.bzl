"""Bzlmod extension: register Rust toolchains for musl cross-compilation.

rules_rust's built-in toolchain registration maps both x86_64-unknown-linux-gnu
and x86_64-unknown-linux-musl to identical Bazel platform constraints
(@platforms//cpu:x86_64 + @platforms//os:linux), making them ambiguous when
both are registered in the same hub.  This extension sidesteps the ambiguity:

  1. For each supported exec triple, creates a rust_toolchain_tools_repository
     (downloads the musl std library targeting x86_64/aarch64-unknown-linux-musl)
     and a toolchain_repository_proxy with target_compatible_with that includes
     @zig_sdk//libc:musl (from hermetic_cc_toolchain).

  2. The root MODULE.bazel registers these proxy repos BEFORE @rust_toolchains//:all,
     so they win for //platforms:linux_x86_64_musl (which carries @zig_sdk//libc:musl)
     while the existing GNU toolchain is filtered out for that platform because it
     doesn't carry @zig_sdk//libc:musl in its target_compatible_with.

  3. For the default linux platform (no @zig_sdk//libc:musl constraint), the musl
     toolchains declared here are filtered out, leaving the GNU toolchain in place.
"""

load(
    "@rules_rust//rust:repositories.bzl",
    "rust_toolchain_tools_repository",
    "toolchain_repository_proxy",
)
load(
    "@rules_rust//rust/platform:triple_mappings.bzl",
    "triple_to_constraint_set",
)

# Must match the version in MODULE.bazel rust.toolchain().
_RUST_VERSION = "1.99.0"

# From https://static.rust-lang.org/dist/channel-rust-1.99.0.toml.
# Pin host tools as well as both musl std archives for every repository below.
_RUST_SHA256S = {
    "cargo-1.99.0-aarch64-apple-darwin.tar.xz": "76abff0ad79a10a3152dff640ea0d8b3c37637d9300bf55da41b95c3ddc069c1",
    "cargo-1.99.0-aarch64-unknown-linux-gnu.tar.xz": "113229fcc16cc1ba004923a4cc275fbf3281dafaaf8b97269f9c00189d497fd4",
    "cargo-1.99.0-x86_64-apple-darwin.tar.xz": "939b71952afd63ae73bc00f3517c19f0dd7ac54884b5fac16a813ee50985d14d",
    "cargo-1.99.0-x86_64-unknown-linux-gnu.tar.xz": "d7674918d28093097614cd9728b6ca60db9ea3038f640f0bd1e9a4188c7568ce",
    "clippy-1.99.0-aarch64-apple-darwin.tar.xz": "c31a0765ed31d98515a09a13b44486048873d90ac75bb1e6690a17fb36a5d4e2",
    "clippy-1.99.0-aarch64-unknown-linux-gnu.tar.xz": "42e49dc02d29949395c0c072585961f6c9b5408110236087828d4e2f94ca96ef",
    "clippy-1.99.0-x86_64-apple-darwin.tar.xz": "cd93518193f16b21cfa7147a42cb03cd2d9ca9b3d0c2daf46dcae44c88a696df",
    "clippy-1.99.0-x86_64-unknown-linux-gnu.tar.xz": "982442e32ad8dd3f0bb3a30db74038da5f71fd9bf1a865cc71da8ea5d4ec71ec",
    "llvm-tools-1.99.0-aarch64-apple-darwin.tar.xz": "9cbaa83827e53c9bbae05fa3a36aff0832448de6d2204829b710cbc85567ad2d",
    "llvm-tools-1.99.0-aarch64-unknown-linux-gnu.tar.xz": "6e921c7c1225ec43dcdd0bda2ce541ac79d45eaa2da64c686f7d78e8cb3dff04",
    "llvm-tools-1.99.0-x86_64-apple-darwin.tar.xz": "708b998c88ccbf2ce847cc8aeeb9fa55b969319848971a9e2085c8e426b907cf",
    "llvm-tools-1.99.0-x86_64-unknown-linux-gnu.tar.xz": "91a6db5668c08ee68e330e5b0fda27c11b79b90592627e89f7c5db12af2ff0ba",
    "rust-std-1.99.0-aarch64-apple-darwin.tar.xz": "a7c6de8aa21e7c31163a7656295b321bb85f5dce55ed8ea95ac9ac561202bb5b",
    "rust-std-1.99.0-aarch64-unknown-linux-gnu.tar.xz": "1cf7e2ef58ed1cfa6f1adea18e155cbf254c1951f39e5f5e3376879f55fd64a4",
    "rust-std-1.99.0-aarch64-unknown-linux-musl.tar.xz": "ea8fd309578a09c9e12401621a470e6d6b1047f25933207fb8a39d66647d4561",
    "rust-std-1.99.0-x86_64-apple-darwin.tar.xz": "18ee81961a73c3ae966ecb1e8f6096438c90aeefece8856201a6ecc17f5bc850",
    "rust-std-1.99.0-x86_64-unknown-linux-gnu.tar.xz": "3e58dff2d0b72196b5ea4e90536e174d400de88564a52694686b81e091169933",
    "rust-std-1.99.0-x86_64-unknown-linux-musl.tar.xz": "b106b0aa4565cc3525fd3161c69203a9e39044e6e6b4d618f261c3eeda14ad50",
    "rustc-1.99.0-aarch64-apple-darwin.tar.xz": "334a66714ca316d71bbe5efe44f71762d0b276cea73be2987374693a837f7450",
    "rustc-1.99.0-aarch64-unknown-linux-gnu.tar.xz": "89c0f1a3a44df63c95e5d5f2ba093d0a12c5f5f57f8635f8a7a0ce50cf86600e",
    "rustc-1.99.0-x86_64-apple-darwin.tar.xz": "7460910254f059b49b99ee3ddbcf7197d16b9505b59c166e14a1465ee323a81d",
    "rustc-1.99.0-x86_64-unknown-linux-gnu.tar.xz": "77171ba2a0345fdf2abc4fedda55d6de078dae7a68527c28be8c77dcc9604bd5",
    "rustfmt-1.99.0-aarch64-apple-darwin.tar.xz": "8b1436085c081e5efa88febadf667171a7a107073d168cd32b249a8eb70800da",
    "rustfmt-1.99.0-aarch64-unknown-linux-gnu.tar.xz": "4b9162fef99fd5246d2f83a2430c144e9bb9095aac6d1de0aaebde45ec01e09a",
    "rustfmt-1.99.0-x86_64-apple-darwin.tar.xz": "e5e59e6910aa6e716fbb8a53950e109fb675ead428688f3ba2d1702b50928f92",
    "rustfmt-1.99.0-x86_64-unknown-linux-gnu.tar.xz": "b22c09ab9e258ec5571da170d88bd1624a4ea602e6d70d95720c5b47494cadce",
}

# Exec triples for which we register a musl cross-compile toolchain.
# Covers macOS (arm/x86) and Linux (arm/x86) CI agents.
_EXEC_TRIPLES = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
]

# (Rust target triple, libc constraint)
_MUSL_TARGETS = [
    ("x86_64-unknown-linux-musl", "@zig_sdk//libc:musl"),
    ("aarch64-unknown-linux-musl", "@zig_sdk//libc:musl"),
]

def _safe(s):
    return s.replace("-", "_")

def _musl_rust_toolchain_impl(_module_ctx):
    for exec_triple in _EXEC_TRIPLES:
        exec_constraints = triple_to_constraint_set(exec_triple)
        for musl_triple, libc_constraint in _MUSL_TARGETS:
            # target_compatible_with from rules_rust triple mapping PLUS the
            # hermetic_cc_toolchain libc discriminator.
            target_constraints = triple_to_constraint_set(musl_triple) + [libc_constraint]
            base = "rust_musl_{}_{}".format(_safe(exec_triple), _safe(musl_triple))
            tools_name = base + "_tools"
            proxy_name = base

            rust_toolchain_tools_repository(
                name = tools_name,
                exec_triple = exec_triple,
                target_triple = musl_triple,
                version = _RUST_VERSION,
                sha256s = _RUST_SHA256S,
                # hermetic_cc_toolchain (Zig CC) provides its own musl sysroot and
                # CRT files (rcrt1.o, crti.o, crtbeginS.o).  Disable Rust's
                # self-contained startup objects to avoid duplicate symbol errors
                # from the linker (_init, _fini, _start, _start_c).
                extra_rustc_flags = ["-C", "link-self-contained=no"],
            )

            toolchain_repository_proxy(
                name = proxy_name,
                toolchain = "@{}//:rust_toolchain".format(tools_name),
                toolchain_type = "@rules_rust//rust:toolchain_type",
                exec_compatible_with = exec_constraints,
                target_compatible_with = target_constraints,
                target_settings = ["@rules_rust//rust/toolchain/channel:stable"],
            )

musl_rust_toolchain = module_extension(
    implementation = _musl_rust_toolchain_impl,
)
