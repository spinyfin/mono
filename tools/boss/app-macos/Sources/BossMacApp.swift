import AppKit
import SwiftUI
import UniformTypeIdentifiers
import UpdateCore
import os.log

private let appUpdateLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "updater")
private let appOpenLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "open-document")

@main
struct BossMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var chatModel = ChatViewModel(paths: BossEnginePaths.production())

    var body: some Scene {
        WindowGroup {
            ContentView()
                .task {
                    appDelegate.liveWorkerStates = chatModel.liveWorkerStates
                    appDelegate.chatModel = chatModel
                    appDelegate.updateModel.startPollingIfNeeded()
                }
        }
        .environmentObject(chatModel)
        .environmentObject(appDelegate.updateModel)
        .windowToolbarStyle(.unified(showsTitle: false))
        .defaultSize(width: 1060, height: 680)
        .commands {
            TextEditingCommands()
            CommandGroup(after: .newItem) {
                OpenMarkdownFileCommand(chatModel: chatModel)
            }
            // Show BossFullVersion (e.g. "1.0.4-dev-f3be785") in the About panel
            // rather than CFBundleShortVersionString (numeric-only — plisttool
            // enforces Apple's format requirement for that key).
            CommandGroup(replacing: .appInfo) {
                Button("About Boss") {
                    let full = Bundle.main.object(forInfoDictionaryKey: "BossFullVersion")
                        as? String ?? Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? ""
                    NSApplication.shared.orderFrontStandardAboutPanel(options: [
                        .applicationVersion: full,
                    ])
                }
            }
            CommandGroup(after: .appInfo) {
                CheckForUpdatesCommand(updateModel: appDelegate.updateModel)
            }
            CommandGroup(after: .toolbar) {
                Divider()
                Menu("Board Style") {
                    BoardStyleMenuItems()
                }
            }
            CommandGroup(after: .windowList) {
                Divider()
                LogViewerCommand()
                MetricsCommand()
                UIStallsCommand()
                TerminalLoopCommand()
            }
        }

        Settings {
            SettingsView()
                .environmentObject(chatModel)
                .environmentObject(appDelegate.updateModel)
        }

        WindowGroup("Description", id: "markdown-viewer", for: MarkdownViewerContent.self) { $content in
            if let content {
                MarkdownViewerView(
                    title: content.title,
                    source: content.markdown,
                    artifact: content.commentArtifact
                )
                .navigationTitle(content.title)
                // Engine-back the comment layer (P529 Phase 2); the layer stays
                // in-memory if `content.commentArtifact` is nil.
                .environment(\.commentBackend, chatModel.commentBridge)
            }
        }
        .defaultSize(width: 760, height: 640)

        // Async variant of the markdown viewer: opens immediately in a
        // loading state when the user clicks a design-doc icon, then
        // transitions to loaded/failed when the raw-content fetch settles.
        // Uses [[ChatViewModel.asyncMarkdownViewerVM]] (injected via
        // environmentObject) rather than a value-type payload so the
        // window content can be updated after it opens.
        Window("Design Doc", id: "async-markdown-viewer") {
            AsyncMarkdownViewerView()
        }
        // Inject the viewer VM directly so the window observes its
        // `.loading -> .loaded` transition immediately, rather than waiting
        // for the next incidental `chatModel` publish (see
        // [[AsyncMarkdownViewerView]]). `chatModel` stays injected for the
        // comment layer and other descendants.
        .environmentObject(chatModel)
        .environmentObject(chatModel.asyncMarkdownViewerVM)
        .environment(\.commentBackend, chatModel.commentBridge)
        .defaultSize(width: 760, height: 640)

        // In-app renderer for a project's design-doc pointer. Wired to
        // the kanban project-card affordance via
        // [[ChatViewModel.designRendererOpener]] so SameProduct /
        // OtherProduct + workspace-available pointers render in this
        // window instead of dispatching to the OS-registered `.md`
        // handler — chore #12 of `project-design-doc-pointer.md`.
        WindowGroup("Design Doc", id: "design-renderer", for: DesignRendererContent.self) { $content in
            if let content {
                DesignRendererView(content: content)
                    .navigationTitle(content.title)
            }
        }
        .environmentObject(chatModel)
        .environment(\.commentBackend, chatModel.commentBridge)
        .defaultSize(width: 880, height: 700)

        // Review-terminal window: opened from a Review-column card's
        // terminal button. Opens immediately in a loading state; transitions
        // to a live Ghostty surface once the engine finishes leasing the
        // workspace and checking out the PR branch (async-markdown-viewer
        // pattern).
        // Transcript viewer: shows all historical executions for one task
        // on the left and the selected execution's transcript on the right.
        // Keyed by TranscriptViewerRef (Hashable on taskId only) so
        // re-invoking "View transcripts" for the same task focuses the
        // existing window instead of spawning a duplicate.
        WindowGroup("Transcripts", id: "transcript-viewer", for: TranscriptViewerRef.self) { $ref in
            if let ref {
                TranscriptViewerView(ref: ref)
                    .environmentObject(chatModel)
            }
        }
        .defaultSize(width: 900, height: 640)

        Window("Review Terminal", id: "review-terminal") {
            ReviewTerminalView()
        }
        .environmentObject(chatModel)
        .environmentObject(chatModel.reviewTerminalVM)
        .defaultSize(width: 1000, height: 660)

        // Notifications (Attentions) window: opened from the bell toolbar item
        // (attentions.md — App UI). A singleton window driven by ChatViewModel
        // (mirrors Activity / Metrics) rather than a value-keyed WindowGroup —
        // there is one product-scoped notifications surface, not many.
        Window("Notifications", id: "attentions") {
            AttentionsView()
        }
        .environmentObject(chatModel)
        .defaultSize(width: 460, height: 560)

        Window("Activity", id: "activity") {
            ActivityView()
        }
        .environmentObject(chatModel)
        .defaultSize(width: 1100, height: 640)

        Window("Metrics", id: "metrics") {
            MetricsViewer()
        }
        .environmentObject(chatModel)
        .defaultSize(width: 720, height: 520)

        Window("UI Stalls", id: "ui-stalls") {
            UIStallsViewer()
        }
        .defaultSize(width: 720, height: 520)

        Window("Terminal Loop", id: "terminal-loop") {
            TerminalLoopViewer()
        }
        .defaultSize(width: 720, height: 540)
    }
}

