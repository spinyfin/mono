# `queuedMessages` `--settings` parse evidence

Captured 2026-11-18 against `agy` **1.5.10** (`/Users/brianduff/.local/bin/agy --version` → `1.5.10`).
Scratch `HOME`, `AGY_CLI_DISABLE_AUTO_UPDATE=true`. Parse-only: `agy --settings '<json>'` with no prompt, so the CLI never starts a session.

The pane study named the `/config` label "Queued Messages" and recorded only the default. These probes pin the JSON key and the two values the published [settings schema](https://antigravity.google/schemas/v1/settings.json) lists (`queue`, `send-immediately`).

| Probe                              | `--settings` body                                                | Exit | Stderr                                                                                                                         |
| ---------------------------------- | ---------------------------------------------------------------- | ---- | ------------------------------------------------------------------------------------------------------------------------------ |
| Valid key, then an unknown sibling | `{"queuedMessages":"queue","toolPermissions":"typo"}`            | 1    | `unknown setting "toolPermissions"` — `queuedMessages` was accepted; a renamed key would have failed on `queuedMessages` first |
| Invalid value                      | `{"queuedMessages":"not-a-real-mode"}`                           | 1    | `invalid value for setting "queuedMessages": queuedMessages: unrecognized value "not-a-real-mode"`                             |
| Typo key                           | `{"queuedMessage":"queue"}`                                      | 1    | `unknown setting "queuedMessage"`                                                                                              |
| Alternate accepted value           | `{"queuedMessages":"send-immediately","toolPermissions":"typo"}` | 1    | `unknown setting "toolPermissions"` — `send-immediately` is a recognised value                                                 |

A session that passed only `{"queuedMessages":"queue","agentMode":"","toolPermission":"request-review"}` also got past parse and reached the authentication prompt (isolated `HOME`, no credentials). That log is not published: it contains an OAuth URL. The parse-only probes above are the gate.

The live conformance test `installed_agy_accepts_queued_messages_settings_key_when_present` re-runs the first two probes.
