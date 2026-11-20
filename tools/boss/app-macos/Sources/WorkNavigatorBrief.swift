import SwiftUI

/// Original brief vs current ask, with per-document drafts that survive
/// section movement and tab changes.
struct WorkNavigatorBrief: View {
    let context: NavigatorDetailContext
    private var model: ChatViewModel { context.model }
    private var root: WorkTask { context.root }
    private var document: WorkTask { model.navigatorDocumentTask(for: root) }
    private var ask: WorkTask? {
        root.navigator?.root?.revisions.currentTaskId.flatMap { model.task(withID: $0) }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            WorkItemBriefEditor(
                title: documentTitle,
                source: document.description,
                draft: draftBinding,
                isEditing: model.navigatorBriefIsEditing(document.id),
                remoteChanged: model.navigatorBriefChangedRemotely(document),
                readOnly: context.isReadOnly || !model.navigatorBriefCanEdit(document),
                showsAgentNotice: root.navigator?.root?.knownSection == .working,
                headerLeading: hasMultipleDocuments ? AnyView(documentPicker) : nil,
                onEdit: { model.beginNavigatorBriefEdit(document) },
                onCancel: { model.cancelNavigatorBriefEdit(document) },
                onSave: { _ = model.saveNavigatorBrief(document) },
                onReload: { model.reloadNavigatorBrief(document) }
            )
            .id(document.id)
        }
    }

    private var documentPicker: some View {
        HStack(spacing: 8) {
            Picker("Document", selection: Binding(
                get: { model.navigatorDocument(for: root.id) },
                set: { model.setNavigatorDocument($0, rootID: root.id) }
            )) {
                Text("Original brief").tag(NavigatorBriefDocument.original)
                if let ask, ask.id != root.id {
                    Text("Current ask").tag(NavigatorBriefDocument.currentAsk)
                }
                if model.navigatorDocument(for: root.id) == .historical {
                    Text("Historical ask").tag(NavigatorBriefDocument.historical)
                }
            }
            .labelsHidden()
            .pickerStyle(.segmented)
            .frame(maxWidth: 280)
            .accessibilityIdentifier("navigator-brief-document")
            if document.id != root.id {
                askLabel(document)
            }
        }
    }

    /// The document picker only exists when there is something to pick: a
    /// distinct current ask, or a historical ask that is being viewed.
    var hasMultipleDocuments: Bool {
        Self.hasMultipleDocuments(rootID: root.id, askID: ask?.id, document: model.navigatorDocument(for: root.id))
    }

    static func hasMultipleDocuments(rootID: String, askID: String?, document: NavigatorBriefDocument) -> Bool {
        (askID != nil && askID != rootID) || document == .historical
    }

    private var draftBinding: Binding<String> {
        Binding(
            get: { model.navigatorBriefText(for: document) },
            set: { text in
                if !model.navigatorBriefIsEditing(document.id) {
                    model.beginNavigatorBriefEdit(document)
                }
                model.setNavigatorBriefText(document.id, text: text)
            }
        )
    }

    /// Title of the resolved document, never the stored choice: a choice
    /// that no longer resolves has already fallen back to the root.
    var documentTitle: String {
        switch model.navigatorDocument(for: root.id) {
        case .original: return "Original brief"
        case .currentAsk: return "Current ask"
        case .historical: return "Historical ask"
        }
    }

    /// Engine-owned revision number and source, as the Kanban header shows.
    static func revisionBadgeInputs(_ ask: WorkTask) -> (seq: Int?, origin: EngineRevisionOrigin?) {
        (ask.revisionSeq, EngineRevisionOrigin(createdVia: ask.createdVia))
    }

    private func askLabel(_ ask: WorkTask) -> some View {
        let inputs = Self.revisionBadgeInputs(ask)
        return HStack(spacing: 6) {
            if let seq = inputs.seq { RevisionBadge(seq: seq) }
            if let origin = inputs.origin { EngineRevisionBadge(origin: origin) }
            Text(ask.name)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
        .accessibilityIdentifier("navigator-brief-ask-label")
    }
}
