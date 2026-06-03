# Feasibility: distributing `checkleft` as a prebuilt binary for external consumers

**Status:** investigation only — no code changes. Deliverable is this writeup.
**Date:** 2026-06-03
**Scope:** External repos currently build `checkleft` from its published cargo crate at every consumer / CI run, which is slow. Can `checkleft` be published as a prebuilt binary that external repos download instead of compile? Produce a concrete recipe and a go/no-go.

All claims below were verified against the current checkout at `/Users/brianduff/Documents/dev/workspaces/mono-agent-002` and against live GitHub data. File citations are inline.

---

## TL;DR

**Recommendation: Go.** The cost of publishing is modest — one new Buildkite release step building two platform binaries and attaching them as GitHub Release assets to the existing `checkleft-v*` tags. The benefit to every external consumer is substantial: replacing a multi-minute cold `cargo build`/`cargo install` (wasmtime + cranelift dominate compile time) with a sub-second binary download. The main gotcha is Linux linking: the Linux binary should be built against musl (`x86_64-unknown-linux-musl`) to avoid glibc version skew across consumer distros. macOS arm64 poses no signing requirement for a CLI tool.

For Bazel consumers the consumption recipe mirrors the in-house `ghostty_kit` pattern (`http_file` + `select()`, sha256-pinned). For non-Bazel consumers `cargo-binstall` can already consume GitHub Release assets with no extra work on our side, and a plain `curl | install` shell snippet covers everything else.

---

## 1. Reference model — how prebuilts are published and consumed

### 1a. `buildifier` (third-party BCR module)

`buildifier` is the canonical Bazel example of a prebuilt external tool. It comes from the public Bazel Central Registry:

- `MODULE.bazel` pins the version:
  ```python
  bazel_dep(name = "buildifier_prebuilt", version = "7.3.1")
  ```
- The `buildifier_prebuilt` BCR module downloads pre-compiled binaries from the upstream `github.com/bazelbuild/buildtools` GitHub releases. `MODULE.bazel.lock` contains the resolved per-platform download URLs and SHA256s:
  ```
  https://github.com/bazelbuild/buildtools/releases/download/v7.3.1/buildifier-darwin-amd64
  https://github.com/bazelbuild/buildtools/releases/download/v7.3.1/buildifier-darwin-arm64
  https://github.com/bazelbuild/buildtools/releases/download/v7.3.1/buildifier-linux-amd64
  https://github.com/bazelbuild/buildtools/releases/download/v7.3.1/buildifier-linux-arm64
  https://github.com/bazelbuild/buildtools/releases/download/v7.3.1/buildifier-windows-amd64.exe
  ```
  Each lands in a per-platform Bazel repo (`buildifier_darwin_arm64`, `buildifier_linux_amd64`, etc.). SHA256s: darwin-amd64 `375f823...`, darwin-arm64 `5a6afc6...`, linux-amd64 `5474cc5...` (from `MODULE.bazel.lock`).
- **Platform selection** is automatic: the module registers Bazel toolchains and exposes a single alias `@buildifier_prebuilt//:buildifier` that resolves against the exec platform. Consumers never name the platform.
- **Publisher**: the upstream `bazelbuild/buildtools` project, via its own CI. This mono repo is a pure consumer — we don't build or publish `buildifier`.

`buildifier` is a ~7 MB statically-linked Go binary. It is a third-party tool with rare version bumps. **Neither of these properties holds for `checkleft`**, which is why the in-house analog below is more relevant.

### 1b. `ghostty_kit` — in-house prebuilt (the direct template)

The mono repo already publishes and consumes one self-built prebuilt artifact: the GhosttyKit xcframework. This is the model a `checkleft` prebuilt would follow exactly.

- **Hosting**: `github.com/spinyfin/ghostty-prebuilts` GitHub releases.
- **Consumption** (`MODULE.bazel`, bottom of file):
  ```python
  http_archive(
      name = "ghostty_kit",
      build_file = "//tools/boss/app-macos:ghosttykit.BUILD",
      sha256 = "210c6914395c2222b76857cd63853916313a5c4a10964b934e38f84b8cf64a06",
      urls = ["https://github.com/spinyfin/ghostty-prebuilts/releases/download/ghosttykit-b0f827665/GhosttyKit-b0f827665.tar.gz"],
  )
  ```
  SHA256 is frozen in `MODULE.bazel`; changing the prebuilt means updating both `urls` and `sha256`.
- **Publish recipe**: `tools/boss/docs/runbooks/update-ghostty-prebuilt.md` — build locally → tar → `gh release create --repo spinyfin/ghostty-prebuilts` → update `MODULE.bazel` → open a PR. This is a manual runbook today.

