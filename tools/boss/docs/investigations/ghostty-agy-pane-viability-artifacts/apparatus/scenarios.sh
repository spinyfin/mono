#!/bin/bash
# scenarios.sh <scenario> <version> [args...]
#
# Every scenario runs agy in a Boss-shaped tmux session on the spike's private
# tmux server, with a GhosttyKit viewer attached the way the Boss app attaches
# one, and writes its evidence under runs/<scenario>-<version>/.
#
#   modes <ver>                          raw startup stream of a provisioned pane
#   launcher <ver> <small|big>           brief delivery by Boss's launcher, then a typed probe
#   launcher-untrusted <ver>             the launcher against an untrusted workspace
#   iflag <ver> <tmux|direct>            does `agy -i "<prompt>"` submit by itself?
#   discovery <ver> <cell> <hooks> <trust> <add-dir>
#                                        one cell of hooks.json location x trust x --add-dir
#   trust-tool <ver>                     does an unanswered trust dialog stop a tool call?
#   esc <ver> <tool|gen>                 interrupt a turn with `tmux send-keys Escape`
#   midturn <ver>                        type a prompt while a turn is in flight
#   bg <ver>                             a command that becomes a background task
#   home <ver>                           what a tool child sees under the scoped HOME; resize
#   hookterm <ver>                       a PreInvocation hook that terminates the turn
#   signin <ver>                         the pane when the scoped HOME has no credential
#   hideaccount <ver>                    AGY_CLI_HIDE_ACCOUNT_INFO
#   submitkey <ver>                      is `tmux send-keys C-m` Return to agy?
#   hookreply <ver> <mode>               what a PreToolUse hook's reply does to the tool call
source "$(dirname "$0")/common.sh"
scenario="$1"; ver="$2"; shift 2

sc_modes() {
  setup_run modes "$ver"
  seed_onboarding; seed_settings "$(trusted_json "$WS_REAL")"
  # The sleep leaves time to attach pipe-pane before agy's first byte.
  "$P" new "$SESSION" "$WS" "sleep 1.5; $(pane_env)exec $(printf '%q' "$AGY")$(quote_args --log-file "$RUN/cli.log" --model "$MODEL")"
  "$TM" pipe-pane -t "$SESSION" -o "cat > '$RUN/raw_output.bin'"
  start_poll 0.2
  sleep 8
  "$P" info "$SESSION" | tee "$RUN/info_idle.txt"
  "$P" cap "$SESSION" > "$RUN/idle_80x24.txt"
  "$P" literal "$SESSION" "/quit"; sleep 0.6; "$P" key "$SESSION" Enter; sleep 2.5
  "$P" info "$SESSION" | tee "$RUN/info_after_quit.txt"
  "$P" cap "$SESSION" > "$RUN/after_quit.txt"
  stop_poll
  "$P" kill "$SESSION"
  python3 "$TOOLS" seqs "$RUN/raw_output.bin" | tee "$RUN/sequences.txt"
}

sc_launcher() {
  local brief="$1"
  setup_run "launcher-$brief" "$ver"
  seed_onboarding; seed_settings "$(trusted_json "$WS_REAL")"; install_hooks cwd
  launch_with_launcher "$ROOT/briefs/$brief.txt"
  start_viewer
  wait_delivered 240 || { "$P" cap "$SESSION" | tail -8; finish; exit 1; }
  wait_hook Stop 1 180
  sleep 1; snap after_brief_turn
  submit "Reply with exactly the single token PROBE_AFTER_BRIEF_OK. Do not use tools."
  wait_hook Stop 2 90
  sleep 1; snap after_probe
  cp "$LAUNCH/initial-prompt.delivery.jsonl" "$RUN/delivery.jsonl"
  finish
  cat "$RUN/delivery.jsonl"
}

