import Foundation

// ===========================================================================
// Work board and item-creation UI state. Column/grouping vocabulary, the
// create/edit request payloads and their repo form state, the board's section
// model, and the pure-data presentation models the kanban derives from a
// `WorkTask` (repo chips, repo overrides, upstream links) together with the
// repo/PR URL helpers those presentations are built on. Split out of
// Models.swift to keep that file under the repo's file-size check.
// ===========================================================================

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

enum WorkItemPayload {
    case product(WorkProduct)
    case project(WorkProject)
    case task(WorkTask)
    case chore(WorkTask)

    var id: String {
        switch self {
        case .product(let product):
            return product.id
        case .project(let project):
            return project.id
        case .task(let task), .chore(let task):
            return task.id
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
    /// "Trunk queue paused/draining" banner shown in the section header
    /// (`ChatViewModel.mergingSection`'s `MergeQueueDetail.queueStateBanner`
    /// rollup). `nil` for every section except "Merging" while a tracked
    /// Trunk queue is non-`RUNNING`.
    var queueBannerText: String? = nil
}

/// Swift mirror of `boss_protocol::short_name_for(url)` from the
/// multi-repo work modeling design (Q3). The canonical short name is
/// the URL's path basename minus a trailing `.git`. Handles both
/// `https://github.com/foo/bar.git` and SCP-style
/// `git@github.com:foo/bar.git`. Falls back to the trimmed input when
/// neither shape is recognisable so the chip never renders empty.
func shortRepoName(for repoURL: String) -> String {
    let trimmed = repoURL.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !trimmed.isEmpty else { return repoURL }
    let lastSlash = trimmed.split(separator: "/", omittingEmptySubsequences: true).last
        .map(String.init) ?? trimmed
    let lastSegment = lastSlash.split(separator: ":", omittingEmptySubsequences: true).last
        .map(String.init) ?? lastSlash
    if lastSegment.hasSuffix(".git") {
        return String(lastSegment.dropLast(4))
    }
    return lastSegment
}

/// Parsed `(org, repo, number)` triple for a GitHub PR URL like
/// `https://github.com/<org>/<repo>/pull/<n>`. Returns `nil` for any
/// other host or shape — the caller falls back to the raw URL string.
/// Used both by the kanban PR-link label renderer and by the
/// board-local ambiguity detector that decides whether to expand
/// `repo#n` back to `org/repo#n`.
func parseGitHubPRURL(_ urlString: String) -> (org: String, repo: String, number: String)? {
    guard let url = URL(string: urlString),
          let host = url.host?.lowercased(),
          host == "github.com" || host == "www.github.com"
    else {
        return nil
    }
    let parts = url.path.split(separator: "/", omittingEmptySubsequences: true).map(String.init)
    guard parts.count == 4,
          parts[2] == "pull",
          !parts[0].isEmpty,
          !parts[1].isEmpty,
          !parts[3].isEmpty,
          parts[3].allSatisfy(\.isNumber)
    else {
        return nil
    }
    return (org: parts[0], repo: parts[1], number: parts[3])
}

/// Whether two PR URL strings identify the same GitHub pull request.
/// Compares the parsed `(org, repo, number)` triple case-insensitively
/// on org/repo so incidental formatting differences don't defeat the
/// match; falls back to exact string equality when either URL isn't a
/// parseable GitHub PR URL. Used to dedup a card's PR-link rows when
/// two different task fields (e.g. a revision's own `prURL` and its
/// `revisionParentPrUrl`) happen to resolve to the same PR.
func sameGitHubPR(_ a: String, _ b: String) -> Bool {
    if let pa = parseGitHubPRURL(a), let pb = parseGitHubPRURL(b) {
        return pa.org.lowercased() == pb.org.lowercased()
            && pa.repo.lowercased() == pb.repo.lowercased()
            && pa.number == pb.number
    }
    return a == b
}

/// Repo names (lowercased) that appear with two or more distinct orgs
/// across the supplied card set's PR URLs. A name in this set means
/// `repo#n` alone is ambiguous on the current board, so the kanban
/// must fall back to the full `org/repo#n` label for that PR.
///
/// Non-GitHub PR URLs and cards without a PR URL are ignored — they
/// can never collide on a repo-name basis.
func ambiguousPRRepoNames(in cards: [WorkTask]) -> Set<String> {
    var orgsByRepo: [String: Set<String>] = [:]
    for task in cards {
        guard let prURL = task.prURL,
              let parsed = parseGitHubPRURL(prURL)
        else { continue }
        let repoKey = parsed.repo.lowercased()
        let orgKey = parsed.org.lowercased()
        orgsByRepo[repoKey, default: []].insert(orgKey)
    }
    return Set(orgsByRepo.compactMap { $0.value.count > 1 ? $0.key : nil })
}

/// Label to display for a PR URL on a kanban card.
///
/// - Returns `nil` when `urlString` isn't a parseable GitHub PR URL —
///   the caller should fall back to the raw URL string.
/// - Returns `repo#n` when the repo name is unambiguous across the
///   supplied `ambiguousRepoNames` set (the board-local disambiguation
///   key from [[ambiguousPRRepoNames(in:)]]).
/// - Returns `org/repo#n` when the repo name *is* in that set, or when
///   the set is `nil` (caller wants the always-full form, e.g. for the
///   detail popover and the hover tooltip).
func pullRequestLinkLabel(
    for urlString: String,
    ambiguousRepoNames: Set<String>?
) -> String? {
    guard let parsed = parseGitHubPRURL(urlString) else { return nil }
    if let ambiguous = ambiguousRepoNames,
       !ambiguous.contains(parsed.repo.lowercased()) {
        return "\(parsed.repo)#\(parsed.number)"
    }
    return "\(parsed.org)/\(parsed.repo)#\(parsed.number)"
}

/// How the kanban should surface the repo for a product, derived from
/// the work item description for "macOS: kanban card repo chip" and
/// design Q7. Single-repo mode lifts one chip to the product header;
/// multi-repo mode prints a chip on every card. `none` collapses the
/// affordance — the product has no default and no card overrides, so
/// there is nothing repo-shaped to surface.
enum WorkBoardRepoMode: Equatable {
    case singleRepo(url: String)
    case multiRepo
    case none

    /// Compute the mode from the product default and the visible card
    /// set. The rule per the work item description:
    /// - Multi-repo as soon as any card carries a per-task override OR
    ///   resolved URLs differ across cards.
    /// - Single-repo when no overrides exist and a product default is
    ///   set; every card inherits the same URL.
    /// - None when neither product nor any card carries a URL.
    static func compute(
        productRepoURL: String?,
        cards: [WorkTask]
    ) -> WorkBoardRepoMode {
        let productURL = nonEmpty(productRepoURL)
        let overrides = cards.compactMap { nonEmpty($0.repoRemoteURL) }
        if overrides.isEmpty {
            if let productURL { return .singleRepo(url: productURL) }
            return .none
        }
        // Any override → multi-repo, even when overrides happen to all
        // match the product default. A user who set an explicit
        // override on a card has stated *I want this row's repo to be
        // visible*; suppressing the chip would erase that signal.
        return .multiRepo
    }

    private static func nonEmpty(_ value: String?) -> String? {
        guard let value, !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return nil
        }
        return value
    }
}

/// Pure-data presentation for the kanban repo chip — short name for
/// the rendered text, full URL for the hover tooltip, and a
/// provenance string that tells the reader where the URL came from.
/// Lives outside the SwiftUI view so tests can pin chip text + tooltip
/// without spinning up a host (mirrors
/// `ProjectDesignDocAffordancePresentation`).
///
/// Per-card chips only render when the card carries information the
/// product header can't: either an explicit override of the product
/// default, or the card's own URL on a product with no default. A
/// card that simply inherits the product default never gets a chip —
/// the chip would be redundant with the header.
struct RepoChipPresentation: Equatable {
    let shortName: String
    let fullURL: String
    let provenance: Provenance

