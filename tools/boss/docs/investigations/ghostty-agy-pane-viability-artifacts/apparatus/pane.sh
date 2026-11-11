#!/bin/bash
# pane.sh <verb> ... : the tmux control surface, with the argv Boss's
# `boss_tmux::Tmux` sends (tools/boss/tmux/src/lib.rs, session_options.rs).
# One deliberate difference is left to the caller: `type` ends with `C-m` as
# Boss does, which agy does not treat as Return; scenarios submit with
# `literal` + `key <session> Enter`.
here="$(cd "$(dirname "$0")" && pwd)"
tm="$here/tm"
verb="$1"; shift
case "$verb" in
  server-start)
    # start_private_server + session_options::BOSS_SERVER_OPTIONS
    "$tm" start-server \; set-option -s exit-empty off \; set-option -g history-limit 2000 \; set-option -g remain-on-exit on
    "$tm" set-option -s 'terminal-features[100]' 'xterm*:extkeys' \; set-option -s extended-keys on \; set-option -s focus-events on
    ;;
  new)
    # new <session> <cwd> <command>   (Tmux::new_session + BOSS_SESSION_OPTIONS)
    session="$1"; cwd="$2"; command="$3"
    "$tm" new-session -d -s "$session" -c "$cwd" "$command"
    "$tm" set-option -t "$session" status off \; set-option -t "$session" remain-on-exit on
    ;;
  type)
    # Tmux::send_keys: literal text (or a bracketed paste when multi-line), then one C-m.
    session="$1"; text="$2"
    if [[ "$text" == *$'\n'* ]]; then
      printf '%s' "$text" | "$tm" load-buffer -b "boss-deliver-$session-1" -
      "$tm" paste-buffer -b "boss-deliver-$session-1" -p -d -t "$session"
    else
      "$tm" send-keys -t "$session" -l -- "$text"
      sleep 0.03
    fi
    "$tm" send-keys -t "$session" C-m
    ;;
  literal) "$tm" send-keys -t "$1" -l -- "$2" ;;      # text without the Return
  key) "$tm" send-keys -t "$1" "$2" ;;                # Tmux::send_key (Escape, C-m, ...)
  cap) "$tm" capture-pane -p -t "$1" ;;               # Tmux::capture_pane
  caph) "$tm" capture-pane -p -S -2000 -t "$1" ;;     # Tmux::capture_pane_with_history
  info) "$tm" display-message -p -t "$1" 'dead=#{pane_dead} status=#{pane_dead_status} pid=#{pane_pid} size=#{pane_width}x#{pane_height} cmd=#{pane_current_command} alt=#{alternate_on} cursor=#{cursor_x},#{cursor_y}' ;;
  kill) "$tm" kill-session -t "$1" ;;
  kill-server) "$tm" kill-server ;;
  ls) "$tm" list-sessions ;;
  waitfor)
    # waitfor <session> <extended-regex> <timeout-seconds> : exit 0 when the pane shows it
    session="$1"; pattern="$2"; timeout="${3:-30}"
    start=$(python3 -c 'import time; print(time.time())')
    while :; do
      if "$tm" capture-pane -p -t "$session" 2>/dev/null | grep -E -q -- "$pattern"; then
        python3 -c "import time; print('matched after %.2fs' % (time.time()-$start))"; exit 0
      fi
      if python3 -c "import time,sys; sys.exit(0 if time.time()-$start > $timeout else 1)"; then
        echo "TIMEOUT after ${timeout}s waiting for: $pattern"; exit 1
      fi
      sleep 0.2
    done
    ;;
  *) echo "unknown verb $verb" >&2; exit 2 ;;
esac
