# Cranelift for non-release Rust builds in mono

- Date: 2026-11-22
- Scope: rules_rust support gate for faster edit → `bazel test` loops
- Repository baseline: `7fcc88c1`; Rust 1.99.0, edition 2024; patched rules_rust and rules_rust_prost 0.70.0
- Related investigation: [Rust test sharding](rust-test-sharding-engine-lib-test.md)

Cranelift could reduce Rust code generation time, but mono needs a backend that Bazel can supply to both local and remote actions. This investigation checks the existing rules_rust integration before attempting a prototype or performance measurements.

## Verdict

**Do not adopt yet; defer the experiment at the support gate.** rules_rust 0.70.0 can forward backend-selection flags and accommodate a custom compiler toolchain, but it does not provide an integrated Cranelift component download or backend selector. There is **no first supporting release identified through 0.75.0**, the latest published release checked; upgrading alone is not an evidenced solution.

Here, support means an existing integration that provisions the backend and makes it available to Bazel actions. Generic compiler flags plus a newly built custom toolchain are an implementation route, not the advertised experimental feature. This interpretation preserves mono's hermetic toolchain convention and the decision to defer when existing support is absent.

## Evidence and method

Inspected mono's [module declarations](../../MODULE.bazel), [Rust pin](../../rust-toolchain.toml), and both local rules_rust patches. Queried the upstream GitHub API for tagged source, releases, and the Cranelift issue; compared the relevant 0.70.0 and 0.75.0 toolchain extension, repository download, compiler-library, and flag-handling code. This is source inspection, not a compilation experiment.

