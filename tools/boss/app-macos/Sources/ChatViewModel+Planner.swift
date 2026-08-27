import Foundation

extension ChatViewModel {
    // MARK: Planner review/release/undo
    // (design: tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md, task 10)

    /// Planner runs for a project, newest first — as returned by the engine.
    func plannerRuns(forProjectID projectID: String) -> [PlannerRun] {
        plannerRunsByProjectID[projectID] ?? []
    }

    /// The most recent run from the project-scoped audit trail. History,
    /// the inspector, and popover detail key off this list; the kanban
    /// hourglass uses [[plannerAffordancePresentation(forProjectID:)]] so
    /// current running state can come from the global snapshot.
    func latestPlannerRun(forProjectID projectID: String) -> PlannerRun? {
        plannerRuns(forProjectID: projectID).first
    }

    /// Live `project_planner` snapshot item for this project, if the
    /// engine currently attests one. `sourceID` is `planner_runs.id`.
    func livePlannerBackgroundItem(forProjectID projectID: String) -> BackgroundWorkItem? {
        backgroundWork.first { item in
            item.isProjectPlanner && item.projectID == projectID
        }
    }

    /// Kanban accessory presentation: global snapshot for current running
    /// identity, project-scoped query for staged/applied/failed history.
    func plannerAffordancePresentation(forProjectID projectID: String) -> PlannerRunAffordancePresentation? {
        PlannerRunAffordancePresentation.from(
            projectID: projectID,
            cachedRuns: plannerRuns(forProjectID: projectID),
            backgroundWork: backgroundWork
        )
    }

    /// Cached `PlannerRun` matching the affordance identity, or `nil` when
    /// the hourglass is showing a globally-discovered run the project
    /// query has not returned yet. The popover waits for this row.
    func plannerRunForAffordancePopover(forProjectID projectID: String) -> PlannerRun? {
        guard let presentation = plannerAffordancePresentation(forProjectID: projectID) else {
            return nil
        }
        return plannerRuns(forProjectID: projectID).first { $0.id == presentation.runID }
    }

    /// Ask the engine for this project's planner-run audit trail. Safe to
    /// call repeatedly (e.g. from `onAppear`) — the reply simply replaces
    /// the cached array.
    func refreshPlannerRuns(projectID: String) {
        guard isConnected else { return }
        engine.sendListPlannerRuns(projectId: projectID)
    }

    /// Refresh every known project's planner-run audit trail for a product.
    /// Called on `workInvalidated` for the product's work topic so a
    /// planner run created after [[PlannerRunAffordance]]'s first appearance
    /// (e.g. a design-PR-triggered auto-populate landing while the board is
    /// already open) surfaces its icon within seconds instead of only after
    /// the view remounts.
    func refreshPlannerRuns(forProductID productID: String) {
        for project in projectsByProductID[productID] ?? [] {
            refreshPlannerRuns(projectID: project.id)
        }
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
