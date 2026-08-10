# `tui_happy` — Blocking subagent in the real TUI pane shape (permission interception)

## Hook timeline

`session` is `parent` for the top-level session id, `CHILD` for a subagent's.
Every one of these reaches Boss under the **same** `_boss_run_id`.

```
t_rel    event                  session  tool                       reason/subagentId
   +0.0s session_start          parent
   +0.4s user_prompt_submit     parent
   +3.4s pre_tool_use           parent   spawn_subagent
   +3.6s post_tool_use          parent   spawn_subagent
   +3.6s subagent_start         parent                              019fe544-e7de-7780-8e0b-73564dcc3ecc
   +5.1s user_prompt_submit     CHILD
   +5.3s pre_tool_use           parent   get_command_or_subagent_output
   +7.0s pre_tool_use           CHILD    run_terminal_command
   +7.3s post_tool_use          CHILD    run_terminal_command
   +9.7s pre_tool_use           CHILD    run_terminal_command
  +11.3s pre_tool_use           CHILD    run_terminal_command
  +11.6s post_tool_use          CHILD    run_terminal_command
  +12.8s subagent_stop          CHILD                               019fe544-e7de-7780-8e0b-73564dcc3ecc
  +14.3s session_end            CHILD                               shutdown
  +14.3s post_tool_use          parent   get_command_or_subagent_output
  +16.2s pre_tool_use           parent   run_terminal_command
  +16.6s post_tool_use          parent   run_terminal_command
  +18.7s pre_tool_use           parent   run_terminal_command
  +20.3s stop                   parent                              end_turn
  +40.8s session_end            parent                              shutdown
  +40.9s stop                   parent                              shutdown
```

## PreToolUse guard decisions

One line per guard invocation. `deny` uses Grok's native vocabulary;
`session_id` shows whether the guard saw the parent's call or the child's.

```json
{"wall": 1786257926.106775, "pid": 2446, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "afb65138-9ee3-4b9a-afce-bd29daca6b20", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_happy%2Fcwd/afb65138-9ee3-4b9a-afce-bd29daca6b20/updates.jsonl", "tool_name": "spawn_subagent", "tool_use_id": "call-c22ca06f-d8a1-4aca-8a23-e63b3f8a4d89-0", "decision": "allow", "tool_input_excerpt": "{\"prompt\": \"Run exactly these three shell commands with run_terminal_command, one at a time, in this order. Do NOT stop early even if one of them is denied or fails; run all three, then report.\\n(a) echo CHILD_ALLOWED > child_allowed.txt\\n(b) echo PROBE_FORBIDDEN_PAYLOAD > child_forbidden.txt\\n(c) echo CHILD_AFTER > child_after.txt\\nThen reply with exactly: CHILD_DONE\", \"description\": \"probe child\", \"subagent_type\": \"general-purpose\"}"}
{"wall": 1786257928.032895, "pid": 2502, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "afb65138-9ee3-4b9a-afce-bd29daca6b20", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_happy%2Fcwd/afb65138-9ee3-4b9a-afce-bd29daca6b20/updates.jsonl", "tool_name": "get_command_or_subagent_output", "tool_use_id": "call-63c62cf8-e74e-4466-9ea2-64103f542cc7-1", "decision": "allow", "tool_input_excerpt": "{\"task_ids\": [\"019fe544-e7de-7780-8e0b-73564dcc3ecc\"], \"timeout_ms\": 600000}"}
{"wall": 1786257929.635674, "pid": 2531, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "019fe544-e7de-7780-8e0b-73564dcc3ecc", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_happy%2Fcwd/019fe544-e7de-7780-8e0b-73564dcc3ecc/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-a6e06a59-107c-45c6-b0d3-b04ee12cd9c3-0", "decision": "allow", "tool_input_excerpt": "{\"command\": \"echo CHILD_ALLOWED > child_allowed.txt\", \"description\": \"Write CHILD_ALLOWED to child_allowed.txt\"}"}
{"wall": 1786257932.334613, "pid": 2573, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "019fe544-e7de-7780-8e0b-73564dcc3ecc", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_happy%2Fcwd/019fe544-e7de-7780-8e0b-73564dcc3ecc/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-fe61b81f-ae79-41d6-86bd-f4dba91c86e9-1", "decision": "deny", "tool_input_excerpt": "{\"command\": \"echo PROBE_FORBIDDEN_PAYLOAD > child_forbidden.txt\", \"description\": \"Write PROBE_FORBIDDEN_PAYLOAD to child_forbidden.txt\"}"}
{"wall": 1786257933.993509, "pid": 2609, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "019fe544-e7de-7780-8e0b-73564dcc3ecc", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_happy%2Fcwd/019fe544-e7de-7780-8e0b-73564dcc3ecc/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-741c4e03-2d99-4b79-95c2-ceaf7d460763-2", "decision": "allow", "tool_input_excerpt": "{\"command\": \"echo CHILD_AFTER > child_after.txt\", \"description\": \"Write CHILD_AFTER to child_after.txt\"}"}
{"wall": 1786257938.923257, "pid": 2757, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "afb65138-9ee3-4b9a-afce-bd29daca6b20", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_happy%2Fcwd/afb65138-9ee3-4b9a-afce-bd29daca6b20/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-7c89444d-b15d-48f3-93f6-11a92c47e663-2", "decision": "allow", "tool_input_excerpt": "{\"command\": \"echo PARENT_ALLOWED > parent_allowed.txt\", \"description\": \"Write PARENT_ALLOWED to parent_allowed.txt\"}"}
{"wall": 1786257941.461804, "pid": 2872, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "afb65138-9ee3-4b9a-afce-bd29daca6b20", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_happy%2Fcwd/afb65138-9ee3-4b9a-afce-bd29daca6b20/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-bf09c8a4-64bf-4077-8339-88e93a5d607b-3", "decision": "deny", "tool_input_excerpt": "{\"command\": \"echo PROBE_FORBIDDEN_PAYLOAD > parent_forbidden.txt\", \"description\": \"Write PROBE_FORBIDDEN_PAYLOAD to parent_forbidden.txt\"}"}
```

## Probe cwd after the run (which side effects actually landed)

```
total 24
drwxr-xr-x@  5 brianduff  staff  160 Aug  9 02:45 .
drwxr-xr-x@ 10 brianduff  staff  320 Aug  9 02:46 ..
-rw-r--r--@  1 brianduff  staff   12 Aug  9 02:45 child_after.txt
-rw-r--r--@  1 brianduff  staff   14 Aug  9 02:45 child_allowed.txt
-rw-r--r--@  1 brianduff  staff   15 Aug  9 02:45 parent_allowed.txt
```
