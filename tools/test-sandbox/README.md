# `tools/test-sandbox` — why this exists

If you know Bazel, the contents of this directory should look like too much. Bazel already sandboxes actions; a repository that writes its own Seatbelt profile, its own `--run_under` wrapper, and its own audited runtime-tool repository looks like someone who did not read the manual.

This document is the answer to that reaction. The short version: **on macOS, Bazel's test sandbox is a deny-list that permits everything this repository needs denied, and it cannot be extended — so the wrapper replaces it outright. On Linux, Bazel's sandbox is genuinely good, and the wrapper only adds the two things Bazel does not model: the process environment, and a socket-length-safe temp root.**

Everything below was measured, not assumed. Where a claim came from a CI log or a local experiment, the evidence is cited inline.

## The two layers, and why neither alone was enough

`.bazelrc` configures one wrapper for every platform, then diverges on the strategy:

```
common --enable_platform_specific_config

test --sandbox_default_allow_network=false
test:macos --strategy=TestRunner=local
test:linux --strategy=TestRunner=linux-sandbox
test:linux --sandbox_tmpfs_path=/tmp
test --run_under=//tools/test-sandbox:hermetic_test_wrapper
# Align build's analysis config with cquery (which inherits test --run_under
# on Bazel 9.x). Empty cquery --run_under= is invalid; without this, every
# build↔cquery flip discards the analysis cache (repobin hits this constantly).
build --run_under=//tools/test-sandbox:hermetic_test_wrapper
# Interactive runs cannot use the test wrapper (needs TEST_SRCDIR / test runtime).
run --run_under=/usr/bin/env
test --test_env=ANTHROPIC_API_KEY=          # (and six more)
```

`--enable_platform_specific_config` is what makes `test:macos` and `test:linux` apply from the host platform; nothing passes `--config=` for these.

### Linux: Bazel does the isolation, the wrapper does the environment

`linux-sandbox` gives a mount namespace (only declared inputs and the execroot are visible and writable), a network namespace when the network is blocked, and a PID namespace. That is a real boundary and this repo uses it as-is.

What it does _not_ model is what is _inside_ the action: the `PATH` Bazel's own test/XML wrappers need before the test binary starts still points at host tooling, credentials that reach the action as environment variables are still set, and `TEST_TMPDIR` still lives deep inside the execroot. The wrapper fixes exactly those three things. That is its whole job on Linux.

### macOS: Bazel does nothing, because it cannot do both

`test:macos --strategy=TestRunner=local` means Bazel applies **no sandbox at all** to test actions. The wrapper's Seatbelt profile is the entire isolation boundary. That is a deliberate choice forced by two measured facts.

**Fact one: Bazel's macOS sandbox profile is a deny-list, and a permissive one.** `darwin-sandbox` generates a Seatbelt profile per action. Here is the real one, captured from `bazel test --strategy=TestRunner=darwin-sandbox --sandbox_debug` on Bazel 9.1.0 / macOS arm64 (paths abbreviated):

```scheme
(version 1)
(debug deny)
(allow default)                                   ; <-- everything not named below is allowed
(allow process-exec (with no-sandbox) (literal "/bin/ps"))
(deny network*)                                   ; only because --sandbox_default_allow_network=false
(allow network-inbound (local ip "localhost:*"))
(allow network* (remote ip "localhost:*"))
(allow network* (remote unix-socket))
(deny file-write*)
(allow file-write*
    (subpath "/dev")
    (subpath "$HOME/Library/Logs")
    (subpath "<execroot>")
    (subpath "/private/var/tmp")                  ; <-- shared host temp
    (subpath "$HOME/Library/Caches")              ; <-- shared host cache
    (subpath "<execroot>/_tmp/<hash>")
    (subpath "$TMPDIR/../C")                      ; <-- DARWIN_USER_CACHE_DIR
    (subpath "$TMPDIR")                           ; <-- DARWIN_USER_TEMP_DIR
    (subpath "$HOME/Library/Developer")
    (subpath "/private/tmp")                      ; <-- shared host temp
    (literal "<...>/stats.out")
)
```

