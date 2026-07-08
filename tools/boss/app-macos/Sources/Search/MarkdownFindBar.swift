import AppKit
import SwiftUI

/// Standard macOS in-document find bar: type-ahead query field, "N of M"
/// counter, prev/next chevrons, and a close button. Sits inline at the top
/// of the markdown viewer (opened via ⌘F), matching the Safari/Xcode
/// convention rather than a sheet or separate window.
///
/// ## Focus rule (find-bar-open + text-selected)
///
/// T548's type-to-comment trigger (`CommentLayer.shouldConsumeKeyEvent`)
/// opens the comment popover on any plain-letter keystroke while the
/// document has a text selection. That check inspects whatever the *current*
/// AppKit first responder validates as copyable — normally correct, but if
/// the document still has a stale selection underneath while the user is
/// typing into this find field with the field's own text selected (e.g.
/// after ⌘A in the field), both features would want the same keystroke.
/// The explicit rule: while this field has focus, `suppressTypeToComment`
/// (threaded down from `WithCommentsModifier` via `\.suppressTypeToComment`)
/// is held true, unconditionally short-circuiting the comment trigger for as
/// long as focus stays here — regardless of any selection state elsewhere.
struct MarkdownFindBar: View {
    @ObservedObject var state: MarkdownFindState
    var isFocused: FocusState<Bool>.Binding
    var onClose: () -> Void

    @Environment(\.suppressTypeToComment) private var suppressTypeToComment

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(.secondary)

            TextField("Find in Document", text: $state.query)
                .textFieldStyle(.plain)
                .focused(isFocused)
                .onSubmit { state.selectNext() }
                .onExitCommand { onClose() }

            if !state.counterText.isEmpty {
                Text(state.counterText)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize()
            }

            Divider().frame(height: 14)

            Button {
                state.selectPrevious()
            } label: {
                Image(systemName: "chevron.up")
            }
            .buttonStyle(.borderless)
            .disabled(state.matches.isEmpty)
            .help("Previous match (⇧⌘G)")

            Button {
                state.selectNext()
            } label: {
                Image(systemName: "chevron.down")
            }
            .buttonStyle(.borderless)
            .disabled(state.matches.isEmpty)
            .help("Next match (⌘G)")

            Button {
                onClose()
            } label: {
                Image(systemName: "xmark.circle.fill")
            }
            .buttonStyle(.borderless)
            .help("Close (Esc)")
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .onChange(of: isFocused.wrappedValue) { _, focused in
            suppressTypeToComment.wrappedValue = focused
        }
    }
}

/// Bridges to the AppKit `NSScrollView` backing the markdown viewer's
/// SwiftUI `ScrollView` so the find bar can reveal a match. Textual's
/// `StructuredText` renders as one flowing block of SwiftUI `Text` with no
/// per-character or per-line geometry API — there's nothing like
/// `NSLayoutManager.boundingRect(forGlyphRange:)` in its public surface, so
/// there's no way to ask "where is character offset N on screen." Instead
/// this estimates the match's vertical position as
/// `characterOffset / totalCharacterCount` of the document height —
/// reliable enough for prose-heavy design docs to land the match inside the
/// viewport; it can be off by a screenful in text-sparse regions (large
/// code blocks, images, wide tables). That trade-off is preferable to
/// swapping the renderer or reimplementing text layout from scratch, and
/// the highlight color still draws the eye once the match is nearby.
@MainActor
final class MarkdownScrollController {
    weak var scrollView: NSScrollView?

    func scrollToFraction(_ fraction: Double) {
        guard let scrollView, let documentView = scrollView.documentView else { return }
        let docHeight = documentView.frame.height
        let viewportHeight = scrollView.contentView.bounds.height
        guard docHeight > viewportHeight else { return }

        let clampedFraction = min(max(fraction, 0), 1)
        // SwiftUI hosts ScrollView content in a flipped NSView (y=0 at the
        // top), matching the document's natural reading order — see the
        // coordinate-bridge notes in CommentLayer.swift for the same fact
        // established via the popover-anchor top↔bottom mirror bug.
        let targetY = clampedFraction * docHeight
        let origin = NSPoint(
            x: 0,
            y: min(max(targetY - viewportHeight / 3, 0), docHeight - viewportHeight)
        )
        NSAnimationContext.runAnimationGroup { context in
            context.duration = 0.18
            scrollView.contentView.animator().setBoundsOrigin(origin)
        }
        scrollView.reflectScrolledClipView(scrollView.contentView)
    }
}

/// Zero-size marker view inserted into the scrollable content; on appear it
/// walks up to the enclosing `NSScrollView` and hands it to the controller.
/// Mirrors `WindowMenuRegistrar` (BossMacApp.swift/DesignsView.swift)'s
/// pattern of reaching into AppKit state a SwiftUI-only view can't
/// otherwise obtain.
struct MarkdownScrollViewCapture: NSViewRepresentable {
    let controller: MarkdownScrollController

    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async { [weak view] in
            controller.scrollView = view?.enclosingScrollView
        }
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        if controller.scrollView == nil {
            controller.scrollView = nsView.enclosingScrollView
        }
    }
}
