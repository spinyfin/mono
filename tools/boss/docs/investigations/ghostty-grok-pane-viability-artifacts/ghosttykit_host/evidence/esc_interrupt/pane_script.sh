#!/bin/zsh
set -u
export GROK_HOME="/tmp/grok-pane-spike/home"
export PATH="/Users/brianduff/.grok/bin:$PATH"
export NO_COLOR=
echo $$ > "/private/tmp/grok-pane-spike/ghosttykit_host/run_out/shell_pid.txt"
tty > "/private/tmp/grok-pane-spike/ghosttykit_host/run_out/tty.txt"
print -r -- "GROK_BIN=/Users/brianduff/.grok/bin/grok" > "/private/tmp/grok-pane-spike/ghosttykit_host/run_out/grok_bin.txt"
"/Users/brianduff/.grok/bin/grok" --version > "/private/tmp/grok-pane-spike/ghosttykit_host/run_out/grok_version.txt" 2>&1
print -r -- "scenario=esc_interrupt" > "/private/tmp/grok-pane-spike/ghosttykit_host/run_out/scenario.txt"
print -r -- "sid=bf9b7291-f5ab-48db-9a71-3bffe7c25ea0" >> "/private/tmp/grok-pane-spike/ghosttykit_host/run_out/scenario.txt"
print -r -- "no_alt=true" >> "/private/tmp/grok-pane-spike/ghosttykit_host/run_out/scenario.txt"
print -r -- "grok-start $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "/private/tmp/grok-pane-spike/ghosttykit_host/run_out/timeline.txt"
cd "/tmp/grok-pane-spike/cwd"
# Interactive TUI with positional prompt (auto-submit shape, Claude-like).
"/Users/brianduff/.grok/bin/grok" --no-alt-screen --always-approve --trust --session-id "bf9b7291-f5ab-48db-9a71-3bffe7c25ea0" --cwd "/tmp/grok-pane-spike/cwd"   "Use the shell tool to run: sleep 45. Do not skip the sleep. After it finishes reply with exactly: SLEEP_DONE."
print -r -- $? > "/private/tmp/grok-pane-spike/ghosttykit_host/run_out/grok_exit.txt"
print -r -- "grok-exit $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "/private/tmp/grok-pane-spike/ghosttykit_host/run_out/timeline.txt"
print -r -- "SCRIPT_DONE"
date -u +%Y-%m-%dT%H:%M:%SZ > "/private/tmp/grok-pane-spike/ghosttykit_host/run_out/script_finished.txt"