# Antigravity CLI (`agy`) permission posture, trust, and soft-deny

- **Date:** 2026-11-14
- **Kind:** choosing study. Binary and vendor-documentation inspection plus live sessions. Documentation only; no driver code.
- **Versions under test:**
  - `agy` **1.5.9**, the current release; sha256 `fad3628a…d256741a`, run from a scratch copy of the vendor archive
  - `agy` **1.5.7**, what the host has installed; sha256 `e872cc54…4ab236a8`, run from a scratch copy
  - macOS 27.0.1 (arm64), one signed-in personal account, model `gemini-3.8-flash-low`
- **Related:** [driver design](../designs/antigravity-driver-fourth-driver-google-gemini-cli.md) · [pane viability study](./ghostty-agy-pane-viability.md) (mono#3550) · [hook surface study](./agy-hook-surface.md) (mono#3553) · [credential study](./scoped-home-credential-delegation.md) (mono#3583) · [shared-HOME study](./agy-shared-home-hook-permissions.md) (mono#3614)
- **Evidence:** the matrices and excerpts below. Raw recordings stayed in a scratch directory and are not published.

None of the three candidate expressions of a read-only Reviewer holds up. `toolPermission: strict` ignores its allowlist and raises an approval dialog on every tool call, `--mode plan` is a prompt prefix that enforces nothing, and `--sandbox` cannot run `jj` on this host and is escapable in the modes that avoid dialogs. The posture that works is the ordinary review mode with every decision supplied by a `PreToolUse` hook verdict.

## Verdict (read this first)

### Recommendation

Express every worker kind's posture, Reviewer included, the same way:

1. **Session settings** that pin the default agent mode and the review preset: `"agentMode": ""` and `"toolPermission": "request-review"`.
2. **A `PreToolUse` hook that answers every tool call with a verdict.** `auto_approve` for what the kind may do, `deny` with a `reason` for what it may not. Kinds that run from an allowlist answer `deny_unless_prior_grant` and carry the allowlist in `permissions.allow`.
3. **`permissions.deny` only as a belt for the native file tools** (`write_file`, `edit_file`, `read_file`). Command denies belong in the hook's guard, not in rules.
4. **None of** `--dangerously-skip-permissions`, `always-proceed`, `strict`, `--mode`, or `--sandbox` as the posture.

The settings can be passed per run with `--settings`, or written to `settings.json`; both were measured and `--settings` wins when they disagree.

The recommendation rests on two verdicts, `auto_approve` and `deny_unless_prior_grant`, measured here on 1.5.7 and 1.5.9. `auto_approve` was not measured on 1.5.5, the hook surface study's pin, so the conformance pin should be 1.5.7 or later.

### The candidates

| Candidate                                           | What it did                                                                                                                                                                                                                                         | Verdict                            |
| --------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------- |
| `toolPermission: strict` + allowlist                | Approval dialog on 16 of 16 tool calls in six sessions, workspace reads included. Matching allow rules changed nothing (6 of 6), nor did a hook `auto_approve` (4 of 4).                                                                            | Reject                             |
| `--mode plan`                                       | Prepends `/plan ` to the prompt and nothing else. Same dialogs as the default mode (4 of 4). With the skip-permissions flag, writes ran (3 of 3 on 1.5.9, 2 of 2 on 1.5.7).                                                                         | Reject                             |
| `--sandbox`                                         | Kernel-enforced where it applies, but `jj` cannot start inside it (6 of 6), a nested Seatbelt profile fails, and the model's own `BypassSandbox` argument leaves it without a prompt.                                                               | Reject as the posture              |
| `--mode auto`                                       | The default since 1.5.7 when no mode is saved. Approves workspace edits and sandboxed commands; still raises dialogs for files outside the workspace (2 of 2).                                                                                      | Reject; pin it off                 |
| `always-proceed` or the skip flag, plus deny rules  | No dialogs. Deny rules hold for native file tools. Shell commands get around them, and one rule combination makes a denied command run (6 of 6).                                                                                                    | Works, fails open without the hook |
| **`request-review` + hook verdicts** (`--settings`) | Outside `strict` and with no ask rule matching, 62 tool calls answered `auto_approve`, `deny` or `deny_unless_prior_grant` raised 0 dialogs. Deny and ask rules still apply. With no hook loaded, a tool call stops at a dialog instead of running. | **Recommended**                    |

### Does the interactive path soft-deny an ungranted tool, as headless does?

**No.** An ungranted tool in an interactive session raises an approval dialog and the turn waits. One dialog was left up for 150 s: no timeout, no hook, no `Stop`.

Headless does soft-deny, and more quietly than the design assumed: exit 0, empty stdout, a notice on stderr, and the hook stream ends at `PreToolUse` with **no `Stop`** (3 of 3).

A clean turn boundary on a tool that never ran does occur interactively, by three other routes: a deny rule, a hook `deny`, or `deny_unless_prior_grant`. All 62 such calls produced `PreToolUse`, no `PostToolUse`, and one `Stop` with `NO_TOOL_CALL`, `fullyIdle=true`, `error=""`.

### Five things that will bite an implementation

| #   | Finding                                                                                                                                                                                                                          | Consequence                                                                                                                   |
| --- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| 1   | With no saved `agentMode`, both versions start in **Auto** mode, not the documented default.                                                                                                                                     | Every session must set `agentMode` explicitly. A posture that leaves it out is a different posture on 1.5.7 and later.        |
| 2   | **Text delivered to a pane that is showing a dialog answers the dialog.** A pasted message plus Return approved the pending command. A typed message starting with `3` saved an always-allow rule to the global `settings.json`. | A posture that can raise a dialog is not "stuck", it is approvable by the engine's own nudges. The pane must never get there. |
| 3   | A malformed permission rule is dropped with a log warning and nothing else. A mistyped **deny** rule fails open.                                                                                                                 | Boss must validate rule strings before writing them and check the log for `ignoring invalid`.                                 |
| 4   | `--settings` rejects an unknown key and exits 1. `settings.json` accepts the same key and ignores it.                                                                                                                            | A typo such as `toolPermissions` fails loudly through `--settings` and silently through the file.                             |
| 5   | A broken `settings.json` opens a blocking `Settings Error` screen. Its second option continues with defaults; the screen's own status bar already reads `auto`.                                                                  | Another reason to carry posture in `--settings`, where the same defect is a non-zero exit before any screen.                  |

## Scope and method

### What ran

84 `agy` sessions: 74 on 1.5.9 and 10 on 1.5.7. 79 were interactive sessions in a pseudo-terminal and 5 were headless `-p` runs. They took 282 prompts, recorded 1,634 hook payloads, met 64 approval dialogs, and made 491 model invocations.

Each session ran in a fresh scratch workspace holding one seeded file, with a second seeded file in a sibling directory outside it. Each prompt asked for exactly one tool call and a one-line result:

| Probe                | Tool          | Action                                      |
| -------------------- | ------------- | ------------------------------------------- |
| read in workspace    | `read_file`   | read the seeded file                        |
| `cat`                | `run_command` | `cat inside.txt`                            |
| shell write          | `run_command` | `printf SHELL_OK > shell_marker.txt`        |
| native write         | `write_file`  | create `native_marker.txt`                  |
| native edit          | `edit_file`   | change the seeded file                      |
| read outside         | `read_file`   | read the sibling directory's file           |
| shell write outside  | `run_command` | `printf … > <sibling>/shell_out_marker.txt` |
| native write outside | `write_file`  | create a file in the sibling directory      |

Whether an action happened is read from the marker file on disk, never from the model's answer or the CLI's exit status.

### How it was observed

- **Hooks.** All six events, wired from `<workspace>/.agents/hooks.json`. The hook writes its stdin to a file and replies with nothing unless a cell sets a reply, so the default cells see `agy`'s own permission logic undisturbed. A control session with no hooks at all gave the same result as the neutral hook.
- **Screen.** The pseudo-terminal's output fed a terminal emulator; dialogs were detected from the rendered screen and saved as text.
- **Log and transcript.** `--log-file` at a scratch path, and the conversation's `transcript_full.jsonl`, copied before clean-up.
- **Settings.** Passed with `--settings` in 78 sessions. Seven cells wrote the persistent `settings.json`, six of them instead of `--settings`, to measure that file's own parsing.

### What this is not

- **Not a GhosttyKit pane and not a Boss dispatch.** The sessions ran in a plain pseudo-terminal, the same class of apparatus as the hook surface study. Dialog text and key handling were not re-measured through a GhosttyKit surface.
- **Not a per-run `HOME`.** Every session ran in the real `HOME`. A fresh home cannot sign in without the login-keychain symlink, and that symlink was not created. Trust pre-seeding in a per-run home was measured by the pane study; this study measures the same key in the real home.
- **One account, one model.** A personal account, for which Auto mode and `command(...)` rules apply. Business and enterprise accounts follow different rules that were not reachable.

## 1. The settings vocabulary

Key names and accepted values, from the vendor's published [settings schema](https://antigravity.google/schemas/v1/settings.json) and [CLI reference](https://antigravity.google/docs/cli/reference), each confirmed against the binaries.

| Key                       | Accepted values                                                    | Default          | Measured                                                                              |
| ------------------------- | ------------------------------------------------------------------ | ---------------- | ------------------------------------------------------------------------------------- |
| `toolPermission`          | `request-review`, `proceed-in-sandbox`, `always-proceed`, `strict` | `request-review` | All four behave as section 3 shows. Any other string is an error.                     |
| `agentMode`               | `""`, `accept-edits`, `auto`, `plan`                               | `""` on paper    | All four. With the key absent the session starts in `auto` on both versions.          |
| `permissions.allow`       | array of rule strings                                              | —                | Honoured in `request-review` and with `deny_unless_prior_grant`. Ignored by `strict`. |
| `permissions.ask`         | array of rule strings                                              | —                | Forces the dialog even when a hook answers `auto_approve`.                            |
| `permissions.deny`        | array of rule strings                                              | —                | Applies in every mode measured, including the skip flag.                              |
| `enableTerminalSandbox`   | boolean                                                            | `false`          | Turns the sandbox on, as `--sandbox` does. Used for the `proceed-in-sandbox` cells.   |
| `trustedWorkspaces`       | array of absolute paths                                            | `[]`             | Section 4.                                                                            |
| `allowNonWorkspaceAccess` | boolean                                                            | `false`          | Not varied.                                                                           |

There is no `permissions.allowed`, no `permission` (singular), and no `toolPermissions`. `permissions` accepts exactly `allow`, `ask` and `deny`.

### Rule grammar

A rule is `action(target)` with no space before the parenthesis. The documented actions are `command`, `read_file`, `write_file`, `read_url`, `execute_url`, `mcp`, and `unsandboxed`. This study exercised `command`, `read_file` and `write_file`.

- `command(prefix)` matches word by word from the start of the command. `command(regex:…)` takes a regular expression.
- `read_file(path)` and `write_file(path)` match a path and everything under it. A `write_file` rule covers `edit_file` too.
- `unsandboxed(...)` is ignored on a personal account, with a log warning. A deprecated `unsandboxed(printf)` deny did not stop `printf`.

### What the log proves about a session's settings

Every session logs how it parsed its settings, which gives Boss a readback that needs no model call:

```text
cli_setting_manager.go:98] CLI settings initialized: permissions=&{Allow:[] Deny:[command(printf) write_file(/…/ws) read_file(/…/outside)] Ask:[]}, toolPermission=always-proceed
```

The line prints rule strings as written, valid or not. The validity verdict comes later and separately (section 2).

### The default agent mode is Auto

The CLI reference says a session starts in `default` mode. The 1.5.7 changelog says otherwise, and the binaries agree with the changelog. With no `agentMode` in any settings source, both versions showed `auto` in the status bar and ran a shell write with no dialog.

`"agentMode": ""` restores the default mode. It survives the CLI's rewrite of `settings.json`, and `--settings` accepts it.

### Precedence

`--settings` overrides `settings.json`. With `strict` in the file and `request-review` in `--settings`, a workspace read ran with no dialog. Nothing passed in `--settings` was written back to the file in any session.

The flag is in `agy --help` on both versions, described as "Settings for this session only, never saved: a JSON object of settings.json keys, inline or as a file path". The public flag reference does not list it.

## 2. Validation: what is rejected and what is silently ignored

The same defect is handled differently by the two channels.

| Defect                                            | In `--settings`                                                                                     | In `settings.json`                                                      |
| ------------------------------------------------- | --------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| Invalid JSON                                      | Exit 1: `--settings must be a JSON object of settings.json keys: unexpected end of JSON input`      | `Settings Error` screen at start                                        |
| Unknown key (`toolPermissions`, `Permissions`)    | Exit 1: `--settings: unknown setting "Permissions"`                                                 | **Accepted and ignored.** Kept in the file. The session was not strict. |
| Unrecognised value (`toolPermission: "paranoid"`) | Exit 1: `invalid value for setting "toolPermission": toolPermission: unrecognized value "paranoid"` | `Settings Error` screen at start                                        |
| Unrecognised `agentMode`                          | Exit 1                                                                                              | not run                                                                 |
| Wrong type (`permissions: "x"`)                   | Exit 1: `invalid value for setting "permissions"`                                                   | `Settings Error` screen at start                                        |
| Malformed rule string                             | **Accepted.** Log warning. Rule not applied.                                                        | **Accepted.** Log warning. Rule not applied, and removed from the file. |
| Unrecognised `--mode readonly`                    | One warning line; the session starts anyway, in the mode it would have had without the flag         | —                                                                       |

### Rule strings are never validated at parse time

This is the answer to whether `command(git)` and `write_file(src/)` syntax is checked: it is not. A session given nine malformed rules started normally and showed nothing on screen. The log, about a second later:

```text
permission_grant_store.go:571] ignoring invalid deny entry "comand(printf)": unknown action "comand" in grant string: "comand(printf)"
permission_grant_store.go:571] ignoring invalid deny entry "command(printf": invalid grant string: "command(printf"
permission_grant_store.go:587] ignoring invalid allow entry "command (cat)": unknown action "command " in grant string: "command (cat)"
permission_grant_store.go:587] ignoring invalid allow entry "write_file": invalid grant string: "write_file"
```

In that session `printf` ran, because both `printf` denies were malformed, while the one well-formed deny in the same list was enforced. A dropped allow rule fails closed. A dropped deny rule fails open, and nothing but the log says so.

`read_file()` with an empty target drew no warning. Its effect was not measured.

Through the file, the CLI also rewrote `settings.json` during the session without the malformed rule. The evidence of the mistake is deleted from the file that contained it.

### The `Settings Error` screen

A file the CLI cannot accept opens this before the composer, and waits:

```text
⚠ Settings Error
  ~/.gemini/antigravity-cli/settings.json
  └ invalid settings: toolPermission: unrecognized value "paranoid"
Nothing has been written to your file yet.
> Exit and fix manually
  Continue with defaults this session
  Overwrite the file with defaults
```

Return on the default option exits with status 0. The second option was not exercised. It is documented to continue with defaults for whatever could not be read: one key for a bad value, every key for unparseable JSON. The status bar on the error screen already reads `auto`.

## 3. What each posture does

1.5.9. One session per row; rows marked † were repeated in whole or in part on 1.5.7 with the same result. `ran` means the marker file exists. `DIALOG` means an approval dialog appeared and the turn waited. `denied` means no dialog and the tool did not run. `EPERM` means the command ran inside the sandbox and the kernel refused the write. A dash means the probe was not part of that session.

| Posture                                                     | read in ws | `cat`  | shell write | native write | native edit | read outside | shell write outside | native write outside |
| ----------------------------------------------------------- | ---------- | ------ | ----------- | ------------ | ----------- | ------------ | ------------------- | -------------------- |
| default mode, `request-review` †                            | ran        | DIALOG | DIALOG      | DIALOG       | DIALOG      | DIALOG       | DIALOG              | DIALOG               |
| `strict`                                                    | DIALOG     | DIALOG | DIALOG      | DIALOG       | –           | –            | –                   | –                    |
| `strict` + allow rules covering all four †                  | DIALOG     | DIALOG | DIALOG      | DIALOG       | –           | –            | –                   | –                    |
| `request-review` + allow rules (see below)                  | ran        | ran    | DIALOG      | ran          | DIALOG      | ran          | DIALOG              | DIALOG               |
| `--mode plan`                                               | ran        | DIALOG | DIALOG      | DIALOG       | DIALOG      | –            | –                   | –                    |
| `--mode plan` + skip flag †                                 | –          | –      | ran         | ran          | ran         | –            | –                   | –                    |
| `--mode accept-edits`                                       | –          | DIALOG | DIALOG      | ran          | ran         | –            | –                   | DIALOG               |
| `--mode auto`                                               | ran        | ran    | ran         | ran          | ran         | DIALOG       | ran ‡               | DIALOG               |
| `always-proceed`                                            | ran        | ran    | ran         | ran          | ran         | ran          | ran                 | ran                  |
| `always-proceed` + three deny rules †                       | ran        | ran    | denied      | denied       | denied      | denied       | **ran**             | denied               |
| skip flag + the same deny rules                             | ran        | ran    | denied      | denied       | denied      | denied       | **ran**             | denied               |
| `--sandbox` + `request-review`                              | ran        | DIALOG | DIALOG      | DIALOG       | –           | DIALOG       | DIALOG              | –                    |
| sandbox + `proceed-in-sandbox`                              | ran        | ran    | EPERM       | DIALOG       | DIALOG      | DIALOG       | ran ‡               | DIALOG               |
| `--sandbox` + `always-proceed`                              | ran        | ran    | ran         | ran          | ran         | ran          | ran                 | ran                  |
| `request-review` + hook `auto_approve` †                    | ran        | ran    | ran         | ran          | ran         | ran          | ran                 | ran                  |
| `request-review` + hook `deny_unless_prior_grant`           | ran        | denied | denied      | denied       | –           | –            | –                   | –                    |
| the same + allow rules for `cat`, one file, one directory † | ran        | ran    | denied      | ran          | denied      | ran          | –                   | –                    |

‡ The sibling directory is under `/private/tmp`, which the sandbox mounts writable. Section 6 measures a path that is not.

The deny rules in the two bold rows are `command(printf)`, `write_file(<workspace>)` and `read_file(<sibling>)`. The bold cell is the anomaly in section 5.

The allow rules in the fourth row are `command(cat)`, `command(printf)`, `write_file(<workspace>/native_marker.txt)` and `read_file(<sibling>)`. `command(printf)` did not cover `printf … > file`: a redirect is not matched by a prefix allow rule in review mode.

In the last row the shell write was `touch touch_marker.txt`, and the allowed file and directory were the native-write target and the sibling directory.

### `strict` does not consult the allowlist

`strict` asked before every one of 16 tool calls, including reads of the workspace's own files, which every other mode grants silently. Allow rules for the exact actions made no difference on either version. Its command dialog offers only `Yes, run command` and `No, cancel`: there is no "always allow" option to take.

The vendor reference says the same: `strict` "prompts before every tool action, including file reads, even if an allow rule matches". There is no allowlist-backed strict mode to build on.

### `--mode plan` is a prompt prefix

With `--mode plan` the status bar reads `plan` and every prompt reaches the model with `/plan ` in front of it. The hook payload's `lastUserInput` and the transcript both show it:

```text
/plan Use the run_command tool exactly once to run this exact shell command: printf SHELL_OK > shell_marker.txt
```

The model and the permission decisions are unchanged, and the editing tools stay available. The 1.5.9 binary carries text for a two-model planning flow in which "Editing tools are refused while plan mode is on"; nothing of the kind happened here. A shell write, a `write_file` and an `edit_file` all ran under plan mode with the skip flag.

Plan mode also rewrites the worker's brief, which a Reviewer does not want.

### `always-proceed` and the skip flag behave alike

The two rows with deny rules are identical cell for cell. Deny rules applied under both. The remaining difference between them, documented and not measured here, is the system command denylist that 1.5.2 added to the skip flag.

## 4. Trust

Carried forward from the pane study and re-measured on 1.5.9:

- **The store is `trustedWorkspaces`.** Neither binary contains the string `trustedFolders`.
- **Pre-seeding suppresses the dialog, through either channel.** 63 interactive sessions passed the workspace in `--settings` and three had it written into `settings.json`; none showed the dialog. A session with neither showed it 1.9 s after start.
- **`--settings` trust is not saved.** The real file's hash was unchanged after every such session.
- **Matching is exact.** Trusting the parent directory did not trust the workspace. Trusting the workspace by an unresolved path (`/tmp/…` for `/private/tmp/…`) did not either.
- **The dialog's default answer is yes.**

```text
Do you trust the contents of this project?
Antigravity CLI requires permission to read, edit, and execute files here.
> Yes, I trust this folder
  No, exit
```

The yes answer was not exercised: the CLI records trusted folders in `settings.json`, so a Return delivered to this screen would trust the folder for every later session. `No, exit` exited with status 0.

## 5. Rules against shell commands

All under `always-proceed`, so that nothing but the rule decides. `denied` means the rule stopped the command; `ran` means it executed.

### Deny rules

| Rule                              | Command                                 | Result           |
| --------------------------------- | --------------------------------------- | ---------------- |
| `command(printf)`                 | `printf X`                              | denied           |
|                                   | `printf A > file`                       | denied           |
|                                   | `FOO=1 printf A > file`                 | denied           |
|                                   | `echo hi && printf A > file`            | denied           |
|                                   | `/usr/bin/printf A > file`              | **ran**          |
|                                   | `sh -c 'printf A > file'`               | **ran**          |
| `command(git push)`               | `git push --dry-run`                    | denied           |
|                                   | `GIT_TRACE=0 git push --dry-run`        | denied           |
|                                   | `git -C . push --dry-run`               | **ran**          |
|                                   | `sh -c 'git push --dry-run'`            | **ran**          |
| `write_file(<workspace>)`         | `printf A > file`, `touch`, `cp`, `tee` | **ran** (5 of 5) |
|                                   | native `write_file`                     | denied           |
| `command(regex:echo .*)`          | `echo hello_regex`                      | denied           |
| `command(regex:ls .*secretdir.*)` | `ls -la secretdir_x`                    | **ran**          |
| `command(regex:.*bossctl.*)`      | `echo bossctl_literal`                  | **ran**          |

Three conclusions.

- **A command deny is a prefix match on the command as typed.** An absolute path to the binary, a `sh -c` wrapper, or a flag between two rule words all get past it. Environment-variable prefixes and `&&` chains do not.
- **File rules do not bind the shell.** A `write_file` deny stopped the native tools and nothing else.
- **Regex denies are not a substring filter.** Two of three failed to match a command a plain regex search would match. The matching rule was not established.

### A deny rule that makes another deny rule stop working

With `command(printf)` denied, `printf A > <dir>/x` is denied. Add a `read_file(<dir>)` deny for the directory being written to, and the same command **runs and writes the file**.

| Deny rules in force                                              | `printf … > <dir>/file` |
| ---------------------------------------------------------------- | ----------------------- |
| `command(printf)`                                                | denied                  |
| `command(printf)`, `write_file(<workspace>)`                     | denied                  |
| `command(printf)`, `read_file(<dir>)`                            | **ran**                 |
| `command(printf)`, `write_file(<workspace>)`, `read_file(<dir>)` | **ran** (4 sessions)    |
| the same three, in a different order                             | **ran**                 |

Six of six sessions that included the `read_file` deny, one of them on 1.5.7 and one under the skip flag. In every one of them another `printf` command in the same session was still denied.

The cause was not found. The shape matters because it is the design's structural deny set: a command deny for `sudo`, `rm` and `bossctl` next to a file deny for the Boss state directory. Under that pair, a denied command that redirects its output into the protected directory is both allowed to run and allowed to write there.

### In review mode a deny rule can turn into a question

In review mode, `command(printf)` denied `printf X` with no dialog. For `printf … > file` it raised the ordinary approval dialog instead (3 of 3). Whether approving would then be refused was not tested.

### Allow rules are not a read-only guarantee

Under `deny_unless_prior_grant` with `command(cat)` allowed:

| Command                           | Result                                  |
| --------------------------------- | --------------------------------------- |
| `cat inside.txt`                  | ran                                     |
| `/bin/cat inside.txt`             | denied                                  |
| `cat inside.txt && touch x`       | denied                                  |
| `cat inside.txt > copy.txt`       | **ran**, wrote `copy.txt`               |
| `cat $(touch x; echo inside.txt)` | **ran**, and the inner `touch` made `x` |

An allow rule for a read-only command also allows that command with a redirect, and anything inside a command substitution. An allowlist of "safe" commands is not a write barrier.

## 6. The sandbox

On macOS the sandbox is `sandbox-exec` with a generated Seatbelt profile. What it confines depends on the permission mode it is paired with. Two sessions used a `jj` secondary workspace outside the temp tree, with its repository store in a sibling directory, the layout a cube workspace has.

| Probe                                    | sandbox + `proceed-in-sandbox`  | `--sandbox` + `always-proceed`                      |
| ---------------------------------------- | ------------------------------- | --------------------------------------------------- |
| read a file in the workspace             | ran                             | ran                                                 |
| write into the workspace                 | EPERM                           | ran; EPERM once `write_file(<workspace>)` is denied |
| read a sibling directory                 | EPERM                           | ran                                                 |
| write a sibling directory                | EPERM                           | **ran**                                             |
| write under `/private/tmp`               | ran                             | ran                                                 |
| network (`curl https://example.com`)     | blocked: could not resolve host | HTTP 200                                            |
| list `~/Library/Application Support`     | blocked                         | blocked                                             |
| `jj status`, `jj log`, `jj diff`         | **all fail**                    | **all fail**                                        |
| nested `sandbox-exec`                    | not run                         | `sandbox_apply: Operation not permitted`, exit 71   |
| `run_command` with `BypassSandbox: true` | DIALOG                          | **ran unsandboxed, no prompt** (1.5.9 and 1.5.7)    |

### Why it cannot be the Reviewer posture

- **`jj` does not start.** Its user configuration is under `~/Library/Application Support/jj/`, which the sandbox hides: `Cannot access …/jj/config.toml … Operation not permitted`. A reviewer that cannot run `jj diff` cannot review. Under `proceed-in-sandbox` the repository store, which sits outside a secondary workspace, is unreadable as well.
- **The repository's own tests cannot run.** `bazel test` here wraps each test in `sandbox-exec`, and a nested profile is refused. This is one reason the Grok driver runs with its sandbox off on local macOS.
- **Leaving the sandbox is the model's decision.** `run_command` takes a `BypassSandbox` argument, and the CLI suggests it in the tool result when a command fails. Under `always-proceed` a bypassed `touch` wrote into a workspace that both the sandbox and a `write_file` deny were protecting. A hook `auto_approve` approves a bypass too.
- **Paired with `always-proceed` it confines little.** Sibling directories were writable and the network was open.

### What it can still do

Paired with `proceed-in-sandbox` in the default agent mode, shell commands run without a dialog against a workspace the kernel will not let them write. That is the one place in this study where a shell write was stopped by something a command string cannot talk its way around.

A hook can hold a session there: a reply of `{"decision": "auto_approve", "overwrite": {"BypassSandbox": false}}` to a bypass request ran the command inside the sandbox, where it failed with EPERM. That makes the sandbox a possible second layer for a kind whose commands all work inside it. On this host a `jj`-based Reviewer is not such a kind.

## 7. Hook verdicts

The hook surface study measured `allow`, `deny`, `ask`, `force_ask` and an unknown verdict. This section covers what it left open. Review mode unless stated.

| Hook reply                                             | Result                                                                                                                      |
| ------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------- |
| nothing on stdout                                      | Normal evaluation: the dialog                                                                                               |
| `allow`                                                | The dialog                                                                                                                  |
| **`auto_approve`**                                     | **Runs, no dialog.** 31 of 31 calls, nine of them on 1.5.7. Covers shell, native writes, and files outside the workspace.   |
| `auto_approve`, and a deny rule matches                | Denied by the rule (3 of 3)                                                                                                 |
| `auto_approve`, and an ask rule matches                | The dialog                                                                                                                  |
| `auto_approve` under `strict`                          | The dialog (4 of 4)                                                                                                         |
| `deny` with `reason`                                   | Denied, no dialog (10 of 10). The model receives `tool call denied by pre-tool hook: <reason>`.                             |
| **`deny_unless_prior_grant`**                          | **Denied, no dialog, unless an allow rule or the built-in workspace-read grant covers the call** (21 calls, five on 1.5.7). |
| `permissionOverrides` beside `allow` or `ask`          | The dialog, as without it                                                                                                   |
| `permissionOverrides` beside `deny_unless_prior_grant` | Denied. The error names the override, `command(touch)`, as the resource checked. It is not a grant.                         |
| deprecated `allowTool: true` (under `always-proceed`)  | Runs                                                                                                                        |
| deprecated `allowTool: false` with `denyReason`        | Denied; the model receives the reason                                                                                       |

### `auto_approve` and `deny_unless_prior_grant` are the two halves Boss needs

Boss has two kinds of posture. Standard, Reviewer and Triage are allow-by-default behind a deny list and the guard scripts. AnswerAgent, ReviewGuide and Progression are deny-by-default behind an allowlist, which on Claude is `dontAsk`.

- `auto_approve` is the first: the hook, after the guards have passed the call, lets it run without a dialog.
- `deny_unless_prior_grant` is the second: every call is refused without a dialog unless `permissions.allow` covers it. Nothing in settings alone does this; without the verdict an ungranted call asks.

Neither needs `--dangerously-skip-permissions`.

### A Reviewer-shaped session

Settings `{"agentMode": "", "toolPermission": "request-review", "permissions": {"deny": ["write_file(<parent of workspace>)"]}}`. The hook answered `deny` with a reason for `write_file` and `edit_file`, and `auto_approve` for everything else. Identical on 1.5.9 and 1.5.7:

| Probe                             | Result                                                                                            |
| --------------------------------- | ------------------------------------------------------------------------------------------------- |
| read in workspace, read outside   | ran                                                                                               |
| `cat`                             | ran                                                                                               |
| native write, edit, write outside | denied, and the model was told why: `tool call denied by pre-tool hook: reviewer is read-only: …` |
| shell write, shell write outside  | ran, because this hook approved every command                                                     |
| dialogs                           | 0 of 8                                                                                            |

The two shell writes are the residue every driver has: the Grok driver's own notes say a shell-driven write evades its edit rule. Closing it is the guard's job, by judging the command, and section 5 is the evidence that `agy`'s rules cannot do it instead.

### What happens when the hook is not there

This is the case for review mode over the skip flag.

| Hook state            | Review mode                                                                                                   | Skip flag or `always-proceed` |
| --------------------- | ------------------------------------------------------------------------------------------------------------- | ----------------------------- |
| loaded, replies       | the verdict decides                                                                                           | a `deny` still denies         |
| loaded, replies badly | denied for an unknown verdict (hook study); a crash or malformed output was measured only under the skip flag | denied (earlier studies)      |
| **not loaded**        | **dialog on the first ungranted call**                                                                        | **everything runs**           |

A workspace-local `hooks.json` in an untrusted workspace produces the third row (pane study), and so would a `hooks.json` the CLI silently fails to read, as in the byte-order-mark defect fixed in 1.5.8. Review mode turns that into a visible stop. It is not a safe stop by itself, because of section 8.

## 8. Approval dialogs in an unattended pane

### What they look like

Four shapes were met, each with `esc to cancel` in the footer, which the pane study already found reads as busy.

```text
Requesting permission for:
   printf SHELL_OK > shell_marker.txt

Run this command?
> 1. Yes, run command
  2. Yes, and always allow in this conversation for commands that start with '…'
  3. Yes, and always allow for commands that start with '…' (Persist to settings.json)
  4. No, cancel
```

The others are `Allow creation of this file?`, `Accept this file edit?` and `Allow access to this file?`. In all four the highlighted default is yes.

While a dialog is pending, the tool row above it already reads `● Ran (printf SHELL_OK > shell_marker.txt)`. The word is not evidence that anything ran.

### They do not time out, and declining leaves no trace in hooks

- One dialog was held for 150 s. It was still up; no hook fired after `PreToolUse`.
- Declining ends the turn like Escape does: `User declined the tool call`, then `Interrupted · What should Antigravity CLI do instead?`. **No `Stop` fired in 57 of 57 declined turns.**

### Text sent to the pane answers the dialog

| Sent while the dialog was up                                 | What happened                                                                                                                                                                                   |
| ------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A bracketed paste of a one-line message, then Return         | The paste was discarded. Return chose `1. Yes`. **The command ran** and the turn ended with a clean `Stop`. 1.5.9 and 1.5.7, and twice more in a pilot session.                                 |
| The typed text `3 quick questions: are you blocked?`, Return | `3` chose option 3 at once. **The command ran and `"permissions": {"allow": ["command(touch)"]}` was saved to the global `settings.json`.** The rest of the text was submitted as a new prompt. |

These are the two ways the engine delivers a probe, a nudge or an answer. Spawn confirmation's Return retry is a bare Return, which is the first row.

The saved rule is global: it is written to the `HOME`'s `settings.json`, not to anything scoped to the conversation or workspace. In a shared `HOME` it would become every later session's grant, the owner's included.

### Consequence

A session must be configured so that no dialog can appear, and the engine must refuse to type into a pane that shows one. The strings to refuse on:

```text
Run this command?            Allow creation of this file?    Accept this file edit?
Allow access to this file?   Do you trust the contents of this project?
Settings Error
```

## 9. Soft-deny

How each way of not running a tool looks from outside. Interactive unless stated.

| Route                                         | `PreToolUse` | `PostToolUse`         | `Stop`                                       | Transcript step                                                                            | Pane                                   |
| --------------------------------------------- | ------------ | --------------------- | -------------------------------------------- | ------------------------------------------------------------------------------------------ | -------------------------------------- |
| Ungranted tool, review / `strict` / plan mode | fires        | none                  | **none; the turn waits on the dialog**       | —                                                                                          | dialog                                 |
| Dialog declined                               | fires        | none                  | **none**                                     | —                                                                                          | `User declined the tool call`          |
| Deny rule                                     | fires        | none                  | `NO_TOOL_CALL`, `fullyIdle=true`, `error=""` | `status: "ERROR"`, `error: "permission check failed … Matches user-configured deny rule."` | see below                              |
| Hook `deny`                                   | fires        | none                  | the same                                     | `ERROR`, `tool call denied by pre-tool hook: <reason>`                                     | see below                              |
| `deny_unless_prior_grant`, no grant           | fires        | none                  | the same                                     | `ERROR`, `Permission denied for …`                                                         | see below                              |
| Sandbox refuses the write                     | fires        | **fires, `error=""`** | the same                                     | `DONE`, `Command exited with code 1`                                                       | `● Ran (…) failed`                     |
| Headless, ungranted tool                      | fires        | none                  | **none**                                     | —                                                                                          | exit 0, empty stdout, notice on stderr |

The headless notice names the missing rule:

```text
jetski: no output produced — a tool required the "command" permission that headless mode cannot prompt for, so it was auto-denied. Add an allow-rule under permissions.allow in settings.json (e.g. command(<target>)). …
```

### What this means for detection

- **The usable signal is a `PreToolUse` with no `PostToolUse` for the same `toolCall.id` by the next idle `Stop`.** It held for all 62 rule and hook denials. The hook fires before the permission decision, so a rule denial is visible to the hook stream, which was open question 6 of the pane study.
- **`Stop.error` is empty in every case.** It cannot be the signal.
- **The transcript corroborates.** A refused step has `"status": "ERROR"` and the reason in `error`. A command that ran and failed has `DONE`.
- **A sandbox refusal is not a soft-deny.** The command ran and failed, so `PostToolUse` fires. Hooks cannot tell it from any failing command, which is the absence of command outcomes the hook surface study already recorded.
- **The pane's tool row is not a signal.** A refused command reads `● Ran (…) failed`, the same as a command that ran and exited non-zero. A refused `write_file` reads `● Edited 1 file (…)` and a refused `read_file` reads `● Explored 1 file (…)`, exactly as a successful one does.

### What the model said afterwards

In all 62 denials the model answered `RESULT: NOT_DONE` with the reason, because the prompt offered that answer. The pane study's prompt asked only for a completion token and got it four times out of four. The boundary is clean either way; what the model claims depends on what it was asked.

## 10. Choosing the Reviewer read-only expression

What Boss requires of a Reviewer, from `worker_kind.rs` and `reviewer_deny_rules`: read the diff and the workspace, write one result file outside the checkout, never write under the workspaces root, never publish. It must also run unattended, so it must never raise a dialog.

| Requirement                   | `strict` + allowlist | `--mode plan`                                 | `--sandbox`                                    | `request-review` + hook verdicts            |
| ----------------------------- | -------------------- | --------------------------------------------- | ---------------------------------------------- | ------------------------------------------- |
| Never raises a dialog         | No: every call       | No: every command, edit                       | Only with `proceed-in-sandbox`, until a bypass | Yes: 0 in 62 calls                          |
| Reads the workspace           | After a dialog       | Yes                                           | Yes                                            | Yes                                         |
| Runs `jj`                     | After a dialog       | After a dialog                                | **No**                                         | Yes                                         |
| Blocks native file writes     | By dialog only       | By dialog only; not at all with the skip flag | By dialog only, or by deny rule                | Yes: hook `deny` or `write_file` deny rule  |
| Blocks shell writes           | By dialog only       | No                                            | Yes, kernel-enforced, until a bypass           | Only as far as the guard judges the command |
| Allows the out-of-tree result | After a dialog       | After a dialog                                | In the temp tree (measured for `/private/tmp`) | Yes: `auto_approve`                         |
| Fails closed if misconfigured | n/a                  | n/a                                           | No: bypass                                     | Stops at a dialog                           |

**Recommendation: `request-review` with hook verdicts.** Of these four it is the only one that both runs unattended and refuses anything.

Among the three options the study was asked to choose between, only `--sandbox` enforces anything, and it is ruled out here by `jj` and by the bypass argument. `strict` and `--mode plan` do not enforce a read-only workspace in any configuration measured.

### The settings to render

```json
{
  "agentMode": "",
  "toolPermission": "request-review",
  "trustedWorkspaces": ["<absolute, symlink-resolved workspace path>"],
  "permissions": {
    "deny": ["write_file(<workspaces root>)", "read_file(<Boss data directory>)"]
  }
}
```

Passed as `--settings <file>`. For the deny-by-default kinds, add `permissions.allow` and have the hook answer `deny_unless_prior_grant`.

Because of the anomaly in section 5, do not add command denies to a rule list that also has a `read_file` deny. Command denies go in the hook.

### What the recommendation does not give

- **A shell-write barrier for the Reviewer.** The same gap the Grok driver documents. Section 5 shows `agy`'s allow rules do not close it either: a redirect or a command substitution rides on an allowed command.
- **Safety when the hook is missing.** The session stops at a dialog, and section 8 shows the engine can then approve it by accident. The engine must confirm the hook is live before it sends anything, and must not type at a dialog.
- **Protection against the default drifting.** `agentMode` already changed its default once.

## Corrections to the design

| Design says                                                                                     | Measured                                                                                                                                    |
| ----------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| Reviewer read-only is one of `strict` + allowlist, `--mode plan`, or `--sandbox`                | None of them. Review mode with hook verdicts.                                                                                               |
| Posture is a `toolPermission` level plus a `permissions.allow` allowlist per worker kind        | `strict` ignores the allowlist. An allowlist denies without asking only when the hook answers `deny_unless_prior_grant`.                    |
| `agy` has no per-run permission flag other than a blanket allow-all                             | `--settings` takes any `settings.json` keys, inline or from a file, for one session, and is never saved.                                    |
| `PermissionArtifacts::extra_args` carries `--sandbox`, and `--mode plan` if it proves read-only | It carries `--settings <file>`.                                                                                                             |
| The structural deny set (`bossctl`, state directory, `rm`, `sudo`) is rendered as deny rules    | Command denies are evaded by an absolute path, a wrapper, or a flag, and stop working next to a `read_file` deny. They belong in the guard. |
| The interactive path may soft-deny as headless does                                             | It blocks on a dialog. Headless soft-deny emits no `Stop`.                                                                                  |
| Soft-deny detection stages a marker from the progress session                                   | The marker can be derived there from an unanswered `PreToolUse` at the next idle `Stop`. No extra hook is needed.                           |
| The first-run trust prompt "never blocks a pane" once pre-seeded                                | True, and the pre-seed must be the exact resolved path. A parent directory does not count.                                                  |
| (unstated) the session starts in the default agent mode                                         | It starts in Auto unless `agentMode` is set.                                                                                                |

## Incidental observations

- **A vendor error dropped two prompts without a hook.** One session's prompts were answered on screen with `Eligibility check failed: failed to fetch user info: 500 Internal Server Error`. The composer returned to idle; no `SessionStart`, no `Stop`. The next session was unaffected. Spawn confirmation should not assume an accepted paste starts a turn.
- **A version rewrites `~/.gemini/antigravity-cli/builtin/` when it starts.** After 1.5.9 ran in the home that 1.5.7 normally uses, eight paths under it differed. A Boss-pinned binary sharing a `HOME` with a different installed version would presumably flip that tree back and forth.
- **`enableTelemetry` is not a settings key.** The published schema marks it deprecated and unread; the telemetry choice is stored with the account.

## Limits of this study

- A pseudo-terminal, not a GhosttyKit surface, and no Boss dispatch.
- The real `HOME`. Per-run-`HOME` behaviour of `settings.json` is inferred from seven cells that modified the real file.
- One session per cell. Ten sessions repeated the deciding cells on 1.5.7; none disagreed.
- One personal account and one model. Which tool the model picks for a request is the model's choice; prompts named the tool.
- "Outside the workspace" was a sibling under `/private/tmp` except in the two `jj` sessions.
- Not measured: MCP, URL and browser rules; subagent permission inheritance; project and shared rule scopes; how several `PreToolUse` hooks combine, such as a silent forwarder beside a guard that answers; hook timeouts in review mode; `auto_approve` on 1.5.5; Bazel inside the sandbox; approving a dialog raised over a deny rule.
- The deny-rule anomaly is reproduced, not explained.

## Follow-up work

Code and document changes this study implies. None is made here.

1. **Design: replace the Reviewer open choice** with review mode plus hook verdicts, and rewrite the permission-rendering entry accordingly. Pin `agentMode` in every rendered posture.
2. **Permission rendering:** emit the session settings above and pass them with `--settings`. Validate every rule string before writing. After spawn, fail the run if the log contains `ignoring invalid` or the `CLI settings initialized` line does not match what was rendered.
3. **Hook adapter:** map a guard approval to `auto_approve`, never to `allow`. Map everything else to `deny` with a reason. Answer `deny_unless_prior_grant` for the allowlist kinds. Carry the structural command denies in the guard. Deny or rewrite `BypassSandbox: true`.
4. **Pane monitor and launcher:** treat the six dialog strings as a blocked state. Never deliver a probe, nudge, answer or Return retry while one is on screen; fail the run instead. Add `Settings Error` to the launcher's starting markers.
5. **Hook liveness before the first brief:** a session whose hooks did not load must not receive input.
6. **Soft-deny detection:** key on a `PreToolUse` whose `toolCall.id` has no `PostToolUse` by the next idle `Stop`; use the transcript's `status: "ERROR"` as corroboration.
7. **Conformance fixtures:** the Auto default, `--settings` rejecting an unknown key, and the `command` plus `read_file` deny anomaly, so a later version that changes any of them is noticed.

## Appendix A — Reproducing

Each cell is one `agy` session started in an empty directory:

```sh
AGY_CLI_DISABLE_AUTO_UPDATE=true AGY_CLI_HIDE_ACCOUNT_INFO=true \
  agy --settings <settings.json> --model gemini-3.8-flash-low --log-file <scratch log> [--sandbox | --mode … | --dangerously-skip-permissions]
```

- `<settings.json>` holds the cell's keys plus `trustedWorkspaces` naming the directory.
- `<directory>/.agents/hooks.json` wires all six events to a script that saves stdin and prints the cell's reply, or nothing.
- Prompts are pasted into the composer and submitted with Return. A turn is over at the parent's `Stop` with `fullyIdle=true`, or when the composer has been idle for four seconds with no `Stop`.
- A session needs a signed-in account and spends its quota.

## Appendix B — What the study touched on the host

- **No keychain link, copy or accessor.** Sessions ran in the real `HOME` and used the vendor's own sign-in path. No fresh home was pointed at the login keychain.
- **Sign-in checked before every session.** `agy models` exited 0 before each of the 84. Two `agy models` runs refreshed the stored token, each as the only `agy` process running; both saves succeeded. No log contains a failed save, a sign-in screen, or `Logging out`.
- **One session at a time, supervised.** Each was a direct child of a supervisor that killed its children on exit, error or signal. No `nohup`, `setsid`, `disown` or tmux. `caffeinate -dimsu` held the machine awake for every batch. No `agy` process of the study's remained at the end, and the supervisor's process monitor saw none of the owner's during any batch.
- **State restored and verified.** `~/.gemini/config` and `~/.gemini/antigravity-cli` were backed up before each batch and restored after each session. The final comparison against the manifest taken before the first `agy` command found no difference in 2,240 entries, by content, mode and link target. The backups were then deleted.
- **One restore was done by hand.** The closing sign-in check ran the installed binary, which the supervisor's monitor counted as a process that was not its own, so it skipped that batch's restore. The restore was run manually and verified against the same manifest.
- **The real `settings.json` was modified eight times and restored each time.** Seven cells replaced it for under 30 seconds each, keeping the owner's keys. In an eighth, the typed-input test in section 8 made the CLI save `command(touch)` as a global always-allow rule; it was removed when that session ended, under 30 seconds later. The file's hash afterwards equals its hash before the study.
- **No global hooks.** Nothing was installed in `~/.gemini/config/hooks.json`.
- **The installed binary was not modified or updated.** Its hash is unchanged. The 1.5.9 archive was downloaded from the vendor's release manifest and matched its published SHA-512.
- **A throwaway `jj` repository** was created under the workspace's ignored `.claude/` directory for the two sandbox sessions and removed afterwards.