| Question                                          | Observed result                                                                                                                                           | Primary evidence                                                                                                                                                                                                                                                                        |
| ------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Can 0.70.0 pass a backend flag?                   | Yes. `extra_rustc_flags` forwards arbitrary target flags; `extra_exec_rustc_flags` separately covers proc-macros and build scripts.                       | [Flag collection and settings](https://github.com/bazelbuild/rules_rust/blob/0.70.0/rust/private/rustc.bzl#L1308-L1345)                                                                                                                                                                 |
| Does its standard toolchain fetch Cranelift?      | No. The download implementation fetches rustc, Clippy, Cargo, rustfmt, LLVM tools and std, with optional rustc-dev. There is no Cranelift component path. | [Toolchain downloads](https://github.com/bazelbuild/rules_rust/blob/0.70.0/rust/repositories.bzl#L507-L610)                                                                                                                                                                             |
| Does `dev_components = True` supply it?           | No. That option downloads `rustc-dev`, not `rustc-codegen-cranelift-preview`.                                                                             | [Component loader](https://github.com/bazelbuild/rules_rust/blob/0.70.0/rust/private/repository_utils.bzl#L777-L803)                                                                                                                                                                    |
| Is there a backend-specific attribute or setting? | None in the inspected extension, toolchain rule or settings. `codegen_units` controls codegen partitioning, not the backend.                              | [Extension attributes](https://github.com/bazelbuild/rules_rust/blob/0.70.0/rust/extensions.bzl), [toolchain rule](https://github.com/bazelbuild/rules_rust/blob/0.70.0/rust/toolchain.bzl), [settings](https://github.com/bazelbuild/rules_rust/blob/0.70.0/rust/settings/BUILD.bazel) |
| Do mono's patches add backend support?            | No. They add test-wrapper behavior and adapt the compiler action's provider return shape.                                                                 | [Test-wrapper patch](../../third_party/patches/rules_rust-0.70.0-test-wrapper.patch), [Prost compatibility patch](../../third_party/patches/rules_rust_prost-0.70.0-providers-dict.patch)                                                                                               |

### Generic flags are only part of the integration

The 0.70.0 compiler-library filegroup already globs `lib/rustlib/<triple>/codegen-backends/*`, and `rust_toolchain.rustc_lib` carries compiler dependencies into the generated sysroot. That is useful infrastructure for a custom toolchain. However, the standard downloader extracts the `rustc` component; a glob does not fetch the separately distributed Cranelift library. [Compiler files and downloader](https://github.com/bazelbuild/rules_rust/blob/0.70.0/rust/private/repository_utils.bzl), [sysroot construction](https://github.com/bazelbuild/rules_rust/blob/0.70.0/rust/toolchain.bzl#L205-L251).

Rust's `-Zcodegen-backend=<path>` selects a backend dynamic library at runtime. Cranelift's documented distribution is the nightly `rustc-codegen-cranelift-preview` component. Thus adding the flag to mono's stable 1.99.0 toolchain is insufficient, and installing it into a developer's rustup directory would not populate Bazel's downloaded toolchain or remote action inputs. [Rust compiler flag](https://doc.rust-lang.org/unstable-book/compiler-flags/codegen-backend.html), [Cranelift installation](https://github.com/rust-lang/rust/blob/main/compiler/rustc_codegen_cranelift/Readme.md#download-using-rustup).

rules_rust 0.70.0 already accepts dated nightly versions (`nightly/YYYY-MM-DD`) and has a channel setting, `@rules_rust//rust/toolchain/channel:channel`. A future integration would need a matching pinned nightly and backend component for each execution platform, with selection restricted to opted-in non-release configurations. No nightly date or compatibility with mono's patches was validated here. [Version attributes](https://github.com/bazelbuild/rules_rust/blob/0.70.0/rust/extensions.bzl), [channel setting](https://github.com/bazelbuild/rules_rust/blob/0.70.0/rust/toolchain/channel/BUILD.bazel).

### First supporting release

**None verified as of 2026-11-22.** The upstream request, [rules_rust #2555](https://github.com/bazelbuild/rules_rust/issues/2555), remains open. Release notes from 0.70.0 through [0.75.0](https://github.com/bazelbuild/rules_rust/releases/tag/0.75.0) contain no Cranelift/backend-selection announcement. More decisively, 0.75.0's [extension](https://github.com/bazelbuild/rules_rust/blob/0.75.0/rust/extensions.bzl) and [toolchain downloads](https://github.com/bazelbuild/rules_rust/blob/0.75.0/rust/private/repositories.bzl) still lack Cranelift provisioning. These findings do not establish that a custom integration is impossible; they do not justify naming a newer release as the solution.

## What a future upgrade would involve

An upgrade should follow demonstrated backend support, rather than be made speculatively for Cranelift. It would require:

1. Updating both `bazel_dep` and `single_version_override` pins for rules_rust and rules_rust_prost in `MODULE.bazel`, and reviewing the resolved dependency/lockfile changes.
2. Rebasing the test-wrapper patch against the new compiler-action and test-rule APIs. Preserve unconditional test filtering, filtering combined with sharding, failures on unmatched filters or enumeration errors, and the macOS-safe, linear-time shard partitioning. The originating [upstream PR #3774](https://github.com/bazelbuild/rules_rust/pull/3774) is still unmerged; its presence is not a reason to drop mono's additional fixes.
3. Retaining or adapting the Prost provider normalization if the compiler action still returns a dictionary. Even [0.75.0's Prost implementation](https://github.com/bazelbuild/rules_rust/blob/0.75.0/extensions/prost/private/prost.bzl#L128-L147) scans a list of provider instances, so this compatibility requirement cannot be assumed obsolete.
4. Validating wrapper filtering/sharding regressions, generated protocol crates, and the engine, CLI and checkleft suites with `bazel test --remote_download_outputs=minimal`, followed by the normal `bin/checkleft run` gate. The existing [test-sandbox targets](../../tools/test-sandbox/BUILD.bazel) cover wrapper behavior and scale. No patch rebase or upgrade was attempted here.

## Measurements and limits

| Evidence requested                                                               | Runs | Result                             |
| -------------------------------------------------------------------------------- | ---: | ---------------------------------- |
| Engine, CLI and checkleft correctness under Cranelift versus LLVM                |    0 | Not run: support gate did not pass |
| Leaf, shared-crate and test-only edit loops, local and remote                    |    0 | Not measured                       |
| Separate-output-base cold builds, per-action compile/link times and binary sizes |    0 | Not measured                       |

There is no measured speedup, regression, crate exclusion list, or adoption case from mono in this investigation. Proc-macro/build-script compatibility, inline assembly, SIMD, panic behavior, debugger quality and Linux/macOS worker execution remain untested. Upstream specifically documents partial architecture-intrinsic support and experimental panic unwinding unavailable on macOS; those are future correctness checks, not observed mono failures. [Cranelift limitations](https://github.com/rust-lang/rust/blob/main/compiler/rustc_codegen_cranelift/Readme.md#not-yet-supported).

## Follow-up boundary

Revisit when upstream supplies a verifiable integration, or when a separate custom-toolchain experiment is explicitly chosen. That work must preserve stable defaults, release and `-c opt` behavior; account for nightly maintenance and separate backend action-cache entries; and measure real edit-to-test latency before recommending developer or CI adoption. Cold-build measurements must use experiment-only output and disk-cache directories without disabling or clearing shared caches. No prototype, toolchain/configuration change, cache manipulation or benchmark was performed here.
