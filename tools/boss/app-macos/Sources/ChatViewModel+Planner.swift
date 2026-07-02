import Foundation

extension ChatViewModel {
    // MARK: Planner review/release/undo
    // (design: tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md, task 10)

    /// Planner runs for a project, newest first — as returned by the engine.
    func plannerRuns(forProjectID projectID: String) -> [PlannerRun] {
        plannerRunsByProjectID[projectID] ?? []
    }

    /// The most recent run, if any has been fetched. The kanban accessory
    /// keys its icon/tint off this row.
    func latestPlannerRun(forProjectID projectID: String) -> PlannerRun? {
        plannerRuns(forProjectID: projectID).first
    }

    /// Ask the engine for this project's planner-run audit trail. Safe to
    /// call repeatedly (e.g. from `onAppear`) — the reply simply replaces
    /// the cached array.
    func refreshPlannerRuns(projectID: String) {
        guard isConnected else { return }
        engine.sendListPlannerRuns(projectId: projectID)
    }

    /// Release a project's staged auto-populate batch: flips `autostart =
    /// true` on every task from its live `staged` planner run so the
    /// dispatcher picks them up on its next pass.
    func releaseProject(projectID: String) {
        guard !plannerActionInFlightProjectIDs.contains(projectID) else { return }
        plannerActionInFlightProjectIDs.insert(projectID)
        engine.sendReleaseProject(projectId: projectID)
    }

    /// Undo `runID`'s batch: the engine deletes the still-untouched staged
    /// tasks and clears the run's idempotency gate. Tasks already released
    /// and dispatched are preserved, not deleted, and reported back.
    func unpopulateProject(projectID: String, runID: String) {
        guard !plannerActionInFlightProjectIDs.contains(projectID) else { return }
        plannerActionInFlightProjectIDs.insert(projectID)
        engine.sendUnpopulateProject(projectId: projectID, runId: runID)
    }

    /// Open the full Planner Run inspector sheet for a project, refreshing
    /// its run history so the sheet never shows stale data.
    func openPlannerInspector(projectID: String) {
        plannerInspectorProjectID = projectID
        refreshPlannerRuns(projectID: projectID)
    }

    func closePlannerInspector() {
        plannerInspectorProjectID = nil
    }
}
