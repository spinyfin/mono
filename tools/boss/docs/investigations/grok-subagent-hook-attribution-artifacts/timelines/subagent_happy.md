# `subagent_happy` — Headless (-p) cross-check of the blocking-subagent case

## Hook timeline

`session` is `parent` for the top-level session id, `CHILD` for a subagent's.
Every one of these reaches Boss under the **same** `_boss_run_id`.

```
t_rel    event                  session  tool                       reason/subagentId
   +0.0s session_start          parent
   +0.8s user_prompt_submit     parent
   +4.1s pre_tool_use           parent   spawn_subagent
   +4.4s subagent_start         parent                              019fe542-0a52-7982-91c2-83af98ec0500
   +8.4s user_prompt_submit     CHILD
  +10.6s pre_tool_use           CHILD    run_terminal_command
  +11.2s post_tool_use          CHILD    run_terminal_command
  +12.7s pre_tool_use           CHILD    run_terminal_command
  +14.3s pre_tool_use           CHILD    run_terminal_command
  +14.6s post_tool_use          CHILD    run_terminal_command
  +15.6s subagent_stop          CHILD                               019fe542-0a52-7982-91c2-83af98ec0500
  +17.1s session_end            CHILD                               shutdown
  +17.1s post_tool_use          parent   spawn_subagent
  +19.2s pre_tool_use           parent   run_terminal_command
  +19.5s post_tool_use          parent   run_terminal_command
  +20.8s pre_tool_use           parent   run_terminal_command
  +22.4s stop                   parent                              end_turn
  +23.0s session_end            parent                              shutdown
  +23.2s stop                   parent                              shutdown
```

## PreToolUse guard decisions

One line per guard invocation. `deny` uses Grok's native vocabulary;
`session_id` shows whether the guard saw the parent's call or the child's.

```json
{"wall": 1786257738.31634, "pid": 77404, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "d2f2f20f-1d03-4030-bafe-cf934c8b1c18", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Fsubagent_happy%2Fcwd/d2f2f20f-1d03-4030-bafe-cf934c8b1c18/updates.jsonl", "tool_name": "spawn_subagent", "tool_use_id": "call-5ecba317-4d03-44e5-a2a9-80612caa0abb-0", "decision": "allow", "tool_input_excerpt": "{\"prompt\": \"Run exactly these three shell commands with run_terminal_command, one at a time, in this order. Do NOT stop early even if one of them is denied or fails; run all three, then report.\\n   (a) echo CHILD_ALLOWED > child_allowed.txt\\n   (b) echo PROBE_FORBIDDEN_PAYLOAD > child_forbidden.txt\\n   (c) echo CHILD_AFTER > child_after.txt\\n   Then reply with exactly: CHILD_DONE\", \"description\": \"probe child\", \"subagent_type\": \"general-purpose\", \"background\": false}"}
{"wall": 1786257744.9005299, "pid": 77477, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "019fe542-0a52-7982-91c2-83af98ec0500", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Fsubagent_happy%2Fcwd/019fe542-0a52-7982-91c2-83af98ec0500/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-90c58543-2d21-4b19-a7bc-cf0a87d676f7-0", "decision": "allow", "tool_input_excerpt": "{\"command\": \"echo CHILD_ALLOWED > child_allowed.txt\", \"description\": \"Write CHILD_ALLOWED to child_allowed.txt\"}"}
{"wall": 1786257746.9286132, "pid": 77536, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "019fe542-0a52-7982-91c2-83af98ec0500", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Fsubagent_happy%2Fcwd/019fe542-0a52-7982-91c2-83af98ec0500/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-1db01e2e-f7ef-44fe-bb80-70390a35bc8a-1", "decision": "deny", "tool_input_excerpt": "{\"command\": \"echo PROBE_FORBIDDEN_PAYLOAD > child_forbidden.txt\", \"description\": \"Write PROBE_FORBIDDEN_PAYLOAD to child_forbidden.txt\"}"}
{"wall": 1786257748.449635, "pid": 77591, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "019fe542-0a52-7982-91c2-83af98ec0500", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Fsubagent_happy%2Fcwd/019fe542-0a52-7982-91c2-83af98ec0500/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-46e79ba1-77cb-46ec-bbad-b6989028ad5b-2", "decision": "allow", "tool_input_excerpt": "{\"command\": \"echo CHILD_AFTER > child_after.txt\", \"description\": \"Write CHILD_AFTER to child_after.txt\"}"}
{"wall": 1786257753.372085, "pid": 81344, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "d2f2f20f-1d03-4030-bafe-cf934c8b1c18", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Fsubagent_happy%2Fcwd/d2f2f20f-1d03-4030-bafe-cf934c8b1c18/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-03b76017-8aa5-467d-aae0-f5b88357312c-1", "decision": "allow", "tool_input_excerpt": "{\"command\": \"echo PARENT_ALLOWED > parent_allowed.txt\", \"description\": \"Write parent_allowed.txt\"}"}
{"wall": 1786257755.0743759, "pid": 81449, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "d2f2f20f-1d03-4030-bafe-cf934c8b1c18", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Fsubagent_happy%2Fcwd/d2f2f20f-1d03-4030-bafe-cf934c8b1c18/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-0e68c771-7183-4d70-b800-a05d0a1062b4-2", "decision": "deny", "tool_input_excerpt": "{\"command\": \"echo PROBE_FORBIDDEN_PAYLOAD > parent_forbidden.txt\", \"description\": \"Write parent_forbidden.txt\"}"}
```

## Probe cwd after the run (which side effects actually landed)

```
total 24
drwxr-xr-x@  5 brianduff  staff  160 Aug  9 02:42 .
drwxr-xr-x@ 12 brianduff  staff  384 Aug  9 02:42 ..
-rw-r--r--@  1 brianduff  staff   12 Aug  9 02:42 child_after.txt
-rw-r--r--@  1 brianduff  staff   14 Aug  9 02:42 child_allowed.txt
-rw-r--r--@  1 brianduff  staff   15 Aug  9 02:42 parent_allowed.txt
```
