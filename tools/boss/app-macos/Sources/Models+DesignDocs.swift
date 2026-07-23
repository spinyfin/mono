import Foundation

// MARK: - Wire mirrors

/// Swift mirror of `boss_protocol::DesignDocEntry` — one markdown file
/// in a product repo's tree at HEAD.
struct DesignDocEntry: Codable, Hashable {
    /// Repo-relative path, e.g. `docs/design-docs/foo.md`.
    var path: String
    var size: UInt64?
}

/// Swift mirror of `boss_protocol::DesignDocTree`.
///
/// The engine sends the flat, path-sorted file list; nesting into a
/// directory tree happens client-side in
/// [[DesignDocTreeBuilder.build(from:)]]. Documents are addressed by
/// `gitRef` (a commit sha), not by branch, so a push landing while the
/// operator browses cannot change what a click opens.
struct DesignDocTree: Codable, Hashable {
    var repoRemoteURL: String
    var ownerRepo: String
    var branch: String
    var gitRef: String
    var entries: [DesignDocEntry]
    var fetchedAt: String
    var truncated: Bool

    enum CodingKeys: String, CodingKey {
        case repoRemoteURL = "repo_remote_url"
        case ownerRepo = "owner_repo"
        case branch
        case gitRef = "git_ref"
        case entries
        case fetchedAt = "fetched_at"
        case truncated
    }

    init(
        repoRemoteURL: String,
        ownerRepo: String,
        branch: String,
        gitRef: String,
        entries: [DesignDocEntry],
        fetchedAt: String,
        truncated: Bool = false
    ) {
        self.repoRemoteURL = repoRemoteURL
        self.ownerRepo = ownerRepo
        self.branch = branch
        self.gitRef = gitRef
        self.entries = entries
        self.fetchedAt = fetchedAt
        self.truncated = truncated
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        repoRemoteURL = try container.decode(String.self, forKey: .repoRemoteURL)
        ownerRepo = try container.decode(String.self, forKey: .ownerRepo)
        branch = try container.decode(String.self, forKey: .branch)
        gitRef = try container.decode(String.self, forKey: .gitRef)
        entries = try container.decode([DesignDocEntry].self, forKey: .entries)
        fetchedAt = try container.decode(String.self, forKey: .fetchedAt)
        // `#[serde(default)]` on the Rust side: an engine that predates
        // the field omits it entirely rather than sending null.
        truncated = try container.decodeIfPresent(Bool.self, forKey: .truncated) ?? false
    }
}

/// Swift mirror of `boss_protocol::DesignDocTreeState`.
///
/// The four non-`loaded` cases are the four conditions the Designs tab
/// must describe differently — each has a different remedy, and the
/// engine (not the view) decides which one applies.
enum DesignDocTreeState: Hashable {
    /// The product has no repo configured. Remedy: set one.
    case noRepoConfigured
    /// Configured, but GitHub could not be reached / the repo is
    /// missing / the credential cannot see it.
    case unreachable(repoRemoteURL: String, reason: String)
    /// GitHub is throttling us. Remedy: wait, then reload.
    case rateLimited(repoRemoteURL: String, reason: String)
    /// Read fine; the repo genuinely has no markdown at HEAD.
    case empty(repoRemoteURL: String, ownerRepo: String, gitRef: String)
    case loaded(tree: DesignDocTree)
}

extension DesignDocTreeState: Codable {
    enum CodingKeys: String, CodingKey {
        case type
        case repoRemoteURL = "repo_remote_url"
        case ownerRepo = "owner_repo"
        case gitRef = "git_ref"
        case reason
        case tree
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let type = try container.decode(String.self, forKey: .type)
        switch type {
        case "no_repo_configured":
            self = .noRepoConfigured
        case "unreachable":
            self = .unreachable(
                repoRemoteURL: try container.decode(String.self, forKey: .repoRemoteURL),
                reason: try container.decode(String.self, forKey: .reason)
            )
        case "rate_limited":
            self = .rateLimited(
                repoRemoteURL: try container.decode(String.self, forKey: .repoRemoteURL),
                reason: try container.decode(String.self, forKey: .reason)
            )
        case "empty":
            self = .empty(
                repoRemoteURL: try container.decode(String.self, forKey: .repoRemoteURL),
                ownerRepo: try container.decode(String.self, forKey: .ownerRepo),
                gitRef: try container.decode(String.self, forKey: .gitRef)
            )
        case "loaded":
            self = .loaded(tree: try container.decode(DesignDocTree.self, forKey: .tree))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: container,
                debugDescription: "Unknown DesignDocTreeState type: \(type)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .noRepoConfigured:
            try container.encode("no_repo_configured", forKey: .type)
        case .unreachable(let repoRemoteURL, let reason):
            try container.encode("unreachable", forKey: .type)
            try container.encode(repoRemoteURL, forKey: .repoRemoteURL)
            try container.encode(reason, forKey: .reason)
        case .rateLimited(let repoRemoteURL, let reason):
            try container.encode("rate_limited", forKey: .type)
            try container.encode(repoRemoteURL, forKey: .repoRemoteURL)
            try container.encode(reason, forKey: .reason)
        case .empty(let repoRemoteURL, let ownerRepo, let gitRef):
            try container.encode("empty", forKey: .type)
            try container.encode(repoRemoteURL, forKey: .repoRemoteURL)
            try container.encode(ownerRepo, forKey: .ownerRepo)
            try container.encode(gitRef, forKey: .gitRef)
        case .loaded(let tree):
            try container.encode("loaded", forKey: .type)
            try container.encode(tree, forKey: .tree)
        }
    }
}

/// Swift mirror of `boss_protocol::DesignDocContent` — one document's
/// body, or the classified reason it could not be read.
enum DesignDocContent: Hashable {
    case loaded(markdown: String)
    case failed(reason: String)
}