    enum Provenance: Equatable {
        /// Chip lives on the product header, identifying the product's
        /// default repo. Not used for per-card chips.
        case productDefault
        /// Card has its own `repoRemoteURL`. On a product with a
        /// default this is a true override; on a no-default product
        /// the card's URL is just the card's repo. Either way the
        /// chip is informative.
        case taskOverride
    }

    var tooltip: String {
        switch provenance {
        case .productDefault:
            return "\(fullURL)\nInherited from product"
        case .taskOverride:
            return "\(fullURL)\nRepo set on this card"
        }
    }

    var accessibilityLabel: String {
        switch provenance {
        case .productDefault:
            return "Repo \(shortName), inherited from product"
        case .taskOverride:
            return "Repo \(shortName), set on this card"
        }
    }

    /// Build a chip for one card given the parent product's default.
    /// Returns `nil` when the card has no per-row `repoRemoteURL` or when
    /// the task's repo matches the product default (case-insensitive,
    /// trimming `.git` suffix). Returns non-nil only when the task has an
    /// explicit repo that differs from the product default, or when the
    /// product has no default but the task does.
    static func forCard(
        task: WorkTask,
        productRepoURL: String?
    ) -> RepoChipPresentation? {
        guard let override = nonEmpty(task.repoRemoteURL) else {
            return nil
        }
        if let productDefault = nonEmpty(productRepoURL),
           reposEqual(override, productDefault) {
            return nil
        }
        return RepoChipPresentation(
            shortName: shortRepoName(for: override),
            fullURL: override,
            provenance: .taskOverride
        )
    }

    /// Build the chip carried on the product header in single-repo
    /// mode. Always provenance `.productDefault` — single-repo mode
    /// requires zero overrides by construction.
    static func forProductHeader(productRepoURL: String) -> RepoChipPresentation {
        RepoChipPresentation(
            shortName: shortRepoName(for: productRepoURL),
            fullURL: productRepoURL,
            provenance: .productDefault
        )
    }

    private static func nonEmpty(_ value: String?) -> String? {
        guard let value, !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return nil
        }
        return value
    }

