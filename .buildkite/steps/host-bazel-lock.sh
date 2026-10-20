#!/usr/bin/env bash
# host-bazel-lock.sh — at most one heavy Bazel job per CI host at a time.
#
# Sourced (never executed) by ci-env.sh, which calls host_bazel_lock_acquire
# before the first Bazel invocation of a step. Every CI step that runs Bazel
# sources ci-env.sh, so every Bazel-running step on a Mac host takes this lock
# and holds it until the step exits. Two or more such jobs scheduled onto the
# same host therefore run one after another instead of compiling the same
# large crates side by side. See .buildkite/mac-agents-runbook.md for the
# incident history, the memory arithmetic, and the host-side configuration.
#
# This file is deliberately self-contained (no mono-specific dependencies) so
# sibling repos whose pipelines share the Mac hosts (appoint, flunge) can copy
# it verbatim and source it from their own Bazel-running steps. The lock path
# is the cross-repo contract: every pipeline on a host must use the same
# directory or the serialisation is only per-repo.
#
# Mechanism: an atomic `mkdir` of $CI_HOST_BAZEL_LOCK_DIR/lock, with an
# `owner` file naming the holder (pid, boot id, acquire time, Buildkite job).
# Waiters poll (so admission is not FIFO: whichever poll lands first after a
# release wins). A lock whose holder pid is gone, whose boot id does not match
# the running kernel, or which never got an owner file is stale and is
# reclaimed. A live holder is never evicted, however long it has run: reclaim
# only removes the directory, it cannot stop the holder's Bazel work, so
# evicting a slow job would let a successor run beside it. Long holds are
# reported in the waiting message instead.
#
# Reclaim is serialised by a kernel-held exclusive lock (flock on
# $root/reclaim.flock, taken through perl, which every Mac host and the Bazel
# test sandbox have). The staleness check is redone, and the lock renamed
# aside and deleted, only while that flock is held, so a waiter that judged an
# old lock stale but was delayed re-checks and finds the fresh live lock
# someone else acquired meanwhile, and leaves it alone. Because the kernel
# drops an flock when its holder exits, there is no guard file to go stale and
# nothing that has to be deleted by pathname on the strength of an earlier
# read; a reclaimer that dies mid-section cannot wedge anyone. Without perl
# no lock is ever reclaimed (exclusivity is preserved; a stale lock then needs
# the manual remedy in the runbook). Nothing else removes a lock that has a
# living holder: its own release is the only other writer, and a stale lock's
# holder is by definition not releasing.
#
# Pipelines that cannot source this file from their steps can take the lock
# for a whole job from Buildkite agent hooks instead; see
# .buildkite/host-hooks/ and the runbook.
#
# Enablement:
#   CI_HOST_BAZEL_LOCK=1     always take the lock
#   CI_HOST_BAZEL_LOCK=0     never take the lock
#   unset / "auto"           take it on Darwin only (Linux hosts are not
#                            throttled; no evidence they need it)
# Tunables (all optional):
#   CI_HOST_BAZEL_LOCK_DIR           default ${HOME}/.cache/buildkite-host-bazel-lock
#   CI_HOST_BAZEL_LOCK_POLL_SECS     default 5
#   CI_HOST_BAZEL_LOCK_REPORT_SECS   default 60  (progress line while waiting)
#   CI_HOST_BAZEL_LOCK_LONG_HOLD_SECS  default 10800 (3h; a live holder older
#                                    than this is called out while waiting,
#                                    never reclaimed)
#   CI_HOST_BAZEL_LOCK_OWNERLESS_GRACE_SECS  default 30 (how long a lock may
#                                    exist with no owner file before it is
#                                    treated as abandoned)
#   CI_HOST_BAZEL_LOCK_BOOT_ID       override the detected boot id (tests)

# Absolute path of this file, so a reclaim can re-run it under the flock.
HOST_BAZEL_LOCK_LIB="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

host_bazel_lock_enabled() {
  case "${CI_HOST_BAZEL_LOCK:-auto}" in
    1 | true | yes) return 0 ;;
    0 | false | no) return 1 ;;
    *) [[ "$(uname -s)" == "Darwin" ]] ;;
  esac
}

