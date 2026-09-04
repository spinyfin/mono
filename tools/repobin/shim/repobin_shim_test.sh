#!/usr/bin/env bash
# repobin_shim_test.sh — hermetic proof of the two properties the workspace
# shims exist for:
#
#   1. In a freshly provisioned workspace, `bin/checkleft` resolves through
#      repobin built from THAT checkout (the same path CI's bin/checkleft
#      takes), with the caller's cwd intact.
#   2. When repobin cannot be obtained (no bazel, a failing build, an
#      undeclared tool, a cwd outside the checkout) the shim fails loudly and
#      NEVER falls back to a `checkleft` (or `repobin`) found on PATH.
#
# bazel is replaced by a recording fake that "builds" a recording fake
# repobin, and PATH carries decoy `checkleft` / `repobin` executables that
# leave a marker file if they are ever run. The decoys are the ancient
# `cargo install checkleft` this mechanism was written to keep out.
set -euo pipefail

shim_src="$1"
installer_src="$2"
[[ -f "$shim_src" && -f "$installer_src" ]] || {
  echo "usage: $0 <repobin-shim.sh> <install-workspace-shims.sh>" >&2
  exit 1
}

tmp="${TEST_TMPDIR:-$(mktemp -d)}/repobin-shim-test"
rm -rf "$tmp"
mkdir -p "$tmp/tmp"
# The shim's mktemp must land somewhere the bazel sandbox allows. (No
# here-documents anywhere in this test or its fakes: the sandbox's
# /bin/bash 3.2 materialises them under a fixed temp dir it cannot write.)
export TMPDIR="$tmp/tmp"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}
pass() { echo "ok: $*"; }

# ── a fresh "workspace" ──────────────────────────────────────────────────
make_workspace() {
  local ws="$1"
  mkdir -p "$ws/tools/repobin/shim"
  cp "$shim_src" "$ws/tools/repobin/shim/repobin-shim.sh"
  cp "$installer_src" "$ws/tools/repobin/shim/install-workspace-shims.sh"
  chmod +x "$ws/tools/repobin/shim/"*.sh
  printf '%s\n' \
    'version = 1' '' \
    '[tools.boss]' 'target = "//tools/boss/cli:boss"' '' \
    '[tools.bossctl]' 'target = "//tools/boss/cli:bossctl"' '' \
    '[tools.cube]' 'target = "//tools/cube:cube"' '' \
    '[tools.checkleft]' 'target = "//tools/checkleft:checkleft"' '' \
    '[pins.hood]' 'repo = "https://example.invalid/hood.git"' 'version = "v1.2.3"' \
    > "$ws/REPOBIN.toml"
}

ws="$tmp/ws"
make_workspace "$ws"
ws_real="$(cd "$ws" && pwd -P)"

# ── decoys on PATH: must never run ───────────────────────────────────────
decoy_dir="$tmp/decoy"
decoy_marker="$tmp/decoy-invoked"
mkdir -p "$decoy_dir"
for decoy in checkleft repobin; do
  printf '%s\n' \
    '#!/bin/sh' \
    "echo \"decoy $decoy invoked with: \$*\" >> '$decoy_marker'" \
    'echo "checkleft passed cleanly (from the WRONG binary)"' \
    'exit 0' \
    > "$decoy_dir/$decoy"
  chmod +x "$decoy_dir/$decoy"
done

# ── fake bazel ───────────────────────────────────────────────────────────
# Records every invocation. `build` installs a fake repobin (staged below)
# that records ITS argv and cwd; `cquery --output=files` reports where;
# `info output_path` mirrors bazel's absolute output dir.
fake_dir="$tmp/fakebin"
bazel_log="$tmp/bazel-args"
repobin_log="$tmp/repobin-args"
fake_repobin="$tmp/fake-repobin"
mkdir -p "$fake_dir"
printf '%s\n' \
  '#!/bin/sh' \
  'printf "cwd=%s\n" "$(pwd -P)" > "$FAKE_REPOBIN_LOG"' \
  'printf "argv=%s\n" "$*" >> "$FAKE_REPOBIN_LOG"' \
  'echo "fake repobin ran: $*"' \
  > "$fake_repobin"
