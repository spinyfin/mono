import AppKit
import Foundation

/// The narrow engine surface a [`CommentLayer`] needs, so the layer's logic can
/// be unit-tested against a fake. `ChatViewModel` owns the production
/// implementation ([`CommentEngineBridge`]); `CommentLayer` holds a reference to
/// it and calls it to persist mutations, while the bridge calls back into the
/// layer's `apply*` methods when engine replies land.
@MainActor
protocol CommentBackend: AnyObject {
    /// Author identity stamped on created comments (`user:<name>`).
    var author: String { get }

    /// Begin backing `layer` with the engine's comments for the given artifact:
    /// subscribe to the comment topic and kick an initial list. Idempotent.
    func registerCommentLayer(_ layer: CommentLayer, artifactKind: String, artifactId: String)
    /// Stop backing `layer` and unsubscribe (when it's the last on its topic).
    func unregisterCommentLayer(_ layer: CommentLayer)

    func createComment(artifactKind: String, artifactId: String, anchor: CommentAnchor, body: String, docVersion: String)
    func listComments(artifactKind: String, artifactId: String, includeResolved: Bool)
    func resolveComments(artifactKind: String, artifactId: String, plainText: String)
    func dismissComment(commentId: String)
    func setStatus(commentId: String, status: String)
    func updateAnchor(commentId: String, anchor: CommentAnchor, newDocVersion: String)
    /// Manually reclassify a comment's intent (sidebar badge override).
    func setIntent(commentId: String, intent: String)
    /// Fetch the `[Revise]`-banner summary for an artifact.
    func fetchBannerState(artifactKind: String, artifactId: String)
    /// The `[Revise]`-banner action: batch-address every unaddressed
    /// directive/larger_change comment on the artifact.
    func reviseDoc(artifactKind: String, artifactId: String)
}

/// Current comment author identity. Best-effort from the macOS login name until
/// the app carries a real account/email; the engine stores it verbatim for the
/// audit trail (`work_comments.author`).
enum CommentAuthor {
    static let current = "user:\(NSUserName())"
}

/// Routes the engine's comment RPC replies and `comments.artifact.*` topic
/// invalidations to the right open [`CommentLayer`], and forwards the layer's
/// mutations to [`EngineClient`]. Owned by `ChatViewModel`; one instance per
/// session. This is the multiplexer the single `EngineClient.onEvent` funnel
/// lacked — comment events fan out to per-artifact layers here.
@MainActor
final class CommentEngineBridge: CommentBackend {
    private let engine: EngineClient
    let author: String

    /// Active registrations keyed by the layer's identity. Weak layer refs so a
    /// closed viewer deallocates; stale entries are pruned lazily on routing.
    private struct Registration {
        weak var layer: CommentLayer?
        let artifactKind: String
        let artifactId: String
    }
    private var registrations: [ObjectIdentifier: Registration] = [:]

    /// FIFO queue of artifacts with an in-flight `comments_revise_doc` call.
    /// `CommentsReviseDocResult` carries no artifact identity on the wire
    /// (see `ReviseDocOutcome` in `wire.rs`), so replies are correlated to
    /// the request that issued them by send order — the engine handles one
    /// frontend request at a time per session, so a reply always corresponds
    /// to the oldest still-pending call.
    private var pendingReviseDocArtifacts: [(kind: String, id: String)] = []

    init(engine: EngineClient, author: String = CommentAuthor.current) {
        self.engine = engine
        self.author = author
    }

    /// The per-artifact comment topic (`wire.rs` `comment_topic`):
    /// `comments.artifact.<kind>:<id>`. The id itself may contain colons
    /// (`pr_doc:<repo>:<branch>:<path>`); they are not escaped.
    static func topic(artifactKind: String, artifactId: String) -> String {
        "comments.artifact.\(artifactKind):\(artifactId)"
    }

    // MARK: CommentBackend — registration

    func registerCommentLayer(_ layer: CommentLayer, artifactKind: String, artifactId: String) {
        let key = ObjectIdentifier(layer)
        let alreadyBackingArtifact = isArtifactSubscribed(kind: artifactKind, id: artifactId)
        registrations[key] = Registration(layer: layer, artifactKind: artifactKind, artifactId: artifactId)
        if !alreadyBackingArtifact {
            engine.sendSubscribe(topics: [Self.topic(artifactKind: artifactKind, artifactId: artifactId)])
        }
        // The layer drives the initial fetch (list + resolve) from `configure`,
        // once it has the source to compute the plain-text projection from.
    }

    func unregisterCommentLayer(_ layer: CommentLayer) {
        let key = ObjectIdentifier(layer)
        guard let reg = registrations.removeValue(forKey: key) else { return }
        if !isArtifactSubscribed(kind: reg.artifactKind, id: reg.artifactId) {
            engine.sendUnsubscribe(topics: [Self.topic(artifactKind: reg.artifactKind, artifactId: reg.artifactId)])
        }
    }

