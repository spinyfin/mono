import AppKit
import SwiftUI
import OSLog

private let terminalOpenLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "terminal-open")

/// Value type passed to the `"review-terminal"` Window to carry the
/// workspace details once the engine has finished leasing and setting up
/// the PR branch checkout.
struct ReviewTerminalContent: Codable, Hashable, Identifiable {
    let workItemID: String
    let workspacePath: String
    /// `Some` for a fresh PR-branch workspace leased just for this window
    /// (the Review/Done-lane flow) — released when the window closes.
    /// `nil` for a Doing-lane live-execution workspace, whose lease is
    /// owned by the running worker and must be left untouched.
    let leaseID: String?
    /// Human-readable task name, e.g. "Fix the fencer scraper".
    var taskName: String?
    /// Per-product short id, e.g. 808. Displayed as "T808".
    var taskShortID: Int?
    var requestID: String? = nil

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
        case failed(String)
    }

    @Published var state: State = .idle
    @Published var progress = "Preparing workspace"
    private(set) var requestID: String?
    private(set) var workItemID: String?
    private var startedAt = Date()
    private var watchdog: Task<Void, Never>?
    private(set) var preparationID = UUID().uuidString

    var isPreparing: Bool {
        if case .loading = state { return true }
        return false
    }

    func begin(workItemID: String, taskName: String, requestID: String = UUID().uuidString) {
        watchdog?.cancel()
        self.workItemID = workItemID
        windowIsOpen = true
        self.requestID = requestID
        startedAt = Date()
        preparationID = UUID().uuidString
        progress = "Preparing workspace"
        state = .loading(taskName: taskName)
    }

    func track(requestID: String?) {
        self.requestID = requestID
        guard requestID != nil else {
            fail("Could not contact the engine. Reopen the terminal to retry.")
            return
        }
        noteProgress("Request sent")
    }

    func matches(requestID: String?, workItemID: String? = nil) -> Bool {
        self.requestID != nil && self.requestID == requestID &&
            (workItemID == nil || self.workItemID == workItemID)
    }

    func noteProgress(_ stage: String) {
        progress = stage
        log(stage)
        watchdog?.cancel()
        watchdog = Task { @MainActor [weak self] in
            do { try await Task.sleep(for: .seconds(30)) } catch { return }
            self?.fail("Workspace preparation stopped reporting progress. Close and reopen to retry.")
        }
    }

    func ready(_ content: ReviewTerminalContent) {
        watchdog?.cancel()
        log("Workspace received")
        var content = content
        content.requestID = requestID
        state = .ready(content)
    }

    func fail(_ message: String) {
        watchdog?.cancel()
        log("Failed: \(message)")
        state = .failed(message)
    }

    func close() {
        watchdog?.cancel()
        windowIsOpen = false
        requestID = nil
        workItemID = nil
        state = .idle
    }

    private func log(_ stage: String) {
        terminalOpenLog.info("work_item=\(self.workItemID ?? "", privacy: .public) request=\(self.requestID ?? "", privacy: .public) stage=\(stage, privacy: .public) elapsed_ms=\(Date().timeIntervalSince(self.startedAt) * 1000)")
    }
    /// Marked open at the gesture, before the first frame, so a fast reply
    /// is accepted even if SwiftUI has not mounted the window yet.
    /// Closing clears it so late replies release their leases.
    var windowIsOpen: Bool = false
}

/// Opens a standalone shell immediately while the engine prepares a separate
/// PR checkout. Progress and failures stay visible above the shell. Only a
/// positioned workspace replaces the preparation shell; closing releases its lease.
struct ReviewTerminalView: View {
    @EnvironmentObject private var vm: ReviewTerminalViewModel
    @EnvironmentObject private var chatModel: ChatViewModel

    var body: some View {
        ZStack {
            switch vm.state {
            case .idle:
                Color(nsColor: .black)
            case .loading(let taskName):
                preparationTerminal(message: "\(vm.progress)… Shell below is in your home directory; the PR workspace will open when ready.")
                    .navigationTitle("Terminal \u{2014} \(taskName)")
            case .failed(let message):
                preparationTerminal(message: "\(message) Shell below is in your home directory.")
            case .ready(let content):
                ReviewTerminalSurface(content: content)
                    .id(content.workspacePath)
                    .navigationTitle(content.windowTitle)
            }
        }
        .onAppear {
            vm.windowIsOpen = true
        }
        .onDisappear {
            vm.windowIsOpen = false
            if let workItemID = vm.workItemID {
                chatModel.openingReviewTerminalIDs.remove(workItemID)
                chatModel.openingLiveWorkspaceTerminalIDs.remove(workItemID)
            }
            if case .ready(let content) = vm.state, let leaseID = content.leaseID {
                chatModel.releaseReviewTerminal(leaseID: leaseID)
            }
            vm.close()
        }
    }

    private func preparationTerminal(message: String) -> some View {
        VStack(spacing: 0) {
            Text(message).font(.callout).padding().frame(maxWidth: .infinity, alignment: .leading)
            ReviewTerminalSurface(content: ReviewTerminalContent(
                workItemID: vm.workItemID ?? vm.preparationID, workspacePath: NSHomeDirectory(), leaseID: nil,
                requestID: vm.requestID
            ))
            .id(vm.preparationID)
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
            initialInput: "",
            command: "/bin/zsh -f -i"
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
            paneMonitorEnabled: false
        )
        .background(Color(nsColor: .black))
        .overlay(alignment: .top) {
            if let message = session.statusMessage {
                Text(message).padding().frame(maxWidth: .infinity).background(.regularMaterial)
            }
        }
        .onAppear { [session] in
            session.onChildExited = { [weak session] in
                session?.statusMessage = "Terminal shell exited. Close and reopen the window to start a new shell."
            }
            terminalOpenLog.info("work_item=\(content.workItemID, privacy: .public) request=\(content.requestID ?? "", privacy: .public) stage=shell_spawn workspace=\(content.workspacePath, privacy: .public)")
        }
        .onChange(of: session.terminalReady) { _, ready in
            terminalOpenLog.info("work_item=\(content.workItemID, privacy: .public) request=\(content.requestID ?? "", privacy: .public) stage=surface_attach ready=\(ready)")
        }
        .task {
            let start = Date()
            for _ in 0..<30 {
                do { try await Task.sleep(for: .milliseconds(100)) } catch { return }
                if session.hostView?.hasVisibleOutput == true {
                    terminalOpenLog.info("work_item=\(content.workItemID, privacy: .public) request=\(content.requestID ?? "", privacy: .public) stage=shell_output elapsed_ms=\(Date().timeIntervalSince(start) * 1000)")
                    return
                }
            }
            if session.statusMessage == nil {
                session.statusMessage = "Terminal shell produced no output. Close and reopen the window to retry."
            }
        }
        .onDisappear { session.markReleased() }
    }
}
