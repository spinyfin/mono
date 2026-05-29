import SwiftUI

/// Modal sheet shown when a magic-wand dispatch completes. Presents the
/// original document and the proposed edit side-by-side so the user can
/// judge before applying. The user's explicit "Apply" or "Discard" choice
/// is required — there is no auto-apply. A CAS conflict (the doc was
/// edited while the dispatch was in flight) surfaces a "doc has changed"
/// banner instead of the preview.
///
/// Design: tools/boss/docs/designs/comments-in-markdown-viewer.md
/// § "MagicWandResultSheet" and § "Apply step".
struct MagicWandResultSheet: View {
    let originalMarkdown: String
    let proposedMarkdown: String
    let anchorWarning: Bool
    let conflict: Bool

    var onApply: () -> Void
    var onDiscard: () -> Void

    var body: some View {
        VStack(spacing: 0) {
            headerBar
            Divider()
            if conflict {
                conflictBanner
            } else {
                previewPane
            }
            Divider()
            actionBar
        }
        .frame(minWidth: 800, minHeight: 500)
    }

    // MARK: – Header

    private var headerBar: some View {
        HStack {
            Image(systemName: "wand.and.stars")
                .foregroundStyle(.secondary)
            Text("Magic Wand Result")
                .font(.headline)
            Spacer()
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }

    // MARK: – Conflict banner

    private var conflictBanner: some View {
        VStack(spacing: 12) {
            Spacer()
            Image(systemName: "exclamationmark.triangle")
                .font(.largeTitle)
                .foregroundStyle(.orange)
            Text("The document changed while the magic wand was running.")
                .font(.headline)
            Text("Discard this result and re-dispatch after reloading the document.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            Spacer()
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    // MARK: – Side-by-side preview

    private var previewPane: some View {
        HStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 4) {
                Label("Original", systemImage: "doc.text")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 12)
                    .padding(.top, 8)
                Divider()
                ScrollView {
                    Text(originalMarkdown)
                        .font(.system(.callout, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(12)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            Divider()

            VStack(alignment: .leading, spacing: 4) {
                HStack {
                    Label("Proposed", systemImage: "wand.and.stars")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    if anchorWarning {
                        Label("Anchor text may have been removed", systemImage: "exclamationmark.triangle")
                            .font(.caption2)
                            .foregroundStyle(.orange)
                    }
                    Spacer()
                }
                .padding(.horizontal, 12)
                .padding(.top, 8)
                Divider()
                ScrollView {
                    Text(proposedMarkdown)
                        .font(.system(.callout, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(12)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    // MARK: – Action bar

    private var actionBar: some View {
        HStack {
            if anchorWarning && !conflict {
                Label(
                    "The highlighted section may no longer be present in the proposed result.",
                    systemImage: "exclamationmark.triangle"
                )
                .font(.caption)
                .foregroundStyle(.orange)
            }
            Spacer()
            Button("Discard", role: .cancel) {
                onDiscard()
            }
            .keyboardShortcut(.escape)

            if !conflict {
                Button("Apply") {
                    onApply()
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.return)
            } else {
                Button("Close") {
                    onDiscard()
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }
}

#if DEBUG
#Preview("Normal result") {
    MagicWandResultSheet(
        originalMarkdown: "# Original\n\nThis is the original content with some **bold** text.\n\nAdd a second paragraph here.",
        proposedMarkdown: "# Original\n\nThis is the original content with some **bold** text, now edited by the magic wand.\n\nAdd a second paragraph here.",
        anchorWarning: false,
        conflict: false,
        onApply: {},
        onDiscard: {}
    )
}

#Preview("Anchor warning") {
    MagicWandResultSheet(
        originalMarkdown: "# Doc\n\nHighlighted section here.\n\nOther content.",
        proposedMarkdown: "# Doc\n\nThe section was rewritten entirely.\n\nMassive other changes too.",
        anchorWarning: true,
        conflict: false,
        onApply: {},
        onDiscard: {}
    )
}

#Preview("CAS conflict") {
    MagicWandResultSheet(
        originalMarkdown: "",
        proposedMarkdown: "",
        anchorWarning: false,
        conflict: true,
        onApply: {},
        onDiscard: {}
    )
}
#endif