# Boss's launcher against an untrusted workspace: it must hold the brief while
# the trust dialog is on screen, and report the brief as not delivered.
sc_launcher_untrusted() {
  setup_run "launcher-untrusted" "$ver"
  seed_onboarding; seed_settings '{}'; install_hooks global
  launch_with_launcher "$ROOT/briefs/small.txt"
  start_viewer
  wait_pane "Do you trust" 20; snap dialog
  wait_delivered 45; local rc=$?
  sleep 1
  "$P" info "$SESSION" | tee "$RUN/info_after_launcher.txt"
  "$P" cap "$SESSION" > "$RUN/pane_after_launcher.txt"
  tee "$RUN/delivery.jsonl" < "$LAUNCH/initial-prompt.delivery.jsonl"
  ls "$LAUNCH" | tr '\n' ' ' | tee "$RUN/launch_dir.txt"; echo
  echo "delivered_rc=$rc hooks=$(hooks_seen)" | tee "$RUN/result.txt"
  stop_poll; stop_viewer; "$P" kill "$SESSION" 2>/dev/null
}

#   tmux   : agy is the tmux session command; a GhosttyKit viewer is attached.
#   direct : agy runs in the GhosttyKit surface's own pty, no tmux in between.
sc_iflag() {
  local mode="$1"
  setup_run "iflag-$mode" "$ver"
  seed_onboarding; seed_settings "$(trusted_json "$WS_REAL")"; install_hooks cwd
  local prompt="Reply with exactly the single token IFLAG_OK_$(echo "$mode" | tr a-z A-Z). Do not use tools."
  if [ "$mode" = "tmux" ]; then
    launch_direct -i "$prompt"
    start_viewer
    wait_hook Stop 1 90
    sleep 1; snap after_i_turn
    submit "Reply with exactly the single token IFLAG_FOLLOWUP_OK. Do not use tools."
    wait_hook Stop 2 90
    sleep 1
    finish
  else
    start_direct_viewer "cd $(printf '%q' "$WS") && $(pane_env)exec $(printf '%q' "$AGY")$(quote_args --log-file "$RUN/cli.log" --model "$MODEL" -i "$prompt")"
    wait_hook Stop 1 90
    sleep 1; viewer_cmd "snapshot:after_i_turn"
    # follow-up typed through the surface: ghostty_surface_text + Return key (Boss submitText)
    mark "submit via surface: IFLAG_FOLLOWUP_OK"
    viewer_cmd "submit:Reply with exactly the single token IFLAG_FOLLOWUP_OK. Do not use tools."
    wait_hook Stop 2 90
    sleep 1; viewer_cmd "snapshot:after_followup"
    viewer_cmd "submit:/quit"; sleep 3
    viewer_cmd "snapshot:after_quit"; sleep 0.6
    stop_viewer
    echo "hooks: $(hooks_seen)" | tee "$RUN/hooks_seen.txt"
    log_key_lines
  fi
}

# discovery <cell> <hooks: cwd|control|global> <trust: none|cwd|control|both> <add-dir: yes|no>
sc_discovery() {
  local cell="$1" where="$2" trust="$3" adddir="$4"
  setup_run "disc-$cell" "$ver"
  seed_onboarding
  case "$trust" in
    none) seed_settings '{}' ;;
    cwd) seed_settings "$(trusted_json "$WS_REAL")" ;;
    control) seed_settings "$(trusted_json "$CTL_REAL")" ;;
    both) seed_settings "$(trusted_json "$WS_REAL" "$CTL_REAL")" ;;
  esac
  install_hooks "$where"
  echo "cell=$cell hooks=$where trust=$trust add-dir=$adddir version=$ver" >> "$RUN/setup.txt"
  local token="DISC_$(echo "$cell" | tr a-z A-Z)_OK"
  local flags=(-i "Reply with exactly the single token $token. Do not use tools.")
  [ "$adddir" = "yes" ] && flags=(--add-dir "$CTL" "${flags[@]}")
  launch_direct "${flags[@]}"
  start_viewer
  local dialog=no ran=no text
  for _ in $(seq 1 150); do
    [ "$(hook_count Stop)" -ge 1 ] && { ran=yes; break; }
    text="$("$P" cap "$SESSION")"
    if echo "$text" | grep -q "Do you trust"; then dialog=yes; break; fi
    if [ "$(echo "$text" | grep -c "$token")" -ge 2 ]; then ran=yes; break; fi
    sleep 0.2
  done
  snap at_start
  if [ "$dialog" = "yes" ]; then
    mark "trust dialog shown"
    grep 'hooks_manager.go' "$RUN/cli.log" | cut -c1-200 > "$RUN/hooks_manager_before_trust.txt"
    sleep 3
    "$P" key "$SESSION" Enter; mark "accepted trust with Enter"
    for _ in $(seq 1 200); do
      [ "$(hook_count Stop)" -ge 1 ] && { ran=yes; break; }
      [ "$("$P" cap "$SESSION" | grep -c "$token")" -ge 2 ] && { ran=yes; break; }
      sleep 0.2
    done
    snap after_trust
  fi
  sleep 1.5
  echo "dialog=$dialog prompt_ran=$ran stop_hooks=$(hook_count Stop)" | tee "$RUN/result.txt"
  finish > /dev/null
  {
    cat "$RUN/setup.txt"; cat "$RUN/result.txt"; cat "$RUN/hooks_seen.txt"
    grep 'hooks_manager.go' "$RUN/cli.log" | sed -E 's/^.*hooks_manager.go:[0-9]+\] //' | sort | uniq -c
    grep -o 'workspaceDirs=\[[^]]*\]' "$RUN/cli.log" | sed "s|$RUN|\$RUN|g" | sort -u
    echo "settings.json after: $(sed "s|$RUN|\$RUN|g" "$H/.gemini/antigravity-cli/settings.json" | tr -d '\n ')"
  } > "$RUN/CELL_SUMMARY.txt"
  cat "$RUN/CELL_SUMMARY.txt"; echo
}

