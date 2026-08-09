# `tui_child_fails` — Subagent whose tool call exits non-zero (+ descendant-process sampling)

## Hook timeline

`session` is `parent` for the top-level session id, `CHILD` for a subagent's.
Every one of these reaches Boss under the **same** `_boss_run_id`.

```
t_rel    event                  session  tool                       reason/subagentId
   +0.0s session_start          parent
   +0.6s user_prompt_submit     parent
   +3.3s pre_tool_use           parent   spawn_subagent
   +3.7s subagent_start         parent                              019fe54d-1d5c-7e32-a3d6-3f8b195d5cee
   +5.7s user_prompt_submit     CHILD
   +9.2s pre_tool_use           CHILD    run_terminal_command
  +34.5s post_tool_use          CHILD    run_terminal_command
  +36.3s pre_tool_use           CHILD    run_terminal_command
  +36.6s post_tool_use          CHILD    run_terminal_command
  +38.2s subagent_stop          CHILD                               019fe54d-1d5c-7e32-a3d6-3f8b195d5cee
  +39.8s session_end            CHILD                               shutdown
  +39.8s post_tool_use          parent   spawn_subagent
  +41.2s stop                   parent                              end_turn
  +56.5s session_end            parent                              shutdown
  +56.8s stop                   parent                              shutdown
```

## PreToolUse guard decisions

One line per guard invocation. `deny` uses Grok's native vocabulary;
`session_id` shows whether the guard saw the parent's call or the child's.

```json
{"wall": 1786258464.083884, "pid": 72956, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "62102d81-fe5b-4015-9b88-933f5db4ee3a", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_child_fails%2Fcwd/62102d81-fe5b-4015-9b88-933f5db4ee3a/updates.jsonl", "tool_name": "spawn_subagent", "tool_use_id": "call-5dc72b1c-2896-4fc0-8e00-3c3123cad889-0", "decision": "allow", "tool_input_excerpt": "{\"prompt\": \"Run these two shell commands with run_terminal_command, in order, and do not stop early if one fails:\\n   (a) sleep 25 && exit 7\\n   (b) echo CHILD_RECOVERED > child_recovered.txt\\n   Then reply with exactly: CHILD_FAILED_THEN_RECOVERED\", \"description\": \"failing child\", \"subagent_type\": \"general-purpose\", \"background\": false}"}
{"wall": 1786258469.79365, "pid": 73277, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "019fe54d-1d5c-7e32-a3d6-3f8b195d5cee", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_child_fails%2Fcwd/019fe54d-1d5c-7e32-a3d6-3f8b195d5cee/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-6c6380f3-26d3-41ec-92f7-513e3b9413b8-0", "decision": "allow", "tool_input_excerpt": "{\"command\": \"sleep 25 && exit 7\", \"description\": \"Sleep 25s then exit with code 7\", \"timeout\": 35000}"}
{"wall": 1786258496.999249, "pid": 79099, "hook_name": "global/boss-hooks:pre_tool_use[0].hooks[1]", "session_id": "019fe54d-1d5c-7e32-a3d6-3f8b195d5cee", "transcript_path": "/Users/brianduff/.cache/grok-subagent-hook-probe/home/sessions/%2FUsers%2Fbrianduff%2F.cache%2Fgrok-subagent-hook-probe%2Fevidence%2Ftui_child_fails%2Fcwd/019fe54d-1d5c-7e32-a3d6-3f8b195d5cee/updates.jsonl", "tool_name": "run_terminal_command", "tool_use_id": "call-c9e75c92-3c79-41c5-9a45-985a314c940e-1", "decision": "allow", "tool_input_excerpt": "{\"command\": \"echo CHILD_RECOVERED > child_recovered.txt\", \"description\": \"Write CHILD_RECOVERED to child_recovered.txt\"}"}
```

## Probe cwd after the run (which side effects actually landed)

```
total 8
drwxr-xr-x@  3 brianduff  staff   96 Aug  9 02:54 .
drwxr-xr-x@ 11 brianduff  staff  352 Aug  9 02:55 ..
-rw-r--r--@  1 brianduff  staff   16 Aug  9 02:54 child_recovered.txt
```

## Live descendant processes of the `grok` pid, sampled once a second

Mirrors `background_children.rs::count_live_descendants`' walk. A persistent
subagent process would hold this above zero for the child's whole lifetime;
instead the only non-zero samples are the child's own transient shell tool call.

Sample-count by descendant count: 35 samples -> 0 descendants, 2 samples -> 1 descendants, 25 samples -> 2 descendants

```
wall	descendant_count	commands
1786258452.2	0
1786258453.2	0
1786258455.2	0
1786258456.2	0
1786258457.3	0
1786258458.3	0
1786258459.3	0
1786258460.3	1	/Applications/Xcode.app/Contents/Developer/Library/Frameworks/Python3.framework/
1786258461.4	0
1786258462.4	0
1786258463.4	0
1786258464.5	0
1786258465.5	0
1786258466.5	0
1786258467.5	0
1786258468.5	0
1786258469.5	0
1786258470.5	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258471.6	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258472.6	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258473.6	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258474.6	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258475.6	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258476.6	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258477.6	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258478.6	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258479.7	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258480.7	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258481.7	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258482.7	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258483.7	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258484.8	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258485.9	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258486.9	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258487.9	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258489.0	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258490.0	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258491.0	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258492.0	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258493.1	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258494.1	2	/bin/bash -O extglob -c snap=$(command cat <&3); builtin shopt -s extglob 2>/dev || sleep 25
1786258495.2	1
1786258496.2	0
1786258497.2	0
1786258498.2	0
1786258499.3	0
1786258500.3	2	 ||
1786258501.3	0
1786258502.8	0
1786258503.9	0
1786258505.0	0
1786258506.0	0
1786258507.1	0
1786258508.1	0
1786258509.2	0
1786258510.3	0
1786258511.3	0
1786258512.4	0
1786258513.5	0
1786258514.5	0
1786258515.6	0
1786258516.7	0
```
