import SwiftUI

/// Revision, execution and dependency history for the selected root.
struct WorkNavigatorHistory: View {
    let context: NavigatorDetailContext

    private var model: ChatViewModel { context.model }
    private var root: WorkTask { context.root }
    private var family: [WorkTask] { model.navigatorFamily(of: root) }

    // History stays mounted with the selected detail. Keep its consumer alive
    // while another tab is visible: prefetch once, retain rows, and continue
    // receiving invalidations until the detail closes or changes selection.
    var executionsList: WorkExecutionsList {
        WorkExecutionsList(model: model, taskIDs: family.map(\.id), transcriptTaskID: root.id)
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                if let origin = root.followupOriginDescription {
                    LabeledContent("Origin", value: origin)
                }
                revisionList
                NavigatorReviewHistory(model: model, root: root)
                WorkDependenciesSection(model: model, taskID: root.id) { row in
                    model.navigateNavigatorDependency(row)
                }
                executionsList
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.top, 4)
        }
    }

    @ViewBuilder
    private var revisionList: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Revisions").font(.headline)
            ForEach(family, id: \.id) { member in
                Button {
                    model.selectNavigatorRoot(root.id, askID: member.id != root.id ? member.id : nil)
                    if member.id == root.id {
                        model.setNavigatorDocument(.original, rootID: root.id)
                    }
                    model.navigator.tab = .activity
                } label: {
                    HStack {
                        Text(member.id == root.id ? "Original" : member.name)
                            .lineLimit(1)
                        Spacer()
                        Text(member.status.replacingOccurrences(of: "_", with: " ").capitalized)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                .buttonStyle(.plain)
                if !member.description.isEmpty, member.id != root.id {
                    Text(member.description)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(3)
                }
            }
        }
        .accessibilityIdentifier("navigator-history-revisions")
    }

}
