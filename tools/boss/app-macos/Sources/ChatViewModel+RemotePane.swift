import Foundation

extension ChatViewModel {
    /// Connect the remote pane viewer to the engine: it watches a run by
    /// subscribing to the run's pane topic, and acts on it only through the
    /// engine's explicit probe / interrupt / stop requests.
    func bindRemotePaneViewer() {
        remotePaneViewer.subscribe = { [weak self] topics in self?.engine.sendSubscribe(topics: topics) }
        remotePaneViewer.unsubscribe = { [weak self] topics in self?.engine.sendUnsubscribe(topics: topics) }
        remotePaneViewer.sendProbe = { [weak self] runId, text in self?.engine.sendProbeRun(runId: runId, text: text) }
        remotePaneViewer.sendInterrupt = { [weak self] runId in self?.engine.sendInterruptWorkerPane(runId: runId) }
        remotePaneViewer.sendStop = { [weak self] runId in self?.engine.sendStopRun(runId: runId) }
    }
}
