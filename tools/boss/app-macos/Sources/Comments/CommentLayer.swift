@preconcurrency import AppKit
import os.log
import SwiftUI

private let anchorLog = Logger(subsystem: "com.boss.app", category: "CommentPopupAnchor")
/// Stable category for window↔view coordinate-bridge instrumentation. Both the comment
/// popover anchor and the right-click context menu funnel their NSEvent.locationInWindow
/// through `windowPointToView(_:in:)`, which logs here. Keep these lines in place — they
/// are the regression tripwire for the SwiftUI/AppKit isFlipped mismatch that produced
/// the FOURTH-report top↔bottom mirror in the markdown viewer.
///
/// Stream live with:
///   log stream --predicate 'subsystem == "com.boss.markdown" AND category == "coordinates"' --style compact
private let coordLog = Logger(subsystem: "com.boss.markdown", category: "coordinates")

/// Owns the comment array for a single markdown viewer instance and
/// coordinates the selection → authoring → sidebar → highlight flow.
///
/// Since P529 Phase 2 the layer is engine-backed: when [`configure`] is given an
/// artifact id and a [`CommentBackend`], comments load via `comments_list`,
/// persist via `comments_create`/`comments_dismiss`, re-anchor via
/// `comments_resolve`, and stay live via the `comments.artifact.*` subscription
/// — so they survive the viewer closing and the app restarting. Absent an
/// artifact (an artifact-less viewer, or a unit test), it degrades to the
/// Phase-1 in-memory behaviour so those surfaces keep working unchanged.
@MainActor
final class CommentLayer: NSObject, ObservableObject {
    @Published var comments: [Comment] = []
    @Published var isShowingPopover: Bool = false
    @Published var pendingQuotedText: String = ""
    /// Character that seeded the form via type-to-comment entry path.
    @Published var pendingFirstChar: Character? = nil
    /// Anchor of the comment just clicked in the sidebar; clears after the flash.
    @Published var flashingAnchor: CommentAnchor? = nil
    /// Soft-dismiss "show resolved" sidebar toggle (design § "Soft-dismiss").
    /// Resolved/orphaned comments are hidden from the active sidebar unless this
    /// is on; flipping it re-lists with `include_resolved`.
    @Published var showResolved: Bool = false {
        didSet { if showResolved != oldValue { reload() } }
    }

    // MARK: - Engine backing

    /// The artifact these comments attach to (`work_item` / `pr_doc`). Empty on
    /// the artifact-less in-memory path.
    private(set) var artifactKind: String = ""
    private(set) var artifactId: String = ""
    /// The raw markdown the viewer renders; the plain-text projection (for
    /// anchoring + `doc_version`) is derived from it.
    private var source: String = ""
    private var baseURL: URL? = nil
    /// The engine facade. `nil` ⇒ in-memory fallback.
    private weak var backend: (any CommentBackend)? = nil

    /// True when this layer is persisting through the engine.
    var isEngineBacked: Bool { backend != nil && !artifactId.isEmpty }

    /// Occurrence index computed at selection time; consumed by addComment().
    private var pendingOccurrenceIndex: Int = 0
    /// Text that precedes the selection in the Textual NSTextInteractionView path,
    /// captured at mouseUp. Used to count prior occurrences of the quoted text.
    private var anchorTextBeforeSelection: String? = nil

    /// The engine's `[Revise]`-banner summary, fetched via `CommentsBannerState`
    /// alongside every `reload()`. `nil` until the first fetch lands, or always
    /// on the artifact-less fallback (no engine to fetch from) — [`bannerState`]
    /// derives locally from `comments` in that case.
    @Published private var fetchedBannerState: CommentsBannerState? = nil
    /// Transient feedback from the last `CommentsReviseDoc` reply, for the
    /// outcomes that don't already show up via a comment reload
    /// (`NoUnresolvedComments` / `AlreadyInFlight` / `NotApplicable` — a
    /// `Created` outcome is reflected by the topic-invalidation-triggered
    /// reload instead). Clears itself a few seconds after being set.
    @Published private(set) var reviseDocMessage: String? = nil

    // NSEvent monitor tokens; stored nonisolated(unsafe) because the opaque Any
    // tokens are installed/removed only on the main actor.
    nonisolated(unsafe) private var keyMonitor: Any?
    nonisolated(unsafe) private var rightClickMonitor: Any?
    nonisolated(unsafe) private var mouseUpMonitor: Any?

    /// The NSTextView whose selection seeded the pending comment request.
    /// Captured from NSTextView.didChangeSelectionNotification (the object is the text view).
    /// Queried at present-time via firstRect(forCharacterRange:) — never cached as screen coords.
    private weak var anchorTextView: NSTextView?

    // MARK: - Textual/NSTextInteractionView anchor
    //
    // Textual's NSTextInteractionView does NOT post NSTextView.didChangeSelectionNotification,
    // so anchorTextView is never populated from StructuredText selections. Instead we install
    // a leftMouseUp monitor and capture the mouse position (in screen coords) at the moment
    // the user finishes dragging/clicking to make a selection. resolveAnchor() uses this as
    // the popup anchor when anchorTextView is nil.

    /// Screen-space point saved on leftMouseUp while a non-NSTextView first-responder has a selection.
    private var anchorInteractionScreenPoint: NSPoint?
    /// The NSView (Textual's NSTextInteractionView) that owned the selection at mouseUp time.
    private weak var anchorInteractionView: NSView?

    /// The live NSPopover, if one is currently visible.
    private var activePopover: NSPopover?

    // MARK: - Monitor lifecycle

