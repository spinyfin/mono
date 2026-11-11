# common.sh: sourced by scenarios.sh.
#
# Layout under $SPIKE_ROOT (default: the parent of this directory):
#   bin-<version>/agy              a private copy of the agy binary under test
#   viewer/ghosttykit_viewer       the compiled GhosttyKit viewer host
#   briefs/                        test briefs (tools.py briefs)
#   runs/<scenario>-<version>/     one directory per scenario run
#   tmx/                           the private tmux socket
APPARATUS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="${SPIKE_ROOT:-$(cd "$APPARATUS/.." && pwd)}"
RUNS_DIR="${RUNS_DIR:-$ROOT/runs}"
P="$APPARATUS/pane.sh"
TM="$APPARATUS/tm"
TOOLS="$APPARATUS/tools.py"
REAL_HOME="${REAL_HOME:-$HOME}"
# Boss's initial-brief launcher, used unmodified.
BOSS_LAUNCHER="${BOSS_LAUNCHER:?set BOSS_LAUNCHER to tools/boss/engine/driver/src/initial_prompt.py}"
MODEL="${MODEL:-gemini-3.8-flash-low}"

# setup_run <name> <version>: a fresh scoped HOME, workspace and control dir.
#
# The scoped HOME is empty except for the host login keychain, bridged by
# symlink exactly as Boss's Grok driver bridges it for `gh`
# (grok/environment.rs, LOGIN_KEYCHAIN_LEAF). agy keeps its sign-in in the
# keychain, so without the bridge a scoped HOME is signed out.
setup_run() {
  NAME="$1"; VER="$2"
  RUN="$RUNS_DIR/$NAME-$VER"; rm -rf "$RUN"
  H="$RUN/home"; WS="$RUN/ws/repo"; CTL="$RUN/ws/control"; HOOKS="$RUN/hooks"
  SESSION="$(echo "$NAME-$VER" | tr '.' '_')"
  mkdir -p "$WS" "$CTL" "$HOOKS" "$H/Library/Keychains"
  ln -sfn "$REAL_HOME/Library/Keychains/login.keychain-db" "$H/Library/Keychains/login.keychain-db"
  echo "spike workspace" > "$WS/README.txt"
  WS_REAL="$(cd "$WS" && pwd -P)"; CTL_REAL="$(cd "$CTL" && pwd -P)"
  AGY="$ROOT/bin-$VER/agy"
  VIEWER_SEQ=0
}

# seed_onboarding: mark first-run onboarding complete, as a provisioner would.
seed_onboarding() {
  mkdir -p "$H/.gemini/antigravity-cli/cache"
  printf '{\n  "consumerOnboardingComplete": true,\n  "enterpriseOnboardingComplete": false,\n  "onboardingComplete": true\n}\n' \
    > "$H/.gemini/antigravity-cli/cache/onboarding.json"
}

# seed_settings <json>: write antigravity-cli/settings.json.
seed_settings() {
  mkdir -p "$H/.gemini/antigravity-cli"
  printf '%s\n' "$1" > "$H/.gemini/antigravity-cli/settings.json"
}

# trusted_json <path>...: {"trustedWorkspaces": [...]}
trusted_json() {
  python3 -c 'import json,sys; print(json.dumps({"trustedWorkspaces": sys.argv[1:]}, indent=2))' "$@"
}

# install_hooks <cwd|control|global>: wire every event to the dump hook.
install_hooks() {
  local dest
  case "$1" in
    cwd) dest="$WS/.agents" ;;
    control) dest="$CTL/.agents" ;;
    global) dest="$H/.gemini/config" ;;
    *) echo "bad hooks location $1" >&2; return 1 ;;
  esac
  mkdir -p "$dest"
  python3 "$TOOLS" hooksjson "$APPARATUS/dump_hook.sh" "$HOOKS" > "$dest/hooks.json"
  echo "hooks.json at: ${dest#$RUN/}" >> "$RUN/setup.txt"
}