extension DesignDocContent: Codable {
    enum CodingKeys: String, CodingKey {
        case type
        case markdown
        case reason
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let type = try container.decode(String.self, forKey: .type)
        switch type {
        case "loaded":
            self = .loaded(markdown: try container.decode(String.self, forKey: .markdown))
        case "failed":
            self = .failed(reason: try container.decode(String.self, forKey: .reason))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: container,
                debugDescription: "Unknown DesignDocContent type: \(type)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .loaded(let markdown):
            try container.encode("loaded", forKey: .type)
            try container.encode(markdown, forKey: .markdown)
        case .failed(let reason):
            try container.encode("failed", forKey: .type)
            try container.encode(reason, forKey: .reason)
        }
    }
}

// MARK: - Document identity

/// The `(repo, path, ref)` triple that identifies a document on GitHub.
///
/// This is the only handle the app holds on a document. There is no
/// local path and no mirrored body: opening one sends this triple back
/// to the engine, which reads the blob through to GitHub.
struct DesignDocRef: Hashable {
    var repoRemoteURL: String
    var path: String
    var gitRef: String

    /// Last path component — what the reader pane titles itself with.
    var fileName: String { (path as NSString).lastPathComponent }
}

// MARK: - Tree nesting

/// One node in the rendered document tree: either a directory (with
/// children) or a markdown file (with a [[DesignDocRef]]).
struct DesignDocNode: Identifiable, Hashable {
    /// Full repo-relative path of this node — unique within a tree, and
    /// stable across reloads, so `List` selection survives a refresh.
    let id: String
    let name: String
    let isDirectory: Bool
    /// `nil` for files. Directories always have at least one child:
    /// the builder only creates a directory when a file lands inside it.
    var children: [DesignDocNode]?
    /// `nil` for directories.
    var docRef: DesignDocRef?
}

/// Turns the engine's flat path list into the nested tree the sidebar
/// renders.
///
/// The engine deliberately sends flat paths — `docs/design-docs/foo.md`
/// rather than a pre-nested structure — so the wire shape stays simple
/// and the nesting rule lives in exactly one place. This is that place.
enum DesignDocTreeBuilder {
    /// Build the nested tree for `tree`'s entries.
    ///
    /// Directories are synthesised from path components, so a repo that
    /// has `docs/design-docs/foo.md` renders as `docs › design-docs ›
    /// foo.md` even though no directory entry was ever sent. Within each
    /// level, directories sort before files and each group sorts
    /// case-insensitively — matching how Finder and every file sidebar
    /// on the platform order things.
    static func build(from tree: DesignDocTree) -> [DesignDocNode] {
        var root = MutableNode(name: "", path: "")
        for entry in tree.entries {
            let components = entry.path.split(separator: "/").map(String.init)
            guard !components.isEmpty else { continue }
            root.insert(
                components: components,
                docRef: DesignDocRef(
                    repoRemoteURL: tree.repoRemoteURL,
                    path: entry.path,
                    gitRef: tree.gitRef
                )
            )
        }
        return root.finish()
    }

    /// Depth-first search for the node with `id`, used to resolve a
    /// `List` selection back to the node it names.
    static func find(id: String, in nodes: [DesignDocNode]) -> DesignDocNode? {
        for node in nodes {
            if node.id == id { return node }
            if let children = node.children, let hit = find(id: id, in: children) {
                return hit
            }
        }
        return nil
    }

    /// Every directory id in `nodes`. The sidebar expands all of these
    /// on first load so docs nested two or three levels deep (which is
    /// where design docs actually live) are visible without the operator
    /// having to drill in.
    static func directoryIDs(in nodes: [DesignDocNode]) -> [String] {
        var out: [String] = []
        for node in nodes where node.isDirectory {
            out.append(node.id)
            out.append(contentsOf: directoryIDs(in: node.children ?? []))
        }
        return out
    }

    /// Mutable scratch node used only during construction.
    ///
    /// Children are kept in a dictionary while building so inserting the
    /// Nth file into an existing directory stays O(1) rather than a
    /// linear scan; ordering is applied once in `finish()`.
    private struct MutableNode {
        let name: String
        let path: String
        var children: [String: MutableNode] = [:]
        var docRef: DesignDocRef?

        init(name: String, path: String) {
            self.name = name
            self.path = path
        }

        mutating func insert(components: [String], docRef: DesignDocRef) {
            guard let head = components.first else { return }
            let childPath = path.isEmpty ? head : "\(path)/\(head)"
            var child = children[head] ?? MutableNode(name: head, path: childPath)
            if components.count == 1 {
                child.docRef = docRef
            } else {
                child.insert(components: Array(components.dropFirst()), docRef: docRef)
            }
            children[head] = child
        }

        func finish() -> [DesignDocNode] {
            // A node is a file iff it carries a docRef. A path can in
            // principle be both (a file and a directory prefix); treating
            // "has children" as authoritative for directory-ness keeps
            // such a node navigable rather than hiding its contents.
            let built = children.values.map { child -> DesignDocNode in
                let grandchildren = child.finish()
                if grandchildren.isEmpty {
                    return DesignDocNode(
                        id: child.path,
                        name: child.name,
                        isDirectory: false,
                        children: nil,
                        docRef: child.docRef
                    )
                }
                return DesignDocNode(
                    id: child.path,
                    name: child.name,
                    isDirectory: true,
                    children: grandchildren,
                    docRef: nil
                )
            }
            return built.sorted { lhs, rhs in
                if lhs.isDirectory != rhs.isDirectory { return lhs.isDirectory }
                return lhs.name.localizedStandardCompare(rhs.name) == .orderedAscending
            }
        }
    }
}
