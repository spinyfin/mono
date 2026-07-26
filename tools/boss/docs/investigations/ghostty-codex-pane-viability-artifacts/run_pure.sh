#!/bin/zsh
SCRATCH=/tmp/codex-pane-spike
echo $$ > "$SCRATCH/shell_pid.txt"
tty > "$SCRATCH/tty.txt"
python3 -c 'import os; print({i: os.ttyname(i) if os.isatty(i) else f"notty-fd{i}" for i in (0,1,2)})' > "$SCRATCH/fds_before.txt"
/Users/brianduff/.local/bin/codex exec --json --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \
  "run: sleep 20; reply with exactly: pure-done"
echo $? > "$SCRATCH/codex_exit.txt"
python3 -c 'import os; print({i: os.ttyname(i) if os.isatty(i) else f"notty-fd{i}" for i in (0,1,2)})' > "$SCRATCH/fds_after.txt"
printf 'prompt> ' > "$SCRATCH/prompt_shown.txt"
if read -r LINE; then
  print -r -- "$LINE" > "$SCRATCH/shell_got_line.txt"
  eval "$LINE"
  echo $? > "$SCRATCH/eval_exit.txt"
else
  echo "(read failed/eof)" > "$SCRATCH/shell_got_line.txt"
fi
date > "$SCRATCH/finished.txt"
sleep 3
