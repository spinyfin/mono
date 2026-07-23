//! Process-identity trust roots and RPC authorization.
//!
//! The engine trusts two pids: the macOS app ([`ServerState::current_app_pid`])
//! and the Boss session shell ([`ServerState::current_boss_pid`]). Every
//! privileged RPC is admitted by walking the calling peer's process tree
//! against those roots — see [`RpcTier`] and
//! [`ServerState::authorize_rpc`] for the tiers and their semantics.
//! Also holds [`PidFileGuard`], which cleans up the engine's own pid file.
//!
//! Split out of `app.rs`; pure structural move — no behavioural change.

use super::*;

pub(super) struct PidFileGuard {
    pub(super) path: String,
    pub(super) pid: u32,
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(_) => return,
        };

        let parsed = content.trim().parse::<u32>().ok();
        if parsed == Some(self.pid) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Authorization tier for a frontend RPC.
///
/// - `User`: any local client (the human's `boss` CLI, the macOS app,
///   read-only callers, and any documented `bossctl` verb that has no
///   privileged side effect — e.g. `workspace summary`).
/// - `AppOrBoss`: privileged operations the app and the Boss session
///   may both invoke. This is the right level for the imperative
///   `bossctl` verbs (`probe`, `agents stop`, `agents transcript`,
///   `work cancel`): the human runs them from wherever they happen
///   to be — Boss pane, app shell, *inside a worker pane*, or a
///   plain terminal that descends from neither trust root. The
///   admission rule is "descendant of app or Boss, OR not a
///   descendant of any registered worker pane" — workers are the
///   only sibling-process adversary in the V2 threat model, so
///   excluding worker subtrees is sufficient. Earlier revisions
///   gated strictly on app/Boss subtree membership and locked the
///   coordinator out whenever it ran from a shell outside both
///   (e.g. a tmux pane started before the app launched).
/// - `BossOnly`: reserved for future control verbs that must reject
///   worker-pane callers. No live verb uses this tier today; the
///   `bossctl` verbs that previously gated on it (`probe_run`,
///   `tail_run_transcript`, `stop_run`) were all downgraded after
///   they kept locking the coordinator out of legitimate calls. Keep
///   the tier so any future verb can opt into it explicitly rather
///   than accidentally inheriting it.
/// - `Worker`: the odd one out — a *classification* rather than a
///   privilege level. `authorize_rpc(Worker, peer)` asks "is this
///   peer a live worker session?", i.e. does its process ancestry
///   contain a registered worker pane shell. It is the inverse of the
///   worker-exclusion clause the two tiers above use as a fallback,
///   and it is what admits a connection to the worker verb policy
///   ([`boss_engine_worker_policy`]) instead of the unconditional
///   `User` tier. Unlike the other tiers it is deliberately *not*
///   permissive in test mode: "no trust roots configured" must not
///   mean "everything is a worker", or an engine started without the
///   macOS app would confine the coordinator to worker tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcTier {
    User,
    AppOrBoss,
    BossOnly,
    Worker,
}

/// How a frontend connection's socket peer was classified.
///
/// Resolved once per connection rather than per request: the `boss` CLI
/// opens a fresh connection per invocation and the macOS app holds one for
/// its lifetime, so a per-request ancestry walk would buy nothing and cost a
/// `proc_pidinfo` chain on every app request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerClass {
    /// The peer descends from a registered worker pane shell. `run_id` is
    /// the execution that shell is running — the same id the worker knows as
    /// `BOSS_RUN_ID`. Carried here (rather than just a `bool`) because the
    /// verb gate logs it, and because the read verbs coming in later tasks
    /// scope on it.
    Worker { run_id: String },
    /// Everything else: the macOS app, the Boss pane, a plain terminal, a
    /// connection with no local peer pid at all.
    ///
    /// Note the asymmetry this implies, which the design accepts: a worker
    /// whose lineage to its pane shell has been broken (double-fork,
    /// reparenting) classifies here and keeps the old unconstrained `User`
    /// tier, because nothing distinguishes it from the coordinator's own
    /// shell. Closing *that* would require rejecting the coordinator too.
    /// What the design's "strictly closed" lean governs is the case where a
    /// peer *is* classified as a worker but resolution then fails — that
    /// path refuses rather than falling back (see `app::proposals`).
    Other,
}

