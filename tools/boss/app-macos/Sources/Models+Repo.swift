import Foundation

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
