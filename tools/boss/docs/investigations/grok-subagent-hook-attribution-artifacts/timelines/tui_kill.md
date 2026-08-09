# `tui_kill` — SIGKILL of the grok process mid-subagent

## Hook timeline

`session` is `parent` for the top-level session id, `CHILD` for a subagent's.
Every one of these reaches Boss under the **same** `_boss_run_id`.

```
t_rel    event                  session  tool                       reason/subagentId
   +0.0s session_start          parent
   +0.3s user_prompt_submit     parent
   +3.1s pre_tool_use           parent   spawn_subagent
   +3.2s post_tool_use          parent   spawn_subagent
   +3.3s subagent_start         parent                              019fe54c-0eaa-7e91-9246-cef8503e5e02
   +4.5s stop                   parent                              end_turn
   +5.1s user_prompt_submit     CHILD
   +7.7s pre_tool_use           CHILD    run_terminal_command
```

## PreToolUse guard decisions

One line per guard invocation. `deny` uses Grok's native vocabulary;
`session_id` shows whether the guard saw the parent's call or the child's.

```json
{"wall": 1786258394.790627, "pid": 66597, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "878d0b7d-b5a1-43e1-b12c-e5529e591473", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_kill%2Fcwd/878d0b7d-b5a1-43e1-b12c-e5529e591473/updates.jsonl", "tool_name": "spawn_subagent", "tool_use_id": "call-c59198bf-a97e-4bfc-b063-a45f620bdff5-0", "decision": "allow", "tool_input_excerpt": "{\"prompt\": \"Run this single shell command with run_terminal_command and wait for it to finish: sleep 45 && echo SLOW_CHILD > slow_child.txt\\nThen reply with exactly: CHILD_SLOW_DONE\", \"description\": \"slow child\", \"subagent_type\": \"general-purpose\", \"background\": true}"}
{"wall": 1786258399.463486, "pid": 66891, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "019fe54c-0eaa-7e91-9246-cef8503e5e02", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_kill%2Fcwd/019fe54c-0eaa-7e91-9246-cef8503e5e02/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-2d9a322f-fc83-47ec-9a67-a809480d31bf-0", "decision": "allow", "tool_input_excerpt": "{\"command\": \"sleep 45 && echo SLOW_CHILD > slow_child.txt\", \"description\": \"Sleep 45s then write SLOW_CHILD file\", \"timeout\": 60000}"}
```

## Probe cwd after the run (which side effects actually landed)

```
total 0
drwxr-xr-x@ 2 brianduff  staff   64 Aug  9 02:53 .
drwxr-xr-x@ 8 brianduff  staff  256 Aug  9 02:53 ..
```
