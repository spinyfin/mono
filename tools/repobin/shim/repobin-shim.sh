#!/usr/bin/env bash
# repobin-shim.sh — bootstrap shim that stands in for a `repobin install`ed
# tool symlink in a workspace's gitignored bin/.
#
# Why this exists
#
#   CI populates bin/ with the real thing: `.buildkite/steps/ci-env.sh` builds
#   //tools/repobin:repobin and runs `repobin install --bin-dir bin/`, so CI's
#   `bin/checkleft` is repobin dispatching //tools/checkleft:checkleft, built
#   from the checkout under test. Cube workspaces never ran that step, so a
#   worker that tried `./bin/checkleft run` found nothing and fell back to
#   whatever `checkleft` happened to be on PATH — on a developer host, an
#   ancient `cargo install checkleft`. A gate that passed there said nothing
#   about the gate CI enforces.
#
#   A lease has a hard time budget (the Boss engine bounds `cube workspace
#   lease` at ~90s), so cube's setup step cannot afford `bazel build` on a cold
#   workspace. Instead `.cube/setup.yaml` runs install-workspace-shims.sh,
#   which symlinks every eligible REPOBIN.toml tool name in bin/ to THIS script.
#   The engine-owned boss/cube control names are intentionally excluded. The
#   shim defers the build to first use and then does exactly what CI's
#   symlink does:
#
#     1. build //tools/repobin:repobin with bazel, from the checkout that
#        contains this script;
#     2. exec that repobin — `repobin exec <tool> -- "$@"` (or the repobin
#        CLI itself for bin/repobin) — which builds the tool's REPOBIN.toml
#        target from the same checkout and execs it with the caller's cwd
#        intact.
#
# Guarantees
#
#   * It NEVER searches PATH for the tool, and never for repobin either. If
#     anything above fails the shim exits non-zero naming what it tried;
#     there is no fallback binary.
#   * It refuses tools REPOBIN.toml does not declare, and refuses to run
#     from a cwd outside the checkout, so it cannot dispatch some other
#     repo's (or repobin default-mode's) copy.
#   * Every invocation prints one provenance line on stderr (suppress with
#     REPOBIN_SHIM_QUIET=1) so a transcript shows which checkout and target
#     actually ran.
#
# The first invocation can build repobin and its dispatched tool, so callers
# on a critical gate must allow that work to complete rather than approving a
# failed or timed-out invocation.
#
# Honors the same bazel env knobs repobin does: CI_BAZEL_STARTUP_FLAGS
# (startup options) and REPOBIN_BAZEL_FLAGS (build/cquery options).
#
# Tested by //tools/repobin/shim:repobin_shim_test.
set -euo pipefail

die() {
  printf 'repobin-shim: %s\n' "$@" >&2
  exit 1
}

tool="$(basename -- "$0")"

case "$tool" in
  *.sh)
    die "invoke this script through a bin/<tool> symlink (see install-workspace-shims.sh), not directly"
    ;;
esac

# The checkout root is the nearest ancestor of the invoked symlink's
# directory that holds a REPOBIN.toml — for an installed `<root>/bin/<tool>`
# that is one level up. Then confirm the symlink really resolves to this
# file in that checkout (bash's builtin `-ef` compares inodes through
# symlinks), so a link planted elsewhere can never dispatch a different
# checkout's tools. Deliberately no `readlink`/`realpath`: the shim must run
# in environments with only a minimal audited toolset (see
# tools/test-sandbox/repositories.bzl).
root="$(cd -- "$(dirname -- "$0")" && pwd -P)"
while [[ ! -f "$root/REPOBIN.toml" ]]; do
  if [[ "$root" == / ]]; then
    die "no REPOBIN.toml found above $(cd -- "$(dirname -- "$0")" && pwd -P); install this shim as <checkout>/bin/<tool> (see install-workspace-shims.sh)"
  fi
  root="$(dirname -- "$root")"
done
config="$root/REPOBIN.toml"
self="$root/tools/repobin/shim/repobin-shim.sh"
[[ "$0" -ef "$self" ]] || die "$0 does not resolve to $self; refusing to dispatch from a checkout this shim does not belong to"