host_bazel_lock_root() {
  printf '%s\n' "${CI_HOST_BAZEL_LOCK_DIR:-${HOME}/.cache/buildkite-host-bazel-lock}"
}

host_bazel_lock_path() {
  printf '%s/lock\n' "$(host_bazel_lock_root)"
}

# A value that changes on every reboot, so a lock left behind by a pid that
# no longer exists cannot be mistaken for live because the pid got reused
# after a restart. Empty when unknown (the check is then skipped).
host_bazel_lock_boot_id() {
  if [[ -n "${CI_HOST_BAZEL_LOCK_BOOT_ID:-}" ]]; then
    printf '%s\n' "$CI_HOST_BAZEL_LOCK_BOOT_ID"
    return 0
  fi
  case "$(uname -s)" in
    # /usr/sbin is not on every PATH (Bazel's test sandbox, for one).
    Darwin) "$(command -v sysctl || printf '/usr/sbin/sysctl')" -n kern.boottime 2>/dev/null | sed -E 's/.*sec = ([0-9]+).*/\1/' ;;
    Linux) awk '/^btime/ { print $2 }' /proc/stat 2>/dev/null ;;
    *) printf '' ;;
  esac
}

host_bazel_lock_log() {
  printf '[host-bazel-lock] %s\n' "$*"
}

# Prints the value of `key=` from the owner file, or nothing.
host_bazel_lock_owner_field() {
  local lock="$1" key="$2"
  [[ -r "${lock}/owner" ]] || return 0
  grep -m1 "^${key}=" "${lock}/owner" 2>/dev/null | cut -d= -f2-
}

# Prints the mtime in epoch seconds. Prints the current time when it cannot be
# read, so an unreadable mtime never makes a lock look ancient.
host_bazel_lock_mtime() {
  # Bazel's test sandbox PATH has perl but no stat.
  local m
  m=$(perl -e 'print((stat $ARGV[0])[9])' "$1" 2>/dev/null) && [[ "$m" =~ ^[0-9]+$ ]] && { printf '%s\n' "$m"; return 0; }
  m=$(stat -f %m "$1" 2>/dev/null) && [[ "$m" =~ ^[0-9]+$ ]] && { printf '%s\n' "$m"; return 0; }
  m=$(stat -c %Y "$1" 2>/dev/null) && [[ "$m" =~ ^[0-9]+$ ]] && { printf '%s\n' "$m"; return 0; }
  date +%s
}

host_bazel_lock_pid_alive() {
  local pid="$1"
  # `kill -0` answers for any process this user may signal — every CI agent
  # on a host runs as one user, so that is the normal case. It reports EPERM
  # for another user's live process, which `ps -p` then resolves; ps is the
  # second opinion rather than the first because a sandboxed test host can
  # deny it process visibility, which would read every holder as dead.
  if kill -0 "$pid" 2>/dev/null; then
    return 0
  fi
  ps -p "$pid" >/dev/null 2>&1
}

# Prints the start time of process $1 (second resolution), or nothing when it
# cannot be read. Recorded in the owner file so a recycled pid is recognised.
host_bazel_lock_pid_start() {
  LC_ALL=C ps -o lstart= -p "$1" 2>/dev/null | sed -e 's/^ *//' -e 's/ *$//'
}

