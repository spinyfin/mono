import Foundation

enum WorkBoardColumnKey: String, CaseIterable, Identifiable {
    case backlog
    case doing
    case review
    case done

    var id: String { rawValue }

    var title: String {
        switch self {
        case .backlog:
            return "Backlog"
        case .doing:
            return "Doing"
        case .review:
            return "Review"
        case .done:
            return "Done"
        }
    }

    var targetStatus: String {
        switch self {
        case .backlog:
            return "todo"
        case .doing:
            return "active"
        case .review:
            return "in_review"
        case .done:
            return "done"
        }
    }
}

enum WorkBoardGrouping: String, CaseIterable, Identifiable {
    case none
    case project

    var id: String { rawValue }

    var title: String {
        switch self {
        case .none:
            return "Ungrouped"
        case .project:
            return "Project"
        }
    }
}

struct WorkSidebarRow: Identifiable {
    let id: WorkNodeID
    let title: String
    let subtitle: String?
    let statusBadge: String?
    let systemImage: String
    let depth: Int
}

enum WorkCreateKind {
    case product
    case project(productID: String)
    case task(productID: String, projectID: String)
    case chore(productID: String)
}

struct WorkCreateRequest: Identifiable {
    let id = UUID()
    let kind: WorkCreateKind
}

/// Static copy for the Product create form's repo-URL field. Extracted
/// so the wording can be asserted in a unit test without driving the
/// SwiftUI view itself — per design Q10, the form must surface the
/// field as optional and explain that products spanning multiple repos
/// rely on per-work-item overrides.
enum ProductRepoFieldCopy {
    static let placeholder = "Remote URL (optional)"
    static let helperText =
        "Optional. Leave blank if this product spans multiple repos; per-work-item repo overrides will be required."
}

/// Pure-data form state for the chore/task create form's repo field,
/// per design Q10 / follow-up chore #10 of
/// `multi-repo-work-modeling.md`. Lives outside the SwiftUI view so
/// the two render modes ("product has default" vs "product has no
/// default") and the submission shape can be pinned by XCTest without
/// spinning up a host. The view is a thin reflection of this state.
struct WorkCreateRepoFormState: Equatable {
    enum Mode: Equatable {
        /// Parent product has a `repo_remote_url`. The field is hidden
        /// by default, with an "Override repo…" disclosure that
        /// expands the picker. Inheriting the default is the
        /// no-action path.
        case productHasDefault(defaultURL: String)
        /// Parent product has no default. The field is shown and
        /// required. A "Set as product default" affordance becomes
        /// available for fresh URLs (URLs not already in the
        /// product's empirical known-repo set).
        case productHasNoDefault
    }

    var mode: Mode
    /// Distinct URL set across the product's existing tasks / chores
    /// plus the product default — mirrors the CLI's
    /// `known_repos_for_product` (multi-repo design Q4). Drives the
    /// "Recent repos" picker.
    var knownRepos: [String]
    /// In `.productHasDefault`, whether the user expanded the
    /// "Override repo…" disclosure. Ignored in
    /// `.productHasNoDefault` (the field is always visible there).
    var overrideEnabled: Bool
    /// Text in the URL field. Empty when the disclosure is closed in
    /// `.productHasDefault`; user-supplied otherwise.
    var enteredURL: String
    /// State of the "Set as product default" checkbox. Only
    /// meaningful when `showSetAsProductDefaultCheckbox` is `true` —
    /// the view hides the affordance otherwise.
    var setAsProductDefault: Bool

    /// Initial state for a fresh sheet. Picks the mode from the
    /// parent product's repo URL: empty / whitespace → no default.
    init(productRepoURL: String?, knownRepos: [String]) {
        let normalized = productRepoURL?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if let normalized, !normalized.isEmpty {
            mode = .productHasDefault(defaultURL: normalized)
        } else {
            mode = .productHasNoDefault
        }
        self.knownRepos = knownRepos
        overrideEnabled = false
        enteredURL = ""
        setAsProductDefault = false
    }