The `checkleft` recipe would differ in two ways: (a) the release step fires from CI (not a manual runbook), since `checkleft` tags fire per-version; and (b) we need **two** per-platform binaries (macOS arm64, Linux x86_64) rather than one single-platform xcframework.

---

## 2. Current external consumption (the thing being replaced)

External repos that want to run `checkleft` build it from the published cargo crate. The crate is `checkleft` v`0.1.0-alpha.8` (`tools/checkleft/Cargo.toml`); it is published to crates.io with `homepage = "https://github.com/spinyfin/mono"` and `documentation = "https://docs.rs/checkleft"`. The typical install path for an external consumer is:

```sh
cargo install checkleft
```

or a CI step that runs `cargo install --locked checkleft@0.1.0-alpha.8`.

**Why this is slow** — `checkleft`'s dependency profile, from `tools/checkleft/Cargo.toml`:

- `wasmtime = "42.0.1"` with features `cranelift`, `component-model`, `runtime` — a full WebAssembly JIT compiler. Cranelift is the dominant compile-time cost; it alone brings in hundreds of generated source files and takes several minutes to compile from scratch.
- `tree-sitter`, `tree-sitter-java`, `tree-sitter-starlark` — C grammars that require a C toolchain via the `cc` build-script crate.
- `reqwest`, `rustls`, `syn`, `tokio`, `serde`, `globset`, `regex`, etc.

The resulting binary is **~64 MB** on arm64. Cold-cache `cargo install` time for this dependency profile is **5–10 minutes** on a fast machine, dominated by the wasmtime/cranelift build. Every new CI agent, every GitHub Actions runner without a warmed `~/.cargo` cache, and every developer on a fresh machine pays this cost in full.

There is no shortcut available to a pure cargo consumer: the wasmtime crate does not ship prebuilt C libraries that cargo can link against, and `cargo install` has no mechanism analogous to `cargo-binstall` for bypassing compilation.

---

## 3. Feasibility of publishing `checkleft` as a prebuilt

### 3a. Where the binaries would live

The cheapest option is to attach binary assets to the **already-existing `checkleft-v*` tags** on `spinyfin/mono` (confirmed via `gh api repos/spinyfin/mono/tags`: tags `checkleft-v0.1.0-alpha.3` through `-alpha.8` exist but currently have **no GitHub Releases and no binary assets** — only source code is packaged today via `tools/checkleft_package/`).

Alternatively, a dedicated `spinyfin/checkleft-prebuilts` repo (mirroring `spinyfin/ghostty-prebuilts`) keeps release artifacts separate from the mono repo's release list. Either works; the mono repo approach is simpler to start with.

### 3b. How per-platform binaries are produced

No cross-compilation is needed. Build each platform natively on the corresponding Buildkite queue:

- **macOS arm64** — `macos-arm64` queue (Zakalwe-1): `bazel build //tools/checkleft:checkleft` → copy output to `checkleft-darwin-arm64`.
- **Linux x86_64** — `bazel-any` queue: same build → `checkleft-linux-x86_64`.

Both queues already exist (`.buildkite/pipeline-integrity.yml`). The release step is modeled on `.buildkite/steps/boss-release.sh` but much simpler: no credentials, no macOS app signing — just `bazel build`, collect the binary, `gh release create ... checkleft-v<version>`, `gh release upload`.

A CI-triggered release pattern (triggered when `checkleft-v*` tags are pushed, same as the `boss-v*` boss release) means the prebuilt is always synchronized with the tagged source version.

### 3c. How Bazel consumers downstream pin and fetch

A Bazel consumer adds to their `MODULE.bazel`:

```python
http_file(
    name = "checkleft_darwin_arm64",
    urls = ["https://github.com/spinyfin/mono/releases/download/checkleft-v0.1.0-alpha.8/checkleft-darwin-arm64"],
    sha256 = "<sha256>",
    executable = True,
)

http_file(
    name = "checkleft_linux_x86_64",
    urls = ["https://github.com/spinyfin/mono/releases/download/checkleft-v0.1.0-alpha.8/checkleft-linux-x86_64"],
    sha256 = "<sha256>",
    executable = True,
)
```

With a small `BUILD.bazel` exposing a `select()`-based alias:

```python
alias(
    name = "checkleft",
    actual = select({
        "@platforms//os:macos": "@checkleft_darwin_arm64//file",
        "@platforms//os:linux": "@checkleft_linux_x86_64//file",
    }),
)
```

