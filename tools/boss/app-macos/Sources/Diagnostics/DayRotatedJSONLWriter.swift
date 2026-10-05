import Foundation

/// Day-rotated, retention-pruned JSONL file writer shared by diagnostic
/// logs that want `<prefix>YYYY-MM-DD.jsonl` files in one directory.
///
/// Owns the private serial queue, the UTC day formatter, rotate-on-date-change,
/// `O_APPEND` opening and old-file pruning; callers own event construction
/// and any unified-logging mirror. A `nil` directory means "no disk mirror"
/// (isolated instances, tests): `append` is then a no-op.
final class DayRotatedJSONLWriter: @unchecked Sendable {
    private let directory: String?
    private let filePrefix: String
    private let retainDays: Int
    private let site: String
    private let queue: DispatchQueue
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

    init(directory: String?, filePrefix: String, retainDays: Int = 7, site: String) {
        self.directory = directory
        self.filePrefix = filePrefix
        self.retainDays = retainDays
        self.site = site
        self.queue = DispatchQueue(label: "Boss.\(site)")
    }

    /// Append one already-encoded line (trailing newline included) to the
    /// file for `date`'s UTC day. Safe from any thread; the write happens
    /// on the private queue.
    func append(lineData: Data, at date: Date) {
        guard directory != nil else { return }
        queue.async { [self] in
            let dateStr = dateFormatter.string(from: date)
            if dateStr != currentDate || fileHandle == nil {
                if dateStr != currentDate {
                    pruneOldFiles()
                }
                openFile(dateStr: dateStr)
            }
            if let handle = fileHandle {
                DiagnosticWrite.append(lineData, to: handle, site: site, warned: &writeFailureWarned)
            }
        }
    }

    /// Block until queued file writes have drained. Test-only helper.
    func flushForTesting() {
        queue.sync {}
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

        let path = (directory as NSString).appendingPathComponent("\(filePrefix)\(dateStr).jsonl")
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
            guard name.hasPrefix(filePrefix), name.hasSuffix(".jsonl") else { continue }
            let dateStr = String(name.dropFirst(filePrefix.count).dropLast(".jsonl".count))
            if dateStr < cutoffStr {
                let fullPath = (directory as NSString).appendingPathComponent(name)
                try? FileManager.default.removeItem(atPath: fullPath)
            }
        }
    }
}
