import SwiftUI

/// Review-card guide affordance beside the merge control — the five brief
/// states from the design's "Review card and viewer" table, all derived
/// from `ReviewGuideCardPresentation`:
///
/// | State                                | Presentation                                          |
/// | ------------------------------------- | ------------------------------------------------------ |
/// | Queued/generating, no previous content | small indeterminate spinner only, no button            |
/// | Current guide ready                   | `doc.text.magnifyingglass` button                       |
/// | Refreshing with earlier content       | document button + spinner                               |
/// | Failed, no prior content              | `exclamationmark.triangle` + keyboard-accessible Retry  |
/// | Refresh failed with prior content     | document button + error/Retry                           |
struct ReviewGuideCardBadge: View {
    let presentation: ReviewGuideCardPresentation
    var onOpen: () -> Void
    var onRetry: () -> Void

    var body: some View {
        HStack(spacing: 4) {
            if presentation.showsDocumentButton {
                Button(action: onOpen) {
                    Image(systemName: "doc.text.magnifyingglass")
                        .font(.caption)
                        .foregroundStyle(presentation.kind == .refreshFailed ? Color.orange : Color.secondary)
                        .accessibilityLabel(presentation.accessibilityLabel)
                }
                .buttonStyle(.plain)
                .help(presentation.tooltip)
            } else if presentation.kind == .failed {
                Image(systemName: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .accessibilityLabel(presentation.accessibilityLabel)
                    .help(presentation.tooltip)
            }
            if presentation.showsProgress {
                ProgressView()
                    .controlSize(.mini)
                    .accessibilityLabel(presentation.accessibilityLabel)
                    .help(presentation.tooltip)
            }
            if presentation.showsRetry {
                Button("Retry", action: onRetry)
                    .buttonStyle(.plain)
                    .font(.caption2)
                    .foregroundStyle(.orange)
                    .help(presentation.tooltip)
            }
        }
    }
}
