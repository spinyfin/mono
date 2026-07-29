import Foundation
import os

/// Shared logger for diagnostic-write failures across the app's JSONL
/// diagnostic writers (`TerminalLoopLog`, `StallLog`, `PopulationTimingLog`,
/// `IpcLog`, `SpawnDiagnosticsLog`, …).
private let diagnosticWriteLog = Logger(subsystem: "com.boss.app", category: "diagnostic-write")

/// Non-throwing wrapper around `FileHandle.write(contentsOf:)`.
///
/// `FileHandle.write(_:)` — the historical, non-throwing convenience
/// method — raises an Objective-C `NSException` on failure (e.g. `ENOSPC`
/// on a full disk). That exception is uncatchable from Swift and takes the
/// whole process down with it: a full-disk terminal-loop log write once
/// reached `abort`, and the crash-handler thread that should have reported
/// it spun forever instead of dying quickly, turning a crash into an
/// 11.5-hour app livelock. `write(contentsOf:)` is the Swift-throwing
/// equivalent; catching its error lets a diagnostic writer degrade to
/// "drop this line" instead of "abort the app".
enum DiagnosticWrite {
    /// Append `data` to `handle`. Returns `true` on success.
    ///
    /// On failure, logs at most one warning per `warned` "epoch" — callers
    /// reset `warned` to `false` on rotation (a new file / a new day) to
    /// re-arm it, so a persistently failing writer (e.g. a full disk)
    /// produces one warning per rotation period rather than one per
    /// dropped line.
    @discardableResult
    static func append(_ data: Data, to handle: FileHandle, site: String, warned: inout Bool) -> Bool {
        do {
            try handle.write(contentsOf: data)
            warned = false
            return true
        } catch {
            if !warned {
                warned = true
                diagnosticWriteLog.warning(
                    "\(site, privacy: .public): diagnostic write failed, dropping line(s): \(String(describing: error), privacy: .public)"
                )
            }
            return false
        }
    }
}