    private func isArtifactSubscribed(kind: String, id: String) -> Bool {
        registrations.values.contains { $0.layer != nil && $0.artifactKind == kind && $0.artifactId == id }
    }

    // MARK: CommentBackend — mutations / reads

    func createComment(artifactKind: String, artifactId: String, anchor: CommentAnchor, body: String, docVersion: String) {
        engine.sendCommentsCreate(
            artifactKind: artifactKind,
            artifactId: artifactId,
            anchor: anchor,
            body: body,
            author: author,
            docVersion: docVersion,
            plainTextProjectionVersion: CommentProjection.version
        )
    }

    func listComments(artifactKind: String, artifactId: String, includeResolved: Bool) {
        engine.sendCommentsList(artifactKind: artifactKind, artifactId: artifactId, includeResolved: includeResolved)
    }

    func resolveComments(artifactKind: String, artifactId: String, plainText: String) {
        engine.sendCommentsResolve(
            artifactKind: artifactKind,
            artifactId: artifactId,
            plainText: plainText,
            plainTextProjectionVersion: CommentProjection.version
        )
    }

    func dismissComment(commentId: String) {
        engine.sendCommentsDismiss(commentId: commentId, actor: author)
    }

    func setStatus(commentId: String, status: String) {
        engine.sendCommentsSetStatus(commentId: commentId, status: status, actor: author)
    }

    func updateAnchor(commentId: String, anchor: CommentAnchor, newDocVersion: String) {
        engine.sendCommentsUpdateAnchor(
            commentId: commentId,
            anchor: anchor,
            newDocVersion: newDocVersion,
            plainTextProjectionVersion: CommentProjection.version
        )
    }

    func setIntent(commentId: String, intent: String) {
        engine.sendCommentsSetIntent(commentId: commentId, intent: intent)
    }

    func fetchBannerState(artifactKind: String, artifactId: String) {
        engine.sendCommentsBannerState(artifactKind: artifactKind, artifactId: artifactId)
    }

    func reviseDoc(artifactKind: String, artifactId: String) {
        pendingReviseDocArtifacts.append((artifactKind, artifactId))
        engine.sendCommentsReviseDoc(artifactKind: artifactKind, artifactId: artifactId)
    }

    // MARK: Event routing (called from ChatViewModel.handle)

    func handleCommentsList(artifactKind: String, artifactId: String, comments: [CommentWithThread]) {
        forEachLayer(kind: artifactKind, id: artifactId) { $0.applyList(comments) }
    }

    func handleCommentsResolved(artifactKind: String, artifactId: String, comments: [ResolvedComment]) {
        forEachLayer(kind: artifactKind, id: artifactId) { $0.applyResolved(comments) }
    }

    /// A single-comment mutation echo. The origin session may not receive its
    /// own topic invalidation, so reload the owning artifact's layer(s) here to
    /// stay fresh after a self-initiated create/dismiss.
    func handleCommentResult(_ comment: WorkComment) {
        forEachLayer(kind: comment.artifactKind, id: comment.artifactId) { $0.reload() }
    }

    func handleCommentsBannerState(artifactKind: String, artifactId: String, state: CommentsBannerState) {
        forEachLayer(kind: artifactKind, id: artifactId) { $0.applyBannerState(state) }
    }

    func handleCommentsReviseDocResult(_ outcome: ReviseDocOutcome) {
        guard !pendingReviseDocArtifacts.isEmpty else { return }
        let artifact = pendingReviseDocArtifacts.removeFirst()
        forEachLayer(kind: artifact.kind, id: artifact.id) { $0.applyReviseDocOutcome(outcome) }
    }

    /// A `comments.artifact.*` topic invalidation (fired by another session's
    /// mutation). Reload every layer bound to that artifact.
    func handleCommentInvalidation(topic: String) {
        for reg in registrations.values {
            guard let layer = reg.layer else { continue }
            if Self.topic(artifactKind: reg.artifactKind, artifactId: reg.artifactId) == topic {
                layer.reload()
            }
        }
    }

    /// Returns true when `topic` is a comment-artifact topic (so `ChatViewModel`
    /// can route it here without owning the grammar).
    static func isCommentTopic(_ topic: String) -> Bool {
        topic.hasPrefix("comments.artifact.")
    }

    /// On reconnect the engine dropped every subscription; re-subscribe each
    /// active artifact and reload its layers.
    func handleReconnected() {
        var resubscribed = Set<String>()
        for reg in registrations.values {
            guard let layer = reg.layer else { continue }
            let topic = Self.topic(artifactKind: reg.artifactKind, artifactId: reg.artifactId)
            if resubscribed.insert(topic).inserted {
                engine.sendSubscribe(topics: [topic])
            }
            layer.reload()
        }
    }

    private func forEachLayer(kind: String, id: String, _ body: (CommentLayer) -> Void) {
        for reg in registrations.values where reg.artifactKind == kind && reg.artifactId == id {
            if let layer = reg.layer { body(layer) }
        }
    }
}
