import Foundation

/// Keep the prior engine's stderr (including Rust's panic hook) across launch.
enum EngineLaunchLog {
    static func preservePrevious(at current: URL) throws {
        let files = FileManager.default
        guard files.fileExists(atPath: current.path) else { return }
        let previous = current.appendingPathExtension("previous")
        if files.fileExists(atPath: previous.path) {
            try files.removeItem(at: previous)
        }
        // Move before the launch shell opens stderr. If rotation fails, fail
        // launch rather than truncate the evidence we were trying to retain.
        try files.moveItem(at: current, to: previous)
    }
}
