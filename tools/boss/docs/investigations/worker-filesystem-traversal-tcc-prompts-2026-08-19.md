# Boss raises macOS privacy prompts every session: a worker walks `/`

**Date:** 2026-08-19
**Symptom (maintainer):** "it does that each session at least once — it
actually circles through almost all things it requires permission for —
Documents, Desktop, Network Volumes, Photos, etc. I think something is doing a
broad scan of my user dir maybe?"

Observed as a macOS system dialog attributed to Boss: _"Boss would like to
access files on a network volume."_, and the equivalents for the other
categories.

The maintainer's guess was right, and the scan is wider than a user dir.

## Summary

Two independent facts combine to produce the symptom.

1. **A Boss worker's `claude` process recursively walks `/`.** In one observed
   run it logged **1970** kernel sandbox denials, reading into the operator's
   `Desktop`, `Documents`, `Downloads`, `Music`, `Movies`, `Pictures` and
   `Library`, into `/private/var`, `/Library`, `/System/Library`, and into
   **three other users' home directories on the same Mac**.
2. **Boss's TCC grants cannot persist.** The app is ad-hoc signed, so its
   designated requirement is a bare `cdhash` of one exact binary. Every
   rebuild or auto-update changes that hash, `tccd` logs _"Failed to match
   existing code requirement"_, discards the stored decision, and re-prompts.

Fact 1 is the defect. Fact 2 is why one broad walk becomes a prompt storm on
every session instead of a single prompt the operator answers once.

## Method

Everything below is local observation. `fs_usage` needs elevated privileges
that were not available in the worker session, so the filesystem evidence
comes from the kernel's own sandbox denial log via `log show`, which needs
none.

### 1. Catch the prompt and its attribution

```sh
log show --last 24h --predicate 'subsystem == "com.apple.TCC"' --info \
  | grep AUTHREQ_PROMPTING
```

Two bursts in 24h, each cycling the same categories in sequence:

| Time                           | Services prompted, in order                                                  |
| ------------------------------ | ---------------------------------------------------------------------------- |
| 2026-08-18 20:40:24 → 20:44:42 | Documents, Desktop, Downloads, MediaLibrary, Photos, AppData, NetworkVolumes |
| 2026-08-19 09:35:49 → 09:36:40 | Desktop, Documents, Downloads, MediaLibrary, Photos, AppData, NetworkVolumes |

Each prompt is modal, so the gaps between them are the operator clicking, not
the scan's own pace.

### 2. Identify the process actually performing the access

TCC attributes a prompt to the responsible _bundle_, so the dialog only ever
names the app. The attribution record names the real accessor:

```
AUTHREQ_ATTRIBUTION: msgID=414.3653, attribution={
  responsible={identifier=dev.spinyfin.bossmacapp, pid=3333,
               responsible_path=<app bundle>/Contents/MacOS/Boss},
  accessing={identifier=com.anthropic.claude-code, pid=70839,
             binary_path=/Users/<operator>/.local/share/claude/versions/2.1.235},
  requesting={identifier=com.apple.sandboxd, pid=414}}
```

So: a **Boss worker's `claude` CLI** (pid 70839) is the accessor; the app is
merely the responsible bundle it inherits its TCC identity from. `pid 3333` is
the running app; the worker session investigating this had a different
`claude` pid (5884), so pid 70839 is a separate, earlier worker — not the
investigation observing itself.

### 3. Rule out sandbox-entry as the cause

A plausible alternative was that entering a Seatbelt sandbox makes `sandboxd`
pre-negotiate every protected category up front, with no real file access.
Tested directly — stream TCC, enter a sandbox, touch nothing:

```sh
sandbox-exec -f probe.sb /bin/echo hello
```

Zero `kTCCServiceSystemPolicy*` events. Sandbox entry alone triggers nothing,
so the observed events were **real file accesses**.

### 4. Get the paths

The kernel logs each denied read with its path, which TCC does not:

```sh
log show --start "2026-08-19 09:25:00" --end "2026-08-19 09:40:00" \
    --predicate 'eventMessage CONTAINS "(70839)"' \
  | grep 'deny(1)'
```

1970 `file-read-data` denials for that one process:

| Prefix                                                                                    | Denials                                                                                                                |
| ----------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `/Users/<operator>`                                                                       | 1394 (1367 of them under `Library`; also `Desktop`, `Documents`, `Downloads`, `Movies`, `Music`, `Pictures`, `.Trash`) |
| `/private/var`                                                                            | 489                                                                                                                    |
| `/System/Library`                                                                         | 19                                                                                                                     |
| three other users' homes                                                                  | 12 each — `Desktop`, `Downloads`, `.Trash`                                                                             |
| `/Library/Caches`, `/Library/Trial`, `/Library/Bluetooth`, `/Library/Application Support` | 22                                                                                                                     |
| `/System/Volumes`, `/nix/.Trashes`                                                        | 10                                                                                                                     |