private struct CheckForUpdatesCommand: View {
    let updateModel: UpdateModel

    var body: some View {
        Button("Check for Updates…") {
            updateModel.presentUpdateSheet()
        }
    }
}

/// File ▸ Open (⌘O): shows an NSOpenPanel filtered to .md/.markdown and
/// opens the chosen file via [[ChatViewModel.openLocalMarkdownFile(url:)]]
/// — the same entry point used by the OS-registered open-document path
/// (see `AppDelegate.application(_:open:)`).
private struct OpenMarkdownFileCommand: View {
    let chatModel: ChatViewModel

    var body: some View {
        Button("Open…") {
            openMarkdownFile()
        }
        .keyboardShortcut("o", modifiers: .command)
    }

    private func openMarkdownFile() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [
            UTType(filenameExtension: "md") ?? .plainText,
            UTType(filenameExtension: "markdown") ?? .plainText,
            UTType(filenameExtension: "mdown") ?? .plainText,
        ]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        chatModel.openLocalMarkdownFile(url: url)
    }
}

private struct LogViewerCommand: View {
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    @AppStorage("boss.activity.visible") private var isOpen = false

    var body: some View {
        Button("Show Activity") {
            if isOpen {
                isOpen = false
                dismissWindow(id: "activity")
            } else {
                isOpen = true
                openWindow(id: "activity")
            }
        }
        .keyboardShortcut("l", modifiers: [.command, .shift])
    }
}

