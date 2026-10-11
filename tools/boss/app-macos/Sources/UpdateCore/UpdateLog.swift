import Darwin
import Foundation
import os.log

// MARK: - Attempt IDs

/// Mints the per-update attempt identifier that every ``UpdateLogRecord`` carries so
/// one attempt can be followed across the old app (check → download → stage → swap),
/// the detached relaunch helper, and the new app (handshake → reconcile).
///
/// Shape: `upd-<unix-ms>-<4 hex>`. Deliberately digits-only after the prefix so the
/// id never resembles any other identifier shape used in this repository.
public enum UpdateAttemptID {
    public static func make(now: Date = Date()) -> String {
        let millis = Int64(now.timeIntervalSince1970 * 1000)
        let suffix = String(format: "%04x", UInt16.random(in: 0...UInt16.max))
        return "upd-\(millis)-\(suffix)"
    }
}

// MARK: - Record

/// One line of the durable update log (`Updates/log/update-log.jsonl`).
///
/// Written by the app (`actor == "app"`) and by `relaunch-helper.sh`
/// (`actor == "helper"`, which appends the same shape with `printf`). Keys are
/// flat and stable so the file can be grepped and `jq`'d without a schema.
public struct UpdateLogRecord: Codable, Equatable, Sendable {
    /// ISO-8601 UTC timestamp with millisecond precision.
    public var ts: String
    /// The attempt this record belongs to (`nil` for records outside any attempt,
    /// e.g. a routine up-to-date check).
    public var attempt: String?
    /// The version running / being replaced.
    public var from: String?
    /// The version being installed.
    public var to: String?
    /// `"app"` or `"helper"`.
    public var actor: String
    public var pid: Int32
    /// Lifecycle stage: `check`, `download`, `verify`, `extract`, `stage`, `swap`,
    /// `relaunch`, `handshake`, `rollback`, `reconcile`, `blocklist`, `engine`.
    public var stage: String
    /// Short machine-readable event within the stage, e.g. `flag-observed`.
    public var event: String
    /// `info`, `warning`, or `error`.
    public var level: String
    /// Free-form key/value details (paths, durations, exit codes, reasons).
    public var detail: [String: String]

    public init(
        ts: String, attempt: String?, from: String?, to: String?, actor: String, pid: Int32,
        stage: String, event: String, level: String, detail: [String: String]
    ) {
        self.ts = ts
        self.attempt = attempt
        self.from = from
        self.to = to
        self.actor = actor
        self.pid = pid
        self.stage = stage
        self.event = event
        self.level = level
        self.detail = detail
    }
}

// MARK: - Log

/// Durable, structured update-lifecycle log: one JSON object per line, appended with
/// `O_APPEND` so the app and the detached shell helper can interleave safely, rotated
/// by size (`update-log.jsonl` → `update-log.1.jsonl` … `.N`), and mirrored to the
/// unified log under `subsystem dev.spinyfin.bossmacapp`, `category update-lifecycle`
/// at `notice`/`error` level (which, unlike `info`, persists to disk and is visible to
/// `log show`).
///
/// Degrades quietly: a failed write drops the line and warns once per rotation epoch,
/// never aborts (the `FileHandle.write(_:)` NSException trap is avoided by writing via
/// `write(2)` directly).
public final class UpdateLog: @unchecked Sendable {

    /// File name of the live log inside ``directory``.
    public static let fileName = "update-log.jsonl"
    /// Default rotation threshold: 1 MiB, which holds on the order of 5,000 records
    /// — many dozens of complete update attempts per file.
    public static let defaultMaxBytes = 1_048_576
    /// Rotated files retained in addition to the live one.
    public static let defaultKeep = 5

    public let directory: URL
    public let actor: String
    private let maxBytes: Int
    private let keep: Int
    private let mirror: (@Sendable (UpdateLogRecord) -> Void)?
    private let lock = NSLock()
    private var warned = false

