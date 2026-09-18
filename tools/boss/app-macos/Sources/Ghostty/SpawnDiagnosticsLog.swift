import Foundation

/// Durable daily JSONL diagnostics for worker viewer attachment. The historical
/// spawn filenames and event tags remain compatible with bossctl logs spawn.
/// These events describe the app's tmux viewer, not worker process startup or
/// death. I/O uses a private serial queue.
final class SpawnDiagnosticsLog: @unchecked Sendable {
    static let shared: SpawnDiagnosticsLog = {
        let appSupport = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first!
        let dir = appSupport.appendingPathComponent("Boss/diagnostics", isDirectory: true)
        return SpawnDiagnosticsLog(directory: dir.path)
    }()

    static let eventSpawnRequested = "spawn_requested"
    static let eventSurfaceAttached = "surface_attached"
    static let eventSurfaceFailed = "surface_failed"

    /// `nil` directory means no disk mirror (used by tests).
    private let directory: String?
    private let retainDays: Int
    private let queue = DispatchQueue(label: "Boss.SpawnDiagnosticsLog")
    private var currentDate = ""
    private var fileHandle: FileHandle?
    /// Throttles the write-failure warning to at most one per rotation —
    /// see [[DiagnosticWrite]].
    private var writeFailureWarned = false
    private let dateFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "yyyy-MM-dd"
        f.timeZone = TimeZone(identifier: "UTC")
        return f
    }()

    init(directory: String?, retainDays: Int = 7) {
        self.directory = directory
        self.retainDays = retainDays
    }

    /// Record an accepted viewer attach request before surface creation.
    /// The historical event tag remains spawn_requested for log readers.
    func spawnRequested(runId: String, slotId: Int, workspacePath: String) {
        record(
            event: Self.eventSpawnRequested,
            runId: runId,
            extra: ["slot_id": slotId, "workspace_path": workspacePath]
        )
    }

    /// Record the viewer surface attachment and its local tmux client pid.
    func surfaceAttached(runId: String, slotId: Int, shellPid: Int32) {
        record(
            event: Self.eventSurfaceAttached,
            runId: runId,
            extra: ["slot_id": slotId, "shell_pid": Int(shellPid)]
        )
    }

    /// Persist viewer failure context for bossctl logs spawn. This does not
    /// report a worker failure to the engine: the detached tmux worker can
    /// remain healthy while the app cannot create its viewer surface.
    func surfaceFailed(
        runId: String,
        reason: String,
        host: HostDisplaySnapshot? = nil,
        diagnostic: String? = nil
    ) {
        var extra: [String: Any] = ["reason": reason]
        if let host {
            extra["host"] = host.jsonObject
        }
        if let diagnostic {
            extra["diagnostic"] = diagnostic
        }
        record(event: Self.eventSurfaceFailed, runId: runId, extra: extra)
    }

    private func record(event: String, runId: String, extra: [String: Any]) {
        let now = Date()
        let epochMs = Int64(now.timeIntervalSince1970 * 1000)
        guard let lineData = Self.line(event: event, runId: runId, tsEpochMs: epochMs, extra: extra) else {
            return
        }

        queue.async { [self] in
            guard directory != nil else { return }
            let dateStr = dateFormatter.string(from: now)
            if dateStr != currentDate || fileHandle == nil {
                if dateStr != currentDate {
                    pruneOldFiles()
                }
                openFile(dateStr: dateStr)
            }
            if let handle = fileHandle {
                DiagnosticWrite.append(
                    lineData, to: handle, site: "SpawnDiagnosticsLog", warned: &writeFailureWarned
                )
            }
        }
    }

    /// Block until queued file writes have drained. Test-only helper.
    func flushForTesting() {
        queue.sync {}
    }

    /// Pure, testable builder for one JSONL line (trailing newline included).
    /// `ts_epoch_ms`, `event`, and `run_id` are always present; `extra` fields
    /// are merged in. Returns `nil` only if the payload is not JSON-encodable.
    static func line(event: String, runId: String, tsEpochMs: Int64, extra: [String: Any]) -> Data? {
        var entry: [String: Any] = [
            "ts_epoch_ms": tsEpochMs,
            "event": event,
            "run_id": runId,
        ]
        for (key, value) in extra {
            entry[key] = value
        }
        guard let jsonData = try? JSONSerialization.data(withJSONObject: entry, options: [.sortedKeys]) else {
            return nil
        }
        return jsonData + Data([0x0A])
    }

    private func openFile(dateStr: String) {
        guard let directory else { return }
        DiagnosticWrite.closeQuietly(fileHandle)
        fileHandle = nil

        do {
            try FileManager.default.createDirectory(atPath: directory, withIntermediateDirectories: true)
        } catch {
            return
        }

        let path = (directory as NSString).appendingPathComponent("spawn-\(dateStr).jsonl")
        guard let handle = DiagnosticWrite.openForAppending(atPath: path) else { return }
        fileHandle = handle
        currentDate = dateStr
        writeFailureWarned = false
    }

    private func pruneOldFiles() {
        guard let directory else { return }
        let cutoff = Date().addingTimeInterval(-Double(retainDays) * 86_400)
        let cutoffStr = dateFormatter.string(from: cutoff)

        guard let entries = try? FileManager.default.contentsOfDirectory(atPath: directory) else {
            return
        }
        for name in entries {
            guard name.hasPrefix("spawn-"), name.hasSuffix(".jsonl") else { continue }
            // "spawn-YYYY-MM-DD.jsonl" → "YYYY-MM-DD"
            let dateStr = String(name.dropFirst("spawn-".count).dropLast(".jsonl".count))
            if dateStr < cutoffStr {
                let fullPath = (directory as NSString).appendingPathComponent(name)
                try? FileManager.default.removeItem(atPath: fullPath)
            }
        }
    }
}
