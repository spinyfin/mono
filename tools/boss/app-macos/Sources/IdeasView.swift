import SwiftUI
import Textual

/// The Ideas tab: a persistent markdown-authoring surface for the
/// currently selected product (`ChatViewModel.selectedProduct` — the same
/// product the Work tab has selected; see the Automations tab for the
/// same convention). Owns its own `NavigationSplitView` (idea list +
/// editor), so `ContentView` mounts it as a structural conditional rather
/// than an opacity toggle — two concurrent `NavigationSplitView`s fight
/// over the window's toolbar namespace (see the placement note in
/// `ContentView`).
///
/// Autosave — the point of this feature — is entirely owned by
/// `ChatViewModel+Ideas.swift`; this view only binds to the published
/// draft state and flushes on teardown (`.onDisappear`) so switching away
/// from Ideas never strands an edit in the debounce window.
struct IdeasView: View {
    @ObservedObject var chat: ChatViewModel
    @State private var isPreviewShowing = false

    var body: some View {
        NavigationSplitView {
            sidebar
                .navigationSplitViewColumnWidth(min: 240, ideal: 300, max: 420)
        } detail: {
            detail
                .background(Color(nsColor: .windowBackgroundColor))
        }
        .navigationTitle("Ideas")
        .onAppear {
            chat.seedIdeaPendingDraftsIfNeeded()
        }
        .onDisappear {
            chat.flushIdeaDraft()
        }
    }

    // MARK: - Sidebar

    private var sidebar: some View {
        VStack(spacing: 0) {
            if chat.selectedProduct == nil {
                emptyProductState
            } else {
                ideaList
            }

            Divider()

            HStack {
                if let product = chat.selectedProduct {
                    Text(product.name)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer()
                Button {
                    chat.createIdea()
                } label: {
                    Image(systemName: "plus")
                }
                .buttonStyle(.borderless)
                .disabled(chat.selectedProduct == nil || !chat.isConnected)
                .help("New Idea")
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
    }

    private var ideaList: some View {
        List(selection: Binding(
            get: { chat.selectedIdeaID },
            set: { chat.selectIdea($0) }
        )) {
            if chat.ideasForSelectedProduct.isEmpty {
                Text(chat.ideasByProductID[chat.selectedProduct?.id ?? ""] == nil ? "Loading…" : "No ideas yet")
                    .foregroundStyle(.secondary)
                    .font(.callout)
                    .listRowBackground(Color.clear)
            } else {
                ForEach(chat.ideasForSelectedProduct) { idea in
                    IdeaRowView(
                        idea: idea,
                        hasPendingLocalDraft: chat.ideaHasPendingLocalDraft(idea.id)
                    )
                    .tag(idea.id)
                }
            }
        }
        .listStyle(.sidebar)
    }

    private var emptyProductState: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("No product selected")
                .font(.callout.weight(.semibold))
            Text("Select a product from the Work tab to author ideas for it.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(16)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }

    // MARK: - Detail

    @ViewBuilder
    private var detail: some View {
        if let idea = chat.selectedIdea {
            IdeaEditorView(chat: chat, idea: idea, isPreviewShowing: $isPreviewShowing)
        } else {
            IdeasEmptyDetailState(
                hasProduct: chat.selectedProduct != nil,
                hasIdeas: !chat.ideasForSelectedProduct.isEmpty
            )
        }
    }
}

// MARK: - Idea list row

private struct IdeaRowView: View {
    let idea: WorkIdea
    let hasPendingLocalDraft: Bool

    var body: some View {
        HStack(spacing: 6) {
            VStack(alignment: .leading, spacing: 2) {
                Text(idea.name.isEmpty ? "Untitled idea" : idea.name)
                    .font(.body)
                    .lineLimit(1)
                Text(idea.shortLabel)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if hasPendingLocalDraft {
                Circle()
                    .fill(Color.orange)
                    .frame(width: 6, height: 6)
                    .help("Unsaved local changes — will sync when reopened")
            }
            if idea.status != .draft {
                Text(idea.status.label)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 2)
    }
}

// MARK: - Empty detail state

private struct IdeasEmptyDetailState: View {
    let hasProduct: Bool
    let hasIdeas: Bool

    var body: some View {
        VStack(spacing: 8) {
            Text(title)
                .font(.title3.weight(.semibold))
            if hasProduct {
                Text(hasIdeas ? "Choose an idea from the list to start editing." : "Click + to start drafting.")
                    .foregroundStyle(.secondary)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
        .padding(24)
    }

    private var title: String {
        guard hasProduct else { return "No product selected" }
        return hasIdeas ? "Select an idea" : "No ideas yet"
    }
}

// MARK: - Editor

private struct IdeaEditorView: View {
    @ObservedObject var chat: ChatViewModel
    let idea: WorkIdea
    @Binding var isPreviewShowing: Bool
    @State private var showSendFailedAlert = false

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            if isPreviewShowing {
                preview
            } else {
                editor
            }
        }
        .alert("Couldn't send to coordinator", isPresented: $showSendFailedAlert) {
            Button("OK", role: .cancel) {}
        } message: {
            Text("The coordinator pane isn't ready yet. Try again in a moment.")
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                TextField("Idea name", text: $chat.ideaDraftName)
                    .textFieldStyle(.plain)
                    .font(.title3.weight(.semibold))
                    .onChange(of: chat.ideaDraftName) { _, _ in chat.noteIdeaDraftEdited() }
                Spacer()
                saveStatusLabel
            }
            HStack(spacing: 12) {
                Text(idea.shortLabel)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                if idea.status != .draft {
                    statusBadge(idea.status)
                }
                Spacer()
                Picker("", selection: $isPreviewShowing) {
                    Text("Write").tag(false)
                    Text("Preview").tag(true)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .frame(width: 140)
                Button {
                    if !chat.sendIdeaDraftToCoordinator() {
                        showSendFailedAlert = true
                    }
                } label: {
                    Label("Send to Coordinator", systemImage: "arrow.up.message")
                }
                .disabled(chat.ideaDraftBody.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(16)
    }

    @ViewBuilder
    private var saveStatusLabel: some View {
        Group {
            switch chat.ideaSaveStatus {
            case .idle, .savedToEngine:
                Label("Saved", systemImage: "checkmark.circle")
            case .pendingLocal, .savingToEngine:
                Label("Saving…", systemImage: "arrow.triangle.2.circlepath")
            case .offlineSavedLocally:
                Label("Saved locally — offline", systemImage: "externaldrive.badge.exclamationmark")
            }
        }
        .font(.caption)
        .foregroundStyle(chat.ideaSaveStatus == .offlineSavedLocally ? Color.orange : .secondary)
    }

    private func statusBadge(_ status: IdeaStatus) -> some View {
        Text(status.label)
            .font(.caption2.weight(.semibold))
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(Capsule().fill(Color(nsColor: .quaternaryLabelColor).opacity(0.2)))
    }

    private var editor: some View {
        CommentTextEditor(
            text: $chat.ideaDraftBody,
            onSubmit: {},
            onTextViewCreated: { _ in },
            submitOnReturn: false,
            onBlur: { chat.flushIdeaDraft() }
        )
        .padding(.horizontal, 12)
        .onChange(of: chat.ideaDraftBody) { _, _ in chat.noteIdeaDraftEdited() }
    }

    private var preview: some View {
        ScrollView {
            StructuredText(markdown: chat.ideaDraftBody.isEmpty ? "*Nothing written yet.*" : chat.ideaDraftBody)
                .bossMarkdown()
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(20)
        }
    }
}