printf '%s\n' \
  '#!/bin/sh' \
  "printf '%s\\n' \"\$*\" >> '$bazel_log'" \
  'if [ -n "${FAKE_BAZEL_FAIL:-}" ]; then' \
  '  echo "ERROR: fake bazel build failure (simulated)" >&2' \
  '  exit 1' \
  'fi' \
  'sub=' \
  'for a in "$@"; do' \
  '  case "$a" in build | cquery | info) sub=$a; break ;; esac' \
  'done' \
  'out=bazel-out/fake-fastbuild/bin/tools/repobin' \
  'case "$sub" in' \
  '  build)' \
  '    mkdir -p "$out"' \
  "    cp '$fake_repobin' \"\$out/repobin\"" \
  '    chmod +x "$out/repobin"' \
  '    ;;' \
  '  cquery) echo "$out/repobin" ;;' \
  '  info) echo "$(pwd -P)/bazel-out" ;;' \
  '  *)' \
  '    echo "fake bazel: unexpected invocation: $*" >&2' \
  '    exit 2' \
  '    ;;' \
  'esac' \
  > "$fake_dir/bazel"
chmod +x "$fake_dir/bazel"

export PATH="$fake_dir:$decoy_dir:/usr/bin:/bin"
export FAKE_REPOBIN_LOG="$repobin_log"
export REPOBIN_SHIM_QUIET=
# The installer's skip/require lists are generic and read from the
# environment (see its header); the rest of this suite exercises them set
# to mono's actual `.cube/setup.yaml` values, and a dedicated section below
# (see "default: nothing skipped, nothing required") covers the unset
# default for a repobin-using checkout with no such policy of its own.
export REPOBIN_SHIM_SKIP='boss boss-event bossctl cube'
export REPOBIN_SHIM_REQUIRE='checkleft'

run_in() {
  # run_in <dir> <cmd...>: run with cwd=<dir>, capturing rc/stdout/stderr.
  local dir="$1"
  shift
  set +e
  (cd "$dir" && "$@") > "$tmp/stdout" 2> "$tmp/stderr"
  rc=$?
  set -e
}

# 1. Installer populates a fresh bin/ with one shim per REPOBIN.toml tool.
run_in "$ws" bash tools/repobin/shim/install-workspace-shims.sh
[[ $rc -eq 0 ]] || fail "installer exited $rc: $(cat "$tmp/stderr")"
shim_in_ws="$ws/tools/repobin/shim/repobin-shim.sh"
for name in repobin checkleft hood; do
  [[ -L "$ws/bin/$name" ]] || fail "bin/$name is not a symlink after install"
  [[ "$ws/bin/$name" -ef "$shim_in_ws" ]] || fail "bin/$name does not resolve to the workspace's repobin-shim.sh"
  [[ -x "$ws/bin/$name" ]] || fail "bin/$name is not executable"
done
for name in boss bossctl cube; do
  [[ ! -e "$ws/bin/$name" ]] || fail "bin/$name must remain engine-owned, not a repobin shim"
done
grep -q 'bin/ ready (3 installed, 0 already present)' "$tmp/stdout" || fail "unexpected installer report: $(cat "$tmp/stdout")"
pass "fresh workspace: installer creates only non-engine-owned shims"

run_in "$ws" bash tools/repobin/shim/install-workspace-shims.sh
[[ $rc -eq 0 ]] || fail "second install exited $rc"
grep -q 'bin/ ready (0 installed, 3 already present)' "$tmp/stdout" || fail "installer is not idempotent: $(cat "$tmp/stdout")"
pass "installer is idempotent"

# 2. bin/checkleft dispatches through repobin built from this checkout.
rm -f "$repobin_log" "$decoy_marker" "$bazel_log"
run_in "$ws" bin/checkleft run --verbose
[[ $rc -eq 0 ]] || fail "bin/checkleft run exited $rc: $(cat "$tmp/stderr")"
grep -q '^argv=exec checkleft -- run --verbose$' "$repobin_log" || fail "repobin argv: $(cat "$repobin_log")"
grep -q "^cwd=$ws_real\$" "$repobin_log" || fail "cwd not preserved: $(cat "$repobin_log")"
grep -q -- '-- //tools/repobin:repobin' "$bazel_log" || fail "shim did not build repobin: $(cat "$bazel_log")"
grep -q 'fake repobin ran: exec checkleft -- run --verbose' "$tmp/stdout" || fail "stdout: $(cat "$tmp/stdout")"
[[ ! -e "$decoy_marker" ]] || fail "a PATH decoy was invoked: $(cat "$decoy_marker")"
grep -q "^repobin-shim: checkleft -> //tools/checkleft:checkleft built from $ws_real via " "$tmp/stderr" \
  || fail "no provenance line on stderr: $(cat "$tmp/stderr")"
pass "bin/checkleft run -> repobin exec checkleft, built from the checkout, cwd intact, provenance printed"

