import AppKit
import os.log
import SwiftUI
import UpdateCore

struct ResizeDivider: NSViewRepresentable {
    let currentWidth: CGFloat
    let minWidth: CGFloat
    let maxWidth: CGFloat
    let onWidthChanged: (CGFloat) -> Void

    func makeNSView(context: Context) -> ResizeDividerView {
        let view = ResizeDividerView()
        view.minWidth = minWidth
        view.maxWidth = maxWidth
        view.currentWidth = currentWidth
        view.onWidthChanged = onWidthChanged
        return view
    }

    func updateNSView(_ nsView: ResizeDividerView, context: Context) {
        nsView.minWidth = minWidth
        nsView.maxWidth = maxWidth
        nsView.currentWidth = currentWidth
        nsView.onWidthChanged = onWidthChanged
    }
}

class ResizeDividerView: NSView {
    var minWidth: CGFloat = 280
    var maxWidth: CGFloat = 600
    /// The Boss panel's current width, mirrored from the SwiftUI
    /// model. The drag math anchors on this at mouseDown — see the
    /// note in `mouseDown`.
    var currentWidth: CGFloat = 0
    var onWidthChanged: ((CGFloat) -> Void)?

    private var dragStartWidth: CGFloat = 0
    private var dragStartMouseX: CGFloat = 0
    private var isHovering = false
    private var isDragging = false

    /// X offset of the visible 1pt separator line within the view's
    /// bounds. The strip is anchored at the Boss pane's leading edge,
    /// so x = 0 is the boundary and the rest of the strip extends
    /// into the Boss pane as an invisible grip area.
    private let visibleLineX: CGFloat = 0

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        for area in trackingAreas {
            removeTrackingArea(area)
        }
        // `.cursorUpdate` is what actually drives the resize cursor.
        // The SwiftUI overlay hosts this NSView inside the detail pane
        // of a NavigationSplitView (NSSplitView under the hood), and
        // that container intercepts the AppKit cursor-rect machinery —
        // `resetCursorRects` / `addCursorRect` is not called reliably
        // for descendant views, so the cursor never flips on hover.
        // Routing cursor swaps through the tracking area's
        // `cursorUpdate(_:)` event bypasses that and works regardless
        // of the parent container.
        let area = NSTrackingArea(
            rect: bounds,
            options: [.mouseEnteredAndExited, .cursorUpdate, .activeInKeyWindow, .inVisibleRect],
            owner: self,
            userInfo: nil
        )
        addTrackingArea(area)
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        // Visible 1pt separator line at the boundary between the
        // worker grid and the Boss pane. The rest of the view bounds
        // is invisible grab strip — cursor + drag hit area, but not
        // painted.
        let lineX = visibleLineX
        NSColor.separatorColor.setFill()
        NSRect(x: lineX, y: 0, width: 1, height: bounds.height).fill()

        // Hover/active feedback: thicken and tint the line so the
        // user can see that the divider is grabbable / being dragged.
        // Drawn slightly inside the strip so it stays within bounds.
        if isDragging || isHovering {
            let alpha: CGFloat = isDragging ? 0.85 : 0.45
            NSColor.controlAccentColor.withAlphaComponent(alpha).setFill()
            NSRect(x: lineX, y: 0, width: 2, height: bounds.height).fill()
        }
    }

    /// Fires while the cursor is inside the tracking area. Setting
    /// the cursor here (instead of via `addCursorRect`) sidesteps the
    /// NSSplitView ancestor that would otherwise swallow cursor-rect
    /// management for descendant SwiftUI-hosted views. AppKit clears
    /// the cursor automatically when the pointer leaves the tracking
    /// area, so there's no stale-resize-cursor risk.
    override func cursorUpdate(with event: NSEvent) {
        NSCursor.resizeLeftRight.set()
    }

    override func mouseEntered(with event: NSEvent) {
        isHovering = true
        // Belt-and-suspenders: `cursorUpdate` is the primary path, but
        // setting on entry guarantees the swap fires on the first
        // hover even if the tracking area's initial `cursorUpdate`
        // hasn't been dispatched yet.
        NSCursor.resizeLeftRight.set()
        needsDisplay = true
    }

    override func mouseExited(with event: NSEvent) {
        isHovering = false
        // Restore the arrow on exit. Without this, the resize cursor
        // can linger on app-focus changes or when leaving via a route
        // that doesn't trigger another view's `cursorUpdate`.
        NSCursor.arrow.set()
        needsDisplay = true
    }

    override func mouseDown(with event: NSEvent) {
        // Anchor on the panel's width as reported by the SwiftUI model
        // rather than `superview.bounds.width` — the superview here is
        // the SwiftUI host of the (narrow) divider strip itself, not
        // the Boss panel. Using its width as the anchor produces a
        // tiny initial value (≈ the strip width) and clamps every
        // drag straight to `minWidth`, which is exactly the bug this
        // change fixes.
        dragStartWidth = currentWidth
        dragStartMouseX = event.locationInWindow.x
        isDragging = true
        needsDisplay = true
    }

    override func mouseDragged(with event: NSEvent) {
        let deltaX = event.locationInWindow.x - dragStartMouseX
        // The Boss panel sits on the trailing side of the window, so
        // dragging the divider right (positive deltaX) shrinks it.
        let newWidth = max(minWidth, min(maxWidth, dragStartWidth - deltaX))
        onWidthChanged?(newWidth)
    }

    override func mouseUp(with event: NSEvent) {
        isDragging = false
        needsDisplay = true
    }
}

