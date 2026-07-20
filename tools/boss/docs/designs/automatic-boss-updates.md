# Design: Automatic Boss updates

- Status: **Shipped** — updated 2026-07-20 to reflect as-built reality after the T1–T8 implementation PRs (#911, #926, #927, #953, #954, #956, #978, #996) and the follow-on fix/polish PRs (#1086, #1226, #1238, #1855) they triggered.
- Owner: Boss
- Related: [`installable-distribution-package-for-boss.md`](installable-distribution-package-for-boss.md) (this is the deferred "Auto-update / Sparkle / a release feed" follow-on it names), [`buildkite-release-setup.md`](../buildkite-release-setup.md)
- Implementation history: see [Implementation history](#implementation-history) at the end for the task→PR mapping and what diverged.

## Context

Boss is distributed as a macOS `.app` bundle. The Buildkite release pipeline (`.buildkite/steps/boss-release.sh`) cuts a GitHub Release on `spinyfin/mono`, tagged `boss-v1.0.N`, with a single asset `Boss-1.0.N.zip` — a zipped, codesigned `Boss.app`. Users today have no in-app way to learn a newer build exists; they must notice the release manually and re-download. The installable-package design explicitly deferred auto-update to a follow-on project — this is that project.

This document describes the in-app self-update mechanism with three settings-gated modes (manual check, periodic badge notification, automatic background install). It polls the **unauthenticated** GitHub REST API — no token, no `gh` dependency.

### What was verified against the live repo and codebase

These facts, checked at design time (May 2026), shaped the design:

- **`spinyfin/mono` is publicly readable unauthenticated.** `GET https://api.github.com/repos/spinyfin/mono/releases` returns `HTTP 200` with no `Authorization` header, and the response carries `X-RateLimit-Limit: 60` (the unauthenticated per-IP core limit). The unauthenticated premise is sound _as long as the repo stays public_ (see Risks).
- **Releases are tagged `boss-v1.0.N`**, titled `Boss 1.0.N`, with one asset `Boss-1.0.N.zip` (~34 MB) downloadable at `https://github.com/spinyfin/mono/releases/download/boss-v1.0.N/Boss-1.0.N.zip`. The asset is a zipped `Boss.app` bundle produced by `bazel build //tools/boss/app-macos:Boss`.
- **`mono` is a monorepo with multiple release lines.** Alongside `boss-v*` it carries `checkleft-v*` tags (e.g. `checkleft-v0.1.0-alpha.8`). `GET /releases/latest` returns whichever line released most recently — it happened to be `boss-v1.0.27` when I checked, but a later `checkleft` release would make `/releases/latest` return a non-Boss tag. **`/releases/latest` is therefore unsafe; we must list releases and filter by the `boss-v` prefix.**
- **Boss tags are not returned in version order.** The `/releases` listing is sorted by publish date, and observed ordering interleaves (`…1.0.27, 1.0.26, …, 1.0.9, 1.0.8, 1.0.18, 1.0.17…`). We must parse _all_ `boss-v` tags and pick the maximum, not take the first.
- **Some releases lack the asset.** `boss-v1.0.21` exists with no `Boss-1.0.21.zip`. The updater must treat "newest tag that actually has a downloadable asset" as the target and skip assetless releases.
- **The running version is in the bundle Info.plist.** `pkg.bzl`'s `boss_short_version_plist` stamps `CFBundleShortVersionString = CFBundleVersion = "1.0.N"` (numeric, even on dev builds — it uses `STABLE_BOSS_BASE_VERSION`) and `BossFullVersion = "1.0.N"` on a release tag or `"1.0.N-dev-<sha>"` on a dev build (`STABLE_BOSS_VERSION`). `BossMacApp.swift:24-31` already reads `BossFullVersion` for the About panel.
- **The published zip is _not_ notarized.** `boss-release.sh` zips the bazel build output directly; only the separate `installer/release.sh` (`.pkg` path) runs `codesign`/`notarytool`/`stapler`, and that `.pkg` is **not** what GitHub Releases publishes. This is fine for v1: the updater replicates the existing manual workflow (terminal `curl` + unzip + copy), which also works without notarization because command-line downloads do not set `com.apple.quarantine`. See §4 for how quarantine-stripping makes un-notarized releases work reliably.
- **The app may live in `~/Applications`, not `/Applications`.** `installer/release.sh` runs `pkgbuild --install-location ~/Applications` with the `currentUserHomeDirectory` domain ("install for me only", no admin password). The updater must resolve its own location via `Bundle.main.bundleURL` rather than hard-coding `/Applications`.
- **The engine is a bundled sub-binary.** `EngineProcessController` launches `<Bundle.main.resourcePath>/bin/engine` detached and owns its lifecycle. Swapping the app bundle replaces that binary too, so the swap must coordinate an engine restart.
- **Settings today are engine-side and boolean-only.** `engine/src/settings.rs` defines a static `REGISTRY` of boolean `SettingSpec`s persisted to `<state_root>/settings.toml` and surfaced via RPC in `SettingsView.swift`. The app separately uses `@AppStorage` for app-local UI state (e.g. `boss.activity.visible` in `BossMacApp.swift:108`).

---

## Goals

1. **Detect** when a newer Boss release exists in `spinyfin/mono`, using only the unauthenticated GitHub REST API.
2. **Three settings-gated modes**, matching the project brief:
   - **Manual check** — a "Check for Updates…" app-menu item that checks on demand and reports the result.
   - **Periodic badge notification** — interval polling that surfaces a badge/button in the main window's top-right chrome when an update is available; clicking it offers download/install.
   - **Automatic install** — a "Automatically install updates" toggle that downloads new releases in the background as they appear, into `~/Library/Application Support`, and swaps the installed bundle at the next safe boundary (quit or startup, whichever succeeds first).
3. **Respect the 60-req/hour unauthenticated rate limit** with a conservative interval and conditional requests.
4. **Safe install on macOS** — preserve code-signing/notarization validity, never leave the user with a broken/half-swapped bundle, and require no admin password in the common (`~/Applications`) case.
5. **Graceful failure** — network errors, partial downloads, failed swaps, and a new build that won't launch must all degrade to "keep running the current version" with a clear, non-blocking signal.

## Non-goals

- **Delta/binary-diff updates.** We download the full `Boss-1.0.N.zip` (~34 MB). No bsdiff/courgette.
- **Updating the engine or workers independently of the app.** The engine ships inside the app bundle; it is replaced atomically with the app. No separate engine update channel.
- **Downgrade / pin-to-version / channel selection (beta vs stable).** Only "is there a newer `boss-v1.0.N`" matters. No release channels in v1.
- **Auto-updating dev builds.** A build whose `BossFullVersion` contains `-dev-` is a local/unreleased build; the updater will _report_ availability but never auto-install over it (see Failure handling).
- **A general-purpose updater framework** reusable by `checkleft` or other monorepo products. This is Boss-specific; a shared abstraction can come later if a second product needs it.
- **Changing the release pipeline's tag/version scheme or assets.** We consume `boss-v1.0.N` + `Boss-1.0.N.zip` exactly as they exist today; no pipeline changes are required for v1. Notarizing the zip is a potential future improvement, not a prerequisite.
- **Push notifications / server-initiated updates.** Detection is pull-only polling.

---

## Alternatives considered

### Alternative A — Adopt Sparkle

[Sparkle](https://sparkle-project.org/) is the de-facto macOS auto-update framework. It handles the appcast feed, download, signature verification (EdDSA), the in-place swap via a separate `Autoupdate`/`Installer` XPC helper, and relaunch — exactly the hard parts.

**Why not chosen (for v1):**

- **Feed mismatch.** Sparkle consumes an _appcast_ XML feed, not the GitHub Releases API. We'd have to generate and publish an appcast (e.g. via `generate_appcast` to GitHub Pages or a gh-pages branch) as a _new_ pipeline artifact. That's a real release-pipeline change and a second source of truth to keep in sync with `boss-v1.0.N`.
- **Signing model mismatch.** Sparkle wants an EdDSA signature over each archive, with the public key embedded in the app, plus its own key-management. Boss already relies on Apple Developer-ID + notarization for trust; adding a parallel EdDSA scheme is more surface, not less.
- **UI mismatch.** The project specifies a _custom_ 3-mode model and a badge in the window chrome. Sparkle's built-in UI is a modal "A new version is available" sheet; bending it to our chrome-badge + background-auto-install model means using Sparkle's lower-level API anyway, which erodes the "it does it for you" benefit.
- **Dependency weight & SPM.** Adds a sizable third-party dependency (and its XPC helper, which must itself be signed/sandboxed correctly) to an app that currently has exactly one SPM dependency (`textual`).

Sparkle remains the right call _if_ we later want robust staged rollouts, phased percentages, and a battle-tested in-place swapper. It is explicitly revisitable (see Open questions). For v1 the custom approach is a better fit for the GitHub-Releases-as-feed + custom-UI requirements, and reuses trust infrastructure we already have.

### Alternative B — Re-run the installer `.pkg` (delegate the swap to `installer`)

Reuse the existing notarized `.pkg` path: have the updater download a `.pkg`, then shell out to `installer(8)` (or open it in `Installer.app`) to perform the swap, exactly as a fresh install does.

**Why not chosen:**

- **The `.pkg` isn't a release asset.** GitHub Releases publish `Boss-1.0.N.zip`, not a `.pkg`. Adopting this means _also_ publishing the `.pkg` to every release — another pipeline change and a larger asset.
- **No silent path.** `installer -pkg … -target CurrentUserHomeDirectory` works without admin for the `~/Applications` domain, but driving it from a background "automatic install" without any user interaction is awkward and historically fragile; `Installer.app` is interactive by design.
- **Heavier than needed.** The zip _is_ a complete, signed bundle. For self-update we don't need the packaging layer at all — we need an atomic directory swap of one `.app`. The `.pkg` machinery (receipts, scripts, distribution XML) buys us nothing here and adds moving parts.

It stays the right tool for _first_ install (Gatekeeper-friendly, double-click UX). For _self_-update, a direct bundle swap is simpler and faster.

### Alternative C (chosen) — Custom in-app updater over the GitHub Releases API

A small Swift module in the app that polls `GET /repos/spinyfin/mono/releases` unauthenticated, compares versions, downloads `Boss-1.0.N.zip` to Application Support, verifies signature + notarization, and performs an atomic in-place bundle swap at a safe boundary. Rationale above; details below.

---

## Chosen approach

The updater has four responsibilities — **check**, **download/stage**, **swap**, **surface** — described in turn.

**Module layout (as built).** The design originally sketched a single `Sources/Update/` directory inside the app target. During implementation the pure logic instead landed in **`Sources/UpdateCore/`, a standalone `swift_library` Bazel module** (the pilot for per-module Bazel targets in the mac app), with its own `Tests/UpdateCore` test target that gates CI. Only the thin UI/lifecycle glue lives in the app target. The split is deliberate: everything filesystem/network/state-machine-shaped is unit-testable in `UpdateCore` without the app, and the app-target seam (`UpdateLifecycle`) is confined to the two side-effects a library shouldn't own — spawning the detached relaunch `Process` and exiting the app.

### 1. Version detection

**Current running version.** Read `CFBundleShortVersionString` (`"1.0.N"`, numeric) for comparison and `BossFullVersion` (`"1.0.N"` or `"1.0.N-dev-<sha>"`) to detect dev builds. Both are already in `Info.plist`; `BossMacApp.swift:24` shows the pattern. A build is "dev" iff `BossFullVersion` contains `-dev-`; dev builds short-circuit auto-install.

**Endpoint.** `GET https://api.github.com/repos/spinyfin/mono/releases?per_page=100` with headers:

- `Accept: application/vnd.github+json`
- `X-GitHub-Api-Version: 2022-11-28`
- `User-Agent: Boss/<version>` (GitHub requires a UA; missing UA → 403)
- `If-None-Match: <stored ETag>` when we have one (see rate limits)

We deliberately do **not** use `/releases/latest` (returns the wrong product in this monorepo) and do **not** use the tags API (no asset metadata). One page of 100 is far more than the ~28 `boss-v` releases that exist; if pagination is ever needed we stop as soon as we've seen a `boss-v` tag older than our current version (they're roughly date-descending).

**Selection algorithm:**

1. Filter releases to those whose `tag_name` matches `^boss-v(\d+)\.(\d+)\.(\d+)$`, excluding `draft` and `prerelease`.
2. Parse each to a `(major, minor, patch)` tuple.
3. Discard any release that has no asset named `Boss-<major>.<minor>.<patch>.zip` (handles the `boss-v1.0.21`-style assetless release).
4. Pick the **maximum** tuple (not the first in the list — the list isn't version-sorted).
5. Compare against the running `(major, minor, patch)` parsed from `CFBundleShortVersionString` using tuple ordering (semver-style; future-proof if `minor`/`major` ever move beyond the current `1.0.x`).
6. If `latest > current` → an update is available; carry the `tag_name`, version, asset `browser_download_url`, asset `size`, and release notes. As built, `AvailableUpdate` carries not just the newest release's `body` but a **`changelog: [ReleaseNote]` accumulating the notes of every version in `(installed, newest]`**, newest first — added post-T3 so a user several releases behind sees everything they're picking up, not only the latest release's notes.

**Why tuple compare, not "max integer N":** today everything is `1.0.x` so comparing the patch integer suffices, but encoding the assumption "major and minor are always 1.0" into the comparator is a latent bug the day someone cuts `1.1.0`. Tuple compare costs nothing and removes the trap.

**Rate limits.** Unauthenticated = **60 requests/hour/IP**, shared across everything on that IP. Mitigations:

- **Conservative default interval: every 6 hours**, plus one check shortly after launch (jittered 30–120 s so a fleet of machines behind one NAT doesn't thundering-herd the same minute). 6 h ⇒ ~4 checks/day/app, leaving ample headroom even with several Boss installs and other GitHub usage sharing the IP.
- **Conditional requests.** Store the response `ETag`; send `If-None-Match`. A `304 Not Modified` is cheap and, per GitHub's documented policy, conditional `304`s do not count against the primary rate limit — so steady-state polling of an unchanged release list is effectively free. (We still treat the limit as real and back off regardless; see below.) _As built, the ETag lives in the `UpdateChecker` actor's memory for the process lifetime rather than being persisted to disk — each app launch pays one full listing fetch and every subsequent poll in that session is conditional. At ~4 polls/day this is well inside budget, so cross-launch persistence wasn't worth the plumbing._
- **Honor `Retry-After` / secondary limits.** On `403`/`429` with `Retry-After`, or when `X-RateLimit-Remaining: 0`, suspend polling until `X-RateLimit-Reset`. Never retry-storm.
- **Manual checks bypass the interval** but still share the budget; a manual check that hits the limit reports "rate-limited, try again at HH:MM" rather than erroring opaquely.

### 2. Settings model

The brief asks whether the three modes are independent settings or one selector. **Chosen: a single 3-state mode selector**, because the modes are a strict escalation ladder, not orthogonal switches:

| Mode                   | Periodic poll? | Badge on new version? | Auto-download? | Auto-swap?         |
| ---------------------- | -------------- | --------------------- | -------------- | ------------------ |
| **Manual only**        | no             | no                    | no             | no                 |
| **Notify** _(default)_ | yes (6 h)      | yes                   | no             | no                 |
| **Automatic**          | yes (6 h)      | yes (until swapped)   | yes            | yes (quit/startup) |

The "Check for Updates…" menu item works in **all** modes — it is always-on and not gated. Modeling this as three independent booleans would allow nonsensical combinations (auto-install ON but polling OFF), so a single enum is clearer and impossible to misconfigure.

**Default = Notify.** Rationale: detection with zero silent mutation of the user's `/Applications` is the least surprising default; auto-install is opt-in. (Reviewer decision point — see Open questions; an argument exists for `Automatic` as default once notarization lands.)

**Where the setting lives — app-side `UserDefaults`, not engine settings.** The existing engine `settings.rs` registry is **boolean-only** and a 3-state enum doesn't fit it without extending the value type. More fundamentally, the updater is a pure _app_ concern: it polls, downloads, and swaps from the app process, and it must read its mode **at startup and at quit, when the engine may not be running** (indeed, post-swap the engine binary on disk is the _new_ one). Tying the update mode to engine RPC state would couple a UI-process decision to a separate process's lifecycle for no benefit.

As built, persistence is owned by **`UpdateModel`** (a `@MainActor ObservableObject` in `UpdateCore`) writing `UserDefaults` directly through an injected `defaults:` parameter (so tests run against an isolated suite), rather than view-level `@AppStorage` property wrappers as originally sketched. The keys are exactly the designed ones:

```
boss.update.mode              // "manual" | "notify" | "automatic"; default "notify"
boss.update.lastCheck         // epoch seconds, for the Settings "last checked" line
boss.update.skippedVersion    // optional "1.0.N" the user dismissed in Notify mode
// Staged-download bookkeeping lives in the staging dir manifest, not UserDefaults.
```

One consumer reads a key outside `UpdateModel`: the app-target `UpdateLifecycle` glue reads `boss.update.mode` straight from `UserDefaults.standard` at the quit/startup swap boundaries, when constructing view models would be wrong (`UpdateLifecycle.swift` documents the key as matching `UpdateModel`'s storage).

**UI placement:** a new **"Updates"** tab in `SettingsView`'s `TabView` (alongside the existing panes). It shows the segmented mode picker (bound to `UpdateModel.setMode(_:)`), the current version (`BossFullVersion`), a relative "Last checked" line, a "Check Now" button with an in-progress spinner, the last check result, and the live download/stage status ("Downloading… n%" / "1.0.N downloaded, will install on quit"). This pane reads/writes `UserDefaults` via `UpdateModel` and does **not** go through `chatModel.refreshSettings()`/RPC, unlike the other panes.

### 3. Download & staging

**Location.** `~/Library/Application Support/Boss/Updates/`. (`Application Support/Boss/` is already the app's home for `state.db`, `release-config.toml`, etc.) Layout:

```
~/Library/Application Support/Boss/Updates/
  staging/1.0.28/                ← in-progress download+verify working dir, never a complete version
  1.0.28/
    Boss-1.0.28.zip              ← downloaded asset (kept until superseded)
    Boss.app/                    ← extracted, verified bundle ready to swap in
    manifest.json                ← { version, tag, sourceURL, etag, sha256, verifiedAt, state, failureReason }
```

`UpdateDownloader` (an actor in `UpdateCore`) runs the whole download → verify → stage → prune pipeline. Each step must pass before the next; any failure marks the staging directory `failed` without ever promoting a bundle. `sha256` of the zip is recorded for diagnostics and a future checksum-asset check; `failureReason` (an additive field beyond the original manifest sketch) captures why a stage failed.

**Atomicity of download.** The asset downloads into `staging/<version>/` via an injectable `AssetDownloader` seam. Only after the download completes **and** integrity verification passes is the whole version directory `rename(2)`-ed into `Updates/<version>/` — a rename is atomic within the same filesystem, so a crash mid-download never leaves a partial directory masquerading as a ready version. The `manifest.json` `state` field (`downloading` → `verifying` → `ready` → `failed`) is the source of truth on next launch; any directory whose manifest isn't `ready` is garbage-collected.

> **Divergence — foreground, not background, URLSession.** The design originally called for a background `URLSessionDownloadTask` (survives app-state changes, supports resume). As built, `AssetDownloader.live` uses the foreground async `URLSession.shared.download(from:)`: T6 deliberately kept the session lifecycle out of the leaf module behind the `AssetDownloader` seam, and no follow-on task picked the background session up. Consequence: quitting Boss mid-download abandons the partial file (swept by cleanup) and the next poll restarts the ~34 MB download from scratch. Acceptable for a long-lived desktop app; the seam means a background-configured session remains a drop-in if it ever matters.

> **Quarantine-stripping (chosen approach for v1):** The current manual update flow — `curl` download → `unzip` → `cp -R` into `/Applications` — works today without notarization because command-line tools do **not** set `com.apple.quarantine`. Gatekeeper only assesses notarization when that xattr is present; without it, the app launches freely. The updater replicates this: after extracting the staged bundle, we explicitly run `xattr -dr com.apple.quarantine <bundle>` before any swap. This is _necessary_ (not just defensive) because an app-initiated `URLSession` download **can** receive the quarantine xattr from the system; stripping it ensures Gatekeeper never blocks an un-notarized release. Notarization remains a deferred future improvement (trust signal + robustness), but it is explicitly **not a blocker for v1**.

**Integrity verification (in order, all must pass before a download is marked `ready`):**

1. **Transport** — HTTPS to `api.github.com` / `github.com` / `objects.githubusercontent.com`. (We follow GitHub's redirect to the signed object-store URL.)
2. **Size** — bytes received == asset `size` from the API.
3. **Unzip integrity** — extract with `ditto -x -k` (the same tool `boss-release.sh` uses to build/verify the zip), failing on any error.
4. **Code signature** — `codesign --verify --deep --strict` on the extracted `Boss.app`, and confirm the signing identity's Team ID matches the _currently running_ bundle's Team ID (so a swap can never move us to a differently-signed bundle). As built this is an equality of `Optional`s: today's `bazel build`-produced releases are ad-hoc-signed (both Team IDs `nil` → match), while a Developer-ID running bundle would reject an ad-hoc staged one.
5. **Quarantine strip** — `xattr -dr com.apple.quarantine <bundle>` (see the quarantine-stripping note above). This is the final step before the bundle is marked `ready`, ensuring Gatekeeper never has a chance to assess notarization on the swapped-in bundle. `spctl --assess` is intentionally **not** run in v1; it would fail on un-notarized releases and is not needed given the quarantine-strip guarantee.

There is **no published checksum or detached signature asset** today (only the `.zip`). Integrity therefore rests on HTTPS + Apple code-signing (step 4), which combined with quarantine-stripping (step 5) is sufficient for v1. _If_ we later want defense-in-depth, the cheapest additions are a `Boss-1.0.N.zip.sha256` asset checked at step 3 and notarization (deferred; see §4 and Risks); both are optional pipeline enhancements, not v1 requirements.

**Cleanup rule ("delete older versions on success").** `cleanup()` runs after a successful stage and again at launch. It sweeps the `staging/` working area unconditionally (reclaiming kill-mid-download leftovers), deletes any version directory whose manifest is not `ready`, deletes any `ready` version **≤ the running app's version** (superseded), and among the rest keeps only the newest `ready` version. We keep exactly one staged version and never delete a directory mid-verify.

### 4. Install / swap mechanics

The unit of swap is the app's _own_ bundle, located via `Bundle.main.bundleURL` (works whether installed in `~/Applications` or `/Applications`).

**Privilege.**

- `~/Applications` (the installer's default) is **user-writable** → swap needs no admin password. This is the common case and the only one we make seamless in v1.
- `/Applications` may require admin rights to replace. If `Bundle.main.bundleURL` is not writable by the current user, we **do not** silently escalate: `planSwap` returns `.notWritable` and no filesystem mutation happens. Privileged swap via `SMJobBless`/an admin helper is explicitly out of scope for v1. **Known gap (as built):** the designed graceful degradation — surface "update ready, replace manually" and reveal the staged `Boss.app` in Finder — never shipped. `.notWritable` is only logged by `UpdateLifecycle`; in the UI it collapses into the generic install-failed message, whose text ("make sure Boss is installed in /Applications") is actively misleading for this case. Follow-up work is tracked to surface it properly.

**The core problem: you can't replace a running bundle's executable while it's mapped.** macOS lets you `rename` a running `.app` directory (the open file handles keep referencing the old inode), but you cannot relaunch from the swapped bundle in one step without a tiny external helper. So the swap happens at a **boundary where Boss is (about to be) not running**, per the brief's "quit or startup, whichever can be performed successfully".

**Swap strategy — in-process rename + a minimal external relaunch helper (as built).** The design originally sketched the shell helper performing the bundle renames after the app exited. That moved during T7: to keep the swap mechanics unit-testable in `UpdateCore`, **the renames run in-process** in `UpdateInstaller.applySwap` at a safe boundary — `current → Boss.app.bak`, then `staged → install location`, with the first move undone if the second fails, so a thrown error always leaves the previous bundle live. (Renaming a running `.app` directory is the documented-safe operation; the whole-bundle atomic rename never touches the signature.) A pending-swap record is persisted before the renames so the next launch can complete or roll back. The shell helper (`relaunch-helper.sh`, bundled in `Contents/Resources/`) is reduced to the parts Swift can't do after the process exits: wait for the old PID to exit (capped ~60 s), `open` the new bundle, watchdog its first launch by polling for the first-launch-OK flag (30 s), and on timeout restore the `.bak`, reopen the previous version, and write a rolled-back marker that the next launch folds into the failed-version blocklist.

The boundaries, orchestrated by the app-target `UpdateLifecycle` seam:

- **On quit** (`applicationWillTerminate`, automatic mode): apply the swap in-process without relaunching; the next launch runs the new version. Best-effort and non-blocking — a failed swap rolls back, leaves the current bundle intact, and the staged version waits for the next boundary. Quit always proceeds.
- **On startup** (at the engine-launch chokepoint in `ChatViewModel.startIfNeeded()`, _before_ the engine spawns): if a `ready` update wasn't applied at the last quit (crash, failed swap), apply it now, arm the relaunch helper, and `exit(0)` — the helper waits for us to die and relaunches into the new bundle.
- **User-initiated "Install & Relaunch"** (result sheet / badge popover; works in _any_ mode, not just automatic): apply the swap immediately, park the swap plan, and request termination. The helper is armed only in `applicationWillTerminate` — i.e. once the quit is actually **confirmed** — so a vetoed quit (agents still working) strands nothing: the swap is already durably on disk, the UI reports "installed — quit to finish", and a repeated press is idempotent. An explicit Install & Relaunch also intentionally retries a previously blocklisted version (clearing its block); the watchdog re-blocklists it if it still fails to launch.

**Engine coordination.** `EngineProcessController` owns a detached engine spawned from `<bundle>/Contents/Resources/bin/engine`. The swap replaces that binary on disk. On the quit path the engine was already stopped by normal termination; on the startup path the swap happens _before_ the engine is launched, so the new engine binary is what gets spawned. Either way the new app launches the new engine — no stale-engine window.

**Gatekeeper, quarantine, and signing — three risks that must be handled regardless of notarization status.**

**(1) Quarantine-stripping (App Translocation avoidance).** The staged bundle is stripped of `com.apple.quarantine` before swap (see §3 step 5). This matters for two reasons: (a) it prevents Gatekeeper from blocking an un-notarized bundle at launch, and (b) it prevents **App Translocation** — macOS's feature that runs a quarantined app from a randomized read-only shadow mount instead of its actual path. An updater that runs from a Translocation mount cannot reliably locate or replace its own bundle, because `Bundle.main.bundleURL` would point to the shadow path, not `/Applications/Boss.app`. Stripping quarantine before the first launch of the staged bundle ensures Translocation never activates.

**(2) Quit-vs-startup swap of a running bundle.** macOS allows `rename(2)` on a running `.app` directory (open file handles keep referencing the old inode), but replacing a running process's own executable in-place while it is mapped is unsafe. The chosen boundary strategy (swap-on-quit via detached helper, swap-on-startup as fallback, see above) ensures the bundle is not actively running when the rename is performed. The helper explicitly waits for Boss's PID to exit before touching the directory.

**(3) Signature invalidation.** If the bundle is Developer-ID-signed, in-place _modification_ of any signed file (as opposed to an atomic rename of the whole bundle) would break the signature and could cause the kernel to SIGKILL the new process on first exec. The chosen approach avoids this: the staged bundle is a complete, self-contained replacement and is swapped in via atomic `rename(2)` of the top-level `.app` directory — the signature is never touched. Boss releases built with `bazel build //tools/boss/app-macos:Boss` are currently **ad-hoc-signed** (build output, not Developer-ID; `boss-release.sh` does not call `codesign`), so signature invalidation is not an active concern today, but the atomic-rename strategy remains correct regardless of signing level.

**Notarization is deferred.** Un-notarized bundles work fine after quarantine-stripping: Gatekeeper only assesses notarization when the quarantine xattr is present. Notarization would add a trust signal (cryptographic proof of Apple scan) and robustness (survives future Gatekeeper policy tightening), but it is explicitly **not required for v1** and is recorded in Risks as a future improvement, not a blocker.

**Rollback and reconciliation.** The `.bak` of the previous bundle is retained until the new version has launched successfully once (a "first launch OK" flag written by the new version on `applicationDidFinishLaunching`; the relaunch helper's watchdog polls for it). If the new version fails to launch within the watchdog window, the helper restores `.bak`, reopens the previous version, and writes a rolled-back marker. `reconcileAtLaunch()` (run on every launch, regardless of mode) then settles the state: a completed swap drops its `.bak`; a rolled-back marker is folded into the persisted **failed-version blocklist** (`install-state.json`) so that version is never auto-attempted again, and its staged directory is reaped. Stale first-launch flags from other versions are pruned.

### 5. UI surfaces

**(a) Menu item — always available.** `Button("Check for Updates…")` in a `CommandGroup(after: .appInfo)` in `BossMacApp.swift` (directly under "About Boss", the macOS-conventional spot; the model reaches the command via `AppDelegate` injection, since `@EnvironmentObject` is unreliable in `CommandGroup` views). As built, only the _available_ outcome opens the sheet; every other outcome (up-to-date, network error, rate-limited) shows a **transient self-dismissing toast** instead of a dialog — a post-T3 refinement so a routine "you're current" doesn't demand a click:

- _Up to date / error / rate-limited_ — toast with the reason and, if rate-limited, when to retry.
- _Update available_ — the `UpdateResultSheet`: new version, scrollable accumulated release notes (markdown, rendered with the existing `StructuredText`/`bossMarkdown` stack), and **Skip This Version** / **Later** plus a primary action that walks the pipeline: **Download** (stages the verified bundle in-app, with determinate progress) → **Install & Relaunch** → on a vetoed quit, **Quit to Finish**. Install failures render as a terminal disabled "Install Failed" state with the reason — no silent browser fallback.
- _Dev build_ — "Running a development build (1.0.N-dev-…)" caption; Skip hidden; the primary action stays a plain **Download** that opens the asset URL in the browser, since the updater never stages or swaps over a dev build.

The check flow carries `os.log` observability (`subsystem: dev.spinyfin.bossmacapp`, `category: updater`) covering trigger source, result, scheduler cadence, staging, and swap decisions — added post-T3 when field-debugging the first real update.

**(b) Window-chrome badge — Notify/Automatic modes.** When a newer version is known and not yet applied (and not "skipped"), a button appears in the main window's **trailing toolbar region** — a `ToolbarItem(placement: .primaryAction)` in `ContentView`'s `.toolbar` block, rendering top-right under `.windowToolbarStyle(.unified)`. Appearance: `arrow.down.circle.fill` SF Symbol with an accent tint. Clicking opens a **popover** (`UpdateBadgePopover`) rather than the sheet, with full release-notes and action parity with the result sheet (parity restored post-T4 in #1226): the same Download → Install & Relaunch → Quit to Finish pipeline plus Skip This Version / Later. This mirrors the existing `EngineHealthBanner` precedent for non-modal chrome signaling, but as a compact trailing button rather than a full-width banner, since an available update is informational, not an error.

**(c) Progress & error feedback.** A shared `UpdateDownloadState` enum on `UpdateModel` (`idle` / `downloading(fraction)` / `readyToInstall` / `failed` / `installFailed` / `installedPendingRelaunch`) drives all three surfaces consistently:

- Download progress (auto-stage in Automatic mode, or after "Download") surfaces as determinate progress in the sheet, the badge popover, and the Settings "Updates" pane.
- Swap-in-progress is brief; the main feedback is simply relaunching into the new version. A swap that succeeded but couldn't relaunch (vetoed quit, missing helper) is reported truthfully as "installed — quit to finish" (#1855), never as a failure.
- Errors are non-blocking: a status line in the Settings pane and terminal states in the sheet/popover. We never throw a modal that interrupts work.

### Component summary (as built)

```
tools/boss/app-macos/Sources/UpdateCore/          — standalone swift_library, unit-tested in Tests/UpdateCore
  UpdateChecker.swift    — unauth GitHub Releases fetch, ETag cache, filter+select, version compare, changelog
  UpdateModel.swift      — @MainActor ObservableObject: mode + persistence, scheduler, downloadState, auto-stage
  UpdateDownloader.swift — URLSession download → staging → verify → ready, manifest state machine, cleanup rule
  UpdateInstaller.swift  — ready-update discovery, in-process swap + .bak rollback, blocklist, reconciliation

tools/boss/app-macos/Sources/                     — app-target glue and UI
  UpdateLifecycle.swift                — mode/dev-build gating, boundary orchestration, helper spawning, exit
  Update/UpdateResultSheet.swift       — the check-result sheet
  Settings/UpdateSettingsView.swift    — the "Updates" Settings tab
  Resources/RelaunchHelper/relaunch-helper.sh — detached wait-relaunch-watchdog-rollback script
```

Plus edits to `BossMacApp.swift` (menu item, `reconcileAtLaunch()`, quit-swap + helper arming in `applicationWillTerminate`), `ChatViewModel.swift` (startup-swap fallback before the engine spawns), `ContentView.swift` (chrome badge toolbar item + `UpdateBadgePopover`), and `SettingsView.swift` (registers the new tab).

---

## Failure handling

| Failure                                                                      | Detection                                                 | Behavior                                                                                                                                                      |
| ---------------------------------------------------------------------------- | --------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Network unreachable / DNS / timeout**                                      | `URLSession` error                                        | Silent in periodic mode (log only); explicit "couldn't reach GitHub" in manual mode. Retry on next interval.                                                  |
| **Rate-limited (`403`/`429`, `X-RateLimit-Remaining: 0`)**                   | response headers                                          | Suspend polling until `X-RateLimit-Reset`/`Retry-After`. Manual check reports "try again at HH:MM".                                                           |
| **Malformed / unexpected API response**                                      | JSON decode / no `boss-v` releases                        | Treat as "no update"; log. Never crash on schema drift.                                                                                                       |
| **Newest release has no usable asset**                                       | asset filter (§1 step 3)                                  | Skip it; consider the next-newest `boss-v` release with an asset.                                                                                             |
| **Partial / interrupted download**                                           | size mismatch or task error; manifest `state != ready`    | Discard staging temp; retry next interval. The atomic-rename rule means a partial never looks ready.                                                          |
| **Integrity check fails** (bad unzip / `codesign` reject / Team-ID mismatch) | §3 steps 3–5                                              | Mark `failed`, delete the staged dir, do **not** swap, surface "update could not be verified".                                                                |
| **Swap fails** (rename throws mid-swap)                                      | `applySwap` error after in-process rollback               | Rollback restores the previous bundle before the error propagates; the `ready` staged version stays for the next boundary. Quit still proceeds.               |
| **Install location not writable** (`/Applications` without admin)            | `planSwap` pre-flight writability check → `.notWritable`  | No mutation, no escalation; logged. _Gap: the designed "reveal staged bundle in Finder" UI never shipped — the sheet shows a generic install-failed message._ |
| **New version won't launch** (crashes before "first launch OK" flag)         | relaunch helper's watchdog (30 s) sees no flag            | Helper restores `Boss.app.bak`, reopens the old version, writes a rolled-back marker; next launch blocklists the version so it's never auto-attempted again.  |
| **Dev build**                                                                | `BossFullVersion` contains `-dev-`                        | Report availability but never auto-install; "Download" still allowed for manual testing.                                                                      |
| **Agents running at quit**                                                   | existing `activeAgentCount` gate (`BossMacApp.swift:203`) | Do not swap-on-quit if the user cancels quit; the staged version waits for the next clean quit/startup.                                                       |

---

## Risks / open questions

Items that were reviewer decision points at design time are marked with how they resolved.

1. **🟢 Notarization deferred — quarantine-stripping is sufficient for v1.** _Held up as built._ The published `Boss-1.0.N.zip` is un-notarized, and the updater does **not** require notarization. Quarantine-stripping (`xattr -dr com.apple.quarantine`) before swap replicates the existing manual workflow, which works for the same reason. Notarization remains a worthwhile future improvement (trust signal, robustness against Gatekeeper policy tightening); when it lands, §3's verification gains a `spctl --assess` step and the quarantine-strip stays as belt-and-suspenders.
2. **🟠 Repo must stay public.** Still true: the design relies on `spinyfin/mono` being unauthenticated-readable. If the repo goes private, every check 404s and updates silently stop. _Mitigation if that happens:_ publish releases (or a manifest) to a dedicated public repo or Pages site and point the updater there. We shipped without the indirection.
3. **🟠 `/Applications` privileged swap is out of scope.** _Resolved: shipped without it._ v1 is seamless only for a user-writable install location. Note the related as-built gap: even the designed _guided_ degradation (reveal staged bundle in Finder) hasn't shipped yet — `.notWritable` currently surfaces as a generic install failure (§4).
4. **🟡 Default mode.** _Resolved: **Notify** shipped as the default_ (`UpdateModel` falls back to `notify` when the key is unset). Automatic remains opt-in.
5. **🟡 Shared-IP rate limiting.** Unchanged: 6 h interval + jittered launch check + ETag conditional requests + hard backoff on rate-limit responses. No budget problems observed in practice.
6. **🟡 Engine-restart UX during swap.** Unchanged: the existing `activeAgentCount` quit-gate covers running agents; non-agent in-flight engine work at quit is accepted as restart-tolerant. The user-initiated Install & Relaunch path additionally respects a vetoed quit — the swap stands and completes on the next confirmed quit.
7. **🟢 Optional integrity hardening.** No checksum asset is published; integrity rests on HTTPS + code-signature verification + Team-ID pinning. The staged zip's `sha256` is already recorded in the manifest, so a published `Boss-1.0.N.zip.sha256` asset remains a cheap future addition.
8. **🟢 Sparkle revisit.** Unchanged: if we later want phased rollouts / staged percentages / a hardened third-party swapper, migrating to Sparkle (Alternative A) is the natural path. `UpdateCore` is intentionally small to keep that door open.

---

## Implementation history

The design's T1–T8 task breakdown shipped as eight PRs (May 2026), followed by fix/polish PRs that real-world use surfaced. Divergences worth knowing are folded into the sections above; this is the map.

| Task                         | PR                                                | Notes                                                                                                                                                                                                                                                                                   |
| ---------------------------- | ------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| T1 `UpdateChecker`           | [#911](https://github.com/spinyfin/mono/pull/911) | As designed; landed in the new `UpdateCore` module with canned-JSON tests for the live-observed interleaved-order and assetless-release cases.                                                                                                                                          |
| T2 `UpdateModel` + scheduler | [#926](https://github.com/spinyfin/mono/pull/926) | As designed; persistence via injected `UserDefaults` rather than view-level `@AppStorage`.                                                                                                                                                                                              |
| T3 Menu item + result sheet  | [#956](https://github.com/spinyfin/mono/pull/956) | Shipped with browser-download only; the in-app Download/Install pipeline came later (see below).                                                                                                                                                                                        |
| T4 Chrome badge + popover    | [#953](https://github.com/spinyfin/mono/pull/953) | Landed before T3 and carried some of the `BossMacApp` wiring; popover initially had a cut-down UI.                                                                                                                                                                                      |
| T5 Updates Settings tab      | [#954](https://github.com/spinyfin/mono/pull/954) | As designed.                                                                                                                                                                                                                                                                            |
| T6 `UpdateDownloader`        | [#927](https://github.com/spinyfin/mono/pull/927) | Foreground `URLSession` behind the `AssetDownloader` seam (§3 divergence note); also widened the `mac-app-build` CI step from `:BossTests` to `bazel test //tools/boss/app-macos/...` so the updater tests actually gate merges.                                                        |
| T7 `UpdateInstaller`         | [#978](https://github.com/spinyfin/mono/pull/978) | Biggest design adaptation: renames moved in-process into `applySwap` for unit-testability; the shell helper reduced to wait/relaunch/watchdog/rollback (§4).                                                                                                                            |
| T8 End-to-end verification   | [#996](https://github.com/spinyfin/mono/pull/996) | Delivered as automated integration tests (`EndToEndUpdateTests`) chaining checker → downloader → installer, including the kill-mid-download, corrupt-zip, rollback-blocklist, and writability-degradation scenarios — rather than the manual verification pass the breakdown described. |

Notable follow-ons the initial cut needed:

- **[#1086](https://github.com/spinyfin/mono/pull/1086) — wired up the orphaned download/stage step.** As merged, T1–T8 never actually invoked `UpdateDownloader` from the app: automatic mode detected updates but nothing downloaded them, so the installer's `newestReadyUpdate` never found anything. This PR added the `UpdateStager` seam and `maybeAutoStage` on every check, plus the user-initiated in-app Download → Install & Relaunch pipeline in the sheet. The T2↔T6 handoff ("app layer will own the download lifecycle") had fallen through the task boundaries — a lesson for future task splits: name an owner for every seam.
- **[#1226](https://github.com/spinyfin/mono/pull/1226)** — badge-popover parity with the sheet (release notes + full action set).
- **[#1238](https://github.com/spinyfin/mono/pull/1238) / [#1855](https://github.com/spinyfin/mono/pull/1855)** — Install & Relaunch honesty: blocklisted-version handling (an explicit user install now unblocks and retries) and truthful post-swap reporting (`installedPendingRelaunch` / "Quit to Finish" instead of claiming failure after the bundle was already durably swapped).
- Smaller polish: accumulated changelog across skipped versions, manual-check toast feedback, `os.log` observability, release-notes date formatting.

**Known remaining gap:** the `/Applications`-not-writable guided degradation (§4) — tracked as follow-up work.

**T-future (pipeline, unchanged, not yet scheduled):** extend `boss-release.sh` to Developer-ID-sign + notarize + staple `Boss.app` before zipping, so `Boss-1.0.N.zip` passes `spctl --assess`; then add `spctl --assess` to §3's verification.