The denials arrive in bursts (≈870 in four seconds at 09:36:04–09:36:07),
each burst resuming right after a prompt is answered. That is one traversal
being stalled by modal dialogs and continuing, not many separate accesses.

This is a walk of `/`, not of a user directory. Note the reach into other
people's home directories — this is a shared family Mac.

### 5. Confirm the mechanism by reproduction

Running a root-level traversal from a Boss worker's own Bash tool call:

```sh
find / -maxdepth 4 -name "__nonexistent_probe__"
```

produces the same denial signature — the three sibling home directories, the
operator's home, `/Library/Caches`, `/Library/Trial`, `/Library/Bluetooth`,
`/Library/Application Support`, `/private/var`, `/nix/.Trashes` — just
shallower. **Boss's existing PreToolUse path guard approved that call**, which
is the gap this change closes.

### 6. Why it repeats every session

```
tccd: Failed to match existing code requirement for subject
      dev.spinyfin.bossmacapp and service kTCCServiceSystemPolicyDesktopFolder
```

`codesign -dvvv` on the installed bundle reports `flags=0x2(adhoc)`,
`TeamIdentifier=not set`, `Internal requirements count=0`, and
`codesign -d -r-` reports a designated requirement of the form
`cdhash H"<hash>"`.

The app is ad-hoc signed with no Team ID and no internal requirements, so its
designated requirement is the literal `cdhash` of one build. TCC stores the
operator's decision against that requirement; the next build (or auto-update —
the `.bak` bundle under `Application Support/Boss/Updates` shows these happen)
has a different `cdhash`, fails the match, and re-prompts. The TCC debug
records show `tccd` holding two different cdhashes side by side while doing
exactly this comparison.

## Is the access legitimate?

No. Nothing a Boss worker does requires reading `~/Pictures`, `~/Desktop`, a
network volume, or another user's home directory. Worker file access belongs
in the leased cube workspace, the cube workspace/repo directories, the Boss
state root, and specific files a task names.

The macOS prompts are the system working correctly: they are the only reason
anyone noticed.

## Fix

Boss cannot patch the `claude` CLI, but it owns the worker's PreToolUse guard
(`boss-path-guard.py`, generated in `worker_setup.rs`) — a hook that already
runs on every tool call and already canonicalises paths for the Boss data-dir
boundary. This change adds a **second, independent boundary** to that guard:

> A tool call may not **start** a recursive directory walk rooted at `/`,
> `/Users` or a user home, `/Volumes` or a mounted volume, `/System`,
> `/Library`, `/private`, `/var`, or the `/net` and `/home` autofs maps
> (descending those mounts a network volume on demand — the direct cause of
> the "network volume" prompt).

It judges the **root of a recursive walk** only:

- `Bash` — a program that recurses (`find`, `rg`, `fd`, `du`, `tree`,
  `mdfind`, …, plus `grep`/`ls`/`cp`/`rsync` when given a recursion flag)
  paired with a broad root.
- `Glob` / `Grep` — the `path` argument, or the literal prefix of an absolute
  `pattern`, since Claude Code's `Glob` runs in-process with no shell command
  to inspect.

Reading one **specific named file** outside the workspace (`~/.gitconfig`, a
bazel cache entry, `/etc/hosts`) stays approved. The defect is the breadth of
the traversal, not the fact that a path is external — fencing every external
read would break ordinary worker work without addressing the cause.

## Deliberately not done

- **Nothing was granted or pre-authorised.** No entitlements, no
  `NS*UsageDescription` keys, no pre-seeded TCC, no advice to click Allow.
  Those hide a machine-wide scan instead of stopping it.
- **The protected directories were not excluded from a still-broad scan.** The
  broad scan itself is blocked at its root, which is why `~/Desktop` and
  friends do not appear in the guard's rules at all.
- **The ad-hoc signature was left alone.** Making TCC grants persist (a stable
  signing identity, so the `cdhash` requirement stops changing) would silence
  the prompts while the walk continued — the same forbidden shape. It is worth
  fixing on its own merits, for update UX and for the operator's ability to
  make a privacy decision that sticks, but it is a signing/release change, not
  this defect's fix, and it must not land as a substitute for narrowing the
  scan.

## What this does not establish

The traversal was caught in the kernel log after the fact, which names the
process and every path but not the tool call that started it. Whether this
particular walk was an agent choosing an over-broad search or something in the
driver's own startup is not determined here. It does not change the fix: the
guard blocks the traversal at its root either way, and the reproduction in
step 5 confirms a worker Bash call is a sufficient route. Boss worker
transcripts are coordinator-owned state a worker may not read, so attributing
the specific call needs a coordinator-side look at that run.
