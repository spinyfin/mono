import Foundation

struct WorkProduct: Identifiable, Hashable {
    let id: String
    var name: String
    var slug: String
    var description: String
    var repoRemoteURL: String?
    var status: String
    var createdAt: String
    var updatedAt: String
    /// Discriminator for the bound external tracker (`"github"`, etc.).
    /// `nil` when no tracker is bound. Mirrors `Product.external_tracker_kind`.
    var externalTrackerKind: String? = nil
    /// Kind-specific tracker config as raw JSON string. `nil` when no
    /// tracker is bound. Mirrors `Product.external_tracker_config`.
    var externalTrackerConfig: String? = nil
    /// Optional leading prefix for worker branch names. `nil` → engine
    /// default `"boss/"`. Mirrors `Product.worker_branch_prefix`.
    var workerBranchPrefix: String? = nil
    /// Per-product editorial rules. `nil` when no rules are configured.
    /// Mirrors `Product.editorial_rules`.
    var editorialRules: EditorialRules? = nil
    /// Preferred repo for investigation/design doc PRs. `nil` → fall through
    /// to the user-level `BOSS_USER_DOCS_REPO` default.
    /// Mirrors `Product.docs_repo`.
    var docsRepo: String? = nil
}

/// Swift mirror of `boss_protocol::WorkItemExternalRef`. Stable upstream
/// pointer stored on a work item that is linked to an external tracker issue.
struct WorkItemExternalRef: Codable, Hashable {
    /// Tracker discriminator (`"github"`, etc.).
    var kind: String
    /// Stable opaque lookup key (`"spinyfin/mono#560"` for GitHub).
    var canonicalID: String
    /// Tracker-specific extras as a raw JSON string (engine-opaque).
    var raw: String
    /// Canonical browser URL for the upstream issue.
    var webURL: String
    /// Unix-seconds string of the last successful upstream→Boss reconcile.
    var syncedAt: String?
    /// Unix-seconds string when the binding was cleared. `nil` while active.
    var unboundAt: String?

    enum CodingKeys: String, CodingKey {
        case kind
        case canonicalID = "canonical_id"
        case raw
        case webURL = "web_url"
        case syncedAt = "synced_at"
        case unboundAt = "unbound_at"
    }

    init(kind: String, canonicalID: String, raw: String, webURL: String,
         syncedAt: String? = nil, unboundAt: String? = nil) {
        self.kind = kind
        self.canonicalID = canonicalID
        self.raw = raw
        self.webURL = webURL
        self.syncedAt = syncedAt
        self.unboundAt = unboundAt
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        kind = try container.decode(String.self, forKey: .kind)
        canonicalID = try container.decode(String.self, forKey: .canonicalID)
        webURL = try container.decode(String.self, forKey: .webURL)
        syncedAt = try container.decodeIfPresent(String.self, forKey: .syncedAt)
        unboundAt = try container.decodeIfPresent(String.self, forKey: .unboundAt)
        // `raw` is an arbitrary JSON value; decode into Data then re-encode
        // as a string so callers get a stable type without depending on
        // AnyCodable or similar.
        if let rawValue = try? container.decode(AnyDecodable.self, forKey: .raw) {
            let data = try JSONSerialization.data(withJSONObject: rawValue.value)
            raw = String(data: data, encoding: .utf8) ?? "{}"
        } else {
            raw = "{}"
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(kind, forKey: .kind)
        try container.encode(canonicalID, forKey: .canonicalID)
        try container.encode(webURL, forKey: .webURL)
        try container.encodeIfPresent(syncedAt, forKey: .syncedAt)
        try container.encodeIfPresent(unboundAt, forKey: .unboundAt)
        if let data = raw.data(using: .utf8),
           let obj = try? JSONSerialization.jsonObject(with: data) {
            let wrapped = AnyEncodable(obj)
            try container.encode(wrapped, forKey: .raw)
        } else {
            try container.encode([String: String](), forKey: .raw)
        }
    }
}

/// Type-erased helper for decoding arbitrary JSON values in `WorkItemExternalRef.raw`.
private struct AnyDecodable: Decodable {
    let value: Any
    init(from decoder: Decoder) throws {
        if let container = try? decoder.singleValueContainer() {
            if let v = try? container.decode(Bool.self) { value = v; return }
            if let v = try? container.decode(Int.self) { value = v; return }
            if let v = try? container.decode(Double.self) { value = v; return }
            if let v = try? container.decode(String.self) { value = v; return }
            if let v = try? container.decode([String: AnyDecodable].self) {
                value = v.mapValues { $0.value }; return
            }
            if let v = try? container.decode([AnyDecodable].self) {
                value = v.map { $0.value }; return
            }
        }
        value = NSNull()
    }
}

private struct AnyEncodable: Encodable {
    let value: Any
    init(_ value: Any) { self.value = value }
    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch value {
        case let v as Bool: try container.encode(v)
        case let v as Int: try container.encode(v)
        case let v as Double: try container.encode(v)
        case let v as String: try container.encode(v)
        case let v as [String: Any]:
            let mapped = v.mapValues { AnyEncodable($0) }
            try container.encode(mapped)
        case let v as [Any]:
            try container.encode(v.map { AnyEncodable($0) })
        default:
            try container.encodeNil()
        }
    }
}