# 3. From a subdirectory the caller's cwd is still what repobin sees.
mkdir -p "$ws/sub/dir"
run_in "$ws/sub/dir" ../../bin/checkleft list
[[ $rc -eq 0 ]] || fail "subdir run exited $rc: $(cat "$tmp/stderr")"
grep -q "^cwd=$ws_real/sub/dir\$" "$repobin_log" || fail "subdir cwd not preserved: $(cat "$repobin_log")"
pass "cwd preserved from a subdirectory"

# 4. bin/repobin is the repobin CLI itself (the engine push guard's probe shape).
run_in "$ws" bin/repobin exec checkleft --version
[[ $rc -eq 0 ]] || fail "bin/repobin exited $rc: $(cat "$tmp/stderr")"
grep -q '^argv=exec checkleft --version$' "$repobin_log" || fail "bin/repobin argv: $(cat "$repobin_log")"
grep -q '^repobin-shim: repobin -> bazel-out/' "$tmp/stderr" || fail "no repobin provenance: $(cat "$tmp/stderr")"
pass "bin/repobin exec checkleft --version reaches the built repobin"

# 5. A pinned tool dispatches too, and says so.
run_in "$ws" bin/hood --help
[[ $rc -eq 0 ]] || fail "bin/hood exited $rc: $(cat "$tmp/stderr")"
grep -q '^argv=exec hood -- --help$' "$repobin_log" || fail "hood argv: $(cat "$repobin_log")"
grep -q 'hood -> (pinned upstream tag per REPOBIN.toml)' "$tmp/stderr" || fail "hood provenance: $(cat "$tmp/stderr")"
pass "pinned tool dispatches with a pinned provenance line"

# 6. No bazel on PATH: loud failure, decoys untouched.
rm -f "$decoy_marker" "$repobin_log"
run_in "$ws" env PATH="$decoy_dir:/usr/bin:/bin" bin/checkleft run
[[ $rc -ne 0 ]] || fail "expected a failure without bazel on PATH"
grep -q 'neither `bazel` nor `bazelisk` is on PATH' "$tmp/stderr" || fail "stderr: $(cat "$tmp/stderr")"
grep -q 'will not fall back to a PATH copy of `checkleft`' "$tmp/stderr" || fail "stderr: $(cat "$tmp/stderr")"
[[ ! -e "$decoy_marker" ]] || fail "decoy invoked when bazel was missing: $(cat "$decoy_marker")"
[[ ! -e "$repobin_log" ]] || fail "a repobin ran without bazel: $(cat "$repobin_log")"
pass "no bazel -> exit $rc naming bazel, no PATH fallback"

# 7. bazel build fails: loud failure carrying bazel's output, decoys untouched.
rm -f "$decoy_marker" "$repobin_log"
run_in "$ws" env FAKE_BAZEL_FAIL=1 bin/checkleft run
[[ $rc -ne 0 ]] || fail "expected a failure when bazel build fails"
grep -q 'fake bazel build failure (simulated)' "$tmp/stderr" || fail "bazel output not surfaced: $(cat "$tmp/stderr")"
grep -q 'bazel build //tools/repobin:repobin failed' "$tmp/stderr" || fail "stderr: $(cat "$tmp/stderr")"
grep -q 'refusing to fall back to a PATH copy of `checkleft`' "$tmp/stderr" || fail "stderr: $(cat "$tmp/stderr")"
[[ ! -e "$decoy_marker" ]] || fail "decoy invoked after a failed build: $(cat "$decoy_marker")"
[[ ! -e "$repobin_log" ]] || fail "a repobin ran after a failed build: $(cat "$repobin_log")"
pass "failed bazel build -> exit $rc with bazel's output, no PATH fallback"

# 8. A tool REPOBIN.toml does not declare is refused outright.
ln -s ../tools/repobin/shim/repobin-shim.sh "$ws/bin/mystery"
rm -f "$repobin_log"
run_in "$ws" bin/mystery --anything
[[ $rc -ne 0 ]] || fail "expected refusal for an undeclared tool"
grep -q '`mystery` is not declared in' "$tmp/stderr" || fail "stderr: $(cat "$tmp/stderr")"
[[ ! -e "$repobin_log" ]] || fail "undeclared tool still dispatched: $(cat "$repobin_log")"
pass "undeclared tool -> refused"

