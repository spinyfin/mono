# `tools/test-sandbox` — why this exists

If you know Bazel, the contents of this directory should look like too much. Bazel already sandboxes actions; a repository that writes its own Seatbelt profile, its own `--run_under` wrapper, and its own audited runtime-tool repository looks like someone who did not read the manual.

This document is the answer to that reaction. The short version: **on macOS, Bazel's test sandbox is a deny-list that permits everything this repository needs denied, and it cannot be extended — so the wrapper replaces it outright. On Linux, Bazel's sandbox is genuinely good, and the wrapper only adds the two things Bazel does not model: the process environment, and a socket-length-safe temp root.**

Everything below was measured, not assumed. Where a claim came from a CI log or a local experiment, the evidence is cited inline.

## The two layers, and why neither alone was enough

`.bazelrc` configures one wrapper for every platform, then diverges on the strategy:

```
common --enable_platform_specific_config

test --sandbox_default_allow_network=false
build:macos --strategy=TestRunner=local
build:macos --config=dev-remote-endpoint       # empty unless ~/.bazelrc fills it in, see "Remote execution from a developer's Mac"
test:linux --strategy=TestRunner=linux-sandbox
test:linux --sandbox_tmpfs_path=/tmp
test:remote-tests --strategy=TestRunner=remote   # opt-in, see "Remote execution" below
test:remote-tests --test_env=MONO_TEST_REMOTE_EXECUTION=1
test --run_under=//tools/test-sandbox:hermetic_test_wrapper
# Align build's analysis config with cquery (which inherits test --run_under
# on Bazel 9.x). Empty cquery --run_under= is invalid; without this, every
# build↔cquery flip discards the analysis cache (repobin hits this constantly).
build --run_under=//tools/test-sandbox:hermetic_test_wrapper
# Interactive runs cannot use the test wrapper (needs TEST_SRCDIR / test runtime).
run --run_under=/usr/bin/env
test --test_env=ANTHROPIC_API_KEY=          # (and six more)
```

`--enable_platform_specific_config` is what makes the `build:macos` and `test:linux` lines apply from the host platform; nothing passes `--config=` for these. The macOS strategy is a `build:` line only because of where it has to sit (see "Remote execution from a developer's Mac"); only test actions have the `TestRunner` mnemonic, so it changes nothing for a plain build. The `test:remote-tests` lines apply only when asked for, and `dev-remote-endpoint` does nothing unless the machine's own `~/.bazelrc` fills it in.

### Linux: Bazel does the isolation, the wrapper does the environment

`linux-sandbox` gives a mount namespace (only declared inputs and the execroot are visible and writable), a network namespace when the network is blocked, and a PID namespace. That is a real boundary and this repo uses it as-is.

What it does _not_ model is what is _inside_ the action: the `PATH` Bazel's own test/XML wrappers need before the test binary starts still points at host tooling, credentials that reach the action as environment variables are still set, and `TEST_TMPDIR` still lives deep inside the execroot. The wrapper fixes exactly those three things. That is its whole job on Linux.

### macOS: Bazel does nothing, because it cannot do both

`build:macos --strategy=TestRunner=local` means Bazel applies **no sandbox at all** to test actions. The wrapper's Seatbelt profile is the entire isolation boundary. That is a deliberate choice forced by two measured facts.

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