/// Swift mirror of `boss_protocol::SetProductExternalTrackerInput`.
struct SetProductExternalTrackerInput: Codable, Hashable {
    var productID: String
    var kind: String?
    var config: String?
    var unset: Bool = false

    enum CodingKeys: String, CodingKey {
        case productID = "product_id"
        case kind
        case config
        case unset
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(productID, forKey: .productID)
        try container.encodeIfPresent(kind, forKey: .kind)
        if let config, let data = config.data(using: .utf8),
           let obj = try? JSONSerialization.jsonObject(with: data) {
            try container.encode(AnyEncodable(obj), forKey: .config)
        }
        try container.encode(unset, forKey: .unset)
    }

    init(productID: String, kind: String? = nil, config: String? = nil, unset: Bool = false) {
        self.productID = productID
        self.kind = kind
        self.config = config
        self.unset = unset
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        productID = try container.decode(String.self, forKey: .productID)
        kind = try container.decodeIfPresent(String.self, forKey: .kind)
        unset = (try? container.decodeIfPresent(Bool.self, forKey: .unset)) ?? false
        if let rawValue = try? container.decodeIfPresent(AnyDecodable.self, forKey: .config) {
            let data = try JSONSerialization.data(withJSONObject: rawValue.value)
            config = String(data: data, encoding: .utf8)
        } else {
            config = nil
        }
    }
}

/// Swift mirror of `boss_protocol::LinkExternalRefInput`.
struct LinkExternalRefInput: Codable, Hashable, Equatable {
    var workItemID: String
    var kind: String
    var canonicalID: String

    init(workItemID: String, kind: String, canonicalID: String) {
        self.workItemID = workItemID
        self.kind = kind
        self.canonicalID = canonicalID
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        workItemID = try c.decode(String.self, forKey: .workItemID)
        kind = try c.decode(String.self, forKey: .kind)
        canonicalID = try c.decode(String.self, forKey: .canonicalID)
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(workItemID, forKey: .workItemID)
        try c.encode(kind, forKey: .kind)
        try c.encode(canonicalID, forKey: .canonicalID)
    }

    private enum CodingKeys: String, CodingKey {
        case workItemID = "work_item_id"
        case kind
        case canonicalID = "canonical_id"
    }
}

struct WorkProject: Identifiable, Hashable {
    let id: String
    let productID: String
    var name: String
    var slug: String
    var description: String
    var goal: String
    var status: String
    var priority: String
    var createdAt: String
    var updatedAt: String
    /// `'human'` (default) when the most recent status change came
    /// from a CLI / app caller; `'engine'` when the engine flipped
    /// the status itself (e.g. dependency auto-block / unblock). The
    /// kanban uses this to distinguish auto-blocks (chain badge,
    /// drag refusal) from user-chosen blocks.
    var lastStatusActor: String = "human"
    /// Repo URL the project's design doc lives in. `nil` → inherit
    /// from the project's product. Mirrors
    /// `Project.design_doc_repo_remote_url`.
    var designDocRepoRemoteURL: String? = nil
    /// Branch the design doc lives on. `nil` → inherit from the
    /// product's docs branch (or `"main"`). Mirrors
    /// `Project.design_doc_branch`.
    var designDocBranch: String? = nil
    /// Repo-relative path to the design doc. `nil` → no pointer set,
    /// UI affordance hidden. Mirrors `Project.design_doc_path`.
    var designDocPath: String? = nil
    /// Per-product short id. `nil` only on rows predating the migration
    /// (the engine backfills these at startup, so `nil` is transient).
    /// Mirrors `Project.short_id` on the wire.
    var shortID: Int? = nil
}

/// Swift mirror of `boss_protocol::SetProjectDesignDocInput`.
/// Three optional override fields plus an `unset` switch the engine
/// uses to clear the pointer. `nil` paths are skipped on encode so
/// the wire form matches serde's `skip_serializing_if`.
struct SetProjectDesignDocInput: Codable, Hashable {
    var projectID: String
    var designDocRepoRemoteURL: String?
    var designDocBranch: String?
    var designDocPath: String?
    var unset: Bool = false

    enum CodingKeys: String, CodingKey {
        case projectID = "project_id"
        case designDocRepoRemoteURL = "design_doc_repo_remote_url"
        case designDocBranch = "design_doc_branch"
        case designDocPath = "design_doc_path"
        case unset
    }
}