impl PeerClass {
    /// The execution this connection belongs to, if it is a worker.
    pub fn worker_run_id(&self) -> Option<&str> {
        match self {
            PeerClass::Worker { run_id } => Some(run_id),
            PeerClass::Other => None,
        }
    }

    pub fn is_worker(&self) -> bool {
        matches!(self, PeerClass::Worker { .. })
    }
}

impl ServerState {
    /// Set the Boss session's shell pid (the second trust root). Any
    /// peer whose process tree includes this pid as an ancestor will
    /// satisfy `BossOnly` / `AppOrBoss` checks.
    pub fn set_boss_pid(&self, pid: libc::pid_t) {
        *self.boss_pid.lock().expect("boss_pid mutex poisoned") = Some(pid);
    }

    pub fn current_boss_pid(&self) -> Option<libc::pid_t> {
        *self.boss_pid.lock().expect("boss_pid mutex poisoned")
    }

    /// The pid currently trusted as the macOS app (the `RegisterAppSession`
    /// / RPC-auth trust root). `None` in test mode (no trust root).
    pub fn current_app_pid(&self) -> Option<libc::pid_t> {
        *self.app_pid.lock().expect("app_pid mutex poisoned")
    }

    /// Re-pin the app trust root. Called when a relaunched app
    /// re-registers against a surviving engine with a new pid — the
    /// old pid belongs to a now-dead process, so the live app becomes
    /// the trust root for subsequent engine↔app RPC authorization.
    pub(super) fn set_app_pid(&self, pid: libc::pid_t) {
        *self.app_pid.lock().expect("app_pid mutex poisoned") = Some(pid);
    }

    /// Authorize a peer-pid against an RPC tier. Walks up the peer's
    /// process tree (bounded depth) looking for `app_pid` or
    /// `boss_pid` registered as a trust root, with a worker-exclusion
    /// fallback for the `AppOrBoss` and `BossOnly` tiers.
    ///
    /// Returns `true` when `tier == User`, when the trust root is
    /// `None` (test mode), when an ancestor of `peer_pid` matches a
    /// relevant trust root, or — for `AppOrBoss` — when the peer is
    /// not a descendant of any registered worker shell.
    ///
    /// `AppOrBoss` semantics: workers are the only sibling-process
    /// adversary in the V2 threat model, so the gate is "trusted
    /// subtree, OR not a worker descendant". This matters for the
    /// live coordinator: the Boss session may run from a shell that
    /// descends from neither the app nor the registered Boss pid
    /// (e.g. a tmux pane started before the macOS app launched), and
    /// the strict subtree-only check kept rejecting `bossctl agents
    /// transcript`, `bossctl probe`, `bossctl agents stop`, etc. for
    /// the case the work item names. Worker descendants stay rejected
    /// by the fallback's worker-pid exclusion.
    ///
    /// `BossOnly` semantics: the design names the registered Boss
    /// session's shell pid as the canonical trust root. When that pid
    /// is missing (the macOS app hasn't yet sent
    /// `RegisterBossSession`, or runs that don't set up a Boss pane
    /// at all), we fall back to "descendant of the app, not a
    /// descendant of any registered worker shell". Workers each run
    /// in their own libghostty pane whose shell pid is recorded in
    /// `WorkerRegistry`; a `bossctl` invoked from inside a worker
    /// pane therefore descends from a registered worker pid, while
    /// the same call from the Boss pane (or directly under the app
    /// shell) does not. That distinction is enough to keep workers
    /// out of `BossOnly` even with an unregistered Boss pid.
    pub fn authorize_rpc(&self, tier: RpcTier, peer_pid: Option<libc::pid_t>) -> bool {
        if matches!(tier, RpcTier::User) {
            return true;
        }
        // Evaluated before the trust-root shortcut below, deliberately.
        // `Worker` asks a question about the *worker registry*, not about
        // the app/Boss trust roots, so the "no trust roots => permissive"
        // escape hatch does not apply: answering `true` there would classify
        // the coordinator's own shell as a worker on any engine started
        // without the macOS app.
        if matches!(tier, RpcTier::Worker) {
            return self.classify_peer(peer_pid).is_worker();
        }
        let app_pid = self.current_app_pid();
        let boss_pid = self.current_boss_pid();
        if app_pid.is_none() && boss_pid.is_none() {
            // No trust roots are configured at all — treat as
            // permissive (used by in-process tests).
            return true;
        }
        let Some(peer_pid) = peer_pid else {
            return false;
        };
        match tier {
            // Both handled above; repeated here only to keep the match
            // exhaustive without a wildcard arm.
            RpcTier::User | RpcTier::Worker => true,
            RpcTier::AppOrBoss => {
                // Fast path: peer descends from a known trust root. Common
                // case is the human running bossctl from the Boss pane
                // (boss_pid descendant), the app shell (app_pid
                // descendant), or a worker pane (also app_pid descendant
                // — workers are siblings under the app).
                let trust_set: Vec<libc::pid_t> = [app_pid, boss_pid].into_iter().flatten().collect();
                if !trust_set.is_empty() && is_descendant_of_any(peer_pid, &trust_set) {
                    return true;
                }
                // Fallback: the coordinator session may run from a shell
                // that descends from neither trust root — e.g. a plain
                // terminal, or a tmux pane started before the macOS app
                // launched, or a separate Claude Code instance steering
                // the engine. The earlier subtree-only gate rejected
                // those legitimate calls. Admit any caller that is *not*
                // a descendant of a registered worker pane shell.
                // Workers are the only sibling-process adversary in the
                // V2 threat model (`docs/designs/main.md` §"Worker
                // isolation"), so excluding worker subtrees is enough to
                // keep `bossctl agents transcript` and friends from
                // leaking one worker's transcript to another worker.
                let worker_pids = self.worker_registry.registered_pids();
                !is_descendant_of_any(peer_pid, &worker_pids)
            }
            RpcTier::BossOnly => {
                if let Some(boss_pid) = boss_pid {
                    return is_descendant_of_any(peer_pid, &[boss_pid]);
                }
                // No Boss pid registered. Trust descendants of the
                // app, but reject anyone descending from a registered
                // worker pane shell — those are workers, not the
                // Boss session.
                let Some(app_pid) = app_pid else {
                    return false;
                };
                if !is_descendant_of_any(peer_pid, &[app_pid]) {
                    return false;
                }
                let worker_pids = self.worker_registry.registered_pids();
                if worker_pids.is_empty() {
                    return true;
                }
                !is_descendant_of_any(peer_pid, &worker_pids)
            }
        }
    }

