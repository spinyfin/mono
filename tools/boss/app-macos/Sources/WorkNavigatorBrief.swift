import SwiftUI

/// Original brief vs current ask, with per-document drafts that survive
/// section movement and tab changes.
struct WorkNavigatorBrief: View {
    let context: NavigatorDetailContext
    private var model: ChatViewModel { context.model }
    private var root: WorkTask { context.root }
    private var document: WorkTask { model.navigatorDocumentTask(for: root) }
    private var ask: WorkTask? {
        model.navigator.askID.flatMap { model.task(withID: $0) }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                Picker("Document", selection: Binding(
                    get: { model.navigatorDocument(for: root.id) },
                    set: { model.setNavigatorDocument($0, rootID: root.id) }
                )) {
                    Text("Original brief").tag(NavigatorBriefDocument.original)
                    if let ask, ask.id != root.id {
                        Text("Current ask").tag(NavigatorBriefDocument.currentAsk)
                    }
                }
                .pickerStyle(.segmented)
                .frame(maxWidth: ask != nil && ask?.id != root.id ? 280 : 160)
                .accessibilityIdentifier("navigator-brief-document")
                if let ask, ask.id != root.id, model.navigatorDocument(for: root.id) == .currentAsk {
                    Text(askLabel(ask))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            WorkItemBriefEditor(
                title: model.navigatorDocument(for: root.id) == .currentAsk ? "Current ask" : "Original brief",
                source: document.description,
                draft: draftBinding,
                isEditing: model.navigatorBriefIsEditing(document.id),
                remoteChanged: model.navigatorBriefChangedRemotely(document),
                readOnly: context.isReadOnly,
                showsAgentNotice: root.navigator?.root?.knownSection == .working,
                onEdit: { model.beginNavigatorBriefEdit(document) },
                onCancel: { model.cancelNavigatorBriefEdit(document) },
                onSave: { _ = model.saveNavigatorBrief(document) },
                onReload: { model.reloadNavigatorBrief(document) }
            )
            .id(document.id)
        }
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

    private func askLabel(_ ask: WorkTask) -> String {
        let revisions = root.navigator?.root?.revisions
        let sequence = revisions.map { "Revision \($0.open)" } ?? "Revision"
        let reason = root.navigator?.root?.reasonLabel ?? ask.name
        return "\(sequence) · \(reason)"
    }
}
