import AppKit
import SwiftUI

/// A text editor that submits on plain Return and inserts a newline on Shift+Return.
/// Replaces SwiftUI's TextEditor (which treats all Return keys as newlines) for the
/// comment entry form.
///
/// Focus: when `wantsFocus` is true, every `updateNSView` re-asserts first responder
/// via `onClaimFocus` — the same re-assertion pattern as the find bar's `@FocusState`,
/// rather than a single one-shot `makeFirstResponder` that AppKit can discard during
/// the popover key-window transition.
struct CommentTextEditor: NSViewRepresentable {
    @Binding var text: String
    var onSubmit: () -> Void
    var onTextViewCreated: (NSTextView) -> Void
    /// While true, re-claim first responder on each update until the claim sticks.
    var wantsFocus: Bool = false
    var onClaimFocus: () -> Void = {}
    /// When true (the default, matching every existing caller), plain
    /// Return submits and Shift+Return inserts a newline — the comment-entry
    /// behavior this type was built for. When false, plain Return inserts a
    /// newline like an ordinary multi-line text field and `onSubmit` is
    /// never called from a keystroke — used by the Ideas draft editor,
    /// where Return is prose, not submission.
    var submitOnReturn: Bool = true
    /// Fires when the text view resigns first responder (the user clicks
    /// elsewhere, tabs away, etc.). Used by the Ideas draft editor to flush
    /// a pending autosave on blur; the default no-op leaves every other
    /// caller unaffected.
    var onBlur: () -> Void = {}

    func makeCoordinator() -> Coordinator {
        Coordinator(self)
    }

    func makeNSView(context: Context) -> NSScrollView {
        let textView = SubmitOnReturnTextView()
        textView.delegate = context.coordinator
        textView.isRichText = false
        textView.allowsUndo = true
        textView.isEditable = true
        textView.isSelectable = true
        textView.font = NSFont.systemFont(ofSize: NSFont.systemFontSize(for: .regular) - 1)
        textView.textContainerInset = NSSize(width: 6, height: 6)
        textView.isVerticallyResizable = true
        textView.autoresizingMask = [.width]
        textView.textContainer?.widthTracksTextView = true
        textView.backgroundColor = .clear
        textView.drawsBackground = false

        let scrollView = NSScrollView()
        scrollView.hasVerticalScroller = true
        scrollView.documentView = textView
        scrollView.drawsBackground = false
        scrollView.backgroundColor = .clear
        scrollView.borderType = .noBorder

        textView.onResignFirstResponder = onBlur

        context.coordinator.textView = textView
        // Apply any seed text before the first layout so the caret lands at end.
        if !text.isEmpty {
            textView.string = text
            textView.setSelectedRange(NSRange(location: (text as NSString).length, length: 0))
        }
        onTextViewCreated(textView)

        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        // Always refresh the coordinator's parent so textDidChange / onSubmit see
        // the latest bindings and closures from this render, not makeCoordinator's.
        context.coordinator.parent = self

        guard let textView = scrollView.documentView as? NSTextView else { return }
        (textView as? SubmitOnReturnTextView)?.onResignFirstResponder = onBlur

        if textView.string != text {
            let isFirstResponder = textView.window?.firstResponder === textView
            if isFirstResponder {
                // Live editing: trust the text view. A stale binding (e.g. from an
                // @ObservedObject re-render of the popover mid-keystroke) must not
                // overwrite the user's characters or caret. textDidChange keeps the
                // binding in sync from the view side.
                //
                // Exception: the binding is a strict extension of the current string
                // (typeahead buffer applied via @State before insertText ran) — pull
                // the missing suffix in so nothing is lost either direction.
                if text.hasPrefix(textView.string) && text.count > textView.string.count {
                    let addition = String(text.dropFirst(textView.string.count))
                    textView.insertText(
                        addition,
                        replacementRange: NSRange(location: NSNotFound, length: 0)
                    )
                }
            } else if text.isEmpty && !textView.string.isEmpty {
                // Binding has not yet caught up to content seeded on the text view
                // (or inserted during the dead window). Do not clobber.
            } else {
                let wasEmpty = textView.string.isEmpty
                let sel = textView.selectedRanges
                textView.string = text

                if wasEmpty && !text.isEmpty {
                    // Initial seed: caret at end so continued typing appends.
                    textView.setSelectedRange(NSRange(location: (text as NSString).length, length: 0))
                } else {
                    textView.selectedRanges = sel
                }
            }
        }

        // Re-assert focus every update while the layer still needs it. AppKit's
        // popover key-window transition can discard an earlier claim; one-shot
        // timing is not enough under load.
        if wantsFocus {
            onClaimFocus()
        }
    }

    /// NSTextView's default key bindings map both plain Return and Shift+Return to
    /// `insertNewline:`, so the delegate's `doCommandBy:` can't tell them apart.
    /// Intercept Shift+Return here and route it to `insertNewlineIgnoringFieldEditor:`
    /// (a literal newline) before AppKit's key binding manager collapses it.
    final class SubmitOnReturnTextView: NSTextView {
        /// See `CommentTextEditor.onBlur`.
        var onResignFirstResponder: (() -> Void)?

        override func keyDown(with event: NSEvent) {
            if event.keyCode == 36, event.modifierFlags.contains(.shift) {
                insertNewlineIgnoringFieldEditor(nil)
                return
            }
            super.keyDown(with: event)
        }

        override func resignFirstResponder() -> Bool {
            let result = super.resignFirstResponder()
            if result {
                onResignFirstResponder?()
            }
            return result
        }
    }

    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: CommentTextEditor
        weak var textView: NSTextView?

        init(_ parent: CommentTextEditor) {
            self.parent = parent
        }

        func textDidChange(_ notification: Notification) {
            guard let tv = notification.object as? NSTextView else { return }
            parent.text = tv.string
        }

        func textView(
            _ textView: NSTextView,
            doCommandBy commandSelector: Selector
        ) -> Bool {
            if commandSelector == #selector(NSResponder.insertNewline(_:)) {
                guard parent.submitOnReturn else {
                    // Plain Return is prose, not submission — let AppKit's
                    // default handling insert the newline.
                    return false
                }
                // Plain Return → submit
                parent.onSubmit()
                return true
            }
            if commandSelector == #selector(NSResponder.insertNewlineIgnoringFieldEditor(_:)) {
                // Shift+Return → insert literal newline
                textView.insertNewline(nil)
                return true
            }
            return false
        }
    }
}