/// Resolution kind for a project's design-doc pointer. Discriminator
/// drives the open affordance: same/other product can fast-path into
/// a leased workspace; `.external` always opens the GitHub web URL.
enum ResolvedDesignDocKind: Hashable {
    case sameProduct(productID: String)
    case otherProduct(productID: String)
    case external
}

extension ResolvedDesignDocKind: Codable {
    enum CodingKeys: String, CodingKey {
        case type
        case productID = "product_id"
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let type = try container.decode(String.self, forKey: .type)
        switch type {
        case "same_product":
            self = .sameProduct(productID: try container.decode(String.self, forKey: .productID))
        case "other_product":
            self = .otherProduct(productID: try container.decode(String.self, forKey: .productID))
        case "external":
            self = .external
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: container,
                debugDescription: "Unknown ResolvedDesignDocKind type: \(type)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .sameProduct(let productID):
            try container.encode("same_product", forKey: .type)
            try container.encode(productID, forKey: .productID)
        case .otherProduct(let productID):
            try container.encode("other_product", forKey: .type)
            try container.encode(productID, forKey: .productID)
        case .external:
            try container.encode("external", forKey: .type)
        }
    }
}

/// Swift mirror of `boss_protocol::ResolvedDesignDoc` — the
/// concrete `(repo, branch, path)` triple plus the kind discriminator
/// that decides which open path the affordance should take.
struct ResolvedDesignDoc: Codable, Hashable {
    var repoRemoteURL: String
    var branch: String
    var path: String
    var kind: ResolvedDesignDocKind

    enum CodingKeys: String, CodingKey {
        case repoRemoteURL = "repo_remote_url"
        case branch
        case path
        case kind
    }

    /// The engine-keyed comment artifact (`pr_doc:<repo_remote_url>:<branch>:<path>`)
    /// this resolved doc corresponds to. `nil` when any component is empty
    /// (defensive; the engine should never send empty values in practice).
    var commentArtifact: CommentArtifactRef? {
        guard !repoRemoteURL.isEmpty, !branch.isEmpty, !path.isEmpty else { return nil }
        return .prDoc(repoRemoteURL: repoRemoteURL, branch: branch, path: path)
    }
}

/// Swift mirror of `boss_protocol::ProjectDesignDocState`. Drives
/// the UI affordance: `.notSet` hides the icon, `.resolved` shows a
/// clickable doc icon (with a tooltip rendered from `webURL`), and
/// `.broken` shows a warning glyph that opens the re-point form.
///
/// On `.resolved`, `workspacePath` is the absolute path of a cube
/// workspace currently leased for the resolved repo (or `nil` when
/// none is leased). The open dispatcher uses it to fast-path
/// `$EDITOR` / the in-app renderer onto the workspace file system
/// when the kind is same- or other-product; absence falls back to
/// the `rawContentURL` (GitHub raw-content fetch for in-app rendering)
/// and then the GitHub web URL.
enum ProjectDesignDocState: Hashable {
    case notSet
    case resolved(resolved: ResolvedDesignDoc, workspacePath: String?, webURL: String, rawContentURL: String?)
    case broken(reason: String)
}

extension ProjectDesignDocState: Codable {
    enum CodingKeys: String, CodingKey {
        case type
        case resolved
        case workspacePath = "workspace_path"
        case webURL = "web_url"
        case rawContentURL = "raw_content_url"
        case reason
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let type = try container.decode(String.self, forKey: .type)
        switch type {
        case "not_set":
            self = .notSet
        case "resolved":
            self = .resolved(
                resolved: try container.decode(ResolvedDesignDoc.self, forKey: .resolved),
                workspacePath: try container.decodeIfPresent(String.self, forKey: .workspacePath),
                webURL: try container.decode(String.self, forKey: .webURL),
                rawContentURL: try container.decodeIfPresent(String.self, forKey: .rawContentURL)
            )
        case "broken":
            self = .broken(reason: try container.decode(String.self, forKey: .reason))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: container,
                debugDescription: "Unknown ProjectDesignDocState type: \(type)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .notSet:
            try container.encode("not_set", forKey: .type)
        case .resolved(let resolved, let workspacePath, let webURL, let rawContentURL):
            try container.encode("resolved", forKey: .type)
            try container.encode(resolved, forKey: .resolved)
            try container.encodeIfPresent(workspacePath, forKey: .workspacePath)
            try container.encode(webURL, forKey: .webURL)
            try container.encodeIfPresent(rawContentURL, forKey: .rawContentURL)
        case .broken(let reason):
            try container.encode("broken", forKey: .type)
            try container.encode(reason, forKey: .reason)
        }
    }
}

/// Swift mirror of `boss_protocol::ResolveProjectDesignDocOutput` —
/// the wire envelope returned by the `ResolveProjectDesignDoc` RPC.
struct ResolveProjectDesignDocOutput: Codable, Hashable {
    var projectID: String
    var state: ProjectDesignDocState

    enum CodingKeys: String, CodingKey {
        case projectID = "project_id"
        case state
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
