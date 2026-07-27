# Bazel / Xcode toolchain pinning on macOS hosts

How bazel's Apple/Swift toolchain detection finds Xcode, why a
`mv`+symlink pin silently breaks builds, and how to repair a host
when detection returns an empty config.

This is operational knowledge for anyone maintaining macOS build hosts
(dev machines or Buildkite agents). It is not CI pipeline config; see
`.buildkite/steps/ci-env.sh` for the CI-side Xcode-drift recovery path.

## How bazel finds Xcode

Bazel's apple/swift toolchain detection
(`@bazel_tools//tools/osx` `xcode_configure` → `xcode-locator`) finds
installed Xcodes via **LaunchServices**
(`LSCopyApplicationURLsForBundleIdentifier "com.apple.dt.Xcode"`).

It does **not** use:

- Spotlight (`mdfind` / `mdls`)
- `xcode-select` / `DEVELOPER_DIR`

So a host can look perfectly healthy to every CLI probe and still have
an empty bazel Xcode config.

## Failure mode 1 — LaunchServices loses the registration

### Symptom

After moving `/Applications/Xcode.app` to a versioned path (pinning via
`mv` + symlink), LaunchServices drops the registration.
`xcode_configure` then generates an **empty**
`xcode_config(name='host_xcodes')`, and rules_swift
`xcode_swift_toolchain.bzl` fails in `_is_xcode_at_least_version` with:

```text
Could not determine Xcode version at all. This likely means Xcode isn't available.
```

Even though all of the following look correct:

- `xcode-select -p` resolves
- `xcodebuild -version` works
- Spotlight `mdfind` / `mdls kMDItemVersion` report the expected version

Verified on host `anaplian`: identical system state to a working host;
only the LaunchServices registration differed.

### Fix (no sudo)

1. Re-register the versioned Xcode bundle with LaunchServices:

   ```sh
   /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
     -f /Applications/Xcode-<ver>.app
   ```

2. Clear bazel's cached config repos in the output base so detection
   re-runs. `chmod -R u+w` first if needed, then remove:

   - `bazel_tools+xcode_configure_extension+local_config_xcode`
   - `apple_support++...local_config_apple_cc*`
   - `rules_swift++...build_bazel_rules_swift_local_config`
   - the matching `@....marker` files for each

   Or run `bazel clean --expunge` (heavier, but sufficient once
   `lsregister` has been run).

### Why one host worked and another didn't

`sudo xcodebuild -runFirstLaunch` re-registers Xcode with LaunchServices
as a side effect.

- A moved Xcode that **got** `runFirstLaunch` (host `skaffen`) detects fine.
- A moved Xcode whose `runFirstLaunch` was a no-op because components
  were already installed (host `anaplian`) stays unregistered and fails.

So `runFirstLaunch` accidentally masks this on some hosts and is not a
reliable pin step.

### Prefer `xcodes` over `mv`+symlink

Do **not** pin Xcode by `mv` + symlink. That silently breaks bazel.
Pin via [`xcodes`](https://github.com/XcodesOrg/xcodes): it installs to
a versioned path **and** registers with LaunchServices cleanly.

The move-pin also caused CoreSimulator and exit-36 confusion in the same
incident class.

### Red herrings

- The bazel lock-wait line
  (`Another command is running. Waiting for it to complete`) that often
  shows up alongside is never the failure cause.
- `bazel clean --expunge` alone does **not** fix the LaunchServices
  registration case — `lsregister -f` is also required.

## Failure mode 2 — App Store receipt + auto-update (MAS copy)

### Symptom

An App-Store-installed Xcode keeps its `_MASReceipt`. The App Store
updater tracks the app by bundle id and can update the bundle **in
place** while the folder stays named for the old pin
(e.g. folder still `Xcode-26.5.0.app`, contents already 26.6).

Bazel then asks for the pinned version (e.g. `26.5.0.17F42`) while
`xcode-locator` only finds 26.6 aliases all pointing at the
26.5-named path.

### Diagnose

```sh
test -d <app>/Contents/_MASReceipt   # present ⇒ MAS copy = time bomb
defaults read com.apple.commerce AutoUpdate
# unset means auto-update is ON
```

### Durable fix

Applied on hosts `anaplian` and `skaffen`:

1. Delete/replace the MAS copy with a receipt-free
   `xcodes install <version>` copy.
2. Disable App Store auto-update for this machine:

   ```sh
   defaults write com.apple.commerce AutoUpdate -bool false
   ```

3. Re-register:

   ```sh
   /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
     -f /Applications/Xcode-<ver>.app
   ```

4. Verify by invoking the cached locator directly:

   ```sh
   ~/Library/Caches/bazel/_bazel_*/install/*/xcode-locator 26.5.0.17F42
   ```

   (Substitute the pinned version string you need.)

## Buildkite agent maintenance notes

When pausing an agent for host repair:

```sh
bk agent pause <id> --timeout-in-minutes 1440
```

The default pause timeout is only 5 minutes. You cannot extend a live
pause — resume, then re-pause with the longer timeout.

CI checkouts live at:

```text
/opt/homebrew/var/buildkite-agent/builds/<agent>/flunge/<pipeline>
```
