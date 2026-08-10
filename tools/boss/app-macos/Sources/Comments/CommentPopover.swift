import SwiftUI

/// Popover for creating a new comment. Appears anchored near the selection.
/// Does not echo the selected text back — the yellow highlight in the viewer
/// already shows what is being commented on.
///
/// Behaviour:
///   - Plain Return submits (via CommentTextEditor's key handler).
///   - Shift+Return inserts a newline so multi-line comments are still possible.
///   - Cancel clears state without adding a comment.
///   - Initial body is seeded from `layer.pendingTypeahead` (the type-to-comment
///     opener plus any keystrokes buffered before the text view existed). Further
///     dead-window keystrokes are inserted into the live `NSTextView` by
///     `CommentLayer.forwardKeystrokeToPendingComment`.
struct CommentPopover: View {
    @ObservedObject var layer: CommentLayer

    @State private var commentBody: String

    init(layer: CommentLayer) {
        _layer = ObservedObject(wrappedValue: layer)
        // Seed from any typeahead already buffered before the first frame so
        // `CommentTextEditor.makeNSView` / `updateNSView` see the character(s)
        // immediately instead of starting empty and racing a later onAppear.
        _commentBody = State(initialValue: layer.pendingTypeahead)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("New Comment")
                .font(.headline)

            CommentTextEditor(
                text: $commentBody,
                onSubmit: submit,
                onTextViewCreated: { textView in
                    // Drain buffered keys before registering the live view. This
                    // gives typeahead one consumer and prevents an asynchronous
                    // state update from racing a later direct NSTextView insertion.
                    let typeahead = layer.drainPendingTypeahead()
                    commentBody = typeahead
                    textView.string = typeahead
                    textView.setSelectedRange(
                        NSRange(location: (typeahead as NSString).length, length: 0))
                    layer.setCommentTextView(textView)
                },
                // Always request focus while the popover is mounted; the layer
                // no-ops claimCommentTextFocus once the claim has stuck, so the
                // Cancel/Comment buttons remain reachable via Tab.
                wantsFocus: true,
                onClaimFocus: { layer.claimCommentTextFocus() }
            )
                .frame(minHeight: 80, maxHeight: 160)
                .overlay(
                    RoundedRectangle(cornerRadius: 6)
                        .stroke(Color(nsColor: .separatorColor), lineWidth: 0.5)
                )

            HStack {
                Spacer()
                Button("Cancel") {
                    cancel()
                }
                .keyboardShortcut(.cancelAction)

                // Disable is based on a local snapshot of emptiness only — avoid
                // re-reading observed layer fields that would force an extra
                // representable update cycle per keystroke.
                Button("Comment") {
                    submit()
                }
                .keyboardShortcut(.defaultAction)
                .disabled(commentBody.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(16)
        .frame(width: 320)
    }

    private func submit() {
        layer.addComment(quoted: layer.pendingQuotedText, body: commentBody)
        commentBody = ""
    }

    private func cancel() {
        commentBody = ""
        layer.cancelNewComment()
    }
}