/// Persistent, full-width strip pinned to the top of the window when
/// the engine socket can't be reached. Replaces the prior "Work Error"
/// modal that re-popped every dismissal during a reconnect storm (see
/// `ChatViewModel.handle` for the matching transport-error suppression
/// path).
///
/// Carries a "Restart engine" affordance so a stale or hung engine
/// process can be recovered without a shell `pkill` (issue #697). The
/// button drives `ChatViewModel.restartEngine()`, which terminates the
/// engine via the token-auth shutdown RPC (falling back to SIGTERM/
/// SIGKILL when the socket is dead) and relaunches it; the reconnect
/// loop picks the new socket up automatically.

struct EngineUnreachableBanner: View {
    let isRestarting: Bool
    let supervisionState: EngineSupervisionState
    let onRestart: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.white)
                .padding(.top, 2)
            Text(headlineText)
                .font(.callout.weight(.semibold))
                .foregroundStyle(.white)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
            Button(action: onRestart) {
                Text(isRestarting ? "Restarting…" : "Restart engine")
                    .font(.callout.weight(.semibold))
            }
            .buttonStyle(.bordered)
            .controlSize(.small)
            .tint(.white)
            .disabled(isRestarting)
            .help("Terminate the unresponsive engine and start a fresh one.")
            .accessibilityHint("Terminates the unresponsive engine and starts a fresh one.")
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.red.opacity(0.85))
        .accessibilityElement(children: .contain)
    }

    private var headlineText: String {
        if isRestarting {
            return "Restarting Boss engine…"
        }
        switch supervisionState {
        case .running:
            return "Boss engine is unreachable — reconnecting…"
        case .restarting(let attempt, let retryAfter):
            return "Boss engine exited — restarting (attempt \(attempt) in \(Int(retryAfter))s)…"
        case .gaveUp(let attempts):
            return "Boss engine stopped after \(attempts) restart attempts."
        }
    }
}

/// Chrome-level banner surfacing engine-health issues: missing
/// `ANTHROPIC_API_KEY`, dispatch paused, `syspolicyd` wedged, and any
/// future issue the engine emits. Introduced after #699. The first
/// issue's title renders inline; the chevron expands all issues with
/// their remediation bodies.

struct EngineHealthBanner: View {
    let issues: [EngineHealthIssue]
    /// Trigger for the `dispatch_paused` issue's "Unpause" affordance.
    /// Fires `ChatViewModel.resumeDispatch()`, the same
    /// `SetDispatchPaused { paused: false }` RPC `bossctl dispatch
    /// resume` uses — the engine owns the actual state change, this
    /// button is a thin trigger.
    let onUnpauseDispatch: () -> Void
    @State private var isExpanded: Bool = false

    /// `true` when the paused-dispatch issue is present, driving the
    /// banner's "Unpause" button.
    private var isDispatchPaused: Bool {
        issues.contains { $0.kind == "dispatch_paused" }
    }

    /// Highest severity in the issue list — drives banner color so a
    /// single `error` row escalates an otherwise-warning banner.
    private var effectiveSeverity: String {
        issues.contains(where: { $0.severity == "error" }) ? "error" : "warning"
    }

