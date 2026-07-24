import AppKit
import os.log
import SwiftUI
import UpdateCore

struct CollapsibleWorkBoardSection<Accessory: View, Content: View>: View {
    let sectionID: String
    let title: String
    let count: Int
    let defaultExpanded: Bool
    var shortIDLabel: String? = nil
    /// "Trunk queue paused/draining" (or similar) banner shown under the
    /// header, always visible regardless of collapse state so a stalled
    /// queue is noticeable without expanding the section.
    var banner: String? = nil
    @ViewBuilder let accessory: () -> Accessory
    @ViewBuilder let content: () -> Content

    @State private var userToggled: Bool

    init(
        sectionID: String,
        title: String,
        count: Int,
        defaultExpanded: Bool,
        shortIDLabel: String? = nil,
        banner: String? = nil,
        @ViewBuilder accessory: @escaping () -> Accessory = { EmptyView() },
        @ViewBuilder content: @escaping () -> Content
    ) {
        self.sectionID = sectionID
        self.title = title
        self.count = count
        self.defaultExpanded = defaultExpanded
        self.shortIDLabel = shortIDLabel
        self.banner = banner
        self.accessory = accessory
        self.content = content
        let stored = UserDefaults.standard.object(
            forKey: "boss.kanban.section.\(sectionID).userToggled"
        ) as? Bool
        self._userToggled = State(initialValue: stored ?? false)
    }

    private var isExpanded: Bool {
        userToggled ? !defaultExpanded : defaultExpanded
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 6) {
                Button {
                    let next = !userToggled
                    userToggled = next
                    UserDefaults.standard.set(
                        next, forKey: "boss.kanban.section.\(sectionID).userToggled"
                    )
                } label: {
                    HStack(spacing: 6) {
                        Image(systemName: isExpanded ? "chevron.down" : "chevron.right")
                            .font(.caption2.weight(.semibold))
                            .foregroundStyle(.secondary)
                            .frame(width: 10)
                        Text("\(title) (\(count))")
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.secondary)
                        if let label = shortIDLabel {
                            Text(label)
                                .font(.system(.caption2, design: .monospaced))
                                .foregroundStyle(.secondary)
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)

                accessory()
            }

            if let banner {
                HStack(spacing: 4) {
                    Image(systemName: "pause.circle.fill")
                        .font(.caption2)
                    Text(banner)
                        .font(.caption2)
                }
                .foregroundStyle(.orange)
            }

            if isExpanded {
                content()
            }
        }
        .id(sectionID)
    }
}

struct WorkSidebarFilterRow: View {
    let title: String
    let subtitle: String?
    let systemImage: String
    let isSelected: Bool
    let trailing: String?
    var showsCheckbox: Bool = false
    var isCheckboxOn: Bool = false
    /// Render the row in a muted style — used for archived projects so
    /// they're visibly distinct from active ones when the user opts in
    /// to seeing them.
    var dimmed: Bool = false
    /// When non-nil, a green `▶ N` chip is shown for unblocked (todo)
    /// task count. Suppressed when nil (no unblocked tasks).
    var unblockedCount: Int? = nil
    /// When non-nil, a red `⏸ N` chip is shown for dependency-blocked
    /// task count. Suppressed when nil (no dependency-blocked tasks).
    var blockedCount: Int? = nil
    /// When non-nil, shows a design-doc affordance link under the badges.
    /// Suppressed when nil (project has no design doc pointer set).
    var designDocPresentation: ProjectDesignDocAffordancePresentation? = nil
    /// Called when the user clicks the design-doc affordance. Required
    /// when `designDocPresentation` is non-nil.
    var onOpenDesignDoc: (() -> Void)? = nil