# Untrusted workspace, trust dialog never answered. Does an `-i` prompt that
# asks for a shell command still execute it? Global hooks record what fires.
sc_trust_tool() {
  setup_run "trust-tool" "$ver"
  seed_onboarding; seed_settings '{}'; install_hooks global
  local canary="$WS/CANARY_WRITTEN_BEHIND_TRUST_DIALOG.txt"
  launch_direct --dangerously-skip-permissions -i "Use your shell command tool to run exactly: echo behind-dialog > $canary ; then reply with exactly TRUST_TOOL_DONE."
  start_viewer
  wait_pane "Do you trust" 20
  snap dialog
  for _ in $(seq 1 100); do [ "$(hook_count Stop)" -ge 1 ] && break; sleep 0.2; done
  sleep 1
  snap after_wait
  if [ -f "$canary" ]; then mark "CANARY EXISTS: tool ran behind the trust dialog"; else mark "canary absent"; fi
  echo "dialog_still_up=$("$P" cap "$SESSION" | grep -c 'Do you trust') canary=$([ -f "$canary" ] && echo present || echo absent) hooks=$(hooks_seen)" | tee "$RUN/result.txt"
  # leave without trusting: select "No, exit"
  "$P" key "$SESSION" Down; sleep 0.4; "$P" key "$SESSION" Enter; sleep 2
  "$P" info "$SESSION" | tee "$RUN/info_after_no_exit.txt"
  echo "settings.json after: $(tr -d '\n ' < "$H/.gemini/antigravity-cli/settings.json")" | sed "s|$RUN|\$RUN|g" | tee -a "$RUN/result.txt"
  finish > /dev/null
}

