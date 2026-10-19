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
# Waiters poll. A lock whose holder pid is gone, whose boot id does not match
# the running kernel, or whose age exceeds the safety ceiling is stale and is
# reclaimed by renaming it aside (rename is atomic, so two waiters cannot both
# reclaim the same lock, and nobody can delete a lock that was freshly
# re-acquired by someone else between the check and the removal).
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
#   CI_HOST_BAZEL_LOCK_MAX_AGE_SECS  default 10800 (3h safety ceiling)

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

host_bazel_lock_mtime() {
  stat -f %m "$1" 2>/dev/null || stat -c %Y "$1" 2>/dev/null || printf '0'
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

# Returns 0 and prints a reason when the lock at $1 is stale.
host_bazel_lock_stale_reason() {
  local lock="$1" now pid boot acquired max_age
  now=$(date +%s)
  max_age="${CI_HOST_BAZEL_LOCK_MAX_AGE_SECS:-10800}"
  pid=$(host_bazel_lock_owner_field "$lock" pid)
  if [[ -z "$pid" ]]; then
    # mkdir happened but the owner file is not written yet (a few ms window),
    # or the holder died in that window. Give it a grace period.
    if (( now - $(host_bazel_lock_mtime "$lock") > 30 )); then
      printf 'no owner file after 30s\n'
      return 0
    fi
    return 1
  fi
  if ! host_bazel_lock_pid_alive "$pid"; then
    printf 'holder pid %s is gone\n' "$pid"
    return 0
  fi
  boot=$(host_bazel_lock_owner_field "$lock" boot)
  local current_boot
  current_boot=$(host_bazel_lock_boot_id)
  if [[ -n "$boot" && -n "$current_boot" && "$boot" != "$current_boot" ]]; then
    printf 'taken before the last reboot (boot %s, now %s)\n' "$boot" "$current_boot"
    return 0
  fi
  acquired=$(host_bazel_lock_owner_field "$lock" acquired)
  if [[ -n "$acquired" ]] && (( now - acquired > max_age )); then
    printf 'older than the %ss ceiling (held %ss)\n' "$max_age" "$((now - acquired))"
    return 0
  fi
  return 1
}

# Move a stale lock aside atomically and delete it. Only one of several
# concurrent waiters can win the rename; the others simply retry mkdir.
host_bazel_lock_reclaim() {
  local lock="$1" reason="$2" aside
  aside="$(host_bazel_lock_root)/stale.$$.${RANDOM}"
  if mv "$lock" "$aside" 2>/dev/null; then
    host_bazel_lock_log "reclaimed stale lock (${reason}); previous owner: $(tr '\n' ' ' < "${aside}/owner" 2>/dev/null)"
    rm -rf "$aside"
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

host_bazel_lock_acquire() {
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
        printf 'pid=%s\n' "$$"
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
      trap host_bazel_lock_release EXIT
      waited=$(( $(date +%s) - start ))
      if (( waited > 0 )); then
        host_bazel_lock_log "acquired ${lock} after waiting ${waited}s"
      else
        host_bazel_lock_log "acquired ${lock}"
      fi
      return 0
    fi

    local reason
    if reason=$(host_bazel_lock_stale_reason "$lock"); then
      host_bazel_lock_reclaim "$lock" "$reason"
      continue
    fi

    now=$(date +%s)
    if (( announced == 0 )); then
      echo "--- [host-bazel-lock] waiting: another heavy Bazel job holds this host"
      host_bazel_lock_log "held by $(host_bazel_lock_describe_holder "$lock")"
      host_bazel_lock_log "one heavy Bazel job per Mac host at a time — see .buildkite/mac-agents-runbook.md"
      announced=1
    elif (( now - last_report >= report )); then
      host_bazel_lock_log "still waiting ($((now - start))s), held by $(host_bazel_lock_describe_holder "$lock")"
      last_report=$now
    fi
    sleep "$poll"
  done
}

host_bazel_lock_release() {
  [[ -n "${HOST_BAZEL_LOCK_HELD:-}" ]] || return 0
  local lock
  lock=$(host_bazel_lock_path)
  # Only the acquiring process may release; a child that inherited
  # HOST_BAZEL_LOCK_HELD through the environment must not.
  if [[ "$(host_bazel_lock_owner_field "$lock" pid)" == "$$" ]]; then
    rm -rf "$lock"
    host_bazel_lock_log "released ${lock}"
  fi
  unset HOST_BAZEL_LOCK_HELD
}