    private var hasExtraRow: Bool {
        (subtitle != nil && !subtitle!.isEmpty) || designDocPresentation != nil
    }

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            if showsCheckbox {
                Image(systemName: isCheckboxOn ? "checkmark.square.fill" : "square")
                    .foregroundStyle(isCheckboxOn ? Color.accentColor : .secondary)
                    .font(.system(size: 14, weight: .medium))
                    .frame(width: 15, alignment: .center)
                    .padding(.top, 2)
                    .opacity(dimmed && !isCheckboxOn ? 0.6 : 1.0)
            } else {
                Image(systemName: systemImage)
                    .foregroundStyle(isSelected ? .primary : .secondary)
                    .font(.system(size: 14, weight: .medium))
                    .frame(width: 15, alignment: .center)
                    .padding(.top, 2)
            }
            VStack(alignment: .leading, spacing: subtitle != nil && !subtitle!.isEmpty ? 2 : 0) {
                HStack(alignment: .top, spacing: 8) {
                    if dimmed {
                        Image(systemName: systemImage)
                            .foregroundStyle(.secondary)
                            .font(.system(size: 12, weight: .medium))
                            .padding(.top, 3)
                            .help("Archived")
                    }
                    Text(title)
                        .font(.body.weight(isSelected ? .semibold : .regular))
                        .foregroundStyle(dimmed ? .secondary : .primary)
                        .lineLimit(2)
                        .truncationMode(.tail)
                        .fixedSize(horizontal: false, vertical: true)
                        .layoutPriority(1)
                        .help(title)

                    Spacer(minLength: 6)

                    if let trailing, !trailing.isEmpty {
                        WorkStatusBadge(text: trailing, emphasized: isSelected)
                            .fixedSize(horizontal: true, vertical: false)
                            .layoutPriority(2)
                            .opacity(dimmed ? 0.65 : 1.0)
                    }
                    if let blockedCount {
                        ProjectTaskCountChip(count: blockedCount, kind: .blocked)
                            .fixedSize(horizontal: true, vertical: false)
                            .layoutPriority(2)
                            .opacity(dimmed ? 0.65 : 1.0)
                    }
                    if let unblockedCount {
                        ProjectTaskCountChip(count: unblockedCount, kind: .unblocked)
                            .fixedSize(horizontal: true, vertical: false)
                            .layoutPriority(2)
                            .opacity(dimmed ? 0.65 : 1.0)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)

                if (subtitle != nil && !subtitle!.isEmpty) || designDocPresentation != nil {
                    HStack(alignment: .center, spacing: 6) {
                        if let subtitle, !subtitle.isEmpty {
                            Text(subtitle)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .lineLimit(1)
                        }
                        Spacer(minLength: 0)
                        if let presentation = designDocPresentation, let openDoc = onOpenDesignDoc {
                            Button(action: openDoc) {
                                Image(systemName: presentation.systemImage)
                                    .font(.caption)
                                    .foregroundStyle(presentation.tint)
                                    .accessibilityLabel(presentation.accessibilityLabel)
                            }
                            .buttonStyle(.plain)
                            .help(presentation.tooltip)
                        }
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.leading, 8)
        .padding(.trailing, 4)
        .padding(.vertical, hasExtraRow ? 7 : 6)
        .contentShape(Rectangle())
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

struct WorkProjectFilterToolbarButton: View {
    @ObservedObject var model: ChatViewModel
    @State private var isShowingPopover = false

    var body: some View {
        Button {
            isShowingPopover.toggle()
        } label: {
            Image(systemName: "square.stack.3d.up")
                .overlay(alignment: .topTrailing) {
                    if model.hasProjectFilters {
                        Circle()
                            .fill(Color.accentColor)
                            .frame(width: 6, height: 6)
                            .offset(x: 3, y: -3)
                    }
                }
        }
        .help("Project filter")
        .popover(isPresented: $isShowingPopover, arrowEdge: .bottom) {
            ProjectFilterPopover(model: model)
        }
    }
}

struct WorkGroupToolbarMenu: View {
    @ObservedObject var model: ChatViewModel

    var body: some View {
        Menu {
            ForEach(WorkBoardGrouping.allCases) { grouping in
                Button {
                    model.setWorkBoardGrouping(grouping)
                } label: {
                    if model.workBoardGrouping == grouping {
                        Label(grouping.title, systemImage: "checkmark")
                    } else {
                        Text(grouping.title)
                    }
                }
            }
        } label: {
            Image(systemName: "rectangle.3.group")
        }
        .help("Group by")
    }
}

struct WorkSearchToolbarItem: View {
    @ObservedObject var model: ChatViewModel
    @Binding var isExpanded: Bool

    var body: some View {
        if isExpanded {
            SearchTextField(
                text: $model.workSearchText,
                onEscape: {
                    isExpanded = false
                    model.workSearchText = ""
                },
                onFocusLost: {
                    isExpanded = false
                }
            )
            .frame(width: 160)
        } else {
            Button {
                isExpanded = true
            } label: {
                Image(systemName: "magnifyingglass")
            }
            .help("Search (⌘F)")
            .keyboardShortcut("f", modifiers: .command)
        }
    }
}

private struct SearchTextField: View {
    @Binding var text: String
    var onEscape: () -> Void
    var onFocusLost: () -> Void
    @FocusState private var isFocused: Bool

    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(.secondary)
                .font(.system(size: 11))
            TextField("Search", text: $text)
                .textFieldStyle(.plain)
                .focused($isFocused)
                .onKeyPress(.escape) {
                    onEscape()
                    return .handled
                }
        }
        .padding(.horizontal, 7)
        .padding(.vertical, 4)
        .background(.quaternary, in: Capsule())
        .onAppear { isFocused = true }
        .onChange(of: isFocused) { _, focused in
            if !focused { onFocusLost() }
        }
    }
}

private struct ProjectFilterPopover: View {
    @ObservedObject var model: ChatViewModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            let allSelected = !model.hasProjectFilters
            Button {
                model.clearProjectFilters()
            } label: {
                HStack(spacing: 8) {
                    Image(systemName: allSelected ? "checkmark.square.fill" : "square")
                        .foregroundStyle(allSelected ? Color.accentColor : .secondary)
                        .font(.system(size: 14, weight: .medium))
                        .frame(width: 15)
                    Text("All Projects")
                        .font(.body.weight(allSelected ? .semibold : .regular))
                    Spacer()
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            Divider()

            ForEach(model.projectsForSelectedProduct) { project in
                let isOn = model.selectedProjectFilterIDs.contains(project.id)
                Button {
                    model.toggleProjectFilter(project.id)
                } label: {
                    HStack(spacing: 8) {
                        Image(systemName: isOn ? "checkmark.square.fill" : "square")
                            .foregroundStyle(isOn ? Color.accentColor : .secondary)
                            .font(.system(size: 14, weight: .medium))
                            .frame(width: 15)
                        Text(project.name)
                            .font(.body.weight(isOn ? .semibold : .regular))
                            .lineLimit(1)
                            .truncationMode(.tail)
                        if let id = project.shortID {
                            Text("P" + String(id))
                                .font(.system(.caption2, design: .monospaced))
                                .foregroundStyle(.secondary)
                        }
                        Spacer()
                    }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 8)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
        .frame(minWidth: 200, maxWidth: 280)
        .padding(.vertical, 4)
    }
}

/// Compact warning banner shown in the sidebar below the product picker
/// when the external-tracker reconciler has unresolved attention items for
/// the selected product. Clicking the banner opens a popover with each
/// item's title and body. Disappears automatically once all items are
/// resolved (resolved_at set).

struct SidebarProductPicker: View {
    @Binding var selection: String?
    let products: [WorkProduct]

    var body: some View {
        Picker("Product", selection: $selection) {
            ForEach(products) { product in
                Text(product.name).tag(product.id as String?)
            }
        }
        .labelsHidden()
        .pickerStyle(.menu)
    }
}

/// Friendly label for `WorkTaskRuntime.dispatchWaitReason` (mirrors
/// `TaskRuntime.dispatch_wait_reason` / the dispatcher's `details.reason`
/// on the `worker_claimed`/skipped dispatch event). Falls back to the raw
/// reason string for any value this hasn't been taught yet, so a new
/// engine-side defer reason still renders something useful instead of a
/// blank card.
///
/// The `chain_serialized`/`chain_serialized_review_held` defer reasons are
/// no longer matched here as fixed codes: the engine (`coordinator.rs`'s
/// `chain_serialized_wait_reason`) now persists a fully-formed, operator-
/// facing sentence naming the concrete blocking task and PR (e.g. "blocked
/// by T2468 'Fix failing CI' on mono#1901 (revisions on the same PR run one
/// at a time)") directly into `dispatch_wait_reason`, so it falls through
/// to the `default` case below unchanged (T2469 incident: the old fixed
/// "blocked behind a live PR sibling" copy named neither the blocking task
/// nor the PR, and used engine-internal "sibling" vocabulary).