cwd="$(pwd -P)"
case "$cwd/" in
  "$root"/*) ;;
  *)
    die "refusing to run \`$tool\` from $cwd: not inside the checkout at $root" \
      "repobin resolves tools from the working directory, so run it from inside the workspace"
    ;;
esac

target=
if [[ "$tool" != repobin ]]; then
  if awk -v hdr="[tools.$tool]" '
      { line = $0; sub(/^[[:space:]]+/, "", line); sub(/[[:space:]]+$/, "", line) }
      line == hdr { found = 1; exit }
      END { exit !found }
    ' "$config"; then
    target="$(awk -v hdr="[tools.$tool]" '
      { line = $0; sub(/^[[:space:]]+/, "", line); sub(/[[:space:]]+$/, "", line) }
      line == hdr { in_section = 1; next }
      /^\[/ { in_section = 0 }
      in_section && /^target[[:space:]]*=/ {
        sub(/^target[[:space:]]*=[[:space:]]*"/, "")
        sub(/".*$/, "")
        print
        exit
      }' "$config")"
    [[ -n "$target" ]] || target="(target per REPOBIN.toml)"
  elif awk -v hdr="[pins.$tool]" '
      { line = $0; sub(/^[[:space:]]+/, "", line); sub(/[[:space:]]+$/, "", line) }
      line == hdr { found = 1; exit }
      END { exit !found }
    ' "$config"; then
    target="(pinned upstream tag per REPOBIN.toml)"
  else
    die "\`$tool\` is not declared in $config; refusing to run it" \
      "this shim never falls back to a PATH copy of \`$tool\`"
  fi
fi

bazel_bin="$(command -v bazel || command -v bazelisk || true)"
[[ -n "$bazel_bin" ]] || die "cannot build repobin: neither \`bazel\` nor \`bazelisk\` is on PATH (PATH=$PATH)" \
  "install bazel(isk); this shim will not fall back to a PATH copy of \`$tool\`"

# Word-split the same way CI expands its unquoted $BAZEL_STARTUP_FLAGS.
startup_flags=()
if [[ -n "${CI_BAZEL_STARTUP_FLAGS:-}" ]]; then
  read -r -a startup_flags <<< "$CI_BAZEL_STARTUP_FLAGS"
fi
build_flags=()
if [[ -n "${REPOBIN_BAZEL_FLAGS:-}" ]]; then
  read -r -a build_flags <<< "$REPOBIN_BAZEL_FLAGS"
fi
# `${arr[@]+"${arr[@]}"}` keeps `set -u` happy on bash 3.2 when the array
# is empty.
quiet_flags=(--color=no --curses=no --noshow_progress --ui_event_filters=-info)
repobin_target='//tools/repobin:repobin'

log="$(mktemp "${TMPDIR:-/tmp}/repobin-shim.XXXXXX")"
cleanup() { rm -f "$log"; }
trap cleanup EXIT

if ! (
  cd -- "$root" &&
    "$bazel_bin" ${startup_flags[@]+"${startup_flags[@]}"} build \
      "${quiet_flags[@]}" --show_result=0 \
      ${build_flags[@]+"${build_flags[@]}"} -- "$repobin_target"
) >"$log" 2>&1; then
  cat "$log" >&2
  die "bazel build $repobin_target failed in $root (bazel output above)" \
    "refusing to fall back to a PATH copy of \`$tool\`"
fi

# Locate the binary the build just produced. `cquery --output=files` reports
# it relative to the workspace (bazel-out/<config>/bin/...), which resolves
# through the bazel-out convenience symlink; if that symlink is disabled,
# `bazel info output_path` gives the same directory by absolute path.
exe_rel="$(
  (
    cd -- "$root" &&
      "$bazel_bin" ${startup_flags[@]+"${startup_flags[@]}"} cquery \
        "${quiet_flags[@]}" ${build_flags[@]+"${build_flags[@]}"} \
        --output=files -- "$repobin_target"
  ) 2>>"$log" | grep -m1 '^bazel-out/' || true
)"
[[ -n "$exe_rel" ]] || {
  cat "$log" >&2
  die "bazel cquery --output=files $repobin_target reported no bazel-out/ file (output above)"
}
repobin="$root/$exe_rel"
if [[ ! -x "$repobin" ]]; then
  output_path="$(
    (cd -- "$root" && "$bazel_bin" ${startup_flags[@]+"${startup_flags[@]}"} info output_path) 2>>"$log" || true
  )"
  repobin="${output_path}/${exe_rel#bazel-out/}"
fi
[[ -x "$repobin" ]] || {
  cat "$log" >&2
  die "built repobin is not executable at $root/$exe_rel (nor under \`bazel info output_path\`)"
}

if [[ -z "${REPOBIN_SHIM_QUIET:-}" ]]; then
  if [[ "$tool" == repobin ]]; then
    printf 'repobin-shim: repobin -> %s (built from %s)\n' "$exe_rel" "$root" >&2
  else
    printf 'repobin-shim: %s -> %s built from %s via `%s exec %s` (never a PATH copy)\n' \
      "$tool" "$target" "$root" "$exe_rel" "$tool" >&2
  fi
fi

# `exec` replaces this process, so the EXIT trap would never run.
cleanup
trap - EXIT
if [[ "$tool" == repobin ]]; then
  exec "$repobin" "$@"
fi
exec "$repobin" exec "$tool" -- "$@"