# pane_env: the environment the pane shell exports before the driver runs,
# the way a SpawnPlan EnvDirective::Set would. `=true`, not `=1`: only the
# former stops agy replacing its own binary.
pane_env() {
  echo "export HOME=$(printf '%q' "$H"); export AGY_CLI_DISABLE_AUTO_UPDATE=true; ${EXTRA_PANE_ENV:-}"
}

quote_args() { local out=""; for arg in "$@"; do out="$out $(printf '%q' "$arg")"; done; echo "$out"; }

# launch_direct <agy flags...>: the tmux session runs agy itself.
launch_direct() {
  local command; command="$(pane_env)exec $(printf '%q' "$AGY")$(quote_args --log-file "$RUN/cli.log" --model "$MODEL" "$@")"
  printf '%s\n' "$command" > "$RUN/session_command.txt"
  mark "new-session (direct)"
  "$P" new "$SESSION" "$WS" "$command"
  start_poll 0.2
}

# launch_with_launcher <brief-file> <agy flags...>: the tmux session runs Boss's
# unmodified initial-brief launcher, which runs agy and pastes the brief.
launch_with_launcher() {
  local brief="$1"; shift
  LAUNCH="$RUN/launch"; mkdir -p "$LAUNCH"
  cp "$BOSS_LAUNCHER" "$LAUNCH/initial-prompt.py"
  cp "$brief" "$LAUNCH/initial-prompt.txt"; chmod 600 "$LAUNCH/initial-prompt.txt"
  python3 "$TOOLS" launcher-json "$VER" > "$LAUNCH/initial-prompt.json"
  local command; command="$(pane_env)exec python3 $(printf '%q' "$LAUNCH/initial-prompt.py") $(printf '%q' "$AGY")$(quote_args --log-file "$RUN/cli.log" --model "$MODEL" "$@")"
  printf '%s\n' "$command" > "$RUN/session_command.txt"
  mark "new-session (launcher)"
  "$P" new "$SESSION" "$WS" "$command"
  start_poll 0.2
}

# start_viewer: a GhosttyKit surface whose shell attaches to the session, with
# the initial input Boss's WorkerViewerLaunch builds (`exec tmux -S <socket>
# attach-session -t <session>`).
start_viewer() {
  mkdir -p "$RUN/viewer/cmd"
  ( VIEWER_OUT="$RUN/viewer" VIEWER_CWD="$REAL_HOME" VIEWER_DEADLINE_S="${VIEWER_DEADLINE_S:-900}" \
    VIEWER_INITIAL_INPUT="exec $TM attach-session -t $SESSION"$'\n' \
    nohup "$ROOT/viewer/ghosttykit_viewer" > "$RUN/viewer/console.log" 2>&1 & )
  for _ in $(seq 1 40); do [ -f "$RUN/viewer/READY.txt" ] && break; sleep 0.1; done
  mark "viewer attached"
}

# start_direct_viewer <shell command>: no tmux; the command runs in the
# surface's own pty (the topology of the Grok spike).
start_direct_viewer() {
  mkdir -p "$RUN/viewer/cmd"
  printf '%s\n' "$1" > "$RUN/session_command.txt"
  mark "surface created (direct pty)"
  ( env -i HOME="$REAL_HOME" PATH=/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin USER="$(id -un)" LOGNAME="$(id -un)" \
      SHELL=/bin/zsh LANG=en_US.UTF-8 TMPDIR="$TMPDIR" \
      VIEWER_OUT="$RUN/viewer" VIEWER_CWD="$REAL_HOME" VIEWER_DEADLINE_S=300 VIEWER_INITIAL_INPUT="$1"$'\n' \
      nohup "$ROOT/viewer/ghosttykit_viewer" > "$RUN/viewer/console.log" 2>&1 & )
}

viewer_cmd() { VIEWER_SEQ=$((VIEWER_SEQ+1)); printf '%s\n' "$1" > "$RUN/viewer/cmd/$(printf '%03d' $VIEWER_SEQ).cmd"; }
stop_viewer() { viewer_cmd quit; for _ in $(seq 1 30); do [ -f "$RUN/viewer/FINISHED.txt" ] && break; sleep 0.1; done; }