# Interrupt an in-flight turn with `tmux send-keys Escape` (Boss's
# InterruptGesture) and record whether a Stop hook fires, with what
# terminationReason, and whether the pane takes the next prompt.
sc_esc() {
  local kind="$1" brief
  setup_run "esc-$kind" "$ver"
  seed_onboarding; seed_settings "$(trusted_json "$WS_REAL")"; install_hooks global
  if [ "$kind" = "tool" ]; then brief="$ROOT/briefs/sleep60.txt"; else brief="$ROOT/briefs/count.txt"; fi
  launch_with_launcher "$brief" --dangerously-skip-permissions
  start_viewer
  wait_delivered 60 || { finish; exit 1; }
  if [ "$kind" = "tool" ]; then wait_hook PreToolUse 1 60; sleep 3; else wait_hook PreInvocation 1 30; sleep 2.5; fi
  snap busy_before_esc
  pgrep -fl 'sleep 60' > "$RUN/sleep_child_before_esc.txt"
  local stops_before presses=0 ended=no stops_after_esc
  stops_before=$(hook_count Stop)
  while [ $presses -lt 3 ]; do
    presses=$((presses+1))
    "$P" key "$SESSION" Escape; mark "Escape press #$presses sent (tmux send-keys Escape)"
    for _ in $(seq 1 50); do
      if [ "$(hook_count Stop)" -gt "$stops_before" ]; then ended=stop_hook; break; fi
      if "$P" cap "$SESSION" | grep -q '? for shortcuts'; then ended=pane_idle; break; fi
      sleep 0.2
    done
    [ "$ended" != "no" ] && break
  done
  mark "after Escape: ended=$ended presses=$presses stop_hooks=$(hook_count Stop)"
  sleep 2
  snap after_esc
  pgrep -fl 'sleep 60' > "$RUN/sleep_child_after_esc.txt"
  "$P" info "$SESSION" > "$RUN/info_after_esc.txt"
  stops_after_esc=$(hook_count Stop)
  submit "Reply with exactly the single token AFTER_ESC_OK. Do not use tools."
  wait_hook Stop $((stops_after_esc+1)) 60
  sleep 1; snap after_probe
  echo "ended=$ended presses=$presses stop_hooks_from_cancelled_turn=$((stops_after_esc-stops_before)) sleep_child_before=$(wc -l < "$RUN/sleep_child_before_esc.txt" | tr -d ' ') sleep_child_after=$(wc -l < "$RUN/sleep_child_after_esc.txt" | tr -d ' ')" | tee "$RUN/result.txt"
  finish
  python3 "$TOOLS" stops "$HOOKS" | tee "$RUN/stop_payloads.txt"
}

# Type a prompt into the pane while a turn is in flight (Tmux::send_keys shape,
# Enter as the submit key). Is it held as the next prompt, injected into the
# running turn, or lost?
sc_midturn() {
  setup_run "midturn" "$ver"
  seed_onboarding; seed_settings "$(trusted_json "$WS_REAL")"; install_hooks global
  launch_with_launcher "$ROOT/briefs/sleep25.txt" --dangerously-skip-permissions
  start_viewer
  wait_delivered 60 || { finish; exit 1; }
  wait_hook PreToolUse 1 60
  sleep 3
  snap busy_before_type
  "$P" literal "$SESSION" "Reply with exactly the single token MIDTURN_PROBE_OK. Do not use tools."; mark "mid-turn: literal text sent"
  sleep 1; snap after_literal
  "$P" key "$SESSION" Enter; mark "mid-turn: Enter sent"
  sleep 1.5; snap after_enter
  wait_hook Stop 1 120
  sleep 0.5; snap after_first_stop
  if wait_hook Stop 2 60; then mark "second Stop arrived without further input"; else mark "no second Stop within 60s"; fi
  sleep 1; snap settled
  finish
  python3 "$TOOLS" stops "$HOOKS" | tee "$RUN/stop_payloads.txt"
  grep 'HandleUserInput called' "$RUN/cli.log" | cut -c1-200 | tee "$RUN/handle_user_input.txt"
}

# A shell command that outlives the tool's synchronous wait becomes a
# background task. What do the Stop hooks say (fullyIdle), and is the agent
# invoked again by itself when the task ends?
sc_bg() {
  setup_run "bg" "$ver"
  seed_onboarding; seed_settings "$(trusted_json "$WS_REAL")"; install_hooks global
  launch_with_launcher "$ROOT/briefs/sleep25.txt" --dangerously-skip-permissions
  start_viewer
  wait_delivered 60 || { finish; exit 1; }
  wait_hook Stop 1 120
  sleep 1; snap after_first_stop
  mark "no input from here on; watching for hooks for 45s"
  for _ in $(seq 1 225); do sleep 0.2; done
  snap after_watch
  mark "stops=$(hook_count Stop) pre_invocations=$(hook_count PreInvocation) post_tool=$(hook_count PostToolUse)"
  finish
  python3 "$TOOLS" stops "$HOOKS" | tee "$RUN/stop_payloads.txt"
}