private struct ActivityView: View {
    @AppStorage("boss.activity.visible") private var isOpen = false

    var body: some View {
        TabView {
            ActivityLogView()
                .tabItem { Label("Activity", systemImage: "list.bullet") }
            LogViewer()
                .tabItem { Label("Logs", systemImage: "doc.text.magnifyingglass") }
        }
        .onAppear { isOpen = true }
        .onDisappear { isOpen = false }
    }
}

private struct MetricsCommand: View {
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    @AppStorage("boss.metricsViewer.visible") private var isOpen = false

    var body: some View {
        Button("Metrics") {
            if isOpen {
                isOpen = false
                dismissWindow(id: "metrics")
            } else {
                isOpen = true
                openWindow(id: "metrics")
            }
        }
        .keyboardShortcut("m", modifiers: [.command, .shift])
    }
}

private struct UIStallsCommand: View {
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    @AppStorage("boss.uiStalls.visible") private var isOpen = false

    var body: some View {
        Button("UI Stalls") {
            if isOpen {
                isOpen = false
                dismissWindow(id: "ui-stalls")
            } else {
                isOpen = true
                openWindow(id: "ui-stalls")
            }
        }
        .keyboardShortcut("u", modifiers: [.command, .shift])
    }
}

private struct TerminalLoopCommand: View {
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    @AppStorage("boss.terminalLoopViewer.visible") private var isOpen = false