    private var background: Color {
        effectiveSeverity == "error"
            ? Color.red.opacity(0.85)
            : Color.orange.opacity(0.85)
    }

    private var iconName: String {
        effectiveSeverity == "error"
            ? "exclamationmark.octagon.fill"
            : "exclamationmark.triangle.fill"
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(alignment: .top, spacing: 8) {
                Button(action: { withAnimation(.easeInOut(duration: 0.12)) { isExpanded.toggle() } }) {
                    HStack(alignment: .top, spacing: 8) {
                        Image(systemName: iconName)
                            .foregroundStyle(.white)
                            .padding(.top, 2)
                        // No lineLimit: height grows with Dynamic Type and
                        // long pause-since strings so the parent VStack
                        // (ContentView) reserves real space instead of a
                        // hardcoded top padding.
                        Text(headlineText)
                            .font(.callout.weight(.semibold))
                            .foregroundStyle(.white)
                            .multilineTextAlignment(.leading)
                            .fixedSize(horizontal: false, vertical: true)
                        Spacer(minLength: 0)
                        Image(systemName: isExpanded ? "chevron.up" : "chevron.down")
                            .foregroundStyle(.white)
                            .font(.caption.weight(.semibold))
                            .padding(.top, 4)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help(issues.first?.body ?? "")

                if isDispatchPaused {
                    Button(action: onUnpauseDispatch) {
                        Text("Unpause")
                            .font(.callout.weight(.semibold))
                    }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
                    .tint(.white)
                    .help("Resume global dispatch (same as `bossctl dispatch resume`).")
                    .accessibilityHint("Resumes global dispatch.")
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .frame(maxWidth: .infinity, alignment: .leading)

            if isExpanded {
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(issues) { issue in
                        VStack(alignment: .leading, spacing: 2) {
                            Text(issue.title)
                                .font(.callout.weight(.semibold))
                                .foregroundStyle(.white)
                                .fixedSize(horizontal: false, vertical: true)
                            Text(issue.body)
                                .font(.caption)
                                .foregroundStyle(.white.opacity(0.92))
                                .fixedSize(horizontal: false, vertical: true)
                        }
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 14)
                .padding(.bottom, 10)
            }
        }
        .background(background)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(accessibilityLabel)
    }

    private var headlineText: String {
        if issues.count == 1 {
            return issues[0].title
        }
        let first = issues[0].title
        return "\(first) (\(issues.count - 1) more)"
    }

    private var accessibilityLabel: String {
        issues.map { "\($0.title). \($0.body)" }.joined(separator: " ")
    }
}

/// Informational banner offering — never performing — a coordinator reset
/// when the engine reports the installed `claude` is newer than the one the
/// running coordinator session actually launched with. Deliberately a
/// separate view from `EngineHealthBanner`: that banner's color represents
/// operational fault severity (error/warning), and an available update is
/// neither. Rendered above the coordinator pane (the Picard column), not as
/// window chrome. Shown only while `ChatViewModel.coordinatorUpdateAvailable`
/// is non-nil; there is no dismiss — it clears itself once a reset makes the
/// versions match, never earlier.
struct CoordinatorUpdateBanner: View {
    let installedVersion: String
    let onReset: () -> Void
    var isCollapsed: Bool = false

    private var copy: String {
        "The coordinator is running an older Claude Code — \(installedVersion) is installed."
    }

    private var resetHelp: String {
        "Ends the coordinator's current session and starts a fresh one with the installed binary, after confirming."
    }

    var body: some View {
        Group {
            if isCollapsed {
                collapsedBody
            } else {
                expandedBody
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.blue.opacity(0.85))
        .accessibilityElement(children: .contain)
    }

    /// The pane is 280...600pt wide — too narrow to keep the copy and an
    /// intrinsically-sized Reset button on one row, so the control sits
    /// under the copy; the text grows vertically via `fixedSize`.
    private var expandedBody: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .top, spacing: 8) {
                Image(systemName: "arrow.up.circle.fill")
                    .foregroundStyle(.white)
                    .padding(.top, 2)
                Text(copy)
                    .font(.callout.weight(.semibold))
                    .foregroundStyle(.white)
                    .multilineTextAlignment(.leading)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(minWidth: 0, maxWidth: .infinity, alignment: .leading)
            }
            HStack {
                Spacer(minLength: 0)
                Button(action: onReset) {
                    Text("Reset Coordinator…")
                        .font(.callout.weight(.semibold))
                }
                .buttonStyle(.bordered)
                .controlSize(.small)
                .tint(.white)
                .fixedSize()
                .help(resetHelp)
                .accessibilityHint("Opens a confirmation to reset the coordinator session.")
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
    }

    /// The collapsed Picard strip is 88pt — not enough for the copy or
    /// Reset control. Keep the same trigger with a tappable glyph on the
    /// blue strip; hover and VoiceOver still carry the notice.
    private var collapsedBody: some View {
        Button(action: onReset) {
            Image(systemName: "arrow.up.circle.fill")
                .font(.system(size: 16, weight: .semibold))
                .foregroundStyle(.white)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 8)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(copy)
        .accessibilityLabel(copy)
        .accessibilityHint("Opens a confirmation to reset the coordinator session.")
    }
}

// MARK: - Automation pause toggle

/// Operator-facing reason attached to a pause initiated from the toolbar.
/// The engine rejects an anonymous `SetAutomationPaused { paused: true }`;
/// this is the programmatic equivalent of `bossctl automation pause
/// --reason …` for the in-app control. Resume omits it.
enum AutomationPauseControl {
    static let toolbarReason = "Paused from Boss toolbar"

    /// Caption painted on the control. Distinct words so the two states
    /// cannot be told apart only by color or a tooltip.
    static func caption(isPaused: Bool) -> String {
        isPaused ? "Paused" : "Auto"
    }

    /// SF Symbol for the caption. Filled pause vs filled play — paired
    /// with `usesEngagedTreatment` so paused is not a quiet outline of
    /// the same glyph as running.
    static func symbolName(isPaused: Bool) -> String {
        isPaused ? "pause.fill" : "play.fill"
    }

    /// Paused is the engaged treatment (filled orange capsule). Running
    /// is a quiet outline. A glance must distinguish them without hover.
    static func usesEngagedTreatment(isPaused: Bool) -> Bool {
        isPaused
    }
}

/// Primary-toolbar control for the global automation pause. The engine
/// remains authoritative: appearance is derived from the
/// `automation_paused` health issue (the same snapshot
/// `bossctl automation state` reads), and a click sends
/// `SetAutomationPaused` — no local flag, no confirmation.
///
/// Paused vs running must be glanceable without a tooltip. The paused
/// state is an orange filled capsule (custom Text+Capsule, not a glass
/// bezel, so it survives cacheDisplay under FB20272917). The running
/// state is a quiet outline of the same size so the two cannot be
/// confused at a glance.
struct AutomationPauseToolbarButton: View {
    @ObservedObject var model: ChatViewModel

    private var isPaused: Bool { model.isAutomationPaused }

    var body: some View {
        Button {
            model.toggleAutomationPaused()
        } label: {
            HStack(spacing: 4) {
                Image(systemName: AutomationPauseControl.symbolName(isPaused: isPaused))
                    .font(.caption.weight(.bold))
                Text(AutomationPauseControl.caption(isPaused: isPaused))
                    .font(.caption.weight(.semibold))
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(labelBackground)
            .foregroundStyle(isPaused ? Color.white : Color.secondary)
            .overlay(labelOverlay)
        }
        .buttonStyle(.plain)
        .disabled(!model.isConnected)
        .help(isPaused
              ? "Automations are paused. Click to resume (same as `bossctl automation resume`)."
              : "Automations are running. Click to pause (same as `bossctl automation pause`).")
        .accessibilityLabel(isPaused ? "Automations paused" : "Automations running")
        .accessibilityHint(isPaused ? "Resumes automations." : "Pauses automations.")
        .accessibilityIdentifier("boss.automationPauseToggle")
        .accessibilityAddTraits(isPaused ? [.isSelected] : [])
    }

    @ViewBuilder
    private var labelBackground: some View {
        if AutomationPauseControl.usesEngagedTreatment(isPaused: isPaused) {
            Capsule().fill(Color.orange.opacity(0.95))
        } else {
            Capsule().fill(Color.clear)
        }
    }

    @ViewBuilder
    private var labelOverlay: some View {
        if AutomationPauseControl.usesEngagedTreatment(isPaused: isPaused) {
            EmptyView()
        } else {
            Capsule().strokeBorder(Color.secondary.opacity(0.55), lineWidth: 1)
        }
    }
}

// MARK: - Update badge

/// Trailing toolbar button that appears when an update is available in Notify or Automatic mode.
/// Visibility is driven by `UpdateModel`; clicking opens a popover with version info and actions.
/// Notifications bell in the primary toolbar (attentions.md — App UI). Shows
/// a count badge for the selected product's open attention groups and opens
/// the singleton Attentions window. Mirrors the `.badge(openGroupCount)`
/// pattern with an overlay (`.badge` only applies inside List/TabView).

/// Shared small numeric badge overlaid on a toolbar glyph. Caps its
/// displayed text using `BackgroundWorkToolbarChrome.badgeCap` so the
/// "99+" rule lives in one place across all toolbar badges.
struct ToolbarCountBadge: View {
    let text: String
    var fill: Color

    var body: some View {
        Text(text)
            .font(.system(size: 9, weight: .bold))
            .foregroundStyle(.white)
            .padding(.horizontal, 4)
            .padding(.vertical, 1)
            .background(Capsule().fill(fill))
            .offset(x: 9, y: -7)
            .fixedSize()
    }
}

struct NotificationsToolbarButton: View {
    @ObservedObject var model: ChatViewModel
    @Environment(\.openWindow) private var openWindow

    private var count: Int { model.openAttentionGroupCount }

    var body: some View {
        Button {
            openWindow(id: "attentions")
        } label: {
            Image(systemName: count > 0 ? "bell.badge" : "bell")
                .overlay(alignment: .topTrailing) {
                    if let badge = BackgroundWorkToolbarChrome.badgeText(count: count) {
                        ToolbarCountBadge(text: badge, fill: Color.red)
                    }
                }
        }
        .help(count > 0
              ? "\(count) notification\(count == 1 ? "" : "s") need your attention"
              : "Notifications")
    }
}

struct UpdateBadgeToolbarButton: View {
    @ObservedObject var updateModel: UpdateModel
    @State private var isPopoverPresented = false

    var body: some View {
        if let update = visibleUpdate {
            Button {
                isPopoverPresented.toggle()
            } label: {
                Image(systemName: "arrow.down.circle.fill")
                    .foregroundStyle(Color.accentColor)
            }
            .help("Update available: Boss \(update.version.description)")
            .popover(isPresented: $isPopoverPresented, arrowEdge: .bottom) {
                UpdateBadgePopover(update: update, updateModel: updateModel) {
                    isPopoverPresented = false
                }
            }
        }
    }

    private var visibleUpdate: AvailableUpdate? {
        guard updateModel.mode != .manual,
              case .available(let update) = updateModel.lastCheckResult,
              update.version.description != updateModel.skippedVersion
        else { return nil }
        return update
    }
}

private struct UpdateBadgePopover: View {
    let update: AvailableUpdate
    @ObservedObject var updateModel: UpdateModel
    let onDismiss: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .center, spacing: 8) {
                Image(systemName: "arrow.down.circle.fill")
                    .foregroundStyle(Color.accentColor)
                    .font(.title3)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Update Available")
                        .font(.headline)
                    Text("Boss \(update.version.description)")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 16)
            .padding(.top, 14)
            .padding(.bottom, 10)

            Divider()

            if !update.changelog.isEmpty || !update.releaseNotes.isEmpty {
                ScrollView {
                    ReleaseNotesContent(changelog: update.changelog, fallbackNotes: update.releaseNotes)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(16)
                }
                .frame(minHeight: 120, maxHeight: 320)

                Divider()
            }

            if let note = downloadStatusNote {
                Text(note)
                    .font(.caption)
                    .foregroundStyle(downloadFailed ? .orange : .secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, 16)
                    .padding(.bottom, 8)
            }
            downloadProgressBar

            HStack(spacing: 8) {
                Button("Skip This Version") {
                    updateModel.skipCurrentVersion()
                    onDismiss()
                }
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
                .font(.callout)

                Spacer(minLength: 0)

                Button("Later") {
                    onDismiss()
                }
                .keyboardShortcut(.cancelAction)

                primaryActionButton
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 10)
        }
        .frame(minWidth: 300, maxWidth: 360)
    }

    /// Trailing call-to-action mirroring ``UpdateResultSheet``: dev builds keep the
    /// manual browser download; release builds stage the bundle in-app and then offer
    /// "Install & Relaunch".
    @ViewBuilder
    private var primaryActionButton: some View {
        if updateModel.isDevBuild {
            Button("Download") {
                NSWorkspace.shared.open(releasePageURL ?? update.assetURL)
                onDismiss()
            }
            .keyboardShortcut(.defaultAction)
        } else {
            switch updateModel.downloadState {
            case .downloading(let v, _) where v == update.version:
                Button {
                } label: {
                    HStack(spacing: 6) {
                        ProgressView().controlSize(.small)
                        Text("Downloading…")
                    }
                }
                .disabled(true)

            case .installFailed(let v, _) where v == update.version:
                // A pre-swap install failure (live bundle intact). Terminal — mirror
                // UpdateResultSheet: show the failure, no re-download affordance. The
                // "Install failed:" reason is carried by `downloadStatusNote`.
                Button("Install Failed") {}
                    .disabled(true)

            case .readyToInstall(let v) where v == update.version:
                Button("Install & Relaunch") {
                    switch UpdateLifecycle.installStagedAndRelaunch() {
                    case .relaunchPending:
                        // Swap applied; request the quit so the helper relaunches us. If
                        // `terminate` returns, the quit was vetoed — the swap is already
                        // on disk and completes on the next quit.
                        NSApplication.shared.terminate(nil)
                        updateModel.markInstalledPendingRelaunch(version: v, willRelaunch: true)
                    case .installedNoRelaunch:
                        updateModel.markInstalledPendingRelaunch(version: v, willRelaunch: false)
                    case .notInstalled:
                        // Nothing changed — the live bundle is intact. Surface the
                        // terminal error; no browser fallback.
                        updateModel.markInstallFailed(
                            version: v,
                            reason: "The app bundle could not be updated. Make sure Boss is installed in /Applications and try again.")
                    }
                    // Keep the popover open so the resulting state is visible.
                }
                .keyboardShortcut(.defaultAction)

            case .installedPendingRelaunch(let v, _) where v == update.version:
                Button("Quit to Finish") {
                    NSApplication.shared.terminate(nil)
                }
                .keyboardShortcut(.defaultAction)

            case .failed(let v, _) where v == update.version:
                Button("Retry Download") {
                    updateModel.downloadAvailableUpdate()
                }
                .keyboardShortcut(.defaultAction)

            default:
                Button("Download") {
                    updateModel.downloadAvailableUpdate()
                }
                .keyboardShortcut(.defaultAction)
            }
        }
    }

    /// The download progress bar, shown only while `update` is actively
    /// downloading. Determinate when the server reported a content length,
    /// indeterminate otherwise. Renders nothing for every other state.
    @ViewBuilder
    private var downloadProgressBar: some View {
        if case .downloading(let v, let progress) = updateModel.downloadState, v == update.version {
            Group {
                switch progress {
                case .determinate(let fraction):
                    ProgressView(value: fraction)
                case .indeterminate:
                    ProgressView()
                }
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 8)
        }
    }

    private var downloadStatusNote: String? {
        switch updateModel.downloadState {
        case .downloading(let v, let progress) where v == update.version:
            switch progress {
            case .determinate(let fraction):
                let pct = Int((fraction * 100).rounded())
                return pct > 0 ? "Downloading… \(pct)%" : "Downloading…"
            case .indeterminate:
                return "Downloading…"
            }
        case .readyToInstall(let v) where v == update.version:
            return "Downloaded and verified. Install & Relaunch to apply."
        case .installedPendingRelaunch(let v, let willRelaunch) where v == update.version:
            return willRelaunch
                ? "Installed. Quit Boss to finish — it will relaunch on the new version."
                : "Installed. Quit and reopen Boss to finish updating."
        case .failed(let v, let reason) where v == update.version:
            return "Download failed: \(reason)"
        case .installFailed(let v, let reason) where v == update.version:
            return "Install failed: \(reason)"
        default:
            return nil
        }
    }

    private var downloadFailed: Bool {
        switch updateModel.downloadState {
        case .failed(let v, _) where v == update.version: return true
        case .installFailed(let v, _) where v == update.version: return true
        default: return false
        }
    }

    private var releasePageURL: URL? {
        URL(string: "https://github.com/spinyfin/mono/releases/tag/\(update.tagName)")
    }
}
