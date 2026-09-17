#!/bin/zsh
# Concurrent Codex TUI startup reproduction.
#
# usage: repro-concurrent-codex-startup.sh <label> <N> [seed_home_dir] [max_wait_secs]
#
# Mirrors the Boss worker launch shape: one detached tmux session per worker
# on a private tmux server, each running `$SHELL -l -i -c <runner>` (the
# interactive login shell a Boss pane uses), a fresh per-run CODEX_HOME with
# a byte-copied ~/.codex/auth.json and a Boss-shaped config.toml, and a
# sessions dir local to the home. The runner execs
# `codex --strict-config --no-alt-screen -a never` with a one-word prompt so
# the startup thread (and therefore the rollout file) is created at once, as
# Boss's prompt-on-the-command-line does. One tiny model turn runs per
# session; that is the only cost.
#
# Per worker it records: tmux launch time, the shell reaching `exec`,
# Codex's first log row (from the home's logs_2.sqlite), the
# `startup-thread-start` row, the rollout file appearing, and Codex's own
# `bootstrap_ms`. Stragglers are sampled with `sample` at 20 s and 60 s.
#
# Optional: `seed_home_dir` copies cache/, plugins/, models_cache.json and
# version.json from a warm home so the startup downloads are skipped;
# `EXTRA_FEATURES` (newline-separated `key = value` lines) is appended to
# the [features] table, e.g. EXTRA_FEATURES=$'plugins = false\nremote_plugin = false'.
#
# Output root defaults to a fresh mktemp dir; set REPRO_ROOT to keep runs.
set -u
label=$1; N=$2; seed=${3:-}; maxwait=${4:-300}
ROOT=${REPRO_ROOT:-$(mktemp -d "${TMPDIR:-/tmp}/codex-repro.XXXXXX")}/$label
rm -rf "$ROOT"; mkdir -p "$ROOT/ws"
SOCK=codexrepro-$label

for i in $(seq 1 $N); do
  H=$ROOT/home$i
  mkdir -p "$H/sessions"
  cp ~/.codex/auth.json "$H/auth.json"; chmod 600 "$H/auth.json"
  if [ -n "$seed" ]; then
    cp -R "$seed/cache" "$H/cache" 2>/dev/null
    cp -R "$seed/plugins" "$H/plugins" 2>/dev/null
    cp "$seed/models_cache.json" "$H/" 2>/dev/null
    cp "$seed/version.json" "$H/" 2>/dev/null
  fi
  cat > "$H/config.toml" <<EOF
allow_login_shell = false

[notice.external_config_migration_prompts]
home = true

[features]
external_agent_memory_import = false
shell_snapshot = false
${EXTRA_FEATURES:-}

[shell_environment_policy]
inherit = "all"
experimental_use_profile = false

[projects."$ROOT/ws"]
trust_level = "trusted"
EOF
  cat > "$ROOT/runner_$i.sh" <<EOF
date +%s.%N > "$H/t_shell"
export CODEX_HOME="$H"
echo \$\$ > "$H/pid"
exec codex --strict-config --no-alt-screen -a never --sandbox read-only "Reply with exactly the word OK and nothing else."
EOF
done

# Launch all N as fast as possible.
for i in $(seq 1 $N); do
  H=$ROOT/home$i
  date +%s.%N > "$H/t_launch"
  tmux -L $SOCK -f /dev/null new-session -d -s w$i -x 180 -y 45 -c "$ROOT/ws" \
    "$SHELL -l -i -c 'zsh $ROOT/runner_$i.sh'"
done
t0=$(date +%s)
echo "launched $N at $(date -u +%H:%M:%S) into $ROOT"

# Monitor for rollout files; sample stragglers at 20s and 60s.
typeset -A done_map
sampled20=(); sampled60=()
while :; do
  now=$(date +%s); el=$((now - t0))
  alldone=1
  for i in $(seq 1 $N); do
    H=$ROOT/home$i
    if [ -z "${done_map[$i]:-}" ]; then
      f=$(find "$H/sessions" -name 'rollout-*.jsonl' 2>/dev/null | head -1)
      if [ -n "$f" ]; then
        date +%s.%N > "$H/t_rollout"; done_map[$i]=1
        echo "  w$i rollout at +${el}s"
      else
        alldone=0
        pid=$(cat "$H/pid" 2>/dev/null)
        if [ -n "$pid" ] && [ $el -ge 20 ] && [[ ${sampled20[(Ie)$i]} -eq 0 ]]; then
          sampled20+=($i); sample $pid 1 -file "$ROOT/sample_${i}_20s.txt" >/dev/null 2>&1 &
          lsof -p $pid -a -i 2>/dev/null > "$ROOT/lsof_${i}_20s.txt" &
        fi
        if [ -n "$pid" ] && [ $el -ge 60 ] && [[ ${sampled60[(Ie)$i]} -eq 0 ]]; then
          sampled60+=($i); sample $pid 1 -file "$ROOT/sample_${i}_60s.txt" >/dev/null 2>&1 &
          lsof -p $pid -a -i 2>/dev/null > "$ROOT/lsof_${i}_60s.txt" &
        fi
      fi
    fi
  done
  [ $alldone -eq 1 ] && break
  [ $el -ge $maxwait ] && { echo "timeout at ${el}s"; break; }
  sleep 0.5
done
wait
# Codex batches log-DB writes; give it a moment before the sessions die.
echo "all done or timed out; letting codex flush logs for 15s"; sleep 15
tmux -L $SOCK kill-server 2>/dev/null

echo
echo "worker | launch->shell | shell->first_log | first_log->thread_start | launch->rollout | bootstrap_ms"
for i in $(seq 1 $N); do
  H=$ROOT/home$i
  tl=$(cat "$H/t_launch"); ts=$(cat "$H/t_shell" 2>/dev/null || echo 0); tr=$(cat "$H/t_rollout" 2>/dev/null || echo 0)
  fl=$(sqlite3 "$H/logs_2.sqlite" "select min(ts+ts_nanos/1e9) from logs" 2>/dev/null || echo 0)
  th=$(sqlite3 "$H/logs_2.sqlite" "select min(ts+ts_nanos/1e9) from logs where feedback_log_body like '%startup-thread-start%'" 2>/dev/null || echo 0)
  bm=$(sqlite3 "$H/logs_2.sqlite" "select feedback_log_body from logs where feedback_log_body like 'tui startup initial frame%' limit 1" 2>/dev/null | sed -E 's/.*bootstrap_ms=([0-9]+).*/\1/')
  python3 - "$i" "$tl" "$ts" "$fl" "$th" "$tr" "$bm" <<'PY'
import sys
i,tl,ts,fl,th,tr,bm=sys.argv[1:]
g=lambda x: float(x) if x not in ("","None") else 0.0
f=lambda a,b: ("%.1f"%(g(b)-g(a))) if g(a)>0 and g(b)>0 else "n/a"
print(f"w{i} | {f(tl,ts)} | {f(ts,fl)} | {f(fl,th)} | {f(tl,tr)} | {bm or 'n/a'}")
PY
done