# Scoped HOME in a pane: what a tool child process sees and where state lands.
# Also resizes the GhosttyKit surface mid-session.
sc_home() {
  setup_run "home" "$ver"
  seed_onboarding; seed_settings "$(trusted_json "$WS_REAL")"; install_hooks global
  EXTRA_PANE_ENV="export SPIKE_CANARY=set-in-pane-shell; " launch_with_launcher "$ROOT/briefs/home.txt" --dangerously-skip-permissions
  start_viewer
  wait_delivered 60 || { finish; exit 1; }
  wait_hook Stop 1 90
  sleep 1; snap after_tool_turn
  cp "$WS/tool_env_probe.txt" "$RUN/tool_env_probe.txt" 2>/dev/null
  # resize the GhosttyKit surface: shrink, then grow (Boss panes are resized by the layout)
  viewer_cmd "resize:520x330"; sleep 2; snap resized_small; "$P" info "$SESSION" > "$RUN/info_resized_small.txt"
  viewer_cmd "resize:1200x760"; sleep 2; snap resized_large; "$P" info "$SESSION" > "$RUN/info_resized_large.txt"
  submit "Reply with exactly the single token AFTER_RESIZE_OK. Do not use tools."
  wait_hook Stop 2 60
  sleep 1; snap after_resize_probe
  finish
  ( cd "$H" && find . \( -type f -o -type l \) -not -path './.gemini/antigravity-cli/builtin/*' -not -path './.gemini/antigravity-cli/cache/marketplaces/*' | sort ) > "$RUN/scoped_home_inventory.txt"
}

# A PreInvocation hook that replies terminationBehavior=terminate. No model
# call is made; what terminationReason does the Stop hook report?
sc_hookterm() {
  setup_run "hookterm" "$ver"
  seed_onboarding; seed_settings "$(trusted_json "$WS_REAL")"; install_hooks global
  printf '%s\n' '{"injectSteps":[{"errorMessage":{"userMessage":"spike: PreInvocation hook terminated this turn"}}],"terminationBehavior":"terminate"}' > "$HOOKS/reply.PreInvocation.json"
  launch_with_launcher "$ROOT/briefs/small.txt"
  start_viewer
  wait_delivered 60 || { finish; exit 1; }
  wait_hook Stop 1 30
  sleep 1.5; snap after_terminated_turn
  finish
  python3 "$TOOLS" stops "$HOOKS" | tee "$RUN/stop_payloads.txt"
}

# What the pane shows when the scoped HOME cannot reach a credential (no
# keychain bridge). Onboarding and trust are pre-seeded so only sign-in is left.
sc_signin() {
  setup_run "signin" "$ver"
  rm -rf "$H/Library"
  seed_onboarding; seed_settings "$(trusted_json "$WS_REAL")"
  launch_direct
  start_viewer
  sleep 8
  snap unauthenticated
  "$P" info "$SESSION" | tee "$RUN/info_unauthenticated.txt"
  stop_poll; stop_viewer; "$P" kill "$SESSION" 2>/dev/null
  grep -n -i 'keyring\|credential manager\|not logged\|sign' "$RUN/cli.log" | cut -c1-200 | head -8 > "$RUN/cli_log_auth_lines.txt"
}

# Does AGY_CLI_HIDE_ACCOUNT_INFO keep the account out of the pane banner?
sc_hideaccount() {
  setup_run "hideaccount" "$ver"
  seed_onboarding; seed_settings "$(trusted_json "$WS_REAL")"
  EXTRA_PANE_ENV="export AGY_CLI_HIDE_ACCOUNT_INFO=true; " launch_direct
  start_viewer
  sleep 7
  snap idle
  grep -c '@' "$RUN/pane_idle.txt" | sed 's/^/lines containing @: /' | tee "$RUN/result.txt"
  finish > /dev/null
}

# The submit key. Boss's Tmux::send_keys ends every delivery with `send-keys
# C-m`. Type one line, press C-m, and see whether a turn starts; then press
# Enter.
sc_submitkey() {
  setup_run "submitkey" "$ver"
  seed_onboarding; seed_settings "$(trusted_json "$WS_REAL")"; install_hooks global
  launch_direct
  start_viewer
  wait_pane '\? for shortcuts' 30
  sleep 1
  "$P" literal "$SESSION" "Reply with exactly the single token SUBMITKEY_OK. Do not use tools."; mark "literal text sent"
  sleep 0.5
  "$P" key "$SESSION" C-m; mark "C-m sent (tmux send-keys C-m)"
  sleep 4
  snap after_c_m
  echo "after C-m: pre_invocation_hooks=$(hook_count PreInvocation) handle_user_input_lines=$(grep -c 'HandleUserInput called' "$RUN/cli.log")" | tee "$RUN/result.txt"
  "$P" key "$SESSION" Enter; mark "Enter sent (tmux send-keys Enter)"
  wait_hook Stop 1 60
  sleep 1; snap after_enter
  echo "after Enter: pre_invocation_hooks=$(hook_count PreInvocation) handle_user_input_lines=$(grep -c 'HandleUserInput called' "$RUN/cli.log")" | tee -a "$RUN/result.txt"
  finish
}

