import AppKit
import SwiftUI

/// Value type passed to the `"review-terminal"` Window to carry the
/// workspace details once the engine has finished leasing and setting up
/// the PR branch checkout.
struct ReviewTerminalContent: Codable, Hashable, Identifiable {
    let workItemID: String
    let workspacePath: String
    let leaseID: String
    /// Human-readable task name, e.g. "Fix the fencer scraper".
    var taskName: String?
    /// Per-product short id, e.g. 808. Displayed as "T808".
    var taskShortID: Int?

    var id: String { workItemID }

    /// Formatted window title: "Terminal — <name>" when the task name is
    /// available; falls back to just "Terminal" when it is not.
    var windowTitle: String {
        if let name = taskName, !name.isEmpty {
            return "Terminal \u{2014} \(name)"
        }
        return "Terminal"
    }
}

/// State machine for the review-terminal window, owned by
/// [[ChatViewModel]] and injected into the `"review-terminal"` Window
/// via EnvironmentObject. Uses the same open-immediately-then-fill
/// pattern as [[AsyncMarkdownViewerViewModel]].
@MainActor
final class ReviewTerminalViewModel: ObservableObject {
    enum State {
        case idle
        case loading(taskName: String)
        case ready(ReviewTerminalContent)
    }

    @Published var state: State = .idle
    /// Set true on the window's onAppear, false on onDisappear. Used by
    /// the ChatViewModel's reviewTerminalReady handler to decide whether
    /// to deliver the content (window open) or immediately release the
    /// lease (window already closed while loading).
    var windowIsOpen: Bool = false
}

/// Full-window terminal opened from a Review-column card's terminal
/// button. Shows a loading spinner immediately when clicked; transitions
/// to a live Ghostty surface once the engine finishes leasing the
/// workspace and checking out the PR branch.
///
/// State transitions: idle → loading (click) → ready (engine response).
/// Lease lifecycle: the engine holds the lease while the window is open.
/// The outer onDisappear releases it if the window closes after becoming
/// ready, or no-ops if still loading (ChatViewModel's reviewTerminalReady
/// handler releases the lease in that case).
struct ReviewTerminalView: View {
    @EnvironmentObject private var vm: ReviewTerminalViewModel
    @EnvironmentObject private var chatModel: ChatViewModel

    var body: some View {
        Group {
            switch vm.state {
            case .idle:
                Color(nsColor: .black)
            case .loading(let taskName):
                VStack(spacing: 16) {
                    ProgressView()
                        .progressViewStyle(.circular)
                        .scaleEffect(1.5)
                        .tint(.white)
                    Text("Opening terminal…")
                        .font(.callout)
                        .foregroundStyle(Color.white.opacity(0.7))
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color(nsColor: .black))
                .navigationTitle("Terminal \u{2014} \(taskName)")
            case .ready(let content):
                ReviewTerminalSurface(content: content)
                    .navigationTitle(content.windowTitle)
            }
        }
        .onAppear {
            vm.windowIsOpen = true
        }
        .onDisappear {
            vm.windowIsOpen = false
            if case .ready(let content) = vm.state {
                chatModel.releaseReviewTerminal(leaseID: content.leaseID)
            }
            vm.state = .idle
        }
    }
}

/// NSViewRepresentable wrapper that hosts the Ghostty surface for the
/// review terminal. Creates a fresh `TerminalPaneSession` each time the
/// view is constructed (i.e. once per window open).
private struct ReviewTerminalSurface: View {
    let content: ReviewTerminalContent
    @StateObject private var session: TerminalPaneSession

    init(content: ReviewTerminalContent) {
        self.content = content
        let spec = TerminalLaunchSpec(
            fontSize: 12.0,
            workingDirectory: content.workspacePath,
            initialInput: ""
        )
        _session = StateObject(wrappedValue: TerminalPaneSession(
            id: "review-terminal-\(content.workItemID)",
            role: .boss,
            launchSpec: spec
        ))
    }

    var body: some View {
        GhosttyTerminalView(
            runtime: GhosttyRuntime.shared,
            session: session,
            launchSpec: session.launchSpec,
            claudeMonitorEnabled: false
        )
        .background(Color(nsColor: .black))
    }
}
