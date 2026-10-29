import Foundation

extension ChatViewModel {
    /// Mounting the tab refreshes its summaries and the one open detail.
    /// Cached histories survive unmounting, but cannot cause hidden reads.
    func setAutomationsVisible(_ visible: Bool) {
        automationsIsVisible = visible
        visibleAutomationHistoryID = nil
        if visible {
            refreshAutomations()
            showAutomationHistory(selectedAutomation?.id)
        }
    }

    /// Refresh the mounted tab, including a retry of the open history read.
    func refreshVisibleAutomations() {
        guard automationsIsVisible else { return }
        refreshAutomations()
        if let id = visibleAutomationHistoryID { refreshVisibleAutomationHistory(id) }
    }

    func showAutomationHistory(_ id: String?) {
        guard automationsIsVisible else { return }
        guard visibleAutomationHistoryID != id else { return }
        visibleAutomationHistoryID = id
        if let id { refreshVisibleAutomationHistory(id) }
    }

    func refreshVisibleAutomationHistory(_ id: String) {
        guard isConnected, automationsIsVisible,
              visibleAutomationHistoryID == id,
              selectedAutomation?.id == id else { return }
        engine.sendListAutomationRuns(automationId: id)
    }

    func applyAutomationSummary(_ id: String, count: Int?, lastRun: AppAutomationRun?) {
        if let count { openTaskCountByAutomationID[id] = count }
        if let lastRun { latestRunByAutomationID[id] = lastRun }
    }
}
