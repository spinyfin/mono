#!/bin/sh
# Login-shell replacement for the engine-restart drill
# (tmux_engine_restart_drill_integration.rs). Stands in for a live worker
# "mid-turn": it keeps making independently observable progress (appending
# heartbeats to a file in its workspace) for as long as its process lives,
# regardless of what happens to the engine that spawned it.

trap 'exit 0' HUP INT TERM

i=0
while :; do
    i=$((i + 1))
    printf 'heartbeat %s\n' "$i" >> heartbeat.log
    sleep 0.1
done
