import Foundation

// MARK: - Editorial controls (editorial-controls-for-agent-authored-prs-and-github-comments.md)

/// Swift mirror of `boss_protocol::RedactionKind`.
enum EditorialRedactionKind: String, Codable, Hashable {
    /// Replace the match with the rule's `replacement` string.
    case rewrite
    /// Reject the `gh` invocation outright.
    case block
}

/// Swift mirror of `boss_protocol::RedactionRule`. One user-configured
/// pattern that the editorial hook applies to `gh pr|issue` bodies.
struct EditorialRedactionRule: Codable, Hashable {
    var pattern: String
    var replacement: String
    var kind: EditorialRedactionKind

    enum CodingKeys: String, CodingKey {
        case pattern, replacement, kind
    }

    init(pattern: String, replacement: String, kind: EditorialRedactionKind = .rewrite) {
        self.pattern = pattern
        self.replacement = replacement
        self.kind = kind
    }
}

/// Swift mirror of `boss_protocol::TemplatePolicy`.
enum EditorialTemplatePolicy: String, Codable, Hashable {
    /// No enforcement — worker writes whatever it likes.
    case off
    /// Inject the template as guidance but don't block non-conforming bodies.
    case advise
    /// Block PR bodies that don't contain the mandatory template sections.
    case enforce
}

/// Swift mirror of `boss_protocol::BranchNaming`.
enum EditorialBranchNaming: Hashable {
    /// Engine default: `boss/exec_<id>`.
    case bossExecPrefix
    /// Opaque hash prefix — no hint of Boss origin.
    case opaqueHash
    /// Caller-supplied literal prefix, e.g. `"bduff/"`.
    case customPrefix(prefix: String)
}

extension EditorialBranchNaming: Codable {
    private enum CodingKeys: String, CodingKey {
        case type, prefix
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let typeValue = try container.decode(String.self, forKey: .type)
        switch typeValue {
        case "boss_exec_prefix":
            self = .bossExecPrefix
        case "opaque_hash":
            self = .opaqueHash
        case "custom_prefix":
            self = .customPrefix(prefix: try container.decode(String.self, forKey: .prefix))
        default:
            self = .bossExecPrefix
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .bossExecPrefix:
            try container.encode("boss_exec_prefix", forKey: .type)
        case .opaqueHash:
            try container.encode("opaque_hash", forKey: .type)
        case .customPrefix(let prefix):
            try container.encode("custom_prefix", forKey: .type)
            try container.encode(prefix, forKey: .prefix)
        }
    }
}

/// Swift mirror of `boss_protocol::TrailerPolicy`.
enum EditorialTrailerPolicy: String, Codable, Hashable {
    /// Engine default: worker follows its CLAUDE.md (appends AI co-author trailer).
    case `default`
    /// Strip any AI co-author trailer from commit messages.
    case noAiTrailer = "no_ai_trailer"
}

/// Swift mirror of `boss_protocol::EditorialRules`. Per-product rules
/// constraining what workers write into GitHub-visible surfaces.
struct EditorialRules: Codable, Hashable {
    var instructions: String?
    var redactions: [EditorialRedactionRule]
    var templatePolicy: EditorialTemplatePolicy
    var branchNaming: EditorialBranchNaming
    var commitTrailerPolicy: EditorialTrailerPolicy

    enum CodingKeys: String, CodingKey {
        case instructions
        case redactions
        case templatePolicy = "template_policy"
        case branchNaming = "branch_naming"
        case commitTrailerPolicy = "commit_trailer_policy"
    }

    init(
        instructions: String? = nil,
        redactions: [EditorialRedactionRule] = [],
        templatePolicy: EditorialTemplatePolicy = .off,
        branchNaming: EditorialBranchNaming = .bossExecPrefix,
        commitTrailerPolicy: EditorialTrailerPolicy = .default
    ) {
        self.instructions = instructions
        self.redactions = redactions
        self.templatePolicy = templatePolicy
        self.branchNaming = branchNaming
        self.commitTrailerPolicy = commitTrailerPolicy
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        instructions = try container.decodeIfPresent(String.self, forKey: .instructions)
        redactions = (try? container.decodeIfPresent([EditorialRedactionRule].self, forKey: .redactions)) ?? []
        templatePolicy = (try? container.decodeIfPresent(EditorialTemplatePolicy.self, forKey: .templatePolicy)) ?? .off
        branchNaming = (try? container.decodeIfPresent(EditorialBranchNaming.self, forKey: .branchNaming)) ?? .bossExecPrefix
        commitTrailerPolicy = (try? container.decodeIfPresent(EditorialTrailerPolicy.self, forKey: .commitTrailerPolicy)) ?? .default
    }
}

/// Swift mirror of `boss_protocol::EditorialAction`. One recorded
/// enforcement action taken by the editorial-rules hook.
struct EditorialAction: Identifiable, Codable, Hashable {
    var id: String
    var productID: String
    var executionID: String
    var prURL: String?
    var toolCommand: String
    var action: String
    var reason: String
    var createdAt: String

    enum CodingKeys: String, CodingKey {
        case id
        case productID = "product_id"
        case executionID = "execution_id"
        case prURL = "pr_url"
        case toolCommand = "tool_command"
        case action, reason
        case createdAt = "created_at"
    }
}

/// State for an in-flight or completed `evaluate_editorial_rules` RPC,
/// held on `ChatViewModel.editorialEvaluationState`.
enum EditorialEvaluationState {
    case idle
    case loading
    case result(decision: String, findings: [String], rewrittenBody: String?)
    case failed(String)
}