    /// Classify a frontend connection's socket peer.
    ///
    /// This is *verified* identity: the run id comes from walking the peer's
    /// process ancestry to a pid the engine itself registered at pane spawn
    /// ([`crate::worker_registry::WorkerRegistry::register`]), never from
    /// anything the caller supplied. A worker therefore cannot present as a
    /// different run, and cannot present as the coordinator.
    ///
    /// A peer with no pid — a socket whose `SO_PEERCRED`/`LOCAL_PEERPID`
    /// lookup failed, which in practice means a non-local connection — is
    /// [`PeerClass::Other`]. That is the v1 remote-worker position from the
    /// design's non-goals: remote SSH workers cannot present a local peer pid,
    /// so they get no worker tier (and, at the proposal verbs, an explicit
    /// `no_local_peer` refusal rather than silent admission).
    pub fn classify_peer(&self, peer_pid: Option<libc::pid_t>) -> PeerClass {
        let Some(peer_pid) = peer_pid else {
            return PeerClass::Other;
        };
        match self.worker_registry.lookup_with_ancestor_walk(peer_pid) {
            Some(run_id) => PeerClass::Worker { run_id },
            None => PeerClass::Other,
        }
    }

    /// Decide whether `request` may run on a connection classified as
    /// `peer_class`, returning the typed refusal when it may not.
    ///
    /// Two gates, in order:
    ///
    /// 1. **The flag.** While `worker_rpc_tier` is off, every connection
    ///    keeps the historical unconditional `RpcTier::User` behaviour, so a
    ///    rollback is a flag flip rather than a redeploy.
    /// 2. **The classification.** Only worker-classified peers are subject to
    ///    the verb policy; the app, the Boss pane, and plain terminals are
    ///    untouched, which is the "a human/coordinator shell not descended
    ///    from a worker pid is unaffected" property.
    pub(super) fn worker_tier_denial(
        &self,
        peer_class: &PeerClass,
        request: &FrontendRequest,
    ) -> Option<WorkerTierDenial> {
        if !peer_class.is_worker() {
            return None;
        }
        if !self.feature_flags.is_enabled("worker_rpc_tier") {
            return None;
        }
        worker_verb_decision(request).denial().cloned()
    }
}
