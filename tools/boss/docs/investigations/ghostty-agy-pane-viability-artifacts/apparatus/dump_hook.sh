#!/bin/bash
# dump_hook.sh <event> <out-dir>
#
# Record one hook invocation: the stdin payload verbatim, plus the facts about
# the hook process that the scoped-HOME question needs.
#
# Reply, in order of precedence:
#   <out-dir>/reply.<event>.json   exists -> its contents
#   <out-dir>/reply.<event>.empty  exists -> nothing at all
#   event is PreToolUse                   -> {"decision":"allow"}
#   otherwise                             -> {}
# PreToolUse needs an explicit allow: agy denies the tool call when the hook
# replies without a decision.
# Exit status: contents of <out-dir>/exit.<event> when present, else 0.
event="$1"; out="$2"
mkdir -p "$out"
ts=$(python3 -c 'import time; print(int(time.time()*1000))')
seq=$(printf '%s-%05d' "$ts" "$$")
cat > "$out/$seq.$event.stdin.json"
{
  echo "event=$event"
  echo "wall_ms=$ts"
  echo "pid=$$ ppid=$PPID"
  echo "pwd=$(pwd)"
  echo "HOME=$HOME"
  env | grep -E '^(AGY_|ANTIGRAVITY_|PLUGIN_|TERM=|TMUX|PWD=)' | sort
} > "$out/$seq.$event.env.txt"
if [ -f "$out/reply.$event.json" ]; then
  cat "$out/reply.$event.json"
elif [ -f "$out/reply.$event.empty" ]; then
  :
elif [ "$event" = "PreToolUse" ]; then
  printf '{"decision":"allow"}\n'
else
  printf '{}\n'
fi
if [ -f "$out/exit.$event" ]; then exit "$(cat "$out/exit.$event")"; fi
exit 0
