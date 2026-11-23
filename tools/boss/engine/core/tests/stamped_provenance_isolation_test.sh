#!/usr/bin/env bash
# Guard: the Bazel-stamped leaf crates (`boss_build_provenance` /
# `boss_build_info` and the genrules that produce their source) must not
# be a transitive dependency of `engine_lib`.
#
# Provenance source embeds the commit sha, so it changes on every commit.
# Build-info source embeds the tag-derived version, so it changes on every
# boss-v* tag. If `engine_lib` linked either, every commit or every release
# tag would recompile `engine_lib`, every `rust_test(crate = ":engine_lib")`
# binary, `boss` and `bossctl`, and re-execute their tests (see
# docs/investigations/docs-only-pr-reexecutes-engine-tests-2026-10-21.md).
# The stamps are linked only by the `engine` rust_binary targets (and, for
# the version leaf, by `boss` / `bossctl`), which hand provenance to
# `build_info::install_provenance` at startup.
#
# Inputs are two `genquery` outputs evaluated against the real dependency
# graph at build time (see BUILD.bazel):
#   $1 — stamped targets found in deps(engine_lib_test / engine_lib /
#        boss_lib / boss_test / bossctl_lib / bossctl_test / app_test);
#        must be empty.
#   $2 — stamped targets found in deps(engine);          must be non-empty
#        (positive control, so an empty $1 cannot be a rotted filter).
#   $3 — bundled production binaries with stamp = -1;    must be empty.
#
#   $4 — default-stamp rust_binary found by the same expression as $3 over a
#        known unstamped binary; must be non-empty (positive control, so an
#        empty $3 cannot be a rotted filter).
#
#   $5, $6 — the root .bazelrc and the .ci.bazelrc it imports. `--stamp` must
#        not be set for ordinary builds: with it, every rust_binary's Rustc
#        action takes stable-status.txt (which carries the commit sha) as an
#        input and recompiles on every commit. Under any verb (`build`,
#        `common`, `test`) and any config (`build:ci`, ...) it is allowed only
#        in `build:release`. `--config=release` is what the boss-release
#        step and installer:release pass so the build-info leaf crate embeds
#        the tagged version; check builds stay unstamped and keep the
#        0.0.0-dev placeholder. engine_lib never compiles that file.
#
# Pure bash on purpose: the hermetic test wrapper strips PATH, and reading a
# file with `$(<file)` needs no external tool.
set -euo pipefail

engine_lib_hits_file="$1"
positive_control_file="$2"
default_stamp_binaries_file="$3"
default_stamp_control_file="$4"
bazelrc_files=("$5" "$6")

engine_lib_hits="$(<"$engine_lib_hits_file")"
positive_control="$(<"$positive_control_file")"
default_stamp_binaries="$(<"$default_stamp_binaries_file")"
default_stamp_control="$(<"$default_stamp_control_file")"

if [[ -n "${engine_lib_hits//[[:space:]]/}" ]]; then
  echo "ERROR: engine_lib_test (and therefore engine_lib) transitively depends on a per-commit or per-tag stamped target:" >&2
  echo "$engine_lib_hits" >&2
  echo "" >&2
  echo "The stamped leaf crates may only be linked by the engine rust_binary targets" >&2
  echo "(and, for the version leaf, by boss / bossctl)." >&2
  echo "Reintroducing this edge recompiles engine_lib and re-executes its tests on every commit or release tag." >&2
  echo "Read the values through boss_engine::build_info (installed by main.rs) instead." >&2
  exit 1
fi

if [[ -z "${positive_control//[[:space:]]/}" ]]; then
  echo "ERROR: positive control is empty — the filter no longer finds //tools/boss/build-provenance or //tools/boss/build-info in deps(engine)." >&2
  echo "Either the engine binary stopped linking a stamped leaf (then main.rs can no longer install the" >&2
  echo "real build identity or print --version) or genquery's label spelling changed; fix the regex in BUILD.bazel rather than" >&2
  echo "trusting an empty engine_lib result." >&2
  exit 1
fi

if [[ -z "${default_stamp_control//[[:space:]]/}" ]]; then
  echo "ERROR: default-stamp positive control is empty — attr(\"stamp\", \"^-1\$\", ...) no longer matches a rust_binary that leaves stamp unset." >&2
  echo "Fix the expression in BUILD.bazel (bundled_binaries_default_stamp and default_stamp_positive_control)" >&2
  echo "rather than trusting an empty bundled-binaries result." >&2
  exit 1
fi

if [[ -n "${default_stamp_binaries//[[:space:]]/}" ]]; then
  echo "ERROR: bundled production rust_binary target(s) still use default stamp = -1:" >&2
  echo "$default_stamp_binaries" >&2
  echo "" >&2
  echo "Set stamp = 0 on these targets so --config=release does not feed stable-status.txt into their rustc actions." >&2
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

echo "OK: stamped leaf crates are linked by the engine binary only; engine_lib's closure is free of per-commit and per-tag inputs."
