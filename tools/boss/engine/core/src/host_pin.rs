//! Admission check for an operator-named host pin (`bossctl work start
//! --host` / `bossctl agents launch --host`).
//!
//! The point of `--host` is to *prove something about that host*: an
//! operator who has just repaired a remote's credentials wants a dispatch
//! that either lands there or fails saying why. Host selection
//! ([`crate::host_scheduling::select_host`]) already routes a pinned
//! execution to its host and nowhere else, but it reaches that verdict
//! asynchronously, after a `ready` row exists — a pin at an unusable host
//! would sit queued, indistinguishable from ordinary scheduling latency.
//!
//! So the pin is validated *up front*, at the `RequestExecution`
//! chokepoint, before any row is created: [`validate_host_pin`] answers
//! "would this pin be dispatchable right now?" and its refusal is
//! reported verbatim to the caller, which creates nothing. That is what
//! makes `--host` fail loudly with no queued residue instead of falling
//! back to `local`.
//!
//! This does not weaken the health gate — it reads the same
//! `hosts.enabled` / `consecutive_failures` state the dispatch-time
//! breaker writes ([`crate::host_registry::record_host_dispatch_failure`])
//! and surfaces its verdict early. A host that goes bad *between* this
//! check and the spawn is still caught by the gate at dispatch time, and
//! a pinned execution still never falls back to another host.

use crate::host_registry::HOST_HEALTH_FAILURE_THRESHOLD;
use crate::work::WorkDb;

/// The local host is never slot-gated, mirroring
/// `ExecutionCoordinator::select_host_for_execution`: the worker pool
/// already bounds local concurrency, and `hosts.local.pool_size` defaults
/// to 1, so gating on it too would refuse a local pin whenever any single
/// local worker is running.
const LOCAL_HOST_ID: &str = "local";

/// Why an operator-named host cannot take this dispatch. Each variant
/// renders to operator-facing text naming the specific reason — never a
/// generic "couldn't place it", which is what sends an operator hunting
/// through logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPinRefusal {
    /// No such host in the registry. Carries every known id so the
    /// operator can spot the typo without a second command.
    Unknown { host_id: String, known: Vec<String> },
    /// Registered but turned off by an operator (`bossctl hosts disable`).
    Disabled {
        host_id: String,
        last_error_text: Option<String>,
    },
    /// Registered, turned off by the dispatch-time health breaker after
    /// [`HOST_HEALTH_FAILURE_THRESHOLD`] consecutive failures. Reported
    /// separately from a plain operator disable because the fix differs:
    /// this one wants `bossctl hosts probe <id>` first.
    Unhealthy {
        host_id: String,
        consecutive_failures: i64,
        last_error_text: Option<String>,
    },
    /// Enabled and healthy, but every slot in its pool is busy.
    NoFreeSlots {
        host_id: String,
        pool_size: i64,
        active_runs: i64,
    },
}

impl std::fmt::Display for HostPinRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown { host_id, known } => {
                let known = if known.is_empty() {
                    "(none registered)".to_owned()
                } else {
                    known.join(", ")
                };
                write!(
                    f,
                    "unknown host '{host_id}' — known hosts: {known}. \
                     Register it with `bossctl hosts add`, or run `bossctl hosts list` to check the spelling"
                )
            }
            Self::Disabled {
                host_id,
                last_error_text,
            } => {
                write!(f, "host '{host_id}' is disabled")?;
                if let Some(text) = last_error_text {
                    write!(f, " (last error: {text})")?;
                }
                write!(
                    f,
                    " — re-enable it with `bossctl hosts enable {host_id}`. \
                     `--host` never falls back to another host, so nothing was dispatched"
                )
            }
            Self::Unhealthy {
                host_id,
                consecutive_failures,
                last_error_text,
            } => {
                write!(
                    f,
                    "host '{host_id}' failed its health gate: auto-disabled after \
                     {consecutive_failures} consecutive dispatch failures"
                )?;
                if let Some(text) = last_error_text {
                    write!(f, " (last error: {text})")?;
                }
                write!(
                    f,
                    " — repair it and run `bossctl hosts probe {host_id}`. \
                     `--host` never falls back to another host, so nothing was dispatched"
                )
            }
            Self::NoFreeSlots {
                host_id,
                pool_size,
                active_runs,
            } => write!(
                f,
                "host '{host_id}' has no free slot ({active_runs}/{pool_size} in use) — \
                 wait for one of its runs to finish, or check them with `bossctl agents list`. \
                 `--host` never falls back to another host, so nothing was dispatched"
            ),
        }
    }
}