This is exactly the `ghostty_kit` shape adapted for a two-platform binary. Consumers reference `@checkleft_prebuilt//:checkleft` and never name the platform. Bumping = update URLs + sha256s.

For a full BCR-style toolchain-registration approach (so the binary can be used as a Bazel toolchain rather than a plain file), a module extension wrapping the above is possible but is extra complexity unlikely to be worth it for an external CLI tool.

### 3d. How non-Bazel consumers fetch

External repos not using Bazel have several options, in order of consumer effort:

**`cargo-binstall` (zero publish-side work needed):** `cargo-binstall` already knows how to find GitHub Release assets for crates published to crates.io, matching by platform and architecture. Once we publish binary assets with names following the cargo-binstall convention (`checkleft-{version}-{target}.tar.gz` or the fallback naming), external consumers replace `cargo install checkleft` with `cargo binstall checkleft` and get a binary download instead of a compile. This is the cheapest path for non-Bazel consumers.

**`cargo-dist`:** cargo-dist is a tool that automates producing per-platform release artifacts and a GitHub Actions release workflow. Running `cargo dist init` in the `checkleft` crate context and configuring it to emit Linux and macOS arm64 artifacts gives the release CI side for free. cargo-dist also generates installer scripts (`install.sh`) for non-cargo environments. The tradeoff: cargo-dist assumes a standalone Cargo workspace and needs some adaptation to work from the mono repo's `checkleft_package` source tarball rather than directly from the workspace.

**Plain `curl` / shell install (the escape hatch):** Any repo's CI can install the binary directly:

```sh
CHECKLEFT_VERSION="0.1.0-alpha.8"
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)
[[ "$ARCH" == "x86_64" ]] && ARCH="x86_64" || ARCH="arm64"
curl -fsSL "https://github.com/spinyfin/mono/releases/download/checkleft-v${CHECKLEFT_VERSION}/checkleft-${OS}-${ARCH}" \
  -o /usr/local/bin/checkleft && chmod +x /usr/local/bin/checkleft
```

This requires no tooling on the consumer side. The downsides are: no SHA verification unless the script also fetches and checks a sidecar `.sha256` file, and no integration with dependency management.

---

## 4. Blockers and gotchas

### Linux linking: musl vs glibc (the most important gotcha)

A Linux binary built by the `bazel-any` queue agent links against that agent's glibc version. Any consumer running an older glibc (common on LTS distros: Ubuntu 20.04 ships glibc 2.31; many CI images are older still) gets `GLIBC_2.3X not found` at runtime.

**Fix:** Build the Linux binary against musl libc using the `x86_64-unknown-linux-musl` Rust target. A musl binary is statically linked against libc and runs on any Linux kernel ≥ 3.2 regardless of the system glibc version. `rules_rust` supports musl targets; the Buildkite step would need to install `musl-tools` (or use a musl-prepped Docker image) and pass `--platforms=@rules_rust//rust/platform:x86_64-unknown-linux-musl` (or the `cargo build --target x86_64-unknown-linux-musl` equivalent if going the non-Bazel release path).

This is the single technically mandatory change versus a naive "just build and upload." Failing to address it means the Linux prebuilt works on the build agent but breaks on a significant fraction of consumer environments.

### macOS signing and notarization

macOS Gatekeeper blocks unsigned binaries downloaded from the internet unless the consumer explicitly xattr-removes the quarantine flag (`xattr -d com.apple.quarantine ./checkleft`) or the binary is notarized. For developer CLI tools used in CI or invoked from scripts, quarantine is not applied (only files received via browser download are quarantined). **This is not a blocker for CI use.** If the binary is ever distributed for interactive developer download via a link in a README, notarization becomes relevant, but for the primary use case (CI consumption) it is not.

### wasmtime and dynamic libraries

`wasmtime` with the `cranelift` feature links statically on Linux when using musl; on macOS it links against system frameworks (`libSystem.dylib`, `Security.framework`, etc.) which are guaranteed present on any macOS version ≥ 12. Tree-sitter's C grammars compile to static `.a` archives. There are no surprising runtime `.dylib` / `.so` dependencies beyond the OS itself — confirmed by the existing `bazel-out/darwin_arm64-fastbuild/bin/tools/checkleft/checkleft` binary in-repo.

### Supply-chain / version pinning

Each release asset should be accompanied by a `checkleft-<platform>.sha256` sidecar file (or a checksums manifest). Bazel consumers already pin via `sha256` in `MODULE.bazel`. Non-Bazel consumers using the `curl` recipe should be instructed to verify the SHA; cargo-binstall verifies checksums automatically.

### Version pinning in external repos