`(allow default)` is the whole story. Only `file-write*` and `network*` are constrained at all. **File reads are unrestricted. `process-exec` is unrestricted.** And the write deny-list is punched through for `/private/tmp`, `/private/var/tmp`, `$TMPDIR`, and `~/Library/Caches` — Bazel adds those writable roots itself, and they are not configurable from the build.

Running this repository's own guard test under `darwin-sandbox` with the wrapper replaced by `/usr/bin/env` measures the consequences directly. Four guards fail:

- `writes_outside_the_test_sandbox_are_denied` — _"the test sandbox unexpectedly allowed a write to `/private/tmp/mono-hermeticity-guard-73637`"_.
- `absolute_host_executables_are_denied` — `/opt/homebrew/bin/gh` ran and printed `gh version 2.92.0`.
- `keychain_files_and_securityd_ipc_are_denied` — `/Library/Keychains/System.keychain` opened for read.
- `credentials_and_host_tools_are_not_in_the_test_environment` — see the `--test_env` note below.

With `--sandbox_default_allow_network=true` a fifth fails: a TCP connection to `1.1.1.1:53` succeeded. So `darwin-sandbox` _can_ block the network — that one property is fine. It is the write, read, and exec boundaries that are not there.

**Fact two: you cannot have both.** macOS refuses to apply a Seatbelt profile inside a Seatbelt profile, so "keep `darwin-sandbox` and add our profile on top" is not an available design. Measured directly, with a maximally permissive outer profile:

```console
$ /usr/bin/sandbox-exec -f outer.sb /usr/bin/sandbox-exec -f inner.sb /usr/bin/touch /private/tmp/probe
sandbox-exec: sandbox_apply: Operation not permitted
$ echo $?
71
```

That is the crux. macOS forces a strict either/or: Bazel's fixed permissive profile, **or** a profile of our own. Since Bazel's cannot be extended and permits the exact things that matter here, the only way to a real boundary is to give up `darwin-sandbox` and apply the profile in the wrapper. `--strategy=TestRunner=local` is not "we gave up on sandboxing on macOS"; it is "we took the sandbox over, because the platform would not let us layer one on".

The wrapper's profile is built the other way round from Bazel's: it enumerates every top-level filesystem root at action startup and denies `file-write*` in each one except the action-private root and the Bazel-declared output paths; it denies `process-exec` by default and allow-lists the runfiles tree, the audited runtime manifest, and the test entry point; it denies Keychain reads and `securityd` Mach lookups; and it denies `network*` unless the target opted in.

## What is actually enforced, and by what

| Property                             | Linux                                                                   | macOS                                                                                                              | Guard                                                                 |
| ------------------------------------ | ----------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------- |
| Writes outside the sandbox denied    | `linux-sandbox` mount namespace                                         | wrapper Seatbelt: `deny file-write*` per top-level root, minus the allow-list                                      | `writes_outside_the_test_sandbox_are_denied`                          |
| Network denied by default            | `--sandbox_default_allow_network=false` → network namespace             | wrapper Seatbelt `(deny network*)`, localhost + unix sockets still allowed                                         | `network_is_denied_by_the_test_sandbox`                               |
| Network opt-in works                 | `requires-network` tag from `network_enabled_rust_test`                 | `MONO_TEST_ALLOW_NETWORK=1` read by the wrapper                                                                    | `network_opt_in_test`                                                 |
| Credentials absent from the env      | wrapper `unset`                                                         | wrapper `unset`                                                                                                    | `credentials_and_host_tools_are_not_in_the_test_environment`          |
| Host tools unreachable               | wrapper sets `PATH` to the audited runtime only                         | same, **plus** Seatbelt `deny process-exec` — so an absolute path does not help either                             | same guard; macOS additionally `absolute_host_executables_are_denied` |
| Keychain / `securityd` unreachable   | n/a                                                                     | wrapper Seatbelt `deny file-read*` on Keychain roots + `deny mach-lookup` on securityd                             | `keychain_files_and_securityd_ipc_are_denied`                         |
| Declared output paths still writable | execroot is writable under `linux-sandbox`                              | wrapper allow-lists `TEST_UNDECLARED_OUTPUTS_DIR`, `COVERAGE_DIR`, `XML_OUTPUT_FILE`, the shard/exit/warning files | `private_and_declared_output_paths_remain_writable`                   |
| Short, action-private temp root      | `--sandbox_tmpfs_path=/tmp` + wrapper `mktemp -d /tmp/mono-test.XXXXXX` | wrapper `mktemp` under `/private/tmp`, write-protected by the profile and removed on exit                          | `linux_private_temp_root_is_short_and_action_private` (Linux only)    |

