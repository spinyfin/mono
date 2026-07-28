#!/bin/zsh
set -u
export GROK_HOME="/tmp/grok-liveness-spike/home"
export PATH="/Users/brianduff/.grok/bin:$PATH"
export NO_COLOR=
echo $$ > "/Users/brianduff/.local/share/cube/workspaces/mono-agent-187/tools/boss/docs/investigations/grok-tui-liveness-markers-artifacts/ghosttykit_host/run_out/shell_pid.txt"
tty > "/Users/brianduff/.local/share/cube/workspaces/mono-agent-187/tools/boss/docs/investigations/grok-tui-liveness-markers-artifacts/ghosttykit_host/run_out/tty.txt"
print -r -- "GROK_BIN=/Users/brianduff/.grok/bin/grok" > "/Users/brianduff/.local/share/cube/workspaces/mono-agent-187/tools/boss/docs/investigations/grok-tui-liveness-markers-artifacts/ghosttykit_host/run_out/grok_bin.txt"
"/Users/brianduff/.grok/bin/grok" --version > "/Users/brianduff/.local/share/cube/workspaces/mono-agent-187/tools/boss/docs/investigations/grok-tui-liveness-markers-artifacts/ghosttykit_host/run_out/grok_version.txt" 2>&1
print -r -- "pane_mode=no_alt" > "/Users/brianduff/.local/share/cube/workspaces/mono-agent-187/tools/boss/docs/investigations/grok-tui-liveness-markers-artifacts/ghosttykit_host/run_out/scenario.txt"
print -r -- "mode_flag=--no-alt-screen" >> "/Users/brianduff/.local/share/cube/workspaces/mono-agent-187/tools/boss/docs/investigations/grok-tui-liveness-markers-artifacts/ghosttykit_host/run_out/scenario.txt"
print -r -- "sid=11eb0dd3-9ca9-45b1-8de7-5d7d086bcfb5" >> "/Users/brianduff/.local/share/cube/workspaces/mono-agent-187/tools/boss/docs/investigations/grok-tui-liveness-markers-artifacts/ghosttykit_host/run_out/scenario.txt"
print -r -- "grok-start $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "/Users/brianduff/.local/share/cube/workspaces/mono-agent-187/tools/boss/docs/investigations/grok-tui-liveness-markers-artifacts/ghosttykit_host/run_out/timeline.txt"
cd "/tmp/grok-liveness-spike/cwd"
"/Users/brianduff/.grok/bin/grok" --no-alt-screen --always-approve --trust --session-id "11eb0dd3-9ca9-45b1-8de7-5d7d086bcfb5" --cwd "/tmp/grok-liveness-spike/cwd"   "Use the shell tool to run exactly: sleep 14. Do not skip the sleep. After it finishes reply with exactly: LIVE_SEED_DONE."
print -r -- $? > "/Users/brianduff/.local/share/cube/workspaces/mono-agent-187/tools/boss/docs/investigations/grok-tui-liveness-markers-artifacts/ghosttykit_host/run_out/grok_exit.txt"
print -r -- "grok-exit $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "/Users/brianduff/.local/share/cube/workspaces/mono-agent-187/tools/boss/docs/investigations/grok-tui-liveness-markers-artifacts/ghosttykit_host/run_out/timeline.txt"
print -r -- "SCRIPT_DONE"
date -u +%Y-%m-%dT%H:%M:%SZ > "/Users/brianduff/.local/share/cube/workspaces/mono-agent-187/tools/boss/docs/investigations/grok-tui-liveness-markers-artifacts/ghosttykit_host/run_out/script_finished.txt"