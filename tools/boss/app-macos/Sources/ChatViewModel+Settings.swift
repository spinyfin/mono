import Foundation

/// Engine settings, health, driver-quota, and dispatch-pause actions.
extension ChatViewModel {
    /// Whether the engine currently reports automations paused.
    /// Derived from `engineHealthIssues` — the engine is the source of
    /// truth; the toolbar does not keep a parallel local flag.
    var isAutomationPaused: Bool {
        engineHealthIssues.contains { $0.kind == EngineHealthIssue.automationPausedKind }
    }

    /// Health issues that still render as the chrome banner.
    /// `automation_paused` stays in `engineHealthIssues` (and Settings)
    /// but is not a banner kind.
    var bannerHealthIssues: [EngineHealthIssue] {
        engineHealthIssues.filter { $0.kind != EngineHealthIssue.automationPausedKind }
    }

    /// Ask the engine for the current per-installation settings
    /// snapshot. Called by the Settings window on appear, and on every
    /// (re)connect (see the `.connected` arm of `handle`) so
    /// `tmuxHostingEnabled` has an answer for the Workers grid badge
    /// without requiring the operator to open Settings first.
    func refreshSettings() {
        engine.sendGetSettings()
    }

    /// Whether `workers.tmux_hosting` is currently on — the operator-facing
    /// switch controlling whether worker panes are hosted in durable tmux
    /// sessions or the legacy app-owned pty path. `false` (including while
    /// `engineSettings` hasn't loaded yet) is the safe default: the Workers
    /// grid's "legacy hosting" badge should show whenever this cannot
    /// positively confirm tmux hosting is on, never the other way around.
    var tmuxHostingEnabled: Bool {
        engineSettings.first { $0.key == "workers.tmux_hosting" }?.enabled ?? false
    }

    /// Ask the engine for a fresh engine-health snapshot. Also called
    /// on every reconnect from the `.connected` arm of `handle`; this
    /// wrapper exists so the Settings pane can re-poll on appear
    /// without exposing the private `engine` field.
    func refreshEngineHealth() {
        engine.sendGetEngineHealth()
    }

    /// Ask the engine for the current driver traffic split. Called by the
    /// Settings window on appear so the control reflects the persisted
    /// value rather than whatever it last happened to show.
    func refreshDriverTrafficSplit() {
        engine.sendGetDriverTrafficSplit()
    }

    /// Ask the engine for the per-driver provider quota snapshot. Called on
    /// Settings appear (cached, so usually free) and by the pane's Refresh
    /// button (`refresh: true`).
    ///
    /// Fire-and-forget: the pane renders whatever it already has while this
    /// is in flight, so opening Settings is never delayed by a fetch.
    func refreshDriverQuota(force: Bool = false) {
        isRefreshingDriverQuota = true
        engine.sendGetDriverQuotaUsage(refresh: force)
    }

    /// Move `driver`'s share to `value`, letting the other two absorb the
    /// difference (see `DriverTrafficSplit.adjusting(_:to:)`), and send the
    /// resulting split.
    ///
    /// Local state is patched optimistically here, unlike the settings
    /// controls that let the engine clamp: the engine never repairs a split,
    /// it either stores this exact one or rejects it, and `adjusting` cannot
    /// produce a rejectable split. Patching first keeps the steppers
    /// responsive across a round trip; the echoed
    /// `driver_traffic_split_result` then confirms it.
    func setDriverTrafficShare(_ driver: DriverSlug, to value: Int) {
        let next = driverTrafficSplit.adjusting(driver, to: value)
        guard next != driverTrafficSplit else { return }
        driverTrafficSplit = next
        engine.sendSetDriverTrafficSplit(next)
    }

    /// User-initiated resume from the `dispatch_paused` health-banner
    /// issue. Drives the same `SetDispatchPaused { paused: false }`
    /// RPC `bossctl dispatch resume` uses; the engine owns the actual
    /// state change, this is a thin trigger. The engine has no push
    /// event for a health-state change, so this re-polls
    /// `get_engine_health` right behind the resume request (requests
    /// on one socket are processed in order) so the banner clears
    /// without waiting for the next reconnect.
    func resumeDispatch() {
        engine.sendSetDispatchPaused(paused: false)
        engine.sendGetEngineHealth()
    }

    /// Toggle the global automation pause. Drives the same
    /// `SetAutomationPaused` RPC `bossctl automation pause` /
    /// `bossctl automation resume` use; the engine owns the state and
    /// broadcasts a fresh health report. Local `engineHealthIssues`
    /// is not flipped here — the next `engine_health_result` is the
    /// source of truth. Pausing supplies a fixed operator reason
    /// because the engine rejects an anonymous pause; no confirmation
    /// dialog, this is a cheap reversible action.
    func toggleAutomationPaused() {
        if isAutomationPaused {
            engine.sendSetAutomationPaused(paused: false)
        } else {
            engine.sendSetAutomationPaused(
                paused: true,
                reason: AutomationPauseControl.toolbarReason
            )
        }
        engine.sendGetEngineHealth()
    }

    /// Toggle one per-installation setting. Optimistically patches the
    /// cached snapshot so the UI feels instantaneous; the engine's
    /// `setting_set` echo reconciles state once the on-disk write
    /// returns.
    func setEngineSetting(key: String, enabled: Bool) {
        if let idx = engineSettings.firstIndex(where: { $0.key == key }) {
            let prior = engineSettings[idx]
            engineSettings[idx] = EngineSetting(
                key: prior.key,
                description: prior.description,
                defaultEnabled: prior.defaultEnabled,
                enabled: enabled
            )
        }
        engine.sendSetSetting(key: key, enabled: enabled)
    }
}
