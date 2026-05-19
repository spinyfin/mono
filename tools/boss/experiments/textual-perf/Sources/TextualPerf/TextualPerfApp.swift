import os.log
import SwiftUI

// Records absolute time at process entry — used for interactive_ms.
let processStartTime = Date.now

private let log = Logger(subsystem: "com.boss.textualperf", category: "Render")

enum Sample: String, CaseIterable, Identifiable {
    case full = "sample-46kb"
    case noCode = "sample-nocode"
    case small = "sample-1kb"

    var id: String { rawValue }

    var label: String {
        switch self {
        case .full: "46 KB doc (full)"
        case .noCode: "46 KB doc (no code blocks)"
        case .small: "1 KB doc (baseline)"
        }
    }

    var resourceName: String { rawValue }

    var source: String {
        guard let url = Bundle.module.url(forResource: resourceName, withExtension: "md"),
              let text = try? String(contentsOf: url, encoding: .utf8)
        else {
            return "# Error\n\nCould not load \(resourceName).md"
        }
        return text
    }
}

@main
struct TextualPerfApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}
