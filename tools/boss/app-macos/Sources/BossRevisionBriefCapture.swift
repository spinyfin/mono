import AppKit
import SwiftUI

/// Captures the task-description viewer with a review brief that matches
/// `render_revision_instructions` (blank line before the first finding,
/// two findings, Review summary tail). Sample markdown lives here, behind
/// the isolated capture-mode guard, so it is not a shipping-app fixture type.
@MainActor
enum BossRevisionBriefCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let model = ChatViewModel(paths: BossEnginePaths.production())
        model.openLoadedMarkdown(title: "PR review revision brief", markdown: engineShapedMarkdown)
        let expanded = CommandLine.arguments.contains("--capture-brief-expanded")
        if expanded {
            model.asyncMarkdownViewerVM.collapsedByDefaultHeadings = []
        }
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1100, height: expanded ? 1400 : 600),
            styleMask: [.titled], backing: .buffered, defer: false
        )
        captureWindow.contentView = NSHostingView(rootView: AsyncMarkdownViewerView()
            .environmentObject(model).environmentObject(model.asyncMarkdownViewerVM))
        window = captureWindow
        DispatchQueue.main.asyncAfter(deadline: .now() + BossCaptureArgs.shared.captureAfter) {
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path)
                NSApp.terminate(nil)
            } catch {
                fputs("revision brief capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }

    /// Quiet-capture sample. Layout matches `render_revision_instructions`
    /// in `tools/boss/engine/pr-review/src/render.rs`. Only `start` reads
    /// this, and `start` returns immediately outside isolated capture mode.
    private static let engineShapedMarkdown = """
    Automated PR review of PR #3304 found 2 finding(s) requiring attention.
    Address ALL findings before finalising this revision.

    ## HARD RULE: no punting — do the actual work

    Each finding below requires a real code change that resolves it.
    The following are FORBIDDEN — they do NOT count as addressing a finding:

    - Filing a follow-up task or issue in lieu of fixing the finding.
    - Leaving a `TODO`, `FIXME`, `Phase N`, "wire this later", or similar
    deferral comment and calling the finding done.
    - Acknowledging the finding in a comment or PR description without making
    the substantive code change it requires.
    - Marking the revision complete while a finding's core work is unaddressed.

    For example: a "write-only field" finding requires wiring the READ/consumer
    side so the field is actually used — not a TODO comment promising to do so
    later, and not a follow-up task filed for someone else.

    **If — and only if — a finding genuinely cannot or should not be fixed within
    this revision's scope**, you must EXPLICITLY SURFACE that in your response:
    state which finding you are not fixing and exactly why (technical blocker,
    out-of-scope dependency, requires a decision from the repo owner, etc.). Do NOT silently
    file a follow-up or leave a TODO. The default is to do the work.

    ### [high] Workspace result admission ignores negotiated frame/credit limits; unsendable result wedges the daemon
    **File:** `tools/boss/engine/host-execution/src/lib.rs` (workspace::Executor::dispatch result-size ensure ~L1762-1766)  
    **Category:** correctness  
    **Confidence:** medium

    Verified: dispatch only ensures the result fits the local write buffer.

    ### [medium] Quiet capture should pin the engine blank-line boundary
    **File:** `tools/boss/app-macos/Tests/BossTests/RevisionBriefCaptureFixture.swift`  
    **Category:** tests  
    **Confidence:** medium

    Second finding keeps the multi-finding sequence and summary tail in the fixture.

    **Review summary:** Two findings require a fix.
    """
}