**The macOS private root is write-isolated, not read-isolated.** `/private/tmp/mono-test.XXXXXX` sits on the shared host `/tmp`. Every _other_ action's profile denies writing into it, and the wrapper's `EXIT`/`INT`/`TERM`/`HUP` trap removes it (its watchdog does when the wrapper is killed outright, see the next section), but it is not a tmpfs and its contents are readable host-wide for the life of the action. (`cleanup_guard_test.sh` depends on exactly that readability to observe an inner wrapper's private root.) Linux is stronger here: `/tmp` is a genuine per-action tmpfs.

## A killed wrapper still takes the test with it (macOS)

Linux needs nothing here: the PID namespace ends every process of the action when its command exits. macOS has no such thing, so the wrapper owns the lifetime of whatever the test starts.

The wrapper runs the test in a process group of its own, so that it can signal the whole test tree and nothing else. Its `INT`/`TERM`/`HUP` traps forward the signal to that group, and its `EXIT` trap ends the group and removes the private root. That covers a Bazel test timeout, which arrives as `SIGTERM`.

It does not cover `SIGKILL`, and that is how a test action is abandoned. When a build is interrupted, Bazel's `process-wrapper` kills the group of `test-setup.sh`. `test-setup.sh` runs its child (this wrapper) in another group again and leaves a helper behind for exactly this case; the helper then sends `SIGKILL` to the wrapper's group. A remote executor that kills an action outright has the same effect; that path was not measured. The wrapper cannot run a trap, and the test's group is in neither group that was killed.

Measured on a laptop before the watchdog existed, by sending `SIGINT` to `bazel test --runs_per_test=2` of `work_db_test`, `engine_lib_test`, `completion_test` and `driver_test` fifteen seconds into the tests: 12 test binaries were still running 45 seconds after Bazel had returned, each with parent pid 1 and leading its own process group, and 12 private roots were left in `/private/tmp`. A healthy test in that state runs to its end against a build that is over. A test that is hung never leaves: the same laptop had five such processes between 2 and 20 days old, each blocked in one deadlocked test.

So the wrapper starts a watchdog **before** the test, in a process group of its own. Two FIFOs connect them: the wrapper holds the only write end of the lifeline, and the test launcher writes its pid on the announce FIFO while it is still in the wrapper's group, then `setpgid`s and execs `sandbox-exec`. The kernel closes the write ends however the wrapper dies. A line on the lifeline means the wrapper finished its own cleanup, and the watchdog exits. End-of-file means the wrapper is gone, and the watchdog does the cleanup in its place: `SIGTERM` to the test's group, `SIGKILL` a second later, then the private root. If the announce FIFO reaches end-of-file with no pid (no process holds its write end any more, so no launcher was started or it exited without announcing), the watchdog only has to remove the private root. A launcher that is merely slow to announce still holds the write end, so a kill of the wrapper alone does not take that path; the watchdog gets the pid once it arrives. With it, the same interrupted run left one wrapper and no test two seconds after Bazel returned, and 0 processes and 0 private roots at 20 seconds (the helper in `test-setup.sh` looks every ten seconds).

Four details carry the weight:

- **The watchdog is listening before the test can run.** It writes `watchdog-ready` and the wrapper waits for that file. The fixture asserts the file exists, so a test cannot start unsupervised.
- **The watchdog has a process group of its own.** A kill aimed at the wrapper's group would otherwise take it too.
- **Neither FIFO end reaches the test.** A test descendant holding a write end would keep the watchdog waiting for as long as it lived, which is the case the watchdog exists for.
- **The wrapper reaps the watchdog before it exits.** A test that ends normally leaves no process behind, not even briefly.

What this does not reach is a process that left the test's group on purpose (`setsid`, `setpgid`). A test that starts one owns its lifetime through an owning guard (`spawn_group_leader_sleeper` / `spawn_setsid_sleeper`): Drop kills the group, and the child also exits if the pipe from the test closes, so a killed test binary still reaps it.

`cleanup_guard_test.sh` runs the wrapper around a fixture that ignores `SIGTERM`, then asserts that nothing of the run is left after a normal exit, after `TERM` and `HUP` to the wrapper, after `SIGKILL` to the wrapper's whole group once the fixture is running, and after `SIGKILL` as soon as the watchdog is listening, for `TERM`/`HUP`/`KILL` while the launcher is parked before `setpgid`, for a watchdog that is slow to become ready (the test still runs), a watchdog that dies before readiness and `TERM`/`HUP` while waiting for readiness (bounded exit, the test never starts), that `SIGINT` reaches a test that handles it (the launcher restores the default `SIGINT`/`SIGQUIT` dispositions, which a background shell command would otherwise start ignored), and that the test-only pause hooks are ignored for any path outside the supervisor's own `TEST_TMPDIR`.

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
- `test:remote-tests --strategy=TestRunner=remote`, and that it is the only strategy line on that config
- `test:remote-tests --test_env=MONO_TEST_REMOTE_EXECUTION=1`
- in `linux_remote_action_sandbox.sh`: the read-only root, the new namespaces, `--unshare-net` as the default, the `MONO_TEST_ALLOW_NETWORK` opt-in, the tmpfs `/tmp` for a test action, and the empty `/run` and `/home`
- `network_env["MONO_TEST_ALLOW_NETWORK"] = "1"` in `defs.bzl`
- `network_tags.append("requires-network")` in `defs.bzl`
- `mktemp" -d /tmp/mono-test.XXXXXX` in the wrapper
- the three `BOSS_SHAKE_*` `--test_env` lines, and their presence in the wrapper's `unset` block

It does **not** pin `--run_under`, `common --enable_platform_specific_config`, or the `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GITHUB_TOKEN` / `GH_TOKEN` lines. Those are protected only by the runtime guards, which is weaker: a guard catches a regression when it runs, whereas the pin catches it at edit time. `build:macos --strategy=TestRunner=local` and the `dev-remote` lines are pinned separately, by `dev_remote_source_test.sh` (see "Remote execution from a developer's Mac").

## Host requirement

`linux-sandbox` requires unprivileged user namespaces. Ubuntu 23.10+ blocks these by default via `kernel.apparmor_restrict_unprivileged_userns=1`, which makes Bazel's sandbox-support probe fail silently at server startup so the strategy is never registered — surfacing much later as the `no strategy with that identifier was registered` error above.

The host configuration, the diagnosis procedure (including the trap that running the probe under `sudo` always succeeds and proves nothing), the remedy, the persistence drop-in, the accepted security tradeoff, and the required Bazel server restart are all in **[`.buildkite/linux-agents-runbook.md`](../../.buildkite/linux-agents-runbook.md)**. That is the operational source of truth; do not duplicate it here.

## Remote execution on macOS workers (opt-in)

`--config=remote-tests` is two lines:

```
test:remote-tests --strategy=TestRunner=remote
test:remote-tests --test_env=MONO_TEST_REMOTE_EXECUTION=1
```

Passed on the command line together with a `--remote_executor`, the first overrides `build:macos --strategy=TestRunner=local`, and the test action runs on a remote macOS worker. The same wrapper runs on the worker, and its Seatbelt profile is still the entire boundary there. Without the flag, tests run on the local machine exactly as described above.

The second line keeps the two kinds of result apart. Bazel's action key does not record where an action ran. Before the line existed, a plain local `bazel test //tools/test-sandbox/...` straight after a remote run reported six of the eight tests in this package, `hermeticity_guard_test` among them, as `(cached) PASSED` with the durations of the remote run: results a worker had produced, on a different OS build. Nothing reads the variable; it only makes the remote test action a different action.

The strategy is `remote` alone for the same reason `test:linux` names one strategy. With no usable executor the build stops instead of running the test somewhere else. Measured with `--config=remote-tests` and no `--remote_executor`:

```
ERROR: Testing //tools/test-sandbox:linux_policy_source_test failed: TestRunner spawn cannot be
executed with any of the available strategies: [remote].
```

### What it relies on

- **The executor adds no sandbox of its own.** The farm this was built against runs NativeLink, which on macOS starts an action directly in its work directory. Seatbelt profiles do not nest (fact two, above), so an executor that wrapped actions in a profile of its own would make the wrapper's `sandbox-exec` fail, and every test with it.
- **The workers have the audited runtime where the client recorded it.** The runtime repository is configured on the client, so the paths in `manifest` and `developer_dir` are the client's. A worker needs the same system tools at the same paths and Xcode reachable at the client's `DEVELOPER_DIR`. Reachable through a symlink is enough: on the workers `/Applications/Xcode.app` is a symlink to `/Applications/Xcode-27.2.0.app`, and the wrapper allows the resolved location.
- **An unresolved symlink travels as a symlink.** Measured with Bazel 9.1.0 and NativeLink: a symlink a rule declares arrives on the worker as a symlink with the same target.
- **`USER` names the account the action runs as.** Bazel's `test-setup.sh` sets `USER` from `whoami` on the worker when the action does not carry one, and the wrapper's Keychain denial for `/Users/$USER` is built from it.

### Why the runtime tools are symlinks

`test_runtime_repository` used to expose each tool with `repository_ctx.symlink(path, "bin/" + tool)`. To Bazel such a link is a source file: it follows it, digests the contents, and uploads them. A worker therefore received a _copy_ of the client's `/bin/mkdir`, and macOS will not launch a copy of a platform binary. The first remote test run failed inside the wrapper:

```
hermetic_test_wrapper: line 66: 45844 Killed: 9   "${runtime_bin}/mkdir" -p "${HOME}" "${TMPDIR}"
```

On a worker, copies of `/bin/mkdir`, `/usr/bin/mktemp`, `/bin/rm`, `/bin/sleep`, `/usr/bin/find`, `/bin/sh` and `/usr/bin/sed` all exit 137, and the system log says `AMFI: Launch Constraint Violation (enforcing)`.

The repository rule still discovers the tools and writes `manifest` and `developer_dir`. It no longer creates `bin/`. It writes a small rule into the generated repository (`runtime.bzl`), and that rule's `:runtime` target declares each `bin/<tool>` with `ctx.actions.declare_symlink` and `ctx.actions.symlink(target_path = <resolved host path>)`. A declared symlink is shipped as a symlink, so `bin/mkdir` on a worker points at the worker's own `/bin/mkdir`. The runfiles layout the wrapper sees (`bin/<tool>`, `manifest`, `developer_dir`) is unchanged.

A copy and a symlink behave the same locally, so only a remote run would notice a change back to copies. `runtime_symlink_guard_test` closes that gap. Bazel stages a declared symlink verbatim, so `bin/<tool>` in a test's runfiles points straight at the host path the manifest records; a source file is instead linked to a path inside Bazel's own trees. The guard asserts the first shape for every manifest entry, on every platform. Run against the old repository rule it fails for all 45 tools.

### The wrapper resolves paths where the test runs

The process-exec allow-list used to be the manifest's paths, taken as recorded. The wrapper now resolves each entry on the host that executes the action, before it writes the profile:

- For a tool, `bin/<name>` and the recorded path must resolve (full `realpath`) to the same existing file. That file is what the profile allows.
- For a `*-tree` entry and for `developer_dir`, the profile allows the `realpath` of the recorded directory.
- If an entry is missing or does not match, the action fails closed with exit 126 and names the entry:

```
zz-missing: /nonexistent/zz-missing does not exist on this host
audited Bazel test runtime does not match this host
```

On a worker this produces, for example, `(allow process-exec (subpath "/Applications/Xcode-27.2.0.app/Contents/Developer/usr/libexec/git-core"))` from a manifest that records `/Applications/Xcode.app/...`. Locally the recorded paths are already final, and the profile is the one the wrapper wrote before: the generated profiles for a plain test and an Xcode-capability test were compared before and after this change and differ only in the random temp-root name.

One more input differs remotely. The wrapper also grants every executable in the target's runfiles `MANIFEST`, resolved to its physical path. A remote executor stages the runfiles tree itself and no `MANIFEST`. When that file is absent the wrapper reads the staged tree instead: files inside it are already covered by the `TEST_SRCDIR` grant, so what it adds are declared symlinks that leave the tree, such as `@checkleft_node_test_tools//:runtime`'s `bin/tar`. Nothing in this step reads a path that was recorded on the client.

### The fingerprint

The source-file symlinks carried each tool's content digest into every test action's cache key, so a host OS update invalidated cached test results. A declared symlink carries only its target path. `fingerprint` restores the property explicitly: the repository rule writes the OS build and a sha256 for every resolved tool, and the file is one of the runtime's inputs. Nothing executes or parses it. The repository rule also `watch`es each resolved tool, so replacing one under a running Bazel server refetches the repository and rewrites the file.

It describes the machine that configured the repository, which is the client. A worker's OS update does not change it. Treat a remote cache hit for a test accordingly, and use `--nocache_test_results` for any run whose point is the worker.

### The gate

**`hermeticity_guard_test` passing on a remote worker is the gate for trusting any other remote test result.** The profile is built per action, on whichever worker claims it, from that worker's filesystem. A worker that lacks a recorded tool fails closed, but only the guards show that the boundary held where a test actually ran. Before believing a remote result, run the guards the same way, uncached, and check that the process summary reports them as remote and nothing as local:

```console
$ bazel test //tools/test-sandbox/... --config=remote-tests --remote_executor=... --nocache_test_results
INFO: 21 processes: 314 action cache hit, 13 internal, 16 remote.
//tools/test-sandbox:hermeticity_guard_test                              PASSED in 0.3s
```

Two limits of a remote run are worth knowing:

- A run lands on whichever worker the scheduler picks. One green run says nothing about the other workers.
- Timers fire late on the workers this was measured against. In a test action there, a 50 ms sleep returned after a median of about 180 ms and at most 200 ms, against 55 ms on the client, while a busy loop in the same action got a full CPU. A test that needs real timers to keep a cadence of tens of milliseconds can fail remotely for that reason alone, which says nothing about the sandbox.

### Host tools a single test declares

`@host_lsof//:lsof` and `@worker_zsh//:zsh` are system binaries that one or two tests declare in `data`, kept out of the shared runtime so no other test can run them. Both were `repository_ctx.symlink` links and failed remotely for the reason above, in two different ways:

- `//tools/cube:cube_lib_test` runs the runfile itself. The copy of `/usr/sbin/lsof` was killed at launch, and the test saw an exit status of `None`.
- The worker shell tests run `/bin/zsh` by that path and declare `@worker_zsh//:zsh` only so the wrapper grants it. Locally the runfile resolved to `/bin/zsh`. On a worker it was a file inside the runfiles tree, nothing granted the real shell, and the tests failed with `/bin/sh: /bin/zsh: Operation not permitted`.

`host_tool_symlink` in `repositories.bzl` now generates both repositories. The label is a rule target that declares one symlink to the absolute host path, so it reaches a worker as a symlink that leaves the runfiles tree, and the wrapper grants its resolved target there as it does for the runtime's `bin/` links. Visibility is unchanged, and so is each repository's behaviour where the tool is absent: `host_lsof` still writes its stub script off macOS, and on a host without `/bin/zsh` `@worker_zsh//:zsh` is still a dangling link, which only macOS targets select.

Each repository also writes a `fingerprint` (OS build and sha256 of the tool) that travels in the target's runfiles, so a test's cache key still follows the binary's contents. It is not in the target's `files`, because consumers expand `$(rootpath ...)` on the label and that needs a single file.

`host_tool_symlink_guard_test.sh` is the local check for both, run as `//tools/cube:host_lsof_symlink_guard_test` and `//tools/boss/engine/worker-bin:worker_zsh_symlink_guard_test`: the runfile must be a symlink straight to the host path.

### The per-user temp directory

Foundation stages an atomic write in `<per-user temp dir>/TemporaryItems/NSIRD_<process>_<id>`, whatever `TMPDIR` says, and the Xcode capability allows exactly `TemporaryItems/NSIRD_xctest_*` there. The wrapper used to take that directory from the `TMPDIR` it was started with. Bazel gives a local action the invoking user's `TMPDIR`, which on macOS is the per-user temp directory. A remote action has only the environment it declares:

- a test that sets only the capability, like `xcode_capability_guard_test`, has no `TMPDIR`;
- an Apple test target carries `XCODE_TEST_ENV`, so its `TMPDIR` is the literal `__MONO_TEST_PROCESS_TMPDIR__`. Measured on a worker, at wrapper entry: `TMPDIR=[__MONO_TEST_PROCESS_TMPDIR__]`.

Either way no usable exception was written, every atomic write inside the test's own private root failed with `NSCocoaErrorDomain Code=513` / `Operation not permitted`, and seven Apple test targets failed remotely.

When the capability is set and `TMPDIR` is not an absolute path, the wrapper now asks the executing host with `os.confstr(65537)` (`_CS_DARWIN_USER_TEMP_DIR`, what `getconf DARWIN_USER_TEMP_DIR` prints) and fails the action closed with exit 126 if that yields no directory. The exception is the same pattern as before and nothing else is opened. An absolute `TMPDIR` is used exactly as before, so a local profile is unchanged. `MONO_TEST_HOST_TMPDIR` carries the same value, so on a worker `xcode_capability_retains_the_write_allowlist` and `TestSandboxPolicyTests` now probe the real directory and assert that an ordinary write to it is denied.

### Not yet remote-capable

The audited runtime (`@test_runtime_tools`, and `@checkleft_node_test_tools`, which the same rule generates), `host_lsof` and `worker_zsh` were converted. These repositories still hand a test a copy of a client binary:

- `host_tmux`, `host_claude` and `codex_probe_runtime` in `repositories.bzl`. A copy of a system binary (the Codex probe's login shell is usually `/bin/zsh`) will not launch on a worker. A copy of anything else is the client's build, not the worker's.
- `jj_test_tools` and `cube_jj_newer` ship a pinned download rather than a host binary.

A test tagged `exclusive` cannot be scheduled remotely at all, because the tag also forces local execution. `exclusive-if-local` keeps the serial run locally and lets the test go to a worker.

The list above is about macOS workers. Linux workers are a separate arrangement with its own boundary; see the next section.

## Remote execution on Linux workers

On Linux the boundary around a test is Bazel's local `linux-sandbox`, and a test that executes remotely is not inside it. A remote executor has to provide an equivalent, and CI's Linux workers do: each one runs every action, build or test, through `linux_remote_action_sandbox.sh`, installed as the worker's per-action entrypoint. `ci-remote-linux` in `.ci.bazelrc` selects `remote-tests` on that basis, and it is the only thing that does. A plain `bazel test` on a Linux machine, and a CI step that builds on its agent, still get `test:linux --strategy=TestRunner=linux-sandbox`.

The wrapper is unchanged and does the same three things inside the worker's sandbox that it does inside `linux-sandbox`: the audited `PATH`, the credential `unset`, and the short private temp root under `/tmp`.

### What the worker's sandbox enforces

It is one `bwrap` (bubblewrap) invocation per action, using unprivileged user namespaces, so it needs no root and no setuid helper.

| Property                          | `linux-sandbox` on an agent                                 | The worker's sandbox                                                                                   | Guard                                                 |
| --------------------------------- | ----------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ | ----------------------------------------------------- |
| Writes outside the action denied  | mount namespace, execroot writable                          | `--ro-bind / /`, then only the action's own directory bound writable                                   | `writes_outside_the_test_sandbox_are_denied`          |
| Network denied by default         | `--sandbox_default_allow_network=false` → network namespace | `--unshare-net`: a new network namespace with loopback only                                            | `network_is_denied_by_the_test_sandbox`               |
| Network opt-in works              | `requires-network` tag                                      | the action's environment carries `MONO_TEST_ALLOW_NETWORK=1`, so the script leaves `--unshare-net` out | `network_opt_in_test`                                 |
| Short, action-private temp root   | `--sandbox_tmpfs_path=/tmp`                                 | `--tmpfs /tmp` for a test action (one whose environment has `TEST_TMPDIR`)                             | `linux_private_temp_root_is_short_and_action_private` |
| Leftover processes do not survive | PID namespace                                               | `--unshare-pid`: the namespace is torn down when the action's command exits                            | `cleanup_guard_test` covers the wrapper's own part    |
| The user's home and daemons       | visible, read-only                                          | `/home` and `/run` are empty tmpfs mounts; only the DNS stub configuration is put back                 | none                                                  |

Three things about that table:

- **The opt-in is the environment variable because a worker never sees a tag.** Bazel reads `requires-network` itself, to configure its own sandbox, and does not send tags to a remote executor. `MONO_TEST_ALLOW_NETWORK=1` is in the action's environment, which the worker's entrypoint inherits. It is the same variable the wrapper reads on macOS, and `network_enabled_rust_test` sets it together with the tag, so one macro covers a local Linux run, a remote Linux run and macOS. `linux_policy_source_test.sh` pins that the macro still sets it and that the script still reads it.
- **`/home` and `/run` go further than `linux-sandbox` does.** A worker runs as a login user, not as a dedicated service account. Read-only is not enough for `/run`: it holds the sockets of that user's D-Bus and systemd instances and of the ssh and gpg agents, and a read-only mount does not stop a `connect()`.
- **An action that is not a test gets a `/tmp` on disk**, a directory inside the action directory that the worker deletes with it, so that a compiler's temporary files do not count against the host's RAM. It is as private as the tmpfs. The guard only ever runs as a test, so it sees the tmpfs.

The script also sets `umask 022` before it starts the action. A worker runs in its user's systemd session, where the umask is `002` on a distribution with per-user groups, and an action would otherwise create group-writable directories. The `spoke-package` tests found this: the installer refuses a staging directory that others can write, and every one of its tests that creates one failed on a worker until the umask matched an agent's.

The sandbox fails closed. If `bwrap` is missing, or a namespace cannot be created, the action fails; nothing runs unsandboxed.

### The gate

**The sandbox is not part of any action's cache key, and nothing in a build proves a worker has it.** A worker registered without the entrypoint would run every test with no isolation and report the same green result. The guards are the only evidence, and they are evidence about the worker they ran on. So the rule from the macOS section applies per worker: `hermeticity_guard_test` and `network_opt_in_test` passing, uncached, on a given worker is the gate for trusting any test result from that worker.

Each worker declares a `worker-host` platform property with its own name (`.buildkite/linux-agents-runbook.md`, "Remote execution workers"). Adding that property to a remote run pins every action of the run to the one worker:

```console
$ bazel test //tools/test-sandbox/... <the flags ci-remote-linux expands to, with the endpoint> \
    --remote_default_exec_properties=worker-host=<name> --nocache_test_results
INFO: 341 processes: 326 internal, 24 remote.
//tools/test-sandbox:hermeticity_guard_test                              PASSED in 0.1s
//tools/test-sandbox:network_opt_in_test                                 PASSED in 0.0s
```

Check that the summary reports the actions as `remote` and none as `linux-sandbox` or `local`. Run it against every worker after installing or changing the sandbox script, and against a new worker before it joins the endpoint the agents use. The property changes every action's cache key, so the run also rebuilds the tests' dependencies on that worker, inside the sandbox.

`network_opt_in_test` matters as much as the denial: it fails if the opt-in did not reach the sandbox, which is what would happen if the script stopped reading the variable or a worker ran a different script.

### What is different from a test on an agent

- **The test runs as the worker's login user**, in a user namespace that maps that one user. Files owned by anyone else show up as owned by `nobody`.
- **Runfiles are real files in a real tree**, not symlinks into an execroot.
- **Timers and CPU are shared with whatever else the worker is running**, which includes other tests and compiles up to its slot count, and the CI agent on the same host.
- **A copied host tool runs.** `host_tmux`, `host_claude` and `codex_probe_runtime` hand a test a copy of the client's binary. On a Linux worker that is the same distribution's binary and it launches, unlike on a Mac, though it is still the client's build and not the worker's.

## Remote execution from a developer's Mac

`remote-tests` is for a caller that passes its own flags, which is what CI does. A developer's Mac can instead build and test on the workers by default, with nothing on the command line and no wrapper script, by adding three lines to its own `~/.bazelrc`:

```
build:dev-remote-endpoint --remote_executor=grpc://<host>:<port>
build:dev-remote-endpoint --remote_cache=grpc://<host>:<port>
build:dev-remote-endpoint --config=dev-remote
```

Bazel reads `~/.bazelrc` after the repository's `.bazelrc` and adds up the lines for one config across rc files. `.bazelrc` selects `dev-remote-endpoint` from a `build:macos` line and gives it two empty defaults, so a machine without those lines builds exactly as before. That includes every CI agent, where `.buildkite/steps/remote-execution.sh` decides per step and passes its own flags on the command line. The endpoint is machine configuration and is not in the repository. A checkout whose `.bazelrc` predates the config never selects it, and neither does any other repository, so the three lines are inert there. Commenting them out takes the machine back to local builds.

If the endpoint advertises zstd compression, a fourth line makes blobs travel compressed between the Mac and the remote cache:

```
build:dev-remote-endpoint --remote_cache_compression
```

It goes with the endpoint, not in `dev-remote`, because Bazel 9.1 does not quietly fall back to uncompressed transfers when the server lacks support: the first action that touches the remote cache fails with `--remote_cache_compression requested but remote does not support compression`.

The third line selects `dev-remote`, which `.bazelrc` defines:

```
build:dev-remote --disk_cache=~/.cache/bazelcache-remote
build:dev-remote --grpc_keepalive_time=30s
build:dev-remote --grpc_keepalive_timeout=20s
build:dev-remote --workspace_status_command=tools/remote-execution/dev-remote-workspace-status.sh
build:dev-remote --remote_local_fallback
build:dev-remote --noremote_upload_local_results
test:dev-remote --strategy=TestRunner=remote,local
test:dev-remote --test_env=MONO_TEST_REMOTE_EXECUTION=1
```

What each line is for, and what was measured with Bazel 9.1.0 against NativeLink workers:

- **`--disk_cache=~/.cache/bazelcache-remote`: a disk cache of its own, never the shared one.** Bazel does not download what a cache hit produced, and it assumes a remote action can be handed any such output from the remote cache. An output that a local build put in the disk cache is not there, and the remote action fails with `Lost inputs no longer available remotely`. Bazel restarts the build by itself, but the restart goes back to the disk cache and loses a different input. Measured with the shared `~/.cache/bazelcache` in front, on a laptop where it was well used: `bazel test //...` ended in that error six times in a row and Bazel gave up after its five restarts; three of the four lost files checked were in the laptop's disk cache and absent from the remote cache. Emptying the shared directory would not last, because local builds keep writing to it: checkouts that predate this config, `--config=dev-local`, other repositories. One local build into an otherwise clean directory was enough to bring the error back. The separate directory only ever holds what a remote-mode build downloaded, and the remote cache has that too.
- **That disk cache does little.** Bazel stores only what it downloads, and with remote execution it downloads the final outputs. In a fully cached clean build of two binaries it served 8 of 935 cache hits, and in a fully cached clean run of the whole test suite 732 of 4,173; the rest still came from the remote cache. What it saves is downloading a final output, a test binary for example, a second time. Two runs of the whole suite left about 5 GB in it.
- **Two cases still restart once.** The first remote build in a checkout that has built locally, because its own action cache remembers outputs of the shared disk cache. And a build that needs, remotely, an action the Mac ran itself while in remote mode (after a fallback part-way through a build, say): that result is written to the remote-mode directory and, by the upload rule below, nowhere else. In both the restart re-runs the action and the build succeeds.
- **The default disk cache is a `common` line** above `common --enable_platform_specific_config`, for the reason the macOS strategy is a `build:` line, below: as a plain `build` line it would be applied after `dev-remote` and put the shared directory back.
- **`--remote_local_fallback`: an unreachable endpoint is not an error.** The build runs on the Mac. Bazel finds out once, when the build starts: a refused connection costs a few seconds, a host that does not answer at all about half a minute.
- **The fallback also covers an action the executor gives up on, and not every executor that stops answering.** The scheduler fails an action it cannot run or keep running (no worker connected, a worker lost too often, too many retries) within minutes, and Bazel then runs that action on the Mac: measured with every worker frozen, two minutes later. A hub that accepts connections and says nothing is different. If it is in that state when the build starts, Bazel gives up on it after six tries of `--remote_timeout` each, about six minutes, and builds on the Mac. If it stops answering part-way through, keepalive does not end the wait: measured with keepalive on at Bazel 9.1's 60s default, a build was still waiting 17 minutes after the hub froze, and carried on by itself when the hub ran again. `--grpc_keepalive_time=30s` is still set so the value lives in this file; the bound is `dev-remote`'s workspace-status command, which repeats the hub's status request and stops the in-flight bazel command if the hub accepted a connection and then does not answer. `--config=dev-local` restores the default stamp script so the watchdog does not run. Re-run with `--config=dev-local` to finish on the Mac. [`tools/remote-execution/README.md`](../remote-execution/README.md), "What bounds a stuck action", has the whole table.
- **A fallback build has no cache at all.** This would be so even with a disk cache configured: when the endpoint is unreachable Bazel switches off its whole remote module for that build, and `--disk_cache` is part of it. A machine that will be away from the workers for more than a build or two should comment the three lines out, which brings the shared disk cache back, rather than rely on the fallback.
- **The first build without the workers rebuilds what was never downloaded.** Only top-level outputs are downloaded, so the intermediate outputs of earlier remote builds exist only on the workers. The build logs `Lost inputs no longer available remotely` and an error, then Bazel starts it again by itself and rebuilds those outputs on the Mac.
- **`--noremote_upload_local_results`: what the Mac ran itself stays on the Mac.** The result of an action that ran locally, after a fallback or because it cannot run remotely, is not uploaded to the shared cache. Without the flag it is uploaded: a `no-remote-exec` action built on one machine was a remote cache hit from an empty disk cache. A test result a developer's Mac produced could then satisfy a CI step that is meant to accept only what a worker ran.
- **`remote,local`: a test that cannot go to a worker still runs.** `remote-tests` names `remote` alone so that a CI step fails rather than accept a local run. A developer's own run is the opposite case: a test tagged `local`, `no-remote` or `exclusive` should run the way it does without the config, on the Mac under the wrapper's Seatbelt profile. That is acceptable on macOS only. On Linux `local` is no sandbox at all, so `dev-remote` must stay reachable through `build:macos` alone: never pass `--config=dev-remote` by hand, and never select it from a repository rc line.
- **`MONO_TEST_REMOTE_EXECUTION=1`** is the line `remote-tests` has, for the same reason. It also gives a developer's remote run the key a CI remote step uses.

The macOS strategy is `build:macos --strategy=TestRunner=local`, not a `test:macos` line, because of where `dev-remote` lands. Within one config Bazel applies every `build:` line before any `test:` line, whatever their order in the file. `dev-remote` is reached from a `build:macos` line, so its strategy sits among the `build:macos` lines, and a `test:macos` strategy would always be applied after it and replace it. Tests on an opted-in Mac would run locally again, with nothing to say so.

To run one command on the Mac itself, for example to see whether a test that fails on a worker fails locally too, add `--config=dev-local`. It clears the endpoint, so the `remote,local` strategy runs the test locally, it names the default disk cache again, and it removes `MONO_TEST_REMOTE_EXECUTION`, so the result is recorded under the key every other local run uses: measured, a result produced this way was a cache hit for the same test run with no `~/.bazelrc` at all, and was not one for the default remote run. It names no strategy, which keeps it harmless on Linux.

`dev_remote_source_test.sh` pins these lines, their order, and the absence of an endpoint and of any other selection of `dev-remote` in the repository's rc files. An ordinary build has no endpoint and exercises none of it.

Everything under "What it relies on" applies with the Mac as the client: the workers need the audited runtime at the paths the Mac recorded, Xcode reachable at the Mac's `DEVELOPER_DIR`, and the Xcode version the Mac builds with. The tests listed under "Not yet remote-capable" receive copies of the Mac's own binaries.

## If you are tempted to change this

| Change                                                                   | What breaks                                                                                                                                                                                                                                                                                                                                          |
| ------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `test:linux` → `local` or `standalone`                                   | Removes the entire Linux boundary in one line. Tests run directly on the agent filesystem with host network. Caught by the guards — but only for as long as the guards still run.                                                                                                                                                                    |
| `test:linux` → `processwrapper-sandbox` or `sandboxed`                   | Measured in build 9590: writes to `/var/tmp` succeed and `1.1.1.1:53` connects. `sandboxed` is the worse of the two because it looks like it means "sandboxed".                                                                                                                                                                                      |
| Adding a comma fallback (`linux-sandbox,processwrapper-sandbox`)         | Reintroduces exactly the silent degradation the explicit strategy name exists to prevent. A misconfigured agent goes back to reporting green.                                                                                                                                                                                                        |
| Dropping `common --enable_platform_specific_config`                      | `test:linux` and the `build:macos` lines stop applying. Linux loses the sandbox and the tmpfs; macOS falls back to `darwin-sandbox`, under which the test action was measured to break outright (it fails on the runfiles `MANIFEST` before it even reaches `sandbox-exec`; had it got that far, the nested profile would have been refused anyway). |
| Removing `--sandbox_tmpfs_path=/tmp`, keeping the short `mktemp`         | Sends every test action into the **shared host `/tmp`**. Strictly worse than no scheme at all — see the coupling warning above.                                                                                                                                                                                                                      |
| Tagging a test `no-sandbox` or `local`                                   | That target leaves the sandbox strategy entirely; on Linux it keeps only what the wrapper provides.                                                                                                                                                                                                                                                  |
| Tagging a test `manual`                                                  | Excludes it from `//...`, so CI stops running it altogether. The guards are worth nothing if they are not in the default target set.                                                                                                                                                                                                                 |
| Tagging a test `flaky`                                                   | Retries convert a real hermeticity violation — which is often load- or ordering-dependent — into an intermittent pass.                                                                                                                                                                                                                               |
| Re-pinning CI to macOS-only agents                                       | Makes the whole `test:linux` block dead code again. That is the original defect, below.                                                                                                                                                                                                                                                              |
| Deleting the wrapper's `unset` block because `.bazelrc` has `--test_env` | `--test_env=KEY=` sets the variable to empty, it does not remove it. The `unset` is load-bearing.                                                                                                                                                                                                                                                    |
| Exposing `bin/<tool>` with `repository_ctx.symlink` again                | Nothing locally. On a remote macOS worker the tools arrive as copies and are killed at launch, so every test fails in the wrapper. `runtime_symlink_guard_test` fails locally first.                                                                                                                                                                 |
| Adding a fallback to `test:remote-tests` (`remote,local`)                | A test that cannot run remotely runs under a local strategy instead and still reports a result. Pinned by `linux_policy_source_test.sh`.                                                                                                                                                                                                             |
| Moving the macOS strategy back to a `test:macos` line                    | It is applied after `dev-remote`'s strategy and replaces it. On a developer's Mac that names an endpoint every test runs locally again and nothing says so. Pinned by `dev_remote_source_test.sh`.                                                                                                                                                   |
| Selecting `dev-remote` from a repository rc line, or by hand             | Its `remote,local` test strategy reaches machines it was not written for. On Linux the `local` half is no sandbox at all. Pinned by `dev_remote_source_test.sh`.                                                                                                                                                                                     |
| Dropping `--noremote_upload_local_results` from `dev-remote`             | A result a developer's Mac produced itself is uploaded under the key CI's remote steps use, and a CI step can be satisfied by it. Pinned by `dev_remote_source_test.sh`.                                                                                                                                                                             |
| Exposing a single host tool with `repository_ctx.symlink` again          | Nothing locally. On a remote macOS worker `@host_lsof//:lsof` is killed at launch and `/bin/zsh` is denied. `host_lsof_symlink_guard_test` and `worker_zsh_symlink_guard_test` fail locally first.                                                                                                                                                   |
| Taking the host temp directory from `TMPDIR` alone                       | Nothing locally. A remote Apple test action gets no `NSIRD_xctest_` exception, and every atomic write in its private root fails.                                                                                                                                                                                                                     |
| Widening the `NSIRD_xctest_` exception to the whole temp directory       | Every Xcode-capability test can write a shared host directory. `xcode_capability_retains_the_write_allowlist` and `TestSandboxPolicyTests` fail.                                                                                                                                                                                                     |
| Dropping `fingerprint` from the runtime's files                          | Cached test results survive an OS or Xcode update, because the declared symlinks only carry a path.                                                                                                                                                                                                                                                  |
| Allowing the manifest's paths without resolving them in the wrapper      | On a host where a recorded path is a symlink, Seatbelt sees the physical path and denies the audited tool. Allowing a path that was never checked against `bin/<name>` also drops the fail-closed check for a missing or substituted tool.                                                                                                           |

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

| File                                           | Purpose                                                                                                                                                                                                                          |
| ---------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `hermetic_test_wrapper.sh`                     | The `--run_under` wrapper. Builds the macOS Seatbelt profile, sets the audited `PATH`, unsets credentials, creates the short private temp root, forwards signals, and cleans up the process group.                               |
| `repositories.bzl`                             | `test_runtime_repository` — resolves the audited host tools (`bash`, `git`, `python3`, …) and declares them as symlinks, so the wrapper's `PATH` contains only declared inputs.                                                  |
| `runtime_symlink_guard_test.sh`                | Asserts every `bin/<tool>` of the runtime is a symlink straight to its audited host path, so a change back to shipped copies is caught without a remote run.                                                                     |
| `host_tool_symlink_guard_test.sh`              | The same assertion for one host tool that `host_tool_symlink` declares; the packages allowed to use `@host_lsof` and `@worker_zsh` run it.                                                                                       |
| `hermeticity_guard_test.rs`                    | The guards. Every property in the table above is asserted here. Built twice: `hermeticity_guard_test` and `xcode_capability_guard_test` (the latter with `MONO_TEST_XCODE_TOOLCHAIN=1`).                                         |
| `linux_policy_source_test.sh`                  | Pins the exact `.bazelrc` / wrapper / `defs.bzl` strings so the policy cannot be silently relaxed.                                                                                                                               |
| `linux_remote_action_sandbox.sh`               | The per-action sandbox a Linux remote worker runs every action in. The workers install a copy; this file is the one they must match.                                                                                             |
| `dev_remote_source_test.sh`                    | Pins the `.bazelrc` lines behind remote execution from a developer's Mac: their configs, their order, and that no endpoint or other selection of `dev-remote` is in the repository.                                              |
| `network_opt_in_test.rs`                       | Asserts the opt-in actually reaches the platform sandbox, so `network_enabled_rust_test` is not a no-op.                                                                                                                         |
| `defs.bzl`                                     | `network_enabled_rust_test` — the only sanctioned way to opt a target into external network access.                                                                                                                              |
| `cleanup_child.sh`, `cleanup_guard_test.sh`    | Assert that ending a test action tears down the whole test process group and removes the private root, rather than leaking processes and temp dirs. Covers a normal exit, an interrupted wrapper, and a wrapper killed outright. |
| `xcodebuild.sh`                                | `PATH` shim that refuses unless the target carries the Xcode capability marker, then redirects derived data into the private root.                                                                                               |
| `macos_direct_test_runner.bzl`, `.template.sh` | An XCTest runner that stays inside the test action instead of delegating to a host process.                                                                                                                                      |
