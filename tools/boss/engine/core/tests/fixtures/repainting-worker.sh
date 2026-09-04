#!/bin/sh
# This is deliberately used as a login-shell replacement by the tmux recovery
# integration target. The production spawn flow still passes its normal
# `-l -i -c <driver command>` arguments; the fixture replaces that driver
# command with a long-lived terminal workload whose title and redraws change
# while no semantic driver event is emitted.

trap 'exit 0' HUP INT TERM

while :; do
    repaint_count=$((repaint_count + 1))
    printf '\033]0;boss-repainting-fixture-title\007\033[2K\rboss-repainting-fixture-title %s' "$repaint_count"
    sleep 0.05
done