    /// URL the submission should write to `tasks.repo_remote_url`.
    /// `nil` means "inherit the product default" — the engine treats
    /// an absent field exactly that way.
    var submittedURL: String? {
        let trimmed = enteredURL.trimmingCharacters(in: .whitespacesAndNewlines)
        switch mode {
        case .productHasDefault:
            return (overrideEnabled && !trimmed.isEmpty) ? trimmed : nil
        case .productHasNoDefault:
            return trimmed.isEmpty ? nil : trimmed
        }
    }

    /// True when the create button should be disabled because the
    /// repo field is required and unfilled. The product-has-default
    /// mode never blocks submission on the repo field (inheriting is
    /// always valid); the no-default mode requires a URL.
    var isSubmissionBlocked: Bool {
        switch mode {
        case .productHasDefault:
            return false
        case .productHasNoDefault:
            return enteredURL.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        }
    }

    /// Whether the "Set as product default" checkbox should be
    /// rendered. Only meaningful in `.productHasNoDefault` mode, and
    /// only when the user has typed a *fresh* URL — one not already
    /// in the empirical known-repo set. The design's intent is that
    /// the affordance promotes a brand-new repo URL into the product
    /// default; offering it on a URL the product has already seen
    /// would be redundant.
    var showSetAsProductDefaultCheckbox: Bool {
        guard case .productHasNoDefault = mode else { return false }
        let trimmed = enteredURL.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return false }
        return !knownRepos.contains(trimmed)
    }

    /// Whether the form should send an `update_work_item` patch on
    /// the parent product to set `repo_remote_url` as a side-effect
    /// of work-item creation. Encodes the "Set as product default"
    /// rule end-to-end: the checkbox must be both visible and ticked.
    var shouldSetAsProductDefault: Bool {
        showSetAsProductDefaultCheckbox && setAsProductDefault
    }
}

/// Static copy for the work-item (chore + task) create form's repo
/// field. Extracted from the SwiftUI view for the same reason as
/// `ProductRepoFieldCopy`: the wording is part of the contract with
/// the user (design Q10 calls it out explicitly) and a UI tweak that
/// drops the "required" cue or the override disclosure label should
/// trip a failing test.
enum WorkItemRepoFieldCopy {
    /// Field placeholder when the repo input is required (product has
    /// no default).
    static let requiredPlaceholder = "Repo remote URL (required)"
    /// Field placeholder when the repo input is an optional override
    /// (product has a default and the disclosure is expanded).
    static let overridePlaceholder = "Repo remote URL"
    /// Disclosure title in product-has-default mode.
    static let overrideDisclosureLabel = "Override repo…"
    /// Helper text under the field in product-has-no-default mode.
    static let requiredHelperText =
        "Required. This product has no default repo, so each work item must specify its own."
    /// Helper text under the field in product-has-default mode when
    /// the override disclosure is expanded.
    static let overrideHelperText =
        "Leave blank to inherit the product's default repo."
    /// "Set as product default" checkbox label. Visible only when the
    /// product has no default and the user has entered a fresh URL.
    static let setAsProductDefaultLabel = "Set as product default"
    /// "Recent repos" picker label.
    static let recentReposLabel = "Recent repos"
}

struct WorkEditRequest: Identifiable {
    let id = UUID()
    let item: WorkItemPayload
}

struct WorkBoardSection: Identifiable {
    let id: String
    let title: String
    let items: [WorkTask]
    var isCollapsible: Bool = false
    var defaultExpanded: Bool = true
    /// Project id this section represents when the board is grouped
    /// by project. `nil` for chores / un-projected sections / column
    /// groupings. The kanban project-card affordance (design-doc
    /// icon) reads this to look up the resolved
    /// `ProjectDesignDocState` for the section's header row.
    var projectID: String? = nil
}
