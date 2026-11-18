import Foundation

extension ChatViewModel {
    // MARK: Planner review/release/undo
    // (design: tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md, task 10)

    /// Planner runs for a project, newest first — as last returned by
    /// `list_planner_runs`. Empty until the popover or inspector has asked.
    func plannerRuns(forProjectID projectID: String) -> [PlannerRun] {
        plannerRunsByProjectID[projectID] ?? []
    }

    /// The most recent run from the project-scoped audit trail. History and
    /// the inspector key off this list; the kanban hourglass and glyph use
    /// [[plannerAffordancePresentation(forProjectID:)]], which reads the
    /// tree-carried summary and the global background-work snapshot.
    func latestPlannerRun(forProjectID projectID: String) -> PlannerRun? {
        plannerRuns(forProjectID: projectID).first
    }

    /// Kanban accessory presentation: global snapshot for current running
    /// identity, the work tree's per-project summary for settled history.
    func plannerAffordancePresentation(forProjectID projectID: String) -> PlannerRunAffordancePresentation? {
        PlannerRunAffordancePresentation.from(
            projectID: projectID,
            summary: plannerRunSummaryByProjectID[projectID],
            backgroundWork: backgroundWork
        )
    }

    /// Full `PlannerRun` for the affordance's popover, or `nil` while it has
    /// to be fetched: nothing cached for the affordance's run, or the cached
    /// row predates the state the tree summary reports (e.g. cached as
    /// `staged` after the run was released). The popover waits for this row.
    func plannerRunForAffordancePopover(forProjectID projectID: String) -> PlannerRun? {
        guard let presentation = plannerAffordancePresentation(forProjectID: projectID),
              let cached = plannerRuns(forProjectID: projectID).first(where: { $0.id == presentation.runID })
        else {
            return nil
        }
        if let summary = plannerRunSummaryByProjectID[projectID], summary.runID == cached.id {
            return summary.isNoNewerThan(cached) ? cached : nil
        }
        return cached
    }

    /// Replace the cached planner summaries for one product with those the
    /// engine sent in its work tree (full snapshot or reconciled delta).
    /// A project absent from `summaries` has no settled run. If the inspector
    /// is open on a project whose summary just moved, its history is reloaded
    /// so the sheet never shows a run list older than the board.
    func applyPlannerRunSummaries(_ summaries: [PlannerRunSummary], forProductID productID: String, projects: [WorkProject]) {
        let previous = plannerRunSummaryByProjectID
        var next = previous
        for project in projectsByProductID[productID] ?? [] {
            next.removeValue(forKey: project.id)
        }
        for project in projects {
            next.removeValue(forKey: project.id)
        }
        for summary in summaries {
            next[summary.projectID] = summary
        }
        guard next != previous else { return }
        plannerRunSummaryByProjectID = next
        if let inspected = plannerInspectorProjectID, next[inspected] != previous[inspected] {
            refreshPlannerRuns(projectID: inspected)
        }
    }

    /// Ask the engine for this project's planner-run audit trail. Only the
    /// popover click, the inspector, and the post-action reload call this;
    /// board refreshes get planner state from the work tree instead.
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
        if engine.sendReleaseProject(projectId: projectID) == nil {
            plannerActionInFlightProjectIDs.remove(projectID)
        }
    }

    /// Undo `runID`'s batch: the engine deletes the still-untouched staged
    /// tasks and clears the run's idempotency gate. Tasks already released
    /// and dispatched are preserved, not deleted, and reported back.
    func unpopulateProject(projectID: String, runID: String) {
        guard !plannerActionInFlightProjectIDs.contains(projectID) else { return }
        plannerActionInFlightProjectIDs.insert(projectID)
        if engine.sendUnpopulateProject(projectId: projectID, runId: runID) == nil {
            plannerActionInFlightProjectIDs.remove(projectID)
        }
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