# Returns 0 and prints a reason when the lock at $1 is stale. Age is never a
# reason: a live holder stays exclusive however long it runs.
host_bazel_lock_stale_reason() {
  local lock="$1" now pid boot grace
  now=$(date +%s)
  grace="${CI_HOST_BAZEL_LOCK_OWNERLESS_GRACE_SECS:-30}"
  pid=$(host_bazel_lock_owner_field "$lock" pid)
  if [[ -z "$pid" ]]; then
    # mkdir happened but the owner file is not written yet (a few ms window),
    # or the holder died in that window. Give it a grace period.
    if (( now - $(host_bazel_lock_mtime "$lock") > grace )); then
      printf 'no owner file after %ss\n' "$grace"
      return 0
    fi
    return 1
  fi
  if ! host_bazel_lock_pid_alive "$pid"; then
    printf 'holder pid %s is gone\n' "$pid"
    return 0
  fi
  # The pid is live, but it may be an unrelated process that inherited the pid
  # of a holder that died without releasing. The owner file records the
  # holder's start time; a different start time means the holder is gone.
  local start current_start
  start=$(host_bazel_lock_owner_field "$lock" start)
  if [[ -n "$start" ]]; then
    current_start=$(host_bazel_lock_pid_start "$pid")
    if [[ -n "$current_start" && "$current_start" != "$start" ]]; then
      printf 'holder pid %s was recycled (started %s, now %s)\n' "$pid" "$start" "$current_start"
      return 0
    fi
  fi
  boot=$(host_bazel_lock_owner_field "$lock" boot)
  local current_boot
  current_boot=$(host_bazel_lock_boot_id)
  if [[ -n "$boot" && -n "$current_boot" && "$boot" != "$current_boot" ]]; then
    printf 'taken before the last reboot (boot %s, now %s)\n' "$boot" "$current_boot"
    return 0
  fi
  return 1
}

# Reclaim a lock that a waiter judged stale. The judgement is not trusted:
# the lock path is reusable, so by the time this runs the directory there may
# be a different, live lock. The check is redone on the current occupant while
# holding the reclaim flock, and only a lock that is stale right now is
# removed. If another waiter holds the flock this returns at once and the
# caller retries on its next poll.
host_bazel_lock_reclaim() {
  local lock="$1" root
  root=$(host_bazel_lock_root)
  if ! command -v perl >/dev/null 2>&1; then
    return 0
  fi
  mkdir -p "$root"
  # $^F=255 keeps the flock'd descriptor open across exec, so the re-run below
  # holds the lock for exactly its own lifetime. Exit 1 means busy.
  perl -e '$^F = 255; open(my $f, ">>", shift) or exit 2; flock($f, 6) or exit 1; exec @ARGV or exit 3'     "${root}/reclaim.flock" bash "$HOST_BAZEL_LOCK_LIB" --reclaim-locked "$lock" || true
}

# Runs with the reclaim flock held (see host_bazel_lock_reclaim).
host_bazel_lock_reclaim_locked() {
  local lock="$1" root aside reason
  root=$(host_bazel_lock_root)
  if [[ -d "$lock" ]] && reason=$(host_bazel_lock_stale_reason "$lock"); then
    aside="${root}/stale.$$.${RANDOM}"
    if mv "$lock" "$aside" 2>/dev/null; then
      host_bazel_lock_log "reclaimed stale lock (${reason}); previous owner: $(tr '\n' ' ' < "${aside}/owner" 2>/dev/null)"
      rm -rf "$aside"
    fi
  fi
}

host_bazel_lock_describe_holder() {
  local lock="$1"
  printf '%s (pid %s, job %s, since %s)' \
    "$(host_bazel_lock_owner_field "$lock" label)" \
    "$(host_bazel_lock_owner_field "$lock" pid)" \
    "$(host_bazel_lock_owner_field "$lock" job)" \
    "$(host_bazel_lock_owner_field "$lock" acquired_at)"
}

# Echoes a note when the live holder has run longer than the long-hold
# threshold. Diagnostic only; the holder keeps the lock.
host_bazel_lock_long_hold_note() {
  local lock="$1" acquired held limit
  limit="${CI_HOST_BAZEL_LOCK_LONG_HOLD_SECS:-10800}"
  acquired=$(host_bazel_lock_owner_field "$lock" acquired)
  [[ -n "$acquired" ]] || return 0
  held=$(( $(date +%s) - acquired ))
  if (( held > limit )); then
    printf ' — held %ss, over the %ss long-hold mark; not reclaimed while its process lives, cancel the job in Buildkite if it is wedged' "$held" "$limit"
  fi
}

host_bazel_lock_acquire() {
  host_bazel_lock_take "$$" 1
}