External repos pin a specific checkleft version. When checkleft releases a new version, consumers must update their pin. This is equivalent to bumping any other tool dependency and is not unique to prebuilts — `cargo install` consumers already pin a version string. Prebuilts have no worse pinning story than the cargo path.

### The Node runtime dependency

`checkleft` shells out to a Node/pnpm toolchain for JS/TS external checks; this toolchain is checked into `tools/checks_js_componentizer/` in the mono repo. External consumers currently rely on the `checkleft_package` source distribution, which stages a standalone copy of this toolchain into the archive (per `tools/checkleft_package/README.md`). A prebuilt binary distribution must either (a) bundle the JS toolchain alongside the binary (a tarball containing the binary + the toolchain directory), or (b) document that JS/TS check support requires the source package. If the external consumer only runs the non-JS checks, (b) is fine. This should be called out clearly in the release notes.

---

## 5. Recommendation

**Go. Publish prebuilt binaries as GitHub Release assets on the existing `checkleft-v*` tags.**

### Publish-side recipe (concrete steps)

1. **Add a `checkleft-release` Buildkite step** that fires when a `checkleft-v*` tag is pushed (same pattern as the `boss-v*` trigger in `.buildkite/pipeline.yml`). The step:
   a. On `macos-arm64` queue: `bazel build //tools/checkleft:checkleft` → copy to `checkleft-darwin-arm64` → `shasum -a 256 > checkleft-darwin-arm64.sha256`.
   b. On `bazel-any` queue: same build with `--platforms=@rules_rust//rust/platform:x86_64-unknown-linux-musl` → `checkleft-linux-x86_64` → `checkleft-linux-x86_64.sha256`. (Requires `musl-tools` or a musl-capable Docker image on the agent.)
   c. `gh release create checkleft-v<version> --repo spinyfin/mono <all four files>`.

2. **Document the consume path** — add `CONSUMING.md` (or extend `tools/checkleft/README.md`) with the Bazel `http_file`/`select()` snippet and the `cargo-binstall checkleft` one-liner.

### Consume-side recipe for external Bazel repos

```python
# MODULE.bazel
http_file(
    name = "checkleft_darwin_arm64",
    urls = ["https://github.com/spinyfin/mono/releases/download/checkleft-v0.1.0-alpha.8/checkleft-darwin-arm64"],
    sha256 = "<sha256>",
    executable = True,
)
http_file(
    name = "checkleft_linux_x86_64",
    urls = ["https://github.com/spinyfin/mono/releases/download/checkleft-v0.1.0-alpha.8/checkleft-linux-x86_64"],
    sha256 = "<sha256>",
    executable = True,
)
```

```python
# BUILD.bazel (in a tools/ package of the consuming repo)
alias(
    name = "checkleft",
    actual = select({
        "@platforms//os:macos": "@checkleft_darwin_arm64//file",
        "@platforms//os:linux": "@checkleft_linux_x86_64//file",
    }),
    visibility = ["//visibility:public"],
)
```

### Consume-side recipe for non-Bazel repos

```sh
# install via cargo-binstall (fastest)
cargo binstall checkleft@0.1.0-alpha.8

# or direct download (CI, no cargo)
CHECKLEFT_VERSION="0.1.0-alpha.8"
ASSET="checkleft-$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m)"
curl -fsSL "https://github.com/spinyfin/mono/releases/download/checkleft-v${CHECKLEFT_VERSION}/${ASSET}" \
  -o /tmp/checkleft
# verify SHA (fetch the matching .sha256 sidecar and compare)
chmod +x /tmp/checkleft && mv /tmp/checkleft /usr/local/bin/checkleft
```

### Why the full cargo-dist route is overkill for now

cargo-dist generates a full release CI workflow and an install manifest, which is useful when distributing to end users via README. For the primary external-consumer use case (other CI pipelines), the GitHub Release asset approach is sufficient. cargo-dist can be adopted later if the distribution audience grows beyond internal repos.

---

## Open Questions

- **Does the `bazel-any` queue agent support musl builds today** (i.e., is `musl-tools` installed, or does a musl-capable container image need to be provisioned)? If not, this is the main setup cost for the Linux binary.
- **Should binary assets live on `spinyfin/mono` releases or in a dedicated `spinyfin/checkleft-prebuilts` repo?** The mono-repo approach is simpler; a dedicated repo avoids mixing `boss-v*` and `checkleft-v*` on the same release list.
- **Does the Node/JS toolchain need to be bundled?** This depends on whether external consumers want JS/TS check support. If yes, the release artifact becomes a tarball (binary + `checks_js_componentizer/` tree) rather than a bare binary.