    func installMonitors() {
        // ObjC selector form avoids the @Sendable closure constraint on the block-based
        // addObserver API, which would make `notification` a sending parameter and prevent
        // capturing the non-Sendable NSTextView inside assumeIsolated.
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(textViewSelectionDidChange(_:)),
            name: NSTextView.didChangeSelectionNotification,
            object: nil
        )
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self else { return event }
            let chars = event.charactersIgnoringModifiers
            let mods = event.modifierFlags
            let consume = MainActor.assumeIsolated {
                self.shouldConsumeKeyEvent(chars: chars, mods: mods)
            }
            return consume ? nil : event
        }
        rightClickMonitor = NSEvent.addLocalMonitorForEvents(matching: .rightMouseDown) { [weak self] event in
            guard let self else { return event }
            let loc = event.locationInWindow
            let win = event.window
            let consume = MainActor.assumeIsolated {
                self.handleRightClick(locationInWindow: loc, window: win)
            }
            return consume ? nil : event
        }
        // Capture selection endpoint for Textual's NSTextInteractionView, which does not
        // post NSTextView.didChangeSelectionNotification.
        mouseUpMonitor = NSEvent.addLocalMonitorForEvents(matching: .leftMouseUp) { [weak self] event in
            guard let self else { return event }
            let loc = event.locationInWindow
            let win = event.window
            MainActor.assumeIsolated {
                self.captureInteractionAnchor(locationInWindow: loc, window: win)
            }
            return event
        }
    }

    func removeMonitors() {
        if let m = keyMonitor { NSEvent.removeMonitor(m); keyMonitor = nil }
        if let m = rightClickMonitor { NSEvent.removeMonitor(m); rightClickMonitor = nil }
        if let m = mouseUpMonitor { NSEvent.removeMonitor(m); mouseUpMonitor = nil }
        NotificationCenter.default.removeObserver(
            self, name: NSTextView.didChangeSelectionNotification, object: nil)
        activePopover?.close()
        anchorInteractionScreenPoint = nil
        anchorInteractionView = nil
        anchorTextBeforeSelection = nil
    }

    /// Single coordinate-bridge between an NSEvent's window-space location and a target
    /// view's coordinate system. Both popover-anchor capture and the right-click context
    /// menu call this — any future surface that opens a popover/menu in response to a
    /// click MUST also route through here, never pass `event.locationInWindow` directly
    /// to APIs whose point argument is documented in view coords.
    ///
    /// Why this helper exists: when the window's contentView is SwiftUI's NSHostingView
    /// (`isFlipped == true`) but the NSWindow is bottom-left origin, passing
    /// `event.locationInWindow` straight to APIs like `NSMenu.popUp(at:in:)` — whose
    /// `at:` is in the receiving view's coordinate system — produces a `viewHeight - y`
    /// inversion. A click near the top of the document opens the menu near the bottom
    /// and vice versa (the top↔bottom mirror reported four times against the markdown
    /// viewer). `view.convert(_, from: nil)` is the AppKit-blessed bridge that walks
    /// the view hierarchy and applies every `isFlipped` along the way.
    private func windowPointToView(_ pointInWindow: NSPoint, in view: NSView) -> NSPoint {
        let pointInView = view.convert(pointInWindow, from: nil)
        coordLog.info("windowPointToView: window=\(NSStringFromPoint(pointInWindow)) → view=\(NSStringFromPoint(pointInView)) (\(NSStringFromClass(type(of: view))) isFlipped=\(view.isFlipped) bounds=\(NSStringFromRect(view.bounds)))")
        return pointInView
    }

    /// Called on leftMouseUp. If the current first responder is a non-NSTextView view
    /// (Textual's NSTextInteractionView) whose bounds contain the mouseUp point and there
    /// is a live selection, saves the screen-space anchor for use by resolveAnchor().
    /// The bounds-containment guard prevents the "Add Comment" button click (which lands
    /// outside the text view) from overwriting a valid earlier anchor.
    private func captureInteractionAnchor(locationInWindow: NSPoint, window: NSWindow?) {
        guard !isShowingPopover, let window else { return }
        guard let responder = window.firstResponder as? NSView,
              !(responder is NSTextView) else { return }
        let pointInResponder = windowPointToView(locationInWindow, in: responder)
        guard responder.bounds.contains(pointInResponder) else {
            anchorLog.info("captureInteractionAnchor: mouseUp outside responder bounds (\(NSStringFromClass(type(of: responder))) bounds=\(NSStringFromRect(responder.bounds)) point=\(NSStringFromPoint(pointInResponder))) — anchor not updated")
            return
        }
        guard hasCurrentSelection() else { return }
        let screenOrigin = window.convertToScreen(
            NSRect(origin: locationInWindow, size: CGSize(width: 1, height: 1))
        ).origin
        anchorInteractionScreenPoint = screenOrigin
        anchorInteractionView = responder
        anchorLog.info("captureInteractionAnchor: stored screen anchor \(NSStringFromPoint(screenOrigin)) responder=\(NSStringFromClass(type(of: responder))) bounds=\(NSStringFromRect(responder.bounds))")

        // Capture the text that precedes the selection so requestNewComment() can
        // compute which occurrence of the quoted text the user has selected.
        // NSTextInteractionView (Textual's first responder) conforms to NSTextInputClient,
        // which exposes selectedRange() and attributedSubstring(forProposedRange:actualRange:).
        // If the cast fails on a future AppKit version the fallback is occurrenceIndex=0.
        anchorTextBeforeSelection = nil
        if let inputClient = responder as? NSTextInputClient {
            let selRange = inputClient.selectedRange()
            if selRange.location != NSNotFound, selRange.length > 0 {
                if selRange.location == 0 {
                    anchorTextBeforeSelection = ""
                } else {
                    let beforeRange = NSRange(location: 0, length: selRange.location)
                    anchorTextBeforeSelection = inputClient
                        .attributedSubstring(forProposedRange: beforeRange, actualRange: nil)?
                        .string
                }
            }
        }
    }

    /// Called by NotificationCenter on the main thread when any NSTextView changes selection.
    /// Using the ObjC selector form avoids @Sendable parameter constraints that prevent
    /// capturing the non-Sendable NSTextView across a @Sendable closure boundary.
    @objc nonisolated private func textViewSelectionDidChange(_ notification: Notification) {
        let textView = notification.object as? NSTextView
        MainActor.assumeIsolated { [weak self] in
            guard let self, !self.isShowingPopover else { return }
            // Only update while the popover is closed; the comment form's own
            // NSTextView (CommentTextEditor) would otherwise overwrite the anchor.
            self.anchorTextView = textView
            // An NSTextView firing means focus moved to an NSTextView area; clear the
            // Textual interaction anchor so the NSTextView path runs in resolveAnchor().
            self.anchorInteractionScreenPoint = nil
            self.anchorInteractionView = nil
            self.anchorTextBeforeSelection = nil
            if let tv = textView {
                let range = tv.selectedRange()
                anchorLog.info("textViewSelectionDidChange: NSTextView \(NSStringFromClass(type(of: tv))) range=\(NSStringFromRange(range))")
            }
        }
    }

    // MARK: - Authoring

    func requestNewComment(firstChar: Character? = nil) {
        pendingQuotedText = captureCurrentSelection() ?? ""
        pendingOccurrenceIndex = computeOccurrenceIndex(for: pendingQuotedText)
        pendingFirstChar = firstChar

        guard let (posRect, posView) = resolveAnchor() else {
            anchorLog.error("requestNewComment: resolveAnchor returned nil — popover not shown")
            return
        }

        anchorLog.info("requestNewComment: showing popover relativeTo=\(NSStringFromRect(posRect)) of=\(NSStringFromClass(type(of: posView))) isFlipped=\(posView.isFlipped)")
        coordLog.info("requestNewComment: popover relativeTo \(NSStringFromRect(posRect)) of \(NSStringFromClass(type(of: posView))) isFlipped=\(posView.isFlipped) bounds=\(NSStringFromRect(posView.bounds))")

        let popover = NSPopover()
        popover.contentViewController = NSHostingController(
            rootView: CommentPopover(layer: self)
        )
        // Transient: clicks outside the popover dismiss it automatically, matching
        // the previous SwiftUI .popover default behaviour.
        popover.behavior = .transient
        // NSPopover.delegate is weak; self outlives the popover so this is safe.
        popover.delegate = self
        activePopover = popover
        isShowingPopover = true

        popover.show(relativeTo: posRect, of: posView, preferredEdge: .maxY)
    }

    func addComment(quoted: String, body: String) {
        let trimmed = body.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }

        // Build the W3C anchor from the plain-text projection: locate the
        // selected occurrence, take the verbatim `exact` span plus ~64 chars of
        // `prefix`/`suffix` context around it. Falls back to a bare `exact`
        // anchor when there is no projection (artifact-less tests).
        let projection = currentProjection()
        let anchor = Self.captureAnchor(
            quoted: quoted, occurrenceIndex: pendingOccurrenceIndex, in: projection)

        resetAuthoringState()

        if let backend, !artifactId.isEmpty {
            // Engine-backed: persist and let the reload (triggered by the
            // engine's echo + topic invalidation) materialise the comment.
            backend.createComment(
                artifactKind: artifactKind,
                artifactId: artifactId,
                anchor: anchor,
                body: trimmed,
                docVersion: CommentProjection.docVersion(forPlainText: projection)
            )
        } else {
            // In-memory fallback (artifact-less viewer / unit test).
            let comment = Comment(
                id: "local:\(UUID().uuidString)",
                anchor: anchor,
                body: trimmed,
                author: CommentAuthor.current,
                createdAt: Date()
            )
            comments.append(comment)
        }
    }

    /// Close the popover and clear the pending-authoring scratch state.
    private func resetAuthoringState() {
        activePopover?.close()
        activePopover = nil
        isShowingPopover = false
        pendingQuotedText = ""
        pendingOccurrenceIndex = 0
        pendingFirstChar = nil
    }

    func cancelNewComment() {
        activePopover?.close()
    }

    /// Soft-dismiss. Engine-backed comments transition to `resolved` (hidden
    /// unless `showResolved`) via `comments_dismiss`; in-memory comments are
    /// removed outright.
    func dismiss(_ comment: Comment) {
        if let backend, isEngineBacked, !comment.id.hasPrefix("local:") {
            backend.dismissComment(commentId: comment.id)
        } else {
            comments.removeAll { $0.id == comment.id }
        }
    }

    // MARK: - Engine backing lifecycle

    /// Bind this layer to a viewer's document and (optionally) the engine.
    /// Called from the view modifier's `onAppear`. When `artifact` is non-nil the
    /// layer registers with the backend and loads persisted comments; otherwise
    /// it stays in in-memory mode.
    func configure(
        source: String,
        baseURL: URL?,
        artifact: CommentArtifactRef?,
        backend: (any CommentBackend)?
    ) {
        self.source = source
        self.baseURL = baseURL
        guard let artifact, let backend else { return }
        // Re-configuring the same artifact is a no-op beyond refreshing source.
        if artifactId == artifact.id, self.backend != nil { return }
        self.artifactKind = artifact.kind
        self.artifactId = artifact.id
        self.backend = backend
        backend.registerCommentLayer(self, artifactKind: artifact.kind, artifactId: artifact.id)
        reload()
    }

    /// Unbind from the engine (view `onDisappear`). Keeps monitors teardown a
    /// separate concern so the modifier can order them.
    func unbindFromEngine() {
        backend?.unregisterCommentLayer(self)
    }

    /// Update the rendered source (the viewer may load it asynchronously, after
    /// `configure`). Re-resolves anchors against the freshly-available projection
    /// so highlights + the ⚠/orphan glyphs settle once the doc text arrives.
    func updateSource(_ source: String, baseURL: URL?) {
        guard source != self.source else { return }
        self.source = source
        self.baseURL = baseURL
        guard let backend, isEngineBacked else { return }
        let projection = currentProjection()
        if !projection.isEmpty {
            backend.resolveComments(
                artifactKind: artifactKind, artifactId: artifactId, plainText: projection)
        }
    }

    /// Re-fetch the artifact's comments and re-resolve their anchors against the
    /// current projection. No-op when not engine-backed. Invoked on load, on the
    /// engine's single-comment echo, and on `comments.artifact.*` invalidations.
    func reload() {
        guard let backend, isEngineBacked else { return }
        backend.listComments(
            artifactKind: artifactKind, artifactId: artifactId, includeResolved: showResolved)
        backend.fetchBannerState(artifactKind: artifactKind, artifactId: artifactId)
        let projection = currentProjection()
        if !projection.isEmpty {
            backend.resolveComments(
                artifactKind: artifactKind, artifactId: artifactId, plainText: projection)
        }
    }

    /// Apply a `comments_banner_state` reply.
    func applyBannerState(_ state: CommentsBannerState) {
        fetchedBannerState = state
    }

    /// Apply a `comments_list` reply: rebuild the comment array from the engine's
    /// authoritative rows + thread entries.
    func applyList(_ wire: [CommentWithThread]) {
        // Resolution outcomes arrive on a separate `comments_resolved` reply;
        // preserve the last-known one per id so the ⚠/anchor-lost glyph doesn't
        // flicker off between a list refresh and the following resolve.
        let priorResolved: [String: ResolvedWith] = Dictionary(
            comments.compactMap { c in c.lastResolvedWith.map { (c.id, $0) } },
            uniquingKeysWith: { first, _ in first }
        )
        comments = wire.map { cw in
            var c = Comment.from(
                cw.comment,
                threadEntries: cw.threadEntries,
                answerAgentRunning: cw.answerAgentRunning,
                answerAgentFailed: cw.answerAgentFailed
            )
            if c.lastResolvedWith == nil { c.lastResolvedWith = priorResolved[c.id] }
            return c
        }
    }

    /// Apply a `comments_resolved` reply: stamp each comment's anchor-resolution
    /// outcome (drives the ⚠ fuzzy glyph and the anchor-lost badge). The engine
    /// has already persisted fuzzy re-anchors and orphan flips server-side, so
    /// this only reflects them into the UI.
    func applyResolved(_ resolved: [ResolvedComment]) {
        let byId = Dictionary(resolved.map { ($0.comment.id, $0) }, uniquingKeysWith: { first, _ in first })
        for i in comments.indices {
            guard let rc = byId[comments[i].id] else { continue }
            comments[i].lastResolvedWith = ResolvedWith(rawValue: rc.resolution.kind) ?? .exact
            if rc.resolution.isOrphan { comments[i].status = .orphaned }
        }
    }

    /// The rendered plain-text projection of the current source — the space the
    /// W3C anchor and `doc_version` live in. Computed identically to what
    /// [`HighlightingMarkdownParser`] paints against.
    func currentProjection() -> String {
        guard !source.isEmpty else { return "" }
        return CommentProjection.plainText(for: source, baseURL: baseURL)
    }

    /// Build a W3C `{exact, prefix, suffix}` anchor for the `occurrenceIndex`-th
    /// occurrence of `quoted` in the plain-text projection. `exact` is taken
    /// verbatim from the projection (normalising the pasteboard whitespace), with
    /// up to 64 chars of surrounding context. Falls back to a bare `exact` anchor
    /// when the projection is empty or the occurrence can't be located.
    static func captureAnchor(quoted: String, occurrenceIndex: Int, in plain: String) -> CommentAnchor {
        let contextLength = 64
        guard !plain.isEmpty else { return CommentAnchor(exact: quoted) }
        let ranges = HighlightingMarkdownParser.flexibleMatchRanges(of: quoted, in: plain)
        guard occurrenceIndex >= 0, occurrenceIndex < ranges.count else {
            return CommentAnchor(exact: quoted)
        }
        let range = ranges[occurrenceIndex]
        let exact = String(plain[range])
        let prefixStart = plain.index(range.lowerBound, offsetBy: -contextLength, limitedBy: plain.startIndex)
            ?? plain.startIndex
        let suffixEnd = plain.index(range.upperBound, offsetBy: contextLength, limitedBy: plain.endIndex)
            ?? plain.endIndex
        let prefix = String(plain[prefixStart..<range.lowerBound])
        let suffix = String(plain[range.upperBound..<suffixEnd])
        return CommentAnchor(exact: exact, prefix: prefix, suffix: suffix)
    }

    // MARK: - Intent classification badge

    /// Manually (re)classifies a comment. Engine-backed: calls the real
    /// `CommentsSetIntent` RPC, which sets `intent_overridden_by='user'` and
    /// re-runs routing from the new intent's entry point server-side — no
    /// local mutation here, since the RPC's `comment_result` echo (routed
    /// through `CommentEngineBridge.handleCommentResult`) reloads this layer,
    /// so the badge, nudge, and revise banner all settle on the engine's
    /// authoritative state rather than a client-side guess. Artifact-less
    /// fallback (no persistence to defer to): simulates the same override +
    /// routing locally so that surface keeps working.
    func setIntent(_ intent: CommentIntent, for comment: Comment) {
        if let backend, isEngineBacked {
            backend.setIntent(commentId: comment.id, intent: intent.rawValue)
            return
        }
        guard let index = comments.firstIndex(where: { $0.id == comment.id }) else { return }
        comments[index].intent = intent
        comments[index].intentOverriddenByUser = true

        // Mirrors the engine posting an `entry_kind='nudge'` thread entry
        // immediately on directive/larger_change classification (design §
        // "Buckets 1 & 3 — unified"), before `[Revise]` is ever clicked.
        // One-shot: a comment already carrying a nudge entry doesn't get a
        // second one on reclassification.
        if intent == .directive || intent == .largerChange {
            let alreadyNudged = comments[index].threadEntries.contains { $0.entryKind == .nudge }
            if !alreadyNudged {
                comments[index].threadEntries.append(
                    CommentThreadEntry(
                        id: UUID().uuidString,
                        entryKind: .nudge,
                        author: "engine",
                        body: Self.nudgeBody,
                        createdAt: Date()
                    )
                )
            }
        } else if intent == .question, comments[index].status != .answering {
            // Mirrors the engine's `classifying` → `answering` transition
            // (design § "Comment/thread state machine"): classification as
            // `question` immediately spawns the answer agent. Guarded on not
            // already `.answering` so reclassifying to `question` twice in a
            // row doesn't spawn a second run.
            comments[index].status = .answering
            runAnswerAgent(for: comment.id)
        }
    }

    // MARK: - Bucket 2: answer agent + follow-up loop

    /// Canned reply used only by the artifact-less fallback, which has no
    /// engine to run a real answer agent against. Mirrors an
    /// `entry_kind='answer'` thread entry (design § "Bucket 2").
    static let stubAnswerBody =
        "This is a stubbed answer — real answer-agent replies require an engine-backed artifact."

    /// Artifact-less-fallback-only simulation of the answer agent: after a
    /// short "thinking" delay, posts an `answer` thread entry and flips
    /// `answering → answered`. No-ops if the comment has moved off
    /// `.answering` in the meantime (e.g. dismissed, or manually reclassified
    /// away). Never called on the engine-backed path — there, answer entries
    /// arrive as real `comment_thread_entries` posted by the engine's answer
    /// agent via `CommentsPostAnswer` (worker-callable only), and ride in on
    /// the normal `reload()`/topic-invalidation path.
    private func runAnswerAgent(for commentId: String) {
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: .seconds(1.5))
            guard let self, let index = self.comments.firstIndex(where: { $0.id == commentId }) else { return }
            guard self.comments[index].status == .answering else { return }
            self.comments[index].threadEntries.append(
                CommentThreadEntry(
                    id: UUID().uuidString,
                    entryKind: .answer,
                    author: "engine",
                    body: Self.stubAnswerBody,
                    createdAt: Date()
                )
            )
            self.comments[index].status = .answered
        }
    }

    /// The operator's reply in an `answered` bucket-2 thread. Engine-backed:
    /// calls the real `CommentsPostFollowup` RPC, which appends the
    /// `operator_followup` thread entry, transitions the comment to
    /// `awaiting_followup`, and kicks off the async reclassifier server-side
    /// — its loop-vs-bridge outcome arrives on the artifact's comment-topic
    /// push, which `reload()` picks up, so no local mutation is needed here.
    /// Artifact-less fallback (no engine to persist to): simulates the same
    /// transition locally so that surface keeps working.
    func postFollowup(body: String, for comment: Comment) {
        let trimmed = body.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, comment.status == .answered else { return }
        if let backend, isEngineBacked {
            backend.postFollowup(commentId: comment.id, body: trimmed)
            return
        }
        guard let index = comments.firstIndex(where: { $0.id == comment.id }) else { return }
        comments[index].threadEntries.append(
            CommentThreadEntry(
                id: UUID().uuidString,
                entryKind: .operatorFollowup,
                author: "you",
                body: trimmed,
                createdAt: Date()
            )
        )
        comments[index].status = .awaitingFollowup
    }

    // MARK: - `[Revise]` banner + chips

    /// Matches the engine's `NUDGE_BODY` (`engine/core/src/app/comments.rs`).
    static let nudgeBody = "This looks like it wants a doc change — click [Revise] to start one."

    /// Local stand-in for the engine's `next_id("task")` allocator, used only
    /// by the artifact-less fallback path of `reviseDoc()` to synthesize a
    /// `revise_task_id` (there's no engine to mint a real one for).
    private var revisionCounter = 0

    /// Read-only `[Revise]`-banner summary. Engine-backed: the last
    /// `CommentsBannerState` fetched alongside `reload()`. Artifact-less
    /// fallback (no engine to fetch from): derived locally from `comments`,
    /// mirroring the RPC's own `revisable` rule (at least one active
    /// directive/larger_change comment to batch).
    var bannerState: CommentsBannerState {
        if let fetchedBannerState { return fetchedBannerState }
        let unresolvedCount = comments.filter {
            $0.status == .active && ($0.intent == .directive || $0.intent == .largerChange)
        }.count
        let inRevisionCount = comments.filter { $0.status == .inRevision }.count
        return CommentsBannerState(revisable: unresolvedCount > 0, unresolvedCount: unresolvedCount, inRevisionCount: inRevisionCount)
    }

    /// The `[Revise]`-banner action: batch-address every unaddressed
    /// directive/larger_change comment. Engine-backed: calls the real
    /// `CommentsReviseDoc` RPC. On success the engine flips the addressed
    /// comments to `in_revision` and publishes a `comment_topic`
    /// invalidation, which reloads this layer with the real task id — no
    /// local mutation needed here; `applyReviseDocOutcome` handles the reply
    /// for the outcomes that don't already trigger a reload (nothing to
    /// address, or a race with another batch). Artifact-less fallback:
    /// simulates the same guarded `active` → `in_revision` batch transition
    /// locally, since there's no engine to persist it.
    func reviseDoc() {
        if let backend, isEngineBacked {
            backend.reviseDoc(artifactKind: artifactKind, artifactId: artifactId)
            return
        }
        let indices = comments.indices.filter {
            comments[$0].status == .active && (comments[$0].intent == .directive || comments[$0].intent == .largerChange)
        }
        guard !indices.isEmpty else { return }
        revisionCounter += 1
        let taskId = "T-local-\(revisionCounter)"
        for i in indices {
            comments[i].status = .inRevision
            comments[i].reviseTaskId = taskId
            comments[i].statusActor = "engine"
            // The nudge entry's `revise_task_id` postdates the entry itself —
            // filled in only once a batch actually claims the comment.
            if let nudgeIndex = comments[i].threadEntries.firstIndex(where: { $0.entryKind == .nudge }) {
                comments[i].threadEntries[nudgeIndex].reviseTaskId = taskId
            }
        }
    }

    /// Applies the `CommentsReviseDoc` reply once it arrives. `Created` is
    /// already reflected by the invalidation-triggered `reload()` (the real
    /// `revise_task_id` comes from the wire); the other outcomes don't touch
    /// any comment row, so they only need a transient message telling the
    /// operator why nothing changed.
    func applyReviseDocOutcome(_ outcome: ReviseDocOutcome) {
        switch outcome {
        case .created:
            return
        case .noUnresolvedComments:
            reviseDocMessage = "No unresolved comments to revise."
        case .alreadyInFlight(let taskId):
            reviseDocMessage = "Already being revised as \(taskId)."
        case .notApplicable(let reason):
            reviseDocMessage = "Can't revise this document: \(reason)"
        }
        Task { @MainActor [weak self, message = reviseDocMessage] in
            try? await Task.sleep(for: .seconds(4))
            if self?.reviseDocMessage == message { self?.reviseDocMessage = nil }
        }
    }

    // MARK: - Click-to-jump

    func jumpTo(_ comment: Comment) {
        let anchor = comment.anchor
        flashingAnchor = anchor
        Task {
            try? await Task.sleep(for: .milliseconds(900))
            if flashingAnchor == anchor { flashingAnchor = nil }
        }
    }

    // MARK: - Selection helpers

    /// Returns the 0-based occurrence index of `quotedText` that the user had selected.
    ///
    /// NSTextView path: reads the current selectedRange() directly — the selection is still
    /// live at this call site (inside requestNewComment, before the popover opens).
    ///
    /// Textual NSTextInteractionView path: uses `anchorTextBeforeSelection`, which was
    /// captured at mouseUp time via NSTextInputClient. Counts occurrences of `quotedText`
    /// in the prefix to get the index of the selected occurrence.
    ///
    /// Falls back to 0 when position information is unavailable (e.g. Textual's
    /// NSTextInteractionView does not expose NSTextInputClient on the current OS version).
    private func computeOccurrenceIndex(for quotedText: String) -> Int {
        guard !quotedText.isEmpty else { return 0 }

        // NSTextView path (header text fields and other non-Textual text views).
        if let tv = anchorTextView {
            let range = tv.selectedRange()
            if range.length > 0, range.location != NSNotFound {
                return countPriorOccurrences(of: quotedText, beforeNSOffset: range.location, in: tv.string)
            }
        }

        // Textual NSTextInteractionView path — pre-captured text before the selection.
        if let textBefore = anchorTextBeforeSelection {
            return countOccurrencesIn(prefix: textBefore, of: quotedText)
        }

        return 0
    }

    /// Counts occurrences of `text` that end strictly before `offset` (UTF-16) in `fullText`.
    private func countPriorOccurrences(of text: String, beforeNSOffset offset: Int, in fullText: String) -> Int {
        let nsText = fullText as NSString
        var count = 0
        var from = 0
        while from < offset {
            let found = nsText.range(of: text, options: [], range: NSRange(location: from, length: offset - from))
            if found.location == NSNotFound { break }
            count += 1
            from = found.location + found.length
        }
        return count
    }

    /// Counts non-overlapping occurrences of `text` in `prefix` (Swift string, Unicode-safe).
    private func countOccurrencesIn(prefix: String, of text: String) -> Int {
        var count = 0
        var from = prefix.startIndex
        while let range = prefix.range(of: text, range: from..<prefix.endIndex) {
            count += 1
            from = range.upperBound
        }
        return count
    }

    /// Non-destructively checks whether the current first responder has a text selection
    /// by asking it to validate the "Copy" UI item. Textual's NSTextInteractionView
    /// implements NSUserInterfaceValidations: validateUserInterfaceItem returns true for
    /// "copy" only when there is a non-empty selection.
    func hasCurrentSelection() -> Bool {
        guard let firstResponder = NSApp.keyWindow?.firstResponder else { return false }
        let copyItem = NSMenuItem(
            title: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        var responder: NSResponder? = firstResponder
        while let current = responder {
            if let validator = current as? NSUserInterfaceValidations {
                return validator.validateUserInterfaceItem(copyItem)
            }
            responder = current.nextResponder
        }
        return false
    }

    /// Returns the positioning rect (in positioningView coords) and view for NSPopover.
    ///
    /// Priority order:
    ///   1. Textual NSTextInteractionView anchor — mouseUp screen point captured at
    ///      selection-end. Textual does not post NSTextView.didChangeSelectionNotification,
    ///      so the NSTextView path below never fires for StructuredText.
    ///   2. NSTextView anchor — standard path for non-Textual text views (header Text fields).
    ///   3. Fallback near the top of the key window.
    private func resolveAnchor() -> (NSRect, NSView)? {
        // --- 1. Textual NSTextInteractionView anchor ---
        if let view = anchorInteractionView,
           let window = view.window,
           let screenPoint = anchorInteractionScreenPoint {
            // Convert saved screen point → window coords → view (flipped) coords.
            let windowOrigin = window.convertFromScreen(
                NSRect(origin: screenPoint, size: CGSize(width: 1, height: 1))
            ).origin
            let viewPoint = view.convert(windowOrigin, from: nil)
            // A 1-pt-high rect so the popover arrow tip lands at the character baseline.
            let viewRect = NSRect(x: viewPoint.x - 20, y: viewPoint.y, width: 40, height: 1)
            anchorLog.info("resolveAnchor: Textual path — screenPoint=\(NSStringFromPoint(screenPoint)) windowOrigin=\(NSStringFromPoint(windowOrigin)) viewPoint=\(NSStringFromPoint(viewPoint)) viewRect=\(NSStringFromRect(viewRect)) view.isFlipped=\(view.isFlipped)")
            return (viewRect, view)
        }

        // --- 2. NSTextView anchor ---
        if let tv = anchorTextView, let window = tv.window {
            let range = tv.selectedRange()
            anchorLog.info("resolveAnchor: NSTextView path — range=\(NSStringFromRange(range))")
            if range.length > 0, range.location != NSNotFound {
                let lastCharRange = NSRange(location: range.upperBound - 1, length: 1)
                var actualRange = NSRange()
                let screenRect = tv.firstRect(forCharacterRange: lastCharRange, actualRange: &actualRange)
                if screenRect != .zero {
                    // screen → window → text-view coordinates; AppKit handles the conversion
                    // correctly for any display arrangement without explicit screen lookup.
                    let windowRect = window.convertFromScreen(screenRect)
                    let viewRect = tv.convert(windowRect, from: nil)
                    anchorLog.info("resolveAnchor: NSTextView anchor screenRect=\(NSStringFromRect(screenRect)) viewRect=\(NSStringFromRect(viewRect))")
                    return (viewRect, tv)
                }
            }
        }

        // --- 3. Fallback ---
        // Place anchor near the top-left of the key window's content view.
        // Uses minY (top in flipped SwiftUI hosting views) + small offset.
        if let contentView = NSApp.keyWindow?.contentView {
            let topY: CGFloat = contentView.isFlipped
                ? 60                              // flipped: y=0 is top, increase down
                : contentView.bounds.maxY - 60    // non-flipped: maxY is top
            let fallback = NSRect(
                x: contentView.bounds.midX - 8,
                y: topY,
                width: 16,
                height: 16
            )
            anchorLog.warning("resolveAnchor: fallback — anchorTextView=\(String(describing: self.anchorTextView)) anchorInteractionView=\(String(describing: self.anchorInteractionView)) fallback=\(NSStringFromRect(fallback)) isFlipped=\(contentView.isFlipped)")
            return (fallback, contentView)
        }
        return nil
    }

    /// Reads the selection via pasteboard copy. Acceptable Phase 1 trade-off:
    /// called only when the user explicitly opens the comment form.
    private func captureCurrentSelection() -> String? {
        let before = NSPasteboard.general.changeCount
        NSApp.sendAction(#selector(NSText.copy(_:)), to: nil, from: nil)
        guard NSPasteboard.general.changeCount != before else { return nil }
        return NSPasteboard.general.string(forType: .string)
    }

    // MARK: - Event handling (called from monitor closures via MainActor.assumeIsolated)

    /// Returns true if the key event should be consumed (opens the comment form).
    private func shouldConsumeKeyEvent(
        chars: String?,
        mods: NSEvent.ModifierFlags
    ) -> Bool {
        guard !isShowingPopover else { return false }
        let cleanMods = mods.intersection(.deviceIndependentFlagsMask)
        guard cleanMods.isSubset(of: [.shift, .capsLock]) else { return false }
        guard
            let str = chars,
            str.count == 1,
            let char = str.first,
            char.isLetter || char.isNumber || char.isPunctuation || char.isSymbol
        else { return false }
        guard hasCurrentSelection() else { return false }
        requestNewComment(firstChar: char)
        return true
    }

    /// Returns true if the right-click event is consumed (shows our custom context menu).
    ///
    /// `NSMenu.popUp(positioning:at:in:)` interprets `at:` in the *receiving view's*
    /// coordinate system. Under SwiftUI, `window.contentView` is an NSHostingView whose
    /// `isFlipped` is true, while `event.locationInWindow` is in the window's bottom-left
    /// space — passing the window point directly inverts Y and pops the menu at
    /// `viewHeight - clickY`. Route through `windowPointToView` to apply the flip.
    private func handleRightClick(locationInWindow: NSPoint, window: NSWindow?) -> Bool {
        guard !isShowingPopover, hasCurrentSelection() else { return false }
        guard let window, let view = window.contentView else { return false }

        let menu = NSMenu()
        let target = CommentMenuTarget(layer: self)
        let addItem = NSMenuItem(
            title: "Add Comment",
            action: #selector(CommentMenuTarget.addCommentAction),
            keyEquivalent: ""
        )
        addItem.target = target
        addItem.representedObject = target   // keep target alive during menu
        menu.addItem(addItem)
        menu.addItem(.separator())
        menu.addItem(
            NSMenuItem(title: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c"))

        let pointInView = windowPointToView(locationInWindow, in: view)
        coordLog.info("handleRightClick: popUp at view-coords \(NSStringFromPoint(pointInView)) in \(NSStringFromClass(type(of: view))) (was: locationInWindow \(NSStringFromPoint(locationInWindow)) — passing window coords to a flipped contentView produced the top↔bottom mirror)")
        menu.popUp(positioning: nil, at: pointInView, in: view)
        return true
    }
}

// MARK: - NSPopoverDelegate

extension CommentLayer: NSPopoverDelegate {
    /// Called by AppKit when the popover finishes closing, whether by user dismissal or
    /// programmatic close. Resets authoring state. The extension lives in the same file
    /// so it can access private members directly.
    nonisolated func popoverDidClose(_ notification: Notification) {
        Task { @MainActor [weak self] in
            guard let self else { return }
            self.isShowingPopover = false
            self.pendingFirstChar = nil
            self.pendingQuotedText = ""
            self.pendingOccurrenceIndex = 0
            self.activePopover = nil
            self.anchorInteractionScreenPoint = nil
            self.anchorInteractionView = nil
            self.anchorTextBeforeSelection = nil
        }
    }
}

// MARK: - Menu action target

private final class CommentMenuTarget: NSObject, @unchecked Sendable {
    let layer: CommentLayer
    init(layer: CommentLayer) { self.layer = layer }

    @objc func addCommentAction(_ sender: Any?) {
        Task { @MainActor in layer.requestNewComment() }
    }
}

// MARK: - Artifact reference

/// Identifies the document a viewer's comments attach to. Mirrors the engine's
/// two-part `artifact_kind` + `artifact_id` key (`work_comments`). Threaded into
/// `.withComments(artifact:)` so comments land on the right document.
struct CommentArtifactRef: Equatable {
    let kind: String
    let id: String

    /// A comment on an engine-owned work-item description. `artifact_id` is the
    /// raw work-item id.
    static func workItem(id: String) -> CommentArtifactRef {
        CommentArtifactRef(kind: WireArtifactKind.workItem, id: id)
    }

    /// A comment on a markdown file under a PR's branch. `artifact_id` is the
    /// synthetic `pr_doc:<repo_remote_url>:<branch>:<path>` composite the engine
    /// keys on (`design_detector.rs` / `products_design.rs`).
    static func prDoc(repoRemoteURL: String, branch: String, path: String) -> CommentArtifactRef {
        CommentArtifactRef(kind: WireArtifactKind.prDoc, id: "pr_doc:\(repoRemoteURL):\(branch):\(path)")
    }
}

// MARK: - View modifier

/// Wraps a markdown viewer with the full comment affordance:
/// sidebar, "Add Comment" button, popover authoring form, and three entry paths
/// (type-to-comment, right-click context menu, ⌘⇧K).
///
/// When `artifact` is non-nil and a [`CommentBackend`] is present in the
/// environment (injected as `\.commentBackend`), comments are engine-backed and
/// persist. Otherwise the layer runs in-memory (artifact-less viewers, tests).
///
/// Usage:
/// ```swift
/// MarkdownViewerView(...)
///     .withComments(artifact: .prDoc(repoRemoteURL: …, branch: …, path: …), source: markdown)
/// ```
struct WithCommentsModifier: ViewModifier {
    let artifact: CommentArtifactRef?
    let source: String
    let baseURL: URL?

    @Environment(\.commentBackend) private var commentBackend
    @StateObject private var layer = CommentLayer()

    func body(content: Content) -> some View {
        let commentedAnchors = layer.comments.filter(\.isHighlightable).map(\.anchor)
        let flashingAnchor = layer.flashingAnchor
        let showSidebar = !layer.comments.isEmpty || layer.isEngineBacked

        HStack(spacing: 0) {
            content
                .environment(\.commentedAnchors, commentedAnchors)
                .environment(\.commentFlashAnchor, flashingAnchor)

            if showSidebar {
                Divider()
                CommentSidebar(layer: layer)
                    .frame(width: 280)
            }
        }
        .overlay(alignment: .topTrailing) {
            if !showSidebar {
                addCommentButton
                    .padding(.trailing, 16)
                    .padding(.top, 20)
            }
        }
        // Hidden button for ⌘⇧K shortcut (⌘⇧M is already the Metrics panel shortcut).
        .background {
            Button("") {
                if layer.hasCurrentSelection() { layer.requestNewComment() }
            }
            .keyboardShortcut("k", modifiers: [.command, .shift])
            .frame(width: 0, height: 0)
            .hidden()
        }
        .onAppear {
            layer.configure(source: source, baseURL: baseURL, artifact: artifact, backend: commentBackend)
            layer.installMonitors()
        }
        .onChange(of: source) { _, newSource in
            layer.updateSource(newSource, baseURL: baseURL)
        }
        .onDisappear {
            layer.removeMonitors()
            layer.unbindFromEngine()
        }
    }

    private var addCommentButton: some View {
        Button {
            layer.requestNewComment()
        } label: {
            Label("Add Comment", systemImage: "bubble.left.and.text.bubble.right")
                .font(.callout)
        }
        .buttonStyle(.bordered)
        .controlSize(.small)
        .help("Select text, then click or press ⌘⇧K to add a comment")
    }
}

extension View {
    /// Attach the comment affordance. Pass the rendered markdown `source` (the
    /// anchor projection is derived from it) and, to persist through the engine,
    /// the `artifact` the doc corresponds to.
    func withComments(
        artifact: CommentArtifactRef? = nil,
        source: String = "",
        baseURL: URL? = nil
    ) -> some View {
        modifier(WithCommentsModifier(artifact: artifact, source: source, baseURL: baseURL))
    }
}

// MARK: - Environment keys

private struct CommentedAnchorsKey: EnvironmentKey {
    static var defaultValue: [CommentAnchor] { [] }
}

private struct CommentFlashAnchorKey: EnvironmentKey {
    static var defaultValue: CommentAnchor? { nil }
}

/// The engine facade the comment layer persists through. `nil` (the default)
/// leaves the layer in-memory; `BossMacApp` injects `chatModel.commentBridge`
/// into the markdown-viewer scenes. Using `@Environment` (not `@EnvironmentObject`)
/// means an un-injected host (e.g. a unit test) reads `nil` rather than crashing.
private struct CommentBackendKey: EnvironmentKey {
    static var defaultValue: (any CommentBackend)? { nil }
}

extension EnvironmentValues {
    var commentedAnchors: [CommentAnchor] {
        get { self[CommentedAnchorsKey.self] }
        set { self[CommentedAnchorsKey.self] = newValue }
    }

    var commentFlashAnchor: CommentAnchor? {
        get { self[CommentFlashAnchorKey.self] }
        set { self[CommentFlashAnchorKey.self] = newValue }
    }

    var commentBackend: (any CommentBackend)? {
        get { self[CommentBackendKey.self] }
        set { self[CommentBackendKey.self] = newValue }
    }
}