# Takes the lock on behalf of process $1 (the pid whose death frees it). With
# $2 = 1 an EXIT trap releases it; Buildkite agent hooks pass 0 and the
# job-bootstrap pid, and release from a pre-exit hook instead.
host_bazel_lock_take() {
  local holder="$1" install_trap="$2"
  host_bazel_lock_enabled || return 0
  # Idempotent: a step that sources ci-env.sh twice, or a child script that
  # sources it while the parent already holds the lock, must not deadlock
  # on itself.
  if [[ -n "${HOST_BAZEL_LOCK_HELD:-}" ]]; then
    return 0
  fi

  local root lock poll report start now announced=0 last_report waited
  root=$(host_bazel_lock_root)
  lock=$(host_bazel_lock_path)
  poll="${CI_HOST_BAZEL_LOCK_POLL_SECS:-5}"
  report="${CI_HOST_BAZEL_LOCK_REPORT_SECS:-60}"
  mkdir -p "$root"
  start=$(date +%s)
  last_report=$start

  # Outermost script name as a fallback label outside Buildkite (bash 3.2 on
  # macOS has no negative array indices).
  local label
  label="${BUILDKITE_PIPELINE_SLUG:-local}/${BUILDKITE_BUILD_NUMBER:-0} ${BUILDKITE_STEP_KEY:-${BUILDKITE_LABEL:-$(basename "${BASH_SOURCE[${#BASH_SOURCE[@]}-1]}")}}"

  while :; do
    if mkdir "$lock" 2>/dev/null; then
      {
        printf 'pid=%s\n' "$holder"
        printf 'start=%s\n' "$(host_bazel_lock_pid_start "$holder")"
        printf 'boot=%s\n' "$(host_bazel_lock_boot_id)"
        printf 'acquired=%s\n' "$(date +%s)"
        printf 'acquired_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf 'job=%s\n' "${BUILDKITE_JOB_ID:-}"
        printf 'agent=%s\n' "${BUILDKITE_AGENT_NAME:-}"
        printf 'label=%s\n' "$label"
      } > "${lock}/owner"
      export HOST_BAZEL_LOCK_HELD=1
      # Any step that installs its own EXIT trap after this point must call
      # host_bazel_lock_release from it. If it forgets, the lock is reclaimed
      # by the next waiter as soon as this pid is gone — slower, not broken.
      if [[ "$install_trap" == 1 ]]; then
        trap host_bazel_lock_release EXIT
      fi
      waited=$(( $(date +%s) - start ))
      if (( waited > 0 )); then
        host_bazel_lock_log "acquired ${lock} after waiting ${waited}s"
      else
        host_bazel_lock_log "acquired ${lock}"
      fi
      return 0
    fi

    if [[ -d "$lock" ]] && host_bazel_lock_stale_reason "$lock" >/dev/null; then
      host_bazel_lock_reclaim "$lock"
      sleep 0.2
      continue
    fi

    now=$(date +%s)
    if (( announced == 0 )); then
      echo "--- [host-bazel-lock] waiting: another heavy Bazel job holds this host"
      host_bazel_lock_log "held by $(host_bazel_lock_describe_holder "$lock")$(host_bazel_lock_long_hold_note "$lock")"
      host_bazel_lock_log "one heavy Bazel job per Mac host at a time — see .buildkite/mac-agents-runbook.md"
      announced=1
    elif (( now - last_report >= report )); then
      host_bazel_lock_log "still waiting ($((now - start))s), held by $(host_bazel_lock_describe_holder "$lock")$(host_bazel_lock_long_hold_note "$lock")"
      last_report=$now
    fi
    sleep "$poll"
  done
}

host_bazel_lock_release() {
  [[ -n "${HOST_BAZEL_LOCK_HELD:-}" ]] || return 0
  host_bazel_lock_release_pid "$$"
  unset HOST_BAZEL_LOCK_HELD
}

# Removes the lock if process $1 owns it. Only the owning process may release;
# a child that inherited HOST_BAZEL_LOCK_HELD through the environment must not.
host_bazel_lock_release_pid() {
  local lock
  lock=$(host_bazel_lock_path)
  if [[ "$(host_bazel_lock_owner_field "$lock" pid)" == "$1" ]]; then
    rm -rf "$lock"
    host_bazel_lock_log "released ${lock}"
  fi
}

# Executed (not sourced) by host_bazel_lock_reclaim, under the reclaim flock.
if [[ "${BASH_SOURCE[0]}" == "$0" && "${1:-}" == "--reclaim-locked" ]]; then
  host_bazel_lock_reclaim_locked "$2"
fi