    private static let mirrorLogger = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "update-lifecycle")

    public init(
        directory: URL,
        actor: String = "app",
        maxBytes: Int = UpdateLog.defaultMaxBytes,
        keep: Int = UpdateLog.defaultKeep,
        mirror: (@Sendable (UpdateLogRecord) -> Void)? = nil
    ) {
        self.directory = directory
        self.actor = actor
        self.maxBytes = maxBytes
        self.keep = keep
        self.mirror = mirror
    }

    /// `Updates/log/update-log.jsonl`.
    public var fileURL: URL { directory.appendingPathComponent(Self.fileName) }

    /// Log directory for an `Updates/` root: `Updates/log/`.
    public static func directory(forUpdatesDirectory updates: URL) -> URL {
        updates.appendingPathComponent("log", isDirectory: true)
    }

    private static let liveLock = NSLock()
    nonisolated(unsafe) private static var liveInstances: [String: UpdateLog] = [:]

    /// The process-wide log for an `Updates/` root, mirrored to the unified log. One
    /// instance per path so rotation is serialised within the process.
    public static func live(updatesDirectory: URL) -> UpdateLog {
        let dir = directory(forUpdatesDirectory: updatesDirectory)
        liveLock.lock()
        defer { liveLock.unlock() }
        if let existing = liveInstances[dir.path] { return existing }
        let log = UpdateLog(directory: dir, mirror: { record in
            let line = "update \(record.stage).\(record.event) attempt=\(record.attempt ?? "-") from=\(record.from ?? "-") to=\(record.to ?? "-") \(Self.flatten(record.detail))"
            if record.level == "error" {
                mirrorLogger.error("\(line, privacy: .public)")
            } else {
                mirrorLogger.notice("\(line, privacy: .public)")
            }
        })
        liveInstances[dir.path] = log
        return log
    }

    // MARK: Writing

    /// Append one record. `from`/`to` are the versions the attempt moves between.
    @discardableResult
    public func record(
        stage: String,
        event: String,
        level: String = "info",
        attempt: String?,
        from: VersionTuple? = nil,
        to: VersionTuple? = nil,
        detail: [String: String] = [:]
    ) -> UpdateLogRecord {
        let record = UpdateLogRecord(
            ts: Self.timestamp(Date()),
            attempt: attempt,
            from: from?.description,
            to: to?.description,
            actor: actor,
            pid: getpid(),
            stage: stage,
            event: event,
            level: level,
            detail: detail
        )
        append(record)
        return record
    }

    public func append(_ record: UpdateLogRecord) {
        mirror?(record)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        guard var data = try? encoder.encode(record) else { return }
        data.append(0x0A)

        lock.lock()
        defer { lock.unlock() }
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        rotateIfNeeded(incoming: data.count)
        let fd = fileURL.path.withCString { open($0, O_WRONLY | O_CREAT | O_APPEND, 0o644) }
        guard fd >= 0 else {
            warnOnce("open failed: errno \(errno)")
            return
        }
        defer { close(fd) }
        let written = data.withUnsafeBytes { buf -> Int in
            guard let base = buf.baseAddress else { return 0 }
            return Int(Darwin.write(fd, base, buf.count))
        }
        if written != data.count {
            warnOnce("short write (\(written)/\(data.count)): errno \(errno)")
        } else {
            warned = false
        }
    }

    // MARK: Reading

    /// Every record in the live file plus the rotated ones, oldest first. Lines that do
    /// not decode (a truncated tail, a hand-edited line) are skipped.
    public func allRecords() -> [UpdateLogRecord] {
        var files: [URL] = []
        for i in stride(from: keep, through: 1, by: -1) {
            files.append(rotatedURL(i))
        }
        files.append(fileURL)
        return files.flatMap { Self.readRecords(at: $0) }
    }

    /// Records carrying `attempt`, in file order.
    public func records(attempt: String) -> [UpdateLogRecord] {
        allRecords().filter { $0.attempt == attempt }
    }

    public static func readRecords(at url: URL) -> [UpdateLogRecord] {
        guard let text = try? String(contentsOf: url, encoding: .utf8) else { return [] }
        let decoder = JSONDecoder()
        return text.split(separator: "\n", omittingEmptySubsequences: true).compactMap { line in
            guard let data = line.data(using: .utf8) else { return nil }
            return try? decoder.decode(UpdateLogRecord.self, from: data)
        }
    }

    // MARK: Private

    private func rotatedURL(_ index: Int) -> URL {
        directory.appendingPathComponent("update-log.\(index).jsonl")
    }

    /// Rotate when the live file would exceed `maxBytes` after this append.
    private func rotateIfNeeded(incoming: Int) {
        let fm = FileManager.default
        guard let attrs = try? fm.attributesOfItem(atPath: fileURL.path),
              let size = attrs[.size] as? Int, size > 0, size + incoming > maxBytes else { return }
        try? fm.removeItem(at: rotatedURL(keep))
        for i in stride(from: keep - 1, through: 1, by: -1) {
            let src = rotatedURL(i)
            if fm.fileExists(atPath: src.path) {
                try? fm.moveItem(at: src, to: rotatedURL(i + 1))
            }
        }
        try? fm.moveItem(at: fileURL, to: rotatedURL(1))
        warned = false
    }

    private func warnOnce(_ message: String) {
        guard !warned else { return }
        warned = true
        Self.mirrorLogger.warning("update log write failed, dropping line(s): \(message, privacy: .public)")
    }

    /// UTC ISO-8601 with milliseconds. A fresh formatter per call: records are rare
    /// and `ISO8601DateFormatter` is not `Sendable`, so a shared static would not pass
    /// strict concurrency checking.
    static func timestamp(_ date: Date) -> String {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        f.timeZone = TimeZone(identifier: "UTC")
        return f.string(from: date)
    }

    private static func flatten(_ detail: [String: String]) -> String {
        detail.keys.sorted().map { "\($0)=\(detail[$0] ?? "")" }.joined(separator: " ")
    }
}
