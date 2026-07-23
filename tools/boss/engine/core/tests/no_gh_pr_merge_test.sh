#!/usr/bin/env bash
# Guard: `gh pr merge` may only be invoked from the one sanctioned,
# human-triggered call site — merge_when_ready.rs's `gh_merge_when_ready`,
# the Direct merge-mechanism implementation behind the `MergeWhenReady`
# RPC. `--auto` still honours required-status-checks / branch protection,
# so this is not a bypass; the invariant this guards is that no *other*
# engine code path grows its own ad-hoc gh-merge invocation outside that
# single reviewed implementation.
#
# Matches on the actual invocation shape (the `"pr", "merge"` argv passed
# to the gh subprocess), not the English phrase "gh pr merge" — that
# phrase legitimately appears in comments and error strings all over the
# merge-mechanism code (merge_mechanism.rs, review.rs, runner.rs, etc.)
# describing this exact call site, and matching on it would flag prose,
# not behaviour.
set -euo pipefail

src_dir="$TEST_SRCDIR/mono/tools/boss/engine/core/src"
allowed_file="merge_when_ready.rs"

matches="$(grep -rl '"pr", *"merge"' "$src_dir" --include="*.rs" || true)"
bad="$(echo "$matches" | grep -v "/${allowed_file}\$" || true)"

if [ -n "$bad" ]; then
  echo "ERROR: gh 'pr merge' invocation found outside the sanctioned ${allowed_file}:" >&2
  echo "$bad" >&2
  echo "The engine must not auto-merge PRs outside that one reviewed call site." >&2
  exit 1
fi

echo "OK: gh 'pr merge' invocation is confined to ${allowed_file}."
