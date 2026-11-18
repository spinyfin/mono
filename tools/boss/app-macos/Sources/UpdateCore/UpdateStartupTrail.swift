import Foundation

/// The update context a launch writes its startup records under, captured at the very
/// start of the process — before reconciliation clears the pending swap or consumes
/// the rolled-back marker.
///
/// - `attempt`: the attempt this launch belongs to when an update produced it: the
///   pending swap's id (a relaunch into the new version); else the rolled-back
///   marker's id (the previous version reopened after a rollback); else the newest
///   helper attempt that targeted or restored the running version and that no app
///   launch has answered with `handshake.flag-written` yet (a helper armed by 1.0.734,
///   whose pending record carries no attempt id). `nil` on an ordinary launch, so a
///   routine launch never attaches itself to an old attempt.
/// - `from` / `to`: that attempt's versions. `to` stays the attempt's *target* even
///   when the process running is the restored previous version; the version actually
///   running is in every record's `detail["runningVersion"]`.
public struct UpdateStartupTrail: Sendable {
    public let attempt: String?
    public let from: VersionTuple?
    public let to: VersionTuple?
    public let running: VersionTuple?
    private let log: UpdateLog?

    public init(installer: UpdateInstaller, running: VersionTuple?) {
        log = installer.log
        self.running = running
        let pending = installer.pendingSwap()
        let marker = installer.pendingRollbackMarker()
        if let pending, !pending.attemptID.isEmpty {
            attempt = pending.attemptID
            from = pending.fromVersion
            to = pending.version
        } else if let marker, let id = marker.attemptID {
            attempt = id
            from = marker.restoredVersion.flatMap(VersionTuple.parse)
            to = marker.version
        } else if let helper = Self.unansweredHelperAttempt(in: installer.log, running: running) {
            attempt = helper.attempt
            from = helper.from.flatMap(VersionTuple.parse)
            to = helper.to.flatMap(VersionTuple.parse)
        } else {
            attempt = nil
            from = pending?.fromVersion ?? running
            to = pending?.version ?? marker?.version ?? running
        }
    }

    /// The newest helper attempt that launched this version (`to`) or restored it
    /// (`from`) and that no app has answered with `handshake.flag-written` yet.
    private static func unansweredHelperAttempt(in log: UpdateLog?, running: VersionTuple?) -> UpdateLogRecord? {
        guard let running, let records = log?.allRecords(),
              let helper = records.last(where: { $0.actor == "helper" && $0.attempt != nil }),
              helper.to == running.description || helper.from == running.description else { return nil }
        let answered = records.contains {
            $0.actor == "app" && $0.attempt == helper.attempt && $0.stage == "handshake" && $0.event == "flag-written"
        }
        return answered ? nil : helper
    }

    /// Append one startup record under this launch's context. `runningVersion` is
    /// added to `detail` unless the caller set it.
    @discardableResult
    public func record(_ event: String, stage: String = "startup", detail: [String: String] = [:]) -> UpdateLogRecord? {
        var detail = detail
        if detail["runningVersion"] == nil, let running { detail["runningVersion"] = running.description }
        return log?.record(stage: stage, event: event, attempt: attempt, from: from, to: to, detail: detail)
    }
}
