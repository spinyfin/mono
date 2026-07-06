import Foundation

/// Append-only JSONL log of every worker-pane spawn's lifecycle on the app
/// side — the spawn request, the surface-creation result, and the shell pid
/// (or the failure reason). Modeled on [[IpcLog]]: all I/O runs on a private
/// serial queue so logging never blocks the main thread, daily files rotate,
/// and `retainDays` of history is kept.
///
/// This is the durable, execution-id-correlatable diagnostic the 2026-07-05
/// post-wake incident wished it had: engine-side evidence hit a wall precisely
/// because nothing app-side recorded WHY the shell never started
/// (`ghostty_surface_new` returning NULL leaves only a stderr dump). Every
/// record is keyed by `run_id` — the raw execution id — so a spawn that the
/// engine saw ack `shell_pid: 0` can be joined here to its eventual
/// `surface_attached` (pid) or `surface_failed` (reason).
///
/// Files live at:
///   `~/Library/Application Support/Boss/diagnostics/spawn-YYYY-MM-DD.jsonl`
///
/// Each line is a JSON object:
///   `ts_epoch_ms` – milliseconds since Unix epoch
///   `event`       – `"spawn_requested"` | `"surface_attached"` | `"surface_failed"`
///   `run_id`      – the execution id this pane hosts
///   plus event-specific fields (`slot_id`, `shell_pid`, `reason`, …)
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

    /// Log that the engine asked the app to spawn a worker pane. Recorded the
    /// moment the RPC is honored, before the (asynchronous) surface creation —
    /// so a spawn that never progresses to `surface_attached`/`surface_failed`
    /// is visible as a request with no outcome.
    func spawnRequested(runId: String, slotId: Int, workspacePath: String) {
        record(
            event: Self.eventSpawnRequested,
            runId: runId,
            extra: ["slot_id": slotId, "workspace_path": workspacePath]
        )
    }

    /// Log that the libghostty surface attached and produced a shell pid.
    func surfaceAttached(runId: String, slotId: Int, shellPid: Int32) {
        record(
            event: Self.eventSurfaceAttached,
            runId: runId,
            extra: ["slot_id": slotId, "shell_pid": Int(shellPid)]
        )
    }

    /// Log that surface creation failed and no shell came up — the
    /// post-sleep/wake false-live spawn. `reason` mirrors the NACK sent to the
    /// engine.
    func surfaceFailed(runId: String, reason: String) {
        record(event: Self.eventSurfaceFailed, runId: runId, extra: ["reason": reason])
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
            fileHandle?.write(lineData)
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
        fileHandle?.closeFile()
        fileHandle = nil

        do {
            try FileManager.default.createDirectory(atPath: directory, withIntermediateDirectories: true)
        } catch {
            return
        }

        let path = (directory as NSString).appendingPathComponent("spawn-\(dateStr).jsonl")
        if !FileManager.default.fileExists(atPath: path) {
            FileManager.default.createFile(atPath: path, contents: nil)
        }
        guard let handle = FileHandle(forWritingAtPath: path) else { return }
        handle.seekToEndOfFile()
        fileHandle = handle
        currentDate = dateStr
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
