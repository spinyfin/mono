#!/usr/bin/env bash
# Guard: the Bazel-stamped `boss_build_provenance` crate (and the
# `build_provenance_rs` action that generates its source from
# stable-status.txt) must not be a transitive dependency of `engine_lib`.
#
# That crate's source embeds the commit sha, so it changes on every commit.
# If `engine_lib` linked it, every commit — a docs-only change included —
# would recompile `engine_lib`, every `rust_test(crate = ":engine_lib")`
# binary, `boss` and `bossctl`, and re-execute their tests (see
# docs/investigations/docs-only-pr-reexecutes-engine-tests-2026-10-21.md).
# The stamp is linked only by the `engine` rust_binary targets, which hand
# the values to `build_info::install_provenance` at startup.
#
# Inputs are two `genquery` outputs evaluated against the real dependency
# graph at build time (see BUILD.bazel):
#   $1 — stamped targets found in deps(engine_lib_test); must be empty.
#   $2 — stamped targets found in deps(engine);          must be non-empty
#        (positive control, so an empty $1 cannot be a rotted filter).
#
#   $3, $4 — the root .bazelrc and the .ci.bazelrc it imports. `--stamp` must
#        not be set for ordinary builds: with it, every rust_binary's Rustc
#        action takes stable-status.txt (which carries the commit sha) as an
#        input and recompiles on every commit. Under any verb (`build`,
#        `common`, `test`) and any config (`build:ci`, ...) it is allowed only
#        in `build:release`.
#
# Pure bash on purpose: the hermetic test wrapper strips PATH, and reading a
# file with `$(<file)` needs no external tool.
set -euo pipefail

engine_lib_hits_file="$1"
positive_control_file="$2"
bazelrc_files=("$3" "$4")

engine_lib_hits="$(<"$engine_lib_hits_file")"
positive_control="$(<"$positive_control_file")"

if [[ -n "${engine_lib_hits//[[:space:]]/}" ]]; then
  echo "ERROR: engine_lib_test (and therefore engine_lib) transitively depends on a per-commit stamped target:" >&2
  echo "$engine_lib_hits" >&2
  echo "" >&2
  echo "The stamped provenance crate may only be linked by the engine rust_binary targets." >&2
  echo "Reintroducing this edge recompiles engine_lib and re-executes its tests on every commit." >&2
  echo "Read the values through boss_engine::build_info (installed by main.rs) instead." >&2
  exit 1
fi

if [[ -z "${positive_control//[[:space:]]/}" ]]; then
  echo "ERROR: positive control is empty — the filter no longer finds //tools/boss/build-provenance in deps(engine)." >&2
  echo "Either the engine binary stopped linking the stamped crate (then main.rs can no longer install the" >&2
  echo "real build identity) or genquery's label spelling changed; fix the regex in BUILD.bazel rather than" >&2
  echo "trusting an empty engine_lib result." >&2
  exit 1
fi

for bazelrc_file in "${bazelrc_files[@]}"; do
  while IFS= read -r line; do
    # Strip trailing comments, then match `--stamp` (or `--stamp=true|1`)
    # under build/common/test, optionally scoped to a `:config`.
    line="${line%%#*}"
    if [[ "$line" =~ ^[[:space:]]*(build|common|test)(:([A-Za-z0-9_-]+))?[[:space:]]+(.*[[:space:]])?--stamp(=true|=1|[[:space:]]|$) ]]; then
      if [[ "${BASH_REMATCH[3]}" == "release" ]]; then
        continue
      fi
      echo "ERROR: ${bazelrc_file##*/} enables --stamp outside build:release: $line" >&2
      echo "With --stamp, every rust_binary Rustc action reads stable-status.txt and recompiles on every commit." >&2
      echo "Scope it to 'build:release --stamp' instead." >&2
      exit 1
    fi
  done <"$bazelrc_file"
done

echo "OK: the stamped provenance crate is linked by the engine binary only; engine_lib's closure is free of per-commit inputs."