Two honest caveats on that table:

**The `--test_env=KEY=` lines do not remove anything.** `--test_env=ANTHROPIC_API_KEY=` sets the variable to the _empty string_; it does not unset it. Measured: `ANTHROPIC_API_KEY` is not set in the invoking shell, yet with the wrapper bypassed the guard still saw it present, because the flag introduced it as empty. `env::var_os` returns `Some("")`, and the guard asserts `is_none()`. The wrapper's `unset` block is what actually satisfies that guard — the `.bazelrc` lines are defence in depth and documentation of intent, nothing more. Do not delete the `unset` block on the theory that `.bazelrc` already covers it.

**The macOS private root is write-isolated, not read-isolated.** `/private/tmp/mono-test.XXXXXX` sits on the shared host `/tmp`. Every _other_ action's profile denies writing into it, and the wrapper's `EXIT`/`INT`/`TERM`/`HUP` trap removes it, but it is not a tmpfs and its contents are readable host-wide for the life of the action. (`cleanup_guard_test.sh` depends on exactly that readability to observe an inner wrapper's private root.) Linux is stronger here: `/tmp` is a genuine per-action tmpfs.

## The `SUN_LEN` problem, and why the two Linux lines are coupled

This is the least obvious thing in the directory, and the thing most likely to be "cleaned up" by someone who does not know why it is there.

A Unix domain socket path is bounded by `sockaddr_un.sun_path` — 108 bytes on Linux, 104 on macOS. It is not a `PATH_MAX`-sized field. Any test that binds a unix socket has that entire budget to work with, and a sandboxed Bazel execroot eats most of it before the test gets a say. From build 9590, a real path a test tried to bind:

```
/mnt/ssd/bazel/output_base/5b0ad2625864e133a9a94333825d4a45/sandbox/processwrapper-sandbox/3138/execroot/_main/_tmp/8224321eda7bfe4c9804f22429b3e187/tmp/.tmpwNBus1/engine.sock
```

That is **175 bytes** against a 108-byte budget. The result was `Error { kind: InvalidInput, message: "path must be shorter than SUN_LEN" }`, and downstream, `Error: engine never bound socket ...`.

Build 9590 — the first CI run of this work to land on a Linux agent, before the `test:linux` block existed — finished **84 tests pass and 20 fail**. Breaking down the 20 (verified from the log, not estimated):

- **17** socket-binding failures across `//tools/boss/cli` and `//tools/boss/engine/core` (`engine_control.rs:474` and fifteen distinct sites in `events_socket.rs`, e.g. `:593`).
- **2** hermeticity guards, discussed in the next section.
- **1** unrelated (`//tools/cube:cube_lib_test`, a `list_status_tests` assertion with nothing to do with sandboxing).

The fix is two lines that only work together:

```
test:linux --sandbox_tmpfs_path=/tmp
```

plus the wrapper's

```sh
test_tmpdir="$("${runtime_bin}/mktemp" -d /tmp/mono-test.XXXXXX)"
```

`/tmp/mono-test.XXXXXX` is 21 bytes, which leaves plenty of budget. It is short _because_ it is at the filesystem root — and it is nevertheless private to the action _because_ `--sandbox_tmpfs_path=/tmp` makes `/tmp` a fresh tmpfs inside each action's mount namespace.

> **These two lines are a single mechanism. Removing the tmpfs while keeping the short `mktemp` is strictly worse than having neither.**
>
> Without the tmpfs, `mktemp -d /tmp/mono-test.XXXXXX` still succeeds — it just creates the directory on the **shared host `/tmp`**. Every test action on the agent then writes into one shared directory, concurrent actions can see and collide with each other's state, and the wrapper's cleanup deletes real host paths. The naive "short temp root" looks like it is still working right up until it corrupts a parallel run. Nothing about the path _string_ tells you which of the two situations you are in.

That is precisely why `linux_private_temp_root_is_short_and_action_private` does not stop at checking the path prefix and length. It also reads `/proc/self/mounts` to confirm a `tmpfs` is genuinely mounted at `/tmp`, and lists `/tmp` to confirm it contains nothing but this action's own entries. The prefix check alone would keep passing after someone deleted the tmpfs line; the mount check will not.

## Fail-closed is deliberate

`test:linux` names one concrete strategy:

```
test:linux --strategy=TestRunner=linux-sandbox
```

It is **not** `sandboxed` (Bazel's alias for "the best sandbox available here"), and it is **not** a comma-separated fallback list. Both of those degrade silently to `processwrapper-sandbox` on a host that cannot provide the real thing. `processwrapper-sandbox` is not a weaker sandbox — for these purposes it is not a sandbox. Build 9590 ran under it (`218 processwrapper-sandbox` actions), and the guards recorded, on a real CI agent:

- _"the test sandbox unexpectedly allowed a write to `/var/tmp/mono-hermeticity-guard-4175665`"_
- _"the test sandbox unexpectedly allowed an external network connection: `TcpStream { addr: 192.168.1.77:38988, peer: 1.1.1.1:53 }`"_

Neither boundary. With the explicit strategy name, a host that cannot provide `linux-sandbox` stops the build instead, exactly as intended (build 9627):

```
ERROR: 'linux-sandbox' was requested for mnemonic TestRunner but no strategy with that
identifier was registered. Valid values are: [dynamic_worker, processwrapper-sandbox,
standalone, dynamic, remote, worker, sandboxed, local]
INFO: 0 processes.
ERROR: Build did NOT complete successfully
```

A loud failure on a misconfigured agent is the desired outcome. A green build that silently tested nothing is not.

`linux_policy_source_test.sh` hard-pins the exact `.bazelrc` strings so the policy cannot be quietly relaxed by an edit that "looks equivalent". Know what it does and does not cover — it pins:

- `test:linux --strategy=TestRunner=linux-sandbox`
- `test:linux --sandbox_tmpfs_path=/tmp`
- `test --sandbox_default_allow_network=false`
- `network_tags.append("requires-network")` in `defs.bzl`
- `mktemp" -d /tmp/mono-test.XXXXXX` in the wrapper
- the three `BOSS_SHAKE_*` `--test_env` lines, and their presence in the wrapper's `unset` block

It does **not** pin `--run_under`, `test:macos --strategy=TestRunner=local`, `common --enable_platform_specific_config`, or the `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GITHUB_TOKEN` / `GH_TOKEN` lines. Those are protected only by the runtime guards, which is weaker: a guard catches a regression when it runs, whereas the pin catches it at edit time.

## Host requirement

`linux-sandbox` requires unprivileged user namespaces. Ubuntu 23.10+ blocks these by default via `kernel.apparmor_restrict_unprivileged_userns=1`, which makes Bazel's sandbox-support probe fail silently at server startup so the strategy is never registered — surfacing much later as the `no strategy with that identifier was registered` error above.

The host configuration, the diagnosis procedure (including the trap that running the probe under `sudo` always succeeds and proves nothing), the remedy, the persistence drop-in, the accepted security tradeoff, and the required Bazel server restart are all in **[`.buildkite/linux-agents-runbook.md`](../../.buildkite/linux-agents-runbook.md)**. That is the operational source of truth; do not duplicate it here.

## If you are tempted to change this

| Change                                                                   | What breaks                                                                                                                                                                                                                                                                                                                               |
| ------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `test:linux` → `local` or `standalone`                                   | Removes the entire Linux boundary in one line. Tests run directly on the agent filesystem with host network. Caught by the guards — but only for as long as the guards still run.                                                                                                                                                         |
| `test:linux` → `processwrapper-sandbox` or `sandboxed`                   | Measured in build 9590: writes to `/var/tmp` succeed and `1.1.1.1:53` connects. `sandboxed` is the worse of the two because it looks like it means "sandboxed".                                                                                                                                                                           |
| Adding a comma fallback (`linux-sandbox,processwrapper-sandbox`)         | Reintroduces exactly the silent degradation the explicit strategy name exists to prevent. A misconfigured agent goes back to reporting green.                                                                                                                                                                                             |
| Dropping `common --enable_platform_specific_config`                      | `test:linux` and `test:macos` stop applying. Linux loses the sandbox and the tmpfs; macOS falls back to `darwin-sandbox`, under which the test action was measured to break outright (it fails on the runfiles `MANIFEST` before it even reaches `sandbox-exec`; had it got that far, the nested profile would have been refused anyway). |
| Removing `--sandbox_tmpfs_path=/tmp`, keeping the short `mktemp`         | Sends every test action into the **shared host `/tmp`**. Strictly worse than no scheme at all — see the coupling warning above.                                                                                                                                                                                                           |
| Tagging a test `no-sandbox` or `local`                                   | That target leaves the sandbox strategy entirely; on Linux it keeps only what the wrapper provides.                                                                                                                                                                                                                                       |
| Tagging a test `manual`                                                  | Excludes it from `//...`, so CI stops running it altogether. The guards are worth nothing if they are not in the default target set.                                                                                                                                                                                                      |
| Tagging a test `flaky`                                                   | Retries convert a real hermeticity violation — which is often load- or ordering-dependent — into an intermittent pass.                                                                                                                                                                                                                    |
| Re-pinning CI to macOS-only agents                                       | Makes the whole `test:linux` block dead code again. That is the original defect, below.                                                                                                                                                                                                                                                   |
| Deleting the wrapper's `unset` block because `.bazelrc` has `--test_env` | `--test_env=KEY=` sets the variable to empty, it does not remove it. The `unset` is load-bearing.                                                                                                                                                                                                                                         |

If one of these genuinely should change, change it _and_ update the guards and `linux_policy_source_test.sh` in the same commit, so the next reader sees a deliberate decision rather than a silent hole.

## How these gaps were found

Worth recording, because it explains why the guards are shaped the way they are.

CI runs `bazel-build-test` on the `bazel-any` queue, which is a **mixed fleet** — any Linux or macOS agent may claim the job, and nothing in `.buildkite/pipeline.yml` lets a change pin itself to a platform. The build history of the pull request that introduced this directory (mono#2435) shows what that costs:

| Build      | Agent          | Result                                                                |
| ---------- | -------------- | --------------------------------------------------------------------- |
| 9540       | macOS          | passed                                                                |
| 9569       | macOS          | failed                                                                |
| 9590       | **Linux**      | failed — first Linux landing; 84 pass / 20 fail                       |
| 9600       | macOS          | passed — **this is the build that introduced the `test:linux` block** |
| 9627       | Linux (forced) | failed — `linux-sandbox` unregistered (the AppArmor userns issue)     |
| 9632       | Linux (forced) | failed — guard defect, below                                          |
| 9636       | macOS          | passed                                                                |
| 9638, 9639 | Linux          | passed — 105/105                                                      |

Two distinct defects hide in that table.

**A guard that never runs.** The commit that added `test:linux` was validated by build 9600, which landed on a macOS agent — where every `test:linux` line is dead config. The block was merged with a green build that had never evaluated a single line of it. It took a deliberately forced Linux run (9627) to execute it for the first time, and it failed immediately.

**A guard that could not pass.** `linux_private_temp_root_is_short_and_action_private` used `Path::starts_with`, which matches whole path _components_, not string prefixes — so `Path::new("/tmp/mono-test.596u1E").starts_with("/tmp/mono-test.")` is `false`, always, on every host. From build 9632:

```
thread 'linux_private_temp_root_is_short_and_action_private' panicked at
tools/test-sandbox/hermeticity_guard_test.rs:69:5:
Linux TEST_TMPDIR must live in the per-action /tmp tmpfs: /tmp/mono-test.596u1E
```

The guard could not have passed on any host, under any configuration. It had simply never run.

> **The takeaway: a guard that never runs and a guard that cannot pass are the same defect.** These checks are worth exactly nothing unless they execute on the platform they protect. If you add a platform-conditional guard here, arrange for it to run on that platform before you believe it.

## Upstream references

- [Bazel sandboxing](https://bazel.build/docs/sandboxing) — strategies, and what each provides.
- [`--strategy`](https://bazel.build/reference/command-line-reference#flag--strategy), [`--sandbox_tmpfs_path`](https://bazel.build/reference/command-line-reference#flag--sandbox_tmpfs_path), [`--sandbox_default_allow_network`](https://bazel.build/reference/command-line-reference#flag--sandbox_default_allow_network), [`--run_under`](https://bazel.build/reference/command-line-reference#flag--run_under).
- [Test target tags](https://bazel.build/reference/be/common-definitions#common-attributes-tests) — `requires-network`, `no-sandbox`, `local`, `manual`, `flaky`.
- `sandbox-exec(1)` and Apple's Seatbelt profile language are undocumented by Apple; the profile syntax used here is the same dialect Bazel's own `darwin-sandbox` emits.

## File map

| File                                           | Purpose                                                                                                                                                                                            |
| ---------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `hermetic_test_wrapper.sh`                     | The `--run_under` wrapper. Builds the macOS Seatbelt profile, sets the audited `PATH`, unsets credentials, creates the short private temp root, forwards signals, and cleans up the process group. |
| `repositories.bzl`                             | `test_runtime_repository` — snapshots the audited host tools (`bash`, `git`, `python3`, …) into a Bazel repository so the wrapper's `PATH` contains only declared inputs.                          |
| `hermeticity_guard_test.rs`                    | The guards. Every property in the table above is asserted here. Built twice: `hermeticity_guard_test` and `xcode_capability_guard_test` (the latter with `MONO_TEST_XCODE_TOOLCHAIN=1`).           |
| `linux_policy_source_test.sh`                  | Pins the exact `.bazelrc` / wrapper / `defs.bzl` strings so the policy cannot be silently relaxed.                                                                                                 |
| `network_opt_in_test.rs`                       | Asserts the opt-in actually reaches the platform sandbox, so `network_enabled_rust_test` is not a no-op.                                                                                           |
| `defs.bzl`                                     | `network_enabled_rust_test` — the only sanctioned way to opt a target into external network access.                                                                                                |
| `cleanup_child.sh`, `cleanup_guard_test.sh`    | Assert that interrupting a test action tears down the whole process group and removes the private root, rather than leaking processes and temp dirs.                                               |
| `xcodebuild.sh`                                | `PATH` shim that refuses unless the target carries the Xcode capability marker, then redirects derived data into the private root.                                                                 |
| `macos_direct_test_runner.bzl`, `.template.sh` | An XCTest runner that stays inside the test action instead of delegating to a host process.                                                                                                        |