start_poll() { ( nohup python3 "$TOOLS" poll "$TM" "$SESSION" "$RUN/pane_polls.jsonl" "${1:-0.25}" > /dev/null 2>&1 & ); }
stop_poll() { touch "$RUN/pane_polls.jsonl.stop"; sleep 0.4; }

mark() { printf '%s %s\n' "$(python3 -c 'import time; print(int(time.time()*1000))')" "$*" >> "$RUN/timeline.txt"; echo "[$(date +%H:%M:%S)] $*"; }

# hooks_seen: the recorded hook invocations, in order.
hooks_seen() { ls "$HOOKS" 2>/dev/null | grep '\.stdin\.json$' | sed -E 's/^[0-9]+-[0-9]+\.//; s/\.stdin\.json$//' | tr '\n' ' '; echo; }
hook_count() { ls "$HOOKS" 2>/dev/null | grep -c "\.$1\.stdin\.json$"; }

# wait_delivered <timeout-s>: the launcher's durable submit marker.
wait_delivered() {
  local n=$(( ${1:-60} * 5 ))
  for _ in $(seq 1 $n); do
    [ -f "$LAUNCH/initial-prompt.delivered" ] && { mark "launcher: delivered"; return 0; }
    [ -f "$LAUNCH/initial-prompt.undelivered" ] && { mark "launcher: UNDELIVERED"; return 1; }
    sleep 0.2
  done
  mark "launcher: no marker within ${1:-60}s"; return 1
}

# wait_hook <event> <count> <timeout-s>
wait_hook() {
  local n=$(( ${3:-60} * 5 ))
  for _ in $(seq 1 $n); do [ "$(hook_count "$1")" -ge "$2" ] && { mark "hook $1 count>=$2"; return 0; }; sleep 0.2; done
  mark "TIMEOUT waiting for hook $1 count>=$2 (have $(hook_count "$1"))"; return 1
}

# wait_pane <regex> <timeout-s>
wait_pane() {
  if "$P" waitfor "$SESSION" "$1" "${2:-30}" > /dev/null; then mark "pane shows /$1/"; return 0; fi
  mark "TIMEOUT waiting for pane /$1/"; return 1
}

# submit <text>: what Tmux::send_keys does for one line, except that the final
# key is Enter. `C-m` is not Return to agy under Boss's tmux options.
submit() { mark "submit: $1"; "$P" literal "$SESSION" "$1"; sleep 0.05; "$P" key "$SESSION" Enter; }

snap() { "$P" cap "$SESSION" > "$RUN/pane_$1.txt"; [ -d "$RUN/viewer" ] && viewer_cmd "snapshot:$1"; mark "snapshot $1"; }

log_key_lines() {
  grep -n 'hooks_manager.go\|workspaceDirs=\|HandleUserInput called\|Terminal detection\|Cancelling' "$RUN/cli.log" 2>/dev/null \
    | cut -c1-400 > "$RUN/cli_log_key_lines.txt"
}

# finish: final captures, then stop the TUI with /quit and record how it ended.
finish() {
  snap final
  "$P" caph "$SESSION" > "$RUN/pane_history_final.txt"
  echo "hooks: $(hooks_seen)" > "$RUN/hooks_seen.txt"
  if [ "$("$TM" display-message -p -t "$SESSION" '#{pane_dead}')" = "0" ]; then
    "$P" literal "$SESSION" "/quit"; sleep 0.5; "$P" key "$SESSION" Enter; sleep 2.5
  fi
  "$P" info "$SESSION" > "$RUN/info_end.txt"
  "$P" cap "$SESSION" > "$RUN/pane_after_quit.txt"
  stop_poll
  [ -d "$RUN/viewer" ] && stop_viewer
  "$P" kill "$SESSION" 2>/dev/null
  log_key_lines
  mark "finished: $(cat "$RUN/info_end.txt")"
  cat "$RUN/hooks_seen.txt"
}