    private static func reposEqual(_ url1: String, _ url2: String) -> Bool {
        let normalize = { (url: String) in
            var normalized = url.lowercased()
            if normalized.hasSuffix(".git") {
                normalized.removeLast(4)
            }
            return normalized
        }
        return normalize(url1) == normalize(url2)
    }
}

/// Pure-data presentation for the work-item detail "Repo:" row.
/// Mirrors the CLI `boss <kind> show` Repo line so the macOS detail
/// popover and the terminal output stay in lockstep on the
/// provenance vocabulary (per Follow-up chore #12 of
/// `multi-repo-work-modeling.md`). Three states correspond to the
/// three branches of the engine's `resolve_repo_for_work_item`:
/// override on the work item, inherited from the parent product, or
/// no resolution at all (the work item cannot dispatch).
///
/// `provenanceLabel` is the parenthetical that follows the URL on
/// the CLI; the `.none` case has no URL and the label is the entire
/// line. The Swift view renders the label as a secondary-style
/// caption beneath the URL.
struct RepoOverridePresentation: Equatable {
    let resolvedURL: String?
    let provenanceLabel: String
    let provenance: Provenance

    enum Provenance: Equatable {
        case taskOverride
        case productDefault(productSlug: String)
        case none
    }

    /// Full single-line form, matching the CLI `Repo: <url>
    /// (<provenance>)` shape. Used by tests to pin the wire-shape
    /// agreement between CLI and macOS UI; the view itself renders
    /// the URL and label as separate text rows so each can carry its
    /// own style.
    var cliLine: String {
        switch provenance {
        case .taskOverride, .productDefault:
            if let url = resolvedURL { return "\(url) (\(provenanceLabel))" }
            return provenanceLabel
        case .none:
            return provenanceLabel
        }
    }

    /// Build the presentation for one work item given its parent
    /// product (or `nil` when the product can't be resolved — e.g. a
    /// snapshot in flight). When the product is unavailable, we can
    /// only honour the override; an empty override collapses to the
    /// "cannot dispatch" state so the row never silently looks
    /// inherited from a product that isn't there.
    static func resolve(
        task: WorkTask,
        product: WorkProduct?
    ) -> RepoOverridePresentation {
        if let override = nonEmpty(task.repoRemoteURL) {
            return RepoOverridePresentation(
                resolvedURL: override,
                provenanceLabel: "override on this work item",
                provenance: .taskOverride
            )
        }
        if let product, let inherited = nonEmpty(product.repoRemoteURL) {
            return RepoOverridePresentation(
                resolvedURL: inherited,
                provenanceLabel: "inherited from product `\(product.slug)`",
                provenance: .productDefault(productSlug: product.slug)
            )
        }
        return RepoOverridePresentation(
            resolvedURL: nil,
            provenanceLabel: "(none — work item cannot dispatch)",
            provenance: .none
        )
    }

    private static func nonEmpty(_ value: String?) -> String? {
        guard let value, !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return nil
        }
        return value
    }
}

/// Presentation model for the kanban card's upstream-link affordance.
/// Derived from `WorkTask.externalRef`; `nil` when no external ref is present.
///
/// Three states map to three visual treatments:
/// - `externalRef == nil` → `forTask` returns `nil` (no affordance)
/// - `externalRef.unboundAt == nil` → bound; label in accent color, opens URL
/// - `externalRef.unboundAt != nil` → stale; label dimmed/strikethrough, still opens URL
struct ExternalRefLinkPresentation: Equatable {
    /// Short label rendered on the card, e.g. `↗ #560`.
    let label: String
    /// Canonical browser URL to open on click.
    let url: String
    /// Hover tooltip text.
    let tooltip: String
    /// True when the upstream binding was cleared (`unboundAt` is set).
    let isStale: Bool

    /// Derive the presentation for a task. Returns `nil` when the task has no
    /// external ref — callers use this to suppress the affordance entirely.
    static func forTask(_ task: WorkTask) -> ExternalRefLinkPresentation? {
        guard let ref = task.externalRef else { return nil }
        let stale = ref.unboundAt != nil
        let label = issueLabel(from: ref.canonicalID)
        var tooltip = ref.canonicalID
        if stale {
            tooltip += "\nUpstream binding cleared"
        } else if let syncedAt = ref.syncedAt {
            tooltip += "\nLast synced: \(syncedAt)"
        }
        return ExternalRefLinkPresentation(label: label, url: ref.webURL, tooltip: tooltip, isStale: stale)
    }

    /// Extracts a short display label from a canonical ID. For GitHub
    /// (`"spinyfin/mono#560"`) this yields `"↗ #560"`. Any canonical ID
    /// without a `#` fragment falls back to `"↗ <canonical_id>"`.
    static func issueLabel(from canonicalID: String) -> String {
        if let hashIdx = canonicalID.lastIndex(of: "#") {
            let fragment = String(canonicalID[hashIdx...])
            return "↗ \(fragment)"
        }
        return "↗ \(canonicalID)"
    }
}