    var body: some View {
        Button("Terminal Loop") {
            if isOpen {
                isOpen = false
                dismissWindow(id: "terminal-loop")
            } else {
                isOpen = true
                openWindow(id: "terminal-loop")
            }
        }
        .keyboardShortcut("t", modifiers: [.command, .shift])
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    /// Set by BossMacApp once the main window has appeared. Nil only in the
    /// brief window between launch and first-render — treated as "no agents
    /// working" so a very-early Cmd-Q is never held hostage.
    var liveWorkerStates: LiveWorkerStateStore?
    /// Owned here so the App struct can inject it into CheckForUpdatesCommand and
    /// environment objects before any view renders or menu fires.
    let updateModel: UpdateModel = UpdateModel.makeForApp()

    /// Set by `BossMacApp.task` once `ContentView` has appeared. The
    /// flush that matters is gated on
    /// [[ChatViewModel.onDesignRendererWired]], not on this assignment —
    /// this setter and the `.task` that wires
    /// `chatModel.designRendererOpener` (in `ContentView`) are two
    /// independent SwiftUI tasks with no ordering guarantee, so gating
    /// the flush here could fire before the renderer is wired and fall
    /// through to the `NSWorkspace.shared.open` fallback — which, since
    /// Boss can now be the OS-registered `.md` handler, would bounce the
    /// file straight back to Boss's own `application(_:open:)`.
    var chatModel: ChatViewModel? {
        didSet {
            chatModel?.onDesignRendererWired = { [weak self] in
                self?.flushPendingMarkdownOpens()
            }
            // The renderer may already have been wired by the time
            // `chatModel` lands here (the inner `.task` won the race) —
            // in that case `onDesignRendererWired` already fired once
            // and won't fire again, so flush directly too.
            if chatModel?.designRendererOpener != nil {
                flushPendingMarkdownOpens()
            }
        }
    }
    private var pendingMarkdownOpenURLs: [URL] = []

    /// Handles the open-document Apple Event — delivered when the app is
    /// launched via `open -a Boss foo.md`, double-clicking a `.md` file
    /// with Boss set as its handler, or Finder's "Open With ▸ Boss".
    /// Routes every markdown URL through
    /// [[ChatViewModel.openLocalMarkdownFile(url:)]], the same entry point
    /// File ▸ Open (⌘O) uses, so both surfaces render in the same
    /// design-renderer window. Passes `allowOSFallback: false` — an
    /// event that arrived from LaunchServices must never be handed back
    /// to `NSWorkspace.shared.open`.
    func application(_ application: NSApplication, open urls: [URL]) {
        let markdownURLs = urls.filter(Self.isMarkdownFile)
        let rejectedURLs = urls.filter { !Self.isMarkdownFile($0) }
        for url in rejectedURLs {
            appOpenLog.notice("Ignoring non-markdown open-document URL: \(url.path, privacy: .public)")
        }
        guard !markdownURLs.isEmpty else { return }
        guard let chatModel, chatModel.designRendererOpener != nil else {
            pendingMarkdownOpenURLs.append(contentsOf: markdownURLs)
            return
        }
        for url in markdownURLs {
            chatModel.openLocalMarkdownFile(url: url, allowOSFallback: false)
        }
    }

    private func flushPendingMarkdownOpens() {
        guard let chatModel, chatModel.designRendererOpener != nil, !pendingMarkdownOpenURLs.isEmpty else { return }
        let urls = pendingMarkdownOpenURLs
        pendingMarkdownOpenURLs.removeAll()
        for url in urls {
            chatModel.openLocalMarkdownFile(url: url, allowOSFallback: false)
        }
    }

    private static func isMarkdownFile(_ url: URL) -> Bool {
        let markdownExtensions: Set<String> = ["md", "markdown", "mdown"]
        if markdownExtensions.contains(url.pathExtension.lowercased()) {
            return true
        }
        guard let markdownType = UTType("net.daringfireball.markdown"),
              let type = UTType(filenameExtension: url.pathExtension)
        else { return false }
        return type.conforms(to: markdownType)
    }

    /// App Nap opt-out token (App Nap incident, 2026-07-15): held for the
    /// process lifetime so `ProcessInfo`/`NSApp` never throttles the main
    /// run loop while the display sleeps. Worker fleets run unattended
    /// overnight with the display off, so scoping this narrower (e.g. to
    /// "while an engine connection is registered") buys nothing — the app
    /// needs to stay prompt for the whole session. `endActivity` is
    /// intentionally never called: releasing the token would re-enable App
    /// Nap, and the token itself is released implicitly when the process
    /// exits. `.userInitiatedAllowingIdleSystemSleep` opts out of App Nap
    /// *without* pinning the display or system awake — display/system idle
    /// sleep must still be allowed (the incident was about RPC handling
    /// staying prompt during sleep, not preventing sleep); do not swap in
    /// `.idleDisplaySleepDisabled` or similar, which would do the latter.
    private var appNapOptOutToken: NSObjectProtocol?

    func applicationDidFinishLaunching(_ notification: Notification) {
        appNapOptOutToken = ProcessInfo.processInfo.beginActivity(
            options: [.userInitiatedAllowingIdleSystemSleep],
            reason: "Keep engine RPC handling and diagnostics sampling prompt during display sleep"
        )

        // When launched outside a regular .app bundle (e.g. `swift run`
        // for local dev), macOS does not auto-promote the process to a
        // foreground UI app — the window opens but never becomes key,
        // so keystrokes go to whichever app was active before launch.
        // Forcing .regular + activate restores key-window status without
        // bringing back the manual NSWindow setup #417 removed.
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)

        // Self-updater: complete any pending bundle swap and write this version's
        // first-launch-OK flag (which the relaunch watchdog polls for). Runs before
        // anything else touches the update state so a relaunch helper from the swap
        // that brought us here sees a healthy launch promptly. The startup-swap
        // *fallback* (applying a not-yet-installed staged update) runs later, at the
        // engine-launch chokepoint in ChatViewModel.startIfNeeded(). See
        // [[UpdateLifecycle]] and design doc §4.
        UpdateLifecycle.reconcileAtLaunch()

        // Start the main-thread hang watchdog. Captures the main thread's
        // Mach port here (we are on the main thread), then runs a
        // background watchdog that records a stall + backtrace whenever
        // the main queue goes unresponsive. Surfaced via the "UI Stalls"
        // window (Cmd-Shift-U). See [[MainThreadStallMonitor]].
        MainThreadStallMonitor.shared.start()

        // Start the terminal event-loop diagnostics sampler (1 Hz). Counts
        // libghostty app-loop activity and probes each pane's pty/EOF/pid
        // liveness to verify/refute the busy-spin high-CPU hypothesis.
        // Surfaced via the "Terminal Loop" window (Cmd-Shift-T). See
        // [[TerminalLoopMonitor]].
        TerminalLoopMonitor.shared.start()

        // Record display sleep/wake transitions into the same diagnostics
        // JSONL mirror (App Nap incident, 2026-07-15) — see
        // [[DisplayPowerMonitor]].
        DisplayPowerMonitor.shared.start()
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        let count = liveWorkerStates?.activeAgentCount ?? 0
        guard count > 0 else { return .terminateNow }

        let alert = NSAlert()
        alert.messageText = "Quit Boss?"
        let agentWord = count == 1 ? "agent is" : "agents are"
        alert.informativeText =
            "\(count) \(agentWord) currently working. Quitting will terminate them and discard any unsaved progress."
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: "Quit Anyway")
        alert.alertStyle = .warning

