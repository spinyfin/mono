import Foundation

/// Append-only JSONL log of every IPC exchange between the engine and
/// the macOS app on the Unix socket. Rotates daily; retains the last
/// `retainDays` days. All I/O runs on a private serial queue so log
/// writes never block the calling thread.
///
/// This is the app's own half of the transcript, written alongside the
/// engine-side one:
///
///   `~/Library/Application Support/Boss/ipc/ipc-YYYY-MM-DD.jsonl`
///
/// (Engine side: `ipc/engine-ipc-YYYY-MM-DD.jsonl`, written by the Rust
/// `ipc_log` module.) Each process writes its own file: both sides log
/// both directions, so a single shared file would record every exchange
/// twice with no field distinguishing the writers. Keep them separate —
/// a record present on one side and absent from the other is the
/// drop/timeout signature this exists to make visible.
///
/// Each line is a JSON object:
///   `ts_epoch_ms`  – milliseconds since Unix epoch
///   `direction`    – `"engine→app"` or `"app→engine"`
///   `request_id`   – opaque id that pairs a request with its response
///   `kind`         – snake_case discriminant (e.g. `"release_worker_pane"`)
///   `body`         – the full request or response payload
///
/// Correlation: `request_id` is minted by the engine as
/// `<session_id>-eng-req-<n>`, where `session_id` is itself
/// `session-<boot_id>-<n>` (a millisecond epoch timestamp captured once at
/// engine construction, so it survives an engine restart even though both
/// counters nested inside it restart at 1), and echoed back verbatim
/// here, so it is the join key with `engine-ipc-*.jsonl`:
/// `(request_id, direction, kind)` identifies one exchange leg on either
/// side.
final class IpcLog: @unchecked Sendable {
    static let shared: IpcLog = {
        let appSupport = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first!
        let dir = appSupport.appendingPathComponent("Boss/ipc", isDirectory: true)
        return IpcLog(directory: dir.path)
    }()

    private let directory: String
    private let retainDays: Int
    private let queue = DispatchQueue(label: "Boss.IpcLog")
    private var currentDate: String = ""
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

    init(directory: String, retainDays: Int = 7) {
        self.directory = directory
        self.retainDays = retainDays
    }

    /// Log one IPC record. Thread-safe; returns immediately.
    func log(requestId: String, direction: String, kind: String, body: [String: Any]) {
        let now = Date()
        let epochMs = Int64(now.timeIntervalSince1970 * 1000)
        let entry: [String: Any] = [
            "ts_epoch_ms": epochMs,
            "direction": direction,
            "request_id": requestId,
            "kind": kind,
            "body": body,
        ]
        // Serialize on the caller's thread so the queue closure captures
        // only `Data` (Sendable) rather than `[String: Any]`.
        guard let jsonData = try? JSONSerialization.data(
            withJSONObject: entry,
            options: [.sortedKeys]
        ) else { return }
        let lineData = jsonData + Data([0x0A])

        queue.async { [self] in
            // dateFormatter is only accessed on this serial queue.
            let dateStr = dateFormatter.string(from: now)
            if dateStr != currentDate || fileHandle == nil {
                if dateStr != currentDate {
                    pruneOldFiles()
                }
                openFile(dateStr: dateStr)
            }
            if let handle = fileHandle {
                DiagnosticWrite.append(lineData, to: handle, site: "IpcLog", warned: &writeFailureWarned)
            }
        }
    }

    private func openFile(dateStr: String) {
        DiagnosticWrite.closeQuietly(fileHandle)
        fileHandle = nil

        do {
            try FileManager.default.createDirectory(
                atPath: directory,
                withIntermediateDirectories: true
            )
        } catch {
            return
        }

        let path = (directory as NSString).appendingPathComponent("ipc-\(dateStr).jsonl")
        guard let handle = DiagnosticWrite.openForAppending(atPath: path) else { return }
        fileHandle = handle
        currentDate = dateStr
        writeFailureWarned = false
    }

    private func pruneOldFiles() {
        let cutoff = Date().addingTimeInterval(-Double(retainDays) * 86_400)
        let cutoffStr = dateFormatter.string(from: cutoff)

        guard let entries = try? FileManager.default.contentsOfDirectory(atPath: directory) else {
            return
        }
        for name in entries {
            guard name.hasPrefix("ipc-"), name.hasSuffix(".jsonl") else { continue }
            // "ipc-YYYY-MM-DD.jsonl" → "YYYY-MM-DD"
            let dateStr = String(name.dropFirst(4).dropLast(6))
            if dateStr < cutoffStr {
                let fullPath = (directory as NSString).appendingPathComponent(name)
                try? FileManager.default.removeItem(atPath: fullPath)
            }
        }
    }
}