# hookreply <mode>: what a PreToolUse hook's reply does to the tool call.
#   allow         {"decision":"allow"}
#   empty-object  {}
#   no-output     nothing on stdout, exit 0
#   malformed     not JSON
#   exit-1        nothing on stdout, exit 1 (a crashed hook)
#   exit-2        nothing on stdout, exit 2 (Claude Code's block convention)
# The brief asks for one shell command that writes a canary file.
sc_hookreply() {
  local mode="$1"
  setup_run "hookreply-$mode" "$ver"
  seed_onboarding; seed_settings "$(trusted_json "$WS_REAL")"; install_hooks global
  case "$mode" in
    allow) printf '{"decision":"allow"}\n' > "$HOOKS/reply.PreToolUse.json" ;;
    empty-object) printf '{}\n' > "$HOOKS/reply.PreToolUse.json" ;;
    no-output) : > "$HOOKS/reply.PreToolUse.empty" ;;
    malformed) printf 'NOT JSON\n' > "$HOOKS/reply.PreToolUse.json" ;;
    exit-1) : > "$HOOKS/reply.PreToolUse.empty"; echo 1 > "$HOOKS/exit.PreToolUse" ;;
    exit-2) : > "$HOOKS/reply.PreToolUse.empty"; echo 2 > "$HOOKS/exit.PreToolUse" ;;
    *) echo "unknown mode $mode" >&2; exit 2 ;;
  esac
  printf '%s\n' "Use your shell command tool to run exactly this one command:" "" "    echo ran > hookreply_canary.txt" "" \
    "Try it once only. Then reply with exactly the single token HOOKREPLY_DONE." > "$RUN/brief.txt"
  launch_with_launcher "$RUN/brief.txt" --dangerously-skip-permissions
  start_viewer
  wait_delivered 60 || { finish; exit 1; }
  wait_hook Stop 1 90
  sleep 1; snap after_turn
  local transcript; transcript="$(ls "$H"/.gemini/antigravity-cli/brain/*/.system_generated/logs/transcript_full.jsonl 2>/dev/null | head -1)"
  {
    echo "mode=$mode version=$ver"
    echo "canary=$([ -f "$WS/hookreply_canary.txt" ] && echo written || echo absent)"
    echo "pre_tool_hooks=$(hook_count PreToolUse) post_tool_hooks=$(hook_count PostToolUse)"
    python3 - "$transcript" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    row = json.loads(line)
    if row.get("status") == "ERROR":
        print("step error:", row.get("error", "").replace(" ", " ")[:200])
    if row.get("type") == "PLANNER_RESPONSE" and row.get("content"):
        print("model said:", row["content"][:120].replace("\n", " "))
PY
  } | tee "$RUN/result.txt"
  finish > /dev/null
}

case "$scenario" in
  modes) sc_modes "$@" ;;
  launcher) sc_launcher "$@" ;;
  launcher-untrusted) sc_launcher_untrusted "$@" ;;
  iflag) sc_iflag "$@" ;;
  discovery) sc_discovery "$@" ;;
  trust-tool) sc_trust_tool "$@" ;;
  esc) sc_esc "$@" ;;
  midturn) sc_midturn "$@" ;;
  bg) sc_bg "$@" ;;
  home) sc_home "$@" ;;
  hookterm) sc_hookterm "$@" ;;
  signin) sc_signin "$@" ;;
  hideaccount) sc_hideaccount "$@" ;;
  submitkey) sc_submitkey "$@" ;;
  hookreply) sc_hookreply "$@" ;;
  *) sed -n '2,24p' "$0"; exit 2 ;;
esac