        // Make Cancel (index 0) the default so a stray Cmd-Q doesn't
        // accidentally confirm through the dialog.
        alert.buttons[0].keyEquivalent = "\r"
        alert.buttons[1].keyEquivalent = ""
        alert.buttons[1].hasDestructiveAction = true

        let response = alert.runModal()
        if response == .alertFirstButtonReturn {
            // Cancel — stay running.
            return .terminateCancel
        }
        // Quit Anyway
        return .terminateNow
    }

    /// Swap-on-quit (design doc §4): once termination is confirmed, settle any pending
    /// update. The agents-running gate is upstream in `applicationShouldTerminate(_:)`
    /// — reaching here means the user accepted the quit.
    ///
    /// Two cases: if a user-initiated "Install & Relaunch" already applied the swap and
    /// parked a relaunch plan, arm the detached helper *now* — deferring the arm to this
    /// confirmed-quit point is what stops a vetoed quit from stranding an armed helper.
    /// Otherwise fall back to the best-effort automatic swap-on-quit. Non-blocking; a
    /// failed swap leaves the current bundle untouched and the startup path retries.
    func applicationWillTerminate(_ notification: Notification) {
        if let plan = UpdateLifecycle.consumePendingRelaunch() {
            UpdateLifecycle.armRelaunchHelper(for: plan)
        } else {
            UpdateLifecycle.applyQuitSwapIfNeeded()
        }
    }

    /// When the last window is closed and workers are still alive, keep
    /// the app running instead of quitting. The window-close path
    /// (red traffic light / Cmd-W) does not consistently route through
    /// `applicationShouldTerminate(_:)` under SwiftUI's `WindowGroup`
    /// lifecycle, so returning `true` here let macOS exit silently —
    /// killing every running Claude pane underneath. Returning `false`
    /// while workers are active leaves the process alive (workers keep
    /// running); the user can re-open the window from the Dock or
    /// explicitly Cmd-Q to hit the confirmation modal.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        (liveWorkerStates?.activeAgentCount ?? 0) == 0
    }
}

/// View > Board Style: single-choice menu items that persist the selection
/// in UserDefaults and sync with the kanban board's @AppStorage binding.
private struct BoardStyleMenuItems: View {
    @AppStorage("boss.kanban.boardStyle") private var style: KanbanBoardStyle = .classic

    var body: some View {
        ForEach(KanbanBoardStyle.allCases) { boardStyle in
            Button {
                style = boardStyle
            } label: {
                if style == boardStyle {
                    Label(boardStyle.displayName, systemImage: "checkmark")
                } else {
                    Text(boardStyle.displayName)
                }
            }
        }
    }
}