# 9. Running from outside the checkout is refused (repobin would otherwise
#    resolve from that cwd, i.e. some other repo or default mode).
rm -f "$repobin_log"
run_in "$tmp" "$ws/bin/checkleft" run
[[ $rc -ne 0 ]] || fail "expected refusal from outside the checkout"
grep -q 'not inside the checkout at' "$tmp/stderr" || fail "stderr: $(cat "$tmp/stderr")"
[[ ! -e "$repobin_log" ]] || fail "dispatched from outside the checkout: $(cat "$repobin_log")"
pass "cwd outside the checkout -> refused"

# 10. Invoking the shim directly (not via a bin/ symlink) is refused.
run_in "$ws" tools/repobin/shim/repobin-shim.sh run
[[ $rc -ne 0 ]] || fail "expected refusal for a direct invocation"
grep -q 'through a bin/<tool> symlink' "$tmp/stderr" || fail "stderr: $(cat "$tmp/stderr")"
pass "direct invocation -> refused"

# 11. Installer never clobbers a real `repobin install`.
real="$tmp/real"
make_workspace "$real"
mkdir -p "$real/bin"
printf '#!/bin/sh\necho real repobin\n' > "$real/bin/repobin"
chmod +x "$real/bin/repobin"
ln -s repobin "$real/bin/checkleft"
run_in "$real" bash tools/repobin/shim/install-workspace-shims.sh
[[ $rc -eq 0 ]] || fail "installer exited $rc on a real install: $(cat "$tmp/stderr")"
grep -q 'leaving bin/ untouched' "$tmp/stdout" || fail "stdout: $(cat "$tmp/stdout")"
[[ -L "$real/bin/checkleft" && "$real/bin/checkleft" -ef "$real/bin/repobin" ]] || fail "real checkleft symlink was replaced"
[[ -f "$real/bin/repobin" && ! -L "$real/bin/repobin" ]] || fail "real repobin binary was replaced"
[[ ! -e "$real/bin/boss" ]] || fail "installer added entries next to a real install"
pass "real repobin install is left untouched"

# 12. Installer leaves a stray regular file alone (warns) but still
#     installs the rest, and refuses to report healthy without checkleft.
rm -f "$ws/bin/hood"
echo "not a symlink" > "$ws/bin/hood"
run_in "$ws" bash tools/repobin/shim/install-workspace-shims.sh
[[ $rc -eq 0 ]] || fail "installer exited $rc with a stray file: $(cat "$tmp/stderr")"
grep -q 'bin/hood exists and is not a symlink; leaving it alone' "$tmp/stderr" || fail "stderr: $(cat "$tmp/stderr")"
[[ "$(cat "$ws/bin/hood")" == "not a symlink" ]] || fail "stray bin/hood was clobbered"
rm -f "$ws/bin/checkleft"
echo "not executable" > "$ws/bin/checkleft"
run_in "$ws" bash tools/repobin/shim/install-workspace-shims.sh
[[ $rc -ne 0 ]] || fail "installer reported healthy with an unusable bin/checkleft"
grep -q 'bin/checkleft is not executable after install' "$tmp/stderr" || fail "stderr: $(cat "$tmp/stderr")"
pass "stray regular files are never clobbered; an unusable bin/checkleft fails the install"

# 13. default: nothing skipped, nothing required. A repobin-using checkout
#     that sets neither REPOBIN_SHIM_SKIP nor REPOBIN_SHIM_REQUIRE gets a
#     shim for every declared tool -- including names mono happens to skip
#     -- and a clean exit even though nothing was declared "required".
default_ws="$tmp/default-ws"
make_workspace "$default_ws"
run_in "$default_ws" env -u REPOBIN_SHIM_SKIP -u REPOBIN_SHIM_REQUIRE bash tools/repobin/shim/install-workspace-shims.sh
[[ $rc -eq 0 ]] || fail "default installer exited $rc: $(cat "$tmp/stderr")"
for name in repobin checkleft hood boss bossctl cube; do
  [[ -L "$default_ws/bin/$name" ]] || fail "default install: bin/$name is not a symlink (nothing should be skipped by default)"
done
pass "default (unset SKIP/REQUIRE): every declared tool is shimmed, nothing is required"

# Removing bin/checkleft must NOT fail the install when nothing is required.
rm -f "$default_ws/bin/checkleft"
run_in "$default_ws" env -u REPOBIN_SHIM_SKIP -u REPOBIN_SHIM_REQUIRE bash tools/repobin/shim/install-workspace-shims.sh
[[ $rc -eq 0 ]] || fail "default installer with no bin/checkleft exited $rc: $(cat "$tmp/stderr")"
pass "default (unset REQUIRE): a repo with no checkleft gate installs cleanly"

echo "all repobin shim checks passed"