/// Would a dispatch pinned to `host_id` be placeable right now?
///
/// Checks, in the order an operator would ask them: does the host exist,
/// is it usable (enabled / past its health gate), and does it have room.
/// Capability requirements are deliberately *not* checked — a pin is the
/// design's documented escape hatch from the capability filter, and
/// [`crate::host_scheduling::select_host`] skips that filter for pinned
/// executions too, so checking it here would refuse dispatches the
/// scheduler would happily place.
///
/// Errors from the registry read propagate as [`anyhow::Error`]: a pin we
/// cannot evaluate must refuse rather than optimistically proceed.
pub fn validate_host_pin(work_db: &WorkDb, host_id: &str) -> anyhow::Result<Result<(), HostPinRefusal>> {
    let Some(host) = work_db.get_host(host_id)? else {
        let known = work_db.list_hosts()?.into_iter().map(|h| h.id).collect();
        return Ok(Err(HostPinRefusal::Unknown {
            host_id: host_id.to_owned(),
            known,
        }));
    };

    if !host.enabled {
        // The health breaker auto-disables, so "disabled with a tripped
        // counter" is the unhealthy case and "disabled with a clean
        // counter" is an operator decision. Both refuse; they differ only
        // in what the operator should do next.
        let refusal = if host.consecutive_failures >= HOST_HEALTH_FAILURE_THRESHOLD {
            HostPinRefusal::Unhealthy {
                host_id: host.id.clone(),
                consecutive_failures: host.consecutive_failures,
                last_error_text: host.last_error_text.clone(),
            }
        } else {
            HostPinRefusal::Disabled {
                host_id: host.id.clone(),
                last_error_text: host.last_error_text.clone(),
            }
        };
        return Ok(Err(refusal));
    }

    if host.id != LOCAL_HOST_ID {
        let active_runs = *work_db.active_runs_per_host()?.get(&host.id).unwrap_or(&0);
        if active_runs >= host.pool_size {
            return Ok(Err(HostPinRefusal::NoFreeSlots {
                host_id: host.id.clone(),
                pool_size: host.pool_size,
                active_runs,
            }));
        }
    }

    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::params;

    use super::*;

    fn open_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory work db")
    }

    /// Plant an `active` run attributed to `host_id`, the shape
    /// `active_runs_per_host` counts.
    fn insert_active_run(db: &WorkDb, id: &str, host_id: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_executions
                 (id, work_item_id, kind, status, repo_remote_url, created_at)
             VALUES (?1, 'wi-1', 'chore_implementation', 'running',
                     'https://github.com/test/repo', '100')",
            params![format!("exec-for-{id}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_runs
                 (id, execution_id, agent_id, status, created_at, host_id)
             VALUES (?1, ?2, 'agent-1', 'active', '100', ?3)",
            params![id, format!("exec-for-{id}"), host_id],
        )
        .unwrap();
    }

    fn refusal(db: &WorkDb, host_id: &str) -> HostPinRefusal {
        validate_host_pin(db, host_id)
            .expect("registry read")
            .expect_err("expected a refusal")
    }

    #[test]
    fn accepts_the_local_host() {
        let db = open_db();
        assert!(validate_host_pin(&db, "local").unwrap().is_ok());
    }

    #[test]
    fn local_pin_is_not_slot_gated() {
        let db = open_db();
        // `hosts.local.pool_size` is 1 and a local worker is running: the
        // worker pool — not this gate — bounds local concurrency, so the
        // pin must still be accepted.
        insert_active_run(&db, "run-1", "local");
        assert!(validate_host_pin(&db, "local").unwrap().is_ok());
    }

    #[test]
    fn unknown_host_lists_the_known_ids() {
        let db = open_db();
        db.add_host("zakalwe", "zakalwe.local", 2, &[]).unwrap();
        match refusal(&db, "zakalwee") {
            HostPinRefusal::Unknown { host_id, known } => {
                assert_eq!(host_id, "zakalwee");
                assert!(known.contains(&"local".to_owned()), "known hosts: {known:?}");
                assert!(known.contains(&"zakalwe".to_owned()), "known hosts: {known:?}");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        assert!(refusal(&db, "zakalwee").to_string().contains("zakalwe"));
    }

    #[test]
    fn operator_disabled_host_refuses_as_disabled() {
        let db = open_db();
        db.add_host("zakalwe", "zakalwe.local", 2, &[]).unwrap();
        db.set_host_enabled("zakalwe", false).unwrap();
        match refusal(&db, "zakalwe") {
            HostPinRefusal::Disabled { host_id, .. } => assert_eq!(host_id, "zakalwe"),
            other => panic!("expected Disabled, got {other:?}"),
        }
    }

    /// A host the dispatch-time breaker auto-disabled must be named as a
    /// *health* refusal, carrying the failure count and the recorded
    /// error — the gate's verdict reported, not suppressed.
    #[test]
    fn breaker_disabled_host_refuses_as_unhealthy() {
        let db = open_db();
        db.add_host("zakalwe", "zakalwe.local", 2, &[]).unwrap();
        for _ in 0..HOST_HEALTH_FAILURE_THRESHOLD {
            db.record_host_dispatch_failure("zakalwe", "cube: command not found")
                .unwrap();
        }
        match refusal(&db, "zakalwe") {
            HostPinRefusal::Unhealthy {
                host_id,
                consecutive_failures,
                last_error_text,
            } => {
                assert_eq!(host_id, "zakalwe");
                assert_eq!(consecutive_failures, HOST_HEALTH_FAILURE_THRESHOLD);
                assert!(
                    last_error_text.unwrap_or_default().contains("cube: command not found"),
                    "the breaker's recorded cause must reach the operator",
                );
            }
            other => panic!("expected Unhealthy, got {other:?}"),
        }
    }

    /// A degraded-but-still-enabled host (under the breaker threshold) is
    /// still a legal target: ordinary selection would place work there, so
    /// refusing a pin would be stricter than the scheduler.
    #[test]
    fn degraded_but_enabled_host_is_accepted() {
        let db = open_db();
        db.add_host("zakalwe", "zakalwe.local", 2, &[]).unwrap();
        db.record_host_dispatch_failure("zakalwe", "transient ssh timeout")
            .unwrap();
        assert!(validate_host_pin(&db, "zakalwe").unwrap().is_ok());
    }

    #[test]
    fn remote_host_with_every_slot_busy_refuses() {
        let db = open_db();
        db.add_host("zakalwe", "zakalwe.local", 2, &[]).unwrap();
        insert_active_run(&db, "run-1", "zakalwe");
        assert!(
            validate_host_pin(&db, "zakalwe").unwrap().is_ok(),
            "one of two slots busy must still be placeable"
        );
        insert_active_run(&db, "run-2", "zakalwe");
        match refusal(&db, "zakalwe") {
            HostPinRefusal::NoFreeSlots {
                host_id,
                pool_size,
                active_runs,
            } => {
                assert_eq!(host_id, "zakalwe");
                assert_eq!(pool_size, 2);
                assert_eq!(active_runs, 2);
            }
            other => panic!("expected NoFreeSlots, got {other:?}"),
        }
    }

    /// Every refusal must say the flag does not fall back, so the operator
    /// reading only the error knows nothing was dispatched elsewhere.
    #[test]
    fn refusals_state_that_no_fallback_happened() {
        let db = open_db();
        db.add_host("zakalwe", "zakalwe.local", 1, &[]).unwrap();
        db.set_host_enabled("zakalwe", false).unwrap();
        assert!(refusal(&db, "zakalwe").to_string().contains("never falls back"));

        let db = open_db();
        db.add_host("zakalwe", "zakalwe.local", 1, &[]).unwrap();
        insert_active_run(&db, "run-1", "zakalwe");
        assert!(refusal(&db, "zakalwe").to_string().contains("never falls back"));
    }
}
