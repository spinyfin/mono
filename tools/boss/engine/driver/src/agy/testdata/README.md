# `agy` transcript captures

Verbatim `transcript_full.jsonl` files from real `agy` 1.5.9 sessions, used by the
transcript normaliser's tests (`../transcript_tests.rs`). The file a hook payload
stamps as `transcriptPath` is
`$HOME/.gemini/antigravity-cli/brain/<cid>/.system_generated/logs/transcript_full.jsonl`.

Each was produced by a one-off `agy -p "<prompt>"` print-mode run in an empty scratch
workspace under a scoped `HOME` (login keychain bridged, onboarding and workspace trust
pre-seeded, `AGY_CLI_DISABLE_AUTO_UPDATE=true`), model `gemini-3.8-flash-low`:

| File                          | Prompt asked for                                                     | What it pins                                                                                                             |
| ----------------------------- | -------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `tool_calls_1.5.9.jsonl`      | two foreground shell commands, the second exiting 7                  | `PLANNER_RESPONSE` `tool_calls` (no call id), `GENERIC` result rows, a non-zero exit on a `DONE` row, token counts       |
| `denied_steps_1.5.9.jsonl`    | two shell commands, each refused by a `PreToolUse` hook that exits 2 | `GENERIC` rows with `status: "ERROR"` and an `error` field; prose and a tool call in one `PLANNER_RESPONSE`              |
| `background_task_1.5.9.jsonl` | a shell command run with a 1 s async threshold (`sleep 6`)           | `GENERIC` `status: "RUNNING"` launch acknowledgement; the completion arriving as a `SYSTEM_MESSAGE` (`source: "SYSTEM"`) |
| `blocked_marker_1.5.9.jsonl`  | "reply with exactly `[blocked] reason=…`", no tools                  | the worker's own prose as a `PLANNER_RESPONSE` `content`                                                                 |

Capture notes:

- Rows are byte-for-byte what the CLI wrote, except that `background_task_1.5.9.jsonl`
  has the scratch-directory prefix inside two `file://…/tasks/task-2.log` URLs replaced
  with `/tmp/agy-capture/home`. No other capture contained a host path.
- `denied_steps_1.5.9.jsonl` was captured with `.gemini/antigravity-cli/brain` as a
  symlink to a directory outside the scoped `HOME` — the layout the durable transcript
  store provisions. The CLI wrote the transcript (and its `chunks/` copies) through the
  link and left the link in place. Everything else the CLI keeps per conversation
  (`scratch/`, `.user_uploaded/`, and, when a command ran, `.system_generated/steps/<n>/output.txt`)
  therefore lands in the durable store too.
- Not captured, and therefore not asserted from a capture: a halting model/API error
  (the CLI's step enum names `ERROR_MESSAGE`; the tests that use that type build a
  synthetic row and say so) and a cancelled turn (the pane study's evidence shows the
  cancelled step is absent; the cancelled-step test is built from that description).
