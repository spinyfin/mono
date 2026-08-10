# `tui_bg_outlives` — Background subagent outliving the parent's turn (the blocking finding)

## Hook timeline

`session` is `parent` for the top-level session id, `CHILD` for a subagent's.
Every one of these reaches Boss under the **same** `_boss_run_id`.

```
t_rel    event                  session  tool                       reason/subagentId
   +0.0s session_start          parent
   +1.2s user_prompt_submit     parent
   +4.8s pre_tool_use           parent   spawn_subagent
   +6.4s subagent_start         parent                              019fe548-fe57-7873-b7b3-705dcc6f3c58
   +6.5s post_tool_use          parent   spawn_subagent
   +9.1s stop                   parent                              end_turn
  +24.0s user_prompt_submit     CHILD
  +26.1s pre_tool_use           CHILD    run_terminal_command
  +72.4s post_tool_use          CHILD    run_terminal_command
  +72.3s notification           parent
  +74.8s subagent_stop          CHILD                               019fe548-fe57-7873-b7b3-705dcc6f3c58
  +77.7s session_end            CHILD                               shutdown
  +77.6s user_prompt_submit     parent
  +84.9s stop                   parent                              end_turn
 +130.4s session_end            parent                              shutdown
 +130.6s stop                   parent                              shutdown
```

## PreToolUse guard decisions

One line per guard invocation. `deny` uses Grok's native vocabulary;
`session_id` shows whether the guard saw the parent's call or the child's.

```json
{"wall": 1786258194.001386, "pid": 39327, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "167b7b11-e6b2-49cc-8cda-075b8eb3a32f", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_bg_outlives%2Fcwd/167b7b11-e6b2-49cc-8cda-075b8eb3a32f/updates.jsonl", "tool_name": "spawn_subagent", "tool_use_id": "call-b38d7808-f958-4c23-9978-b78afe804ca1-0", "decision": "allow", "tool_input_excerpt": "{\"prompt\": \"Run this single shell command with run_terminal_command and wait for it to finish: sleep 45 && echo SLOW_CHILD > slow_child.txt\\n   Then reply with exactly: CHILD_SLOW_DONE\", \"description\": \"slow child\", \"subagent_type\": \"general-purpose\", \"background\": true}"}
{"wall": 1786258214.750619, "pid": 41011, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "019fe548-fe57-7873-b7b3-705dcc6f3c58", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_bg_outlives%2Fcwd/019fe548-fe57-7873-b7b3-705dcc6f3c58/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-aca9d4ae-c807-4b86-8961-060f5458bf56-0", "decision": "allow", "tool_input_excerpt": "{\"command\": \"sleep 45 && echo SLOW_CHILD > slow_child.txt\", \"description\": \"Sleep 45s then write slow_child.txt\", \"timeout\": 60000}"}
```

## Probe cwd after the run (which side effects actually landed)

```
total 8
drwxr-xr-x@  3 brianduff  staff   96 Aug  9 02:50 .
drwxr-xr-x@ 10 brianduff  staff  320 Aug  9 02:52 ..
-rw-r--r--@  1 brianduff  staff   11 Aug  9 02:50 slow_child.txt
```
