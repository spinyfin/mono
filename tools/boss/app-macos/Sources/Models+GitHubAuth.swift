import Foundation

// ===========================================================================
// GitHub OAuth device-flow auth. The per-org and top-level auth state machines
// with their tagged-union Codable conformances, plus the presentation model the
// auth affordance renders from. Split out of Models.swift to keep that file
// under the repo's file-size check.
// ===========================================================================

/// Swift mirror of `boss_protocol::OrgAuthState` — the sub-state of an
/// authorized token reflecting whether it can actually reach the org's
/// private resources (OAuth device-flow design §7). Internally tagged by
/// `type` (snake_case), matching the Rust
/// `#[serde(tag = "type", rename_all = "snake_case")]`.
enum OrgAuthState: Equatable {
    /// Token can read the org's private resources; sync should work.
    case ok
    /// The OAuth App has not yet been approved by an org owner.
    /// `requestURL` is the org-owner approval page.
    case needsOrgApproval(requestURL: String)
    /// The token requires SAML SSO authorization for the org. `ssoURL`
    /// is the SSO authorization URL from GitHub's `X-GitHub-SSO` header.
    case needsSso(ssoURL: String)
    /// Org auth state could not be determined (probe pending or failed).
    case unknown
}

extension OrgAuthState: Codable {
    enum CodingKeys: String, CodingKey {
        case type
        case requestURL = "request_url"
        case ssoURL = "sso_url"
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let type = try container.decode(String.self, forKey: .type)
        switch type {
        case "ok":
            self = .ok
        case "needs_org_approval":
            self = .needsOrgApproval(requestURL: try container.decode(String.self, forKey: .requestURL))
        case "needs_sso":
            self = .needsSso(ssoURL: try container.decode(String.self, forKey: .ssoURL))
        case "unknown":
            self = .unknown
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: container,
                debugDescription: "Unknown OrgAuthState type: \(type)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .ok:
            try container.encode("ok", forKey: .type)
        case .needsOrgApproval(let requestURL):
            try container.encode("needs_org_approval", forKey: .type)
            try container.encode(requestURL, forKey: .requestURL)
        case .needsSso(let ssoURL):
            try container.encode("needs_sso", forKey: .type)
            try container.encode(ssoURL, forKey: .ssoURL)
        case .unknown:
            try container.encode("unknown", forKey: .type)
        }
    }
}

/// Swift mirror of `boss_protocol::GitHubAuthStateDto` — the display-safe
/// GitHub OAuth auth state the engine pushes on the `github.auth` topic
/// (and replies with to the `GitHubAuth*` requests). The access token and
/// the private device code never appear here. Internally tagged by `type`
/// (snake_case), matching the Rust serde representation and the
/// `ProjectDesignDocState` precedent above.
enum GitHubAuthState: Equatable {
    /// No stored token; no flow in progress.
    case disconnected
    /// Device code is being requested from GitHub.
    case requestingCode
    /// Device code obtained. The user must type `userCode` at
    /// `verificationURI` (or `verificationURIComplete` if present); the
    /// engine is polling.
    case pendingUserAuth(
        userCode: String,
        verificationURI: String,
        verificationURIComplete: String?,
        /// Unix epoch seconds when the device code expires.
        expiresAt: Int64,
        intervalSeconds: Int
    )
    /// Token obtained, validated, and stored. `grantedScopes` is what
    /// GitHub actually granted (may differ from what was requested).
    case authorized(login: String, grantedScopes: [String], orgState: OrgAuthState)
    /// The device code expired before the user completed authorization.
    case expired
    /// The user denied the authorization request in the browser.
    case denied
    /// A non-recoverable error occurred during the flow.
    case error(message: String)
}

extension GitHubAuthState: Codable {
    enum CodingKeys: String, CodingKey {
        case type
        case userCode = "user_code"
        case verificationURI = "verification_uri"
        case verificationURIComplete = "verification_uri_complete"
        case expiresAt = "expires_at"
        case intervalSeconds = "interval_seconds"
        case login
        case grantedScopes = "granted_scopes"
        case orgState = "org_state"
        case message
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let type = try container.decode(String.self, forKey: .type)
        switch type {
        case "disconnected":
            self = .disconnected
        case "requesting_code":
            self = .requestingCode
        case "pending_user_auth":
            self = .pendingUserAuth(
                userCode: try container.decode(String.self, forKey: .userCode),
                verificationURI: try container.decode(String.self, forKey: .verificationURI),
                verificationURIComplete: try container.decodeIfPresent(String.self, forKey: .verificationURIComplete),
                expiresAt: try container.decode(Int64.self, forKey: .expiresAt),
                intervalSeconds: try container.decode(Int.self, forKey: .intervalSeconds)
            )
        case "authorized":
            self = .authorized(
                login: try container.decode(String.self, forKey: .login),
                grantedScopes: try container.decode([String].self, forKey: .grantedScopes),
                orgState: try container.decode(OrgAuthState.self, forKey: .orgState)
            )
        case "expired":
            self = .expired
        case "denied":
            self = .denied
        case "error":
            self = .error(message: try container.decode(String.self, forKey: .message))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: container,
                debugDescription: "Unknown GitHubAuthState type: \(type)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .disconnected:
            try container.encode("disconnected", forKey: .type)
        case .requestingCode:
            try container.encode("requesting_code", forKey: .type)
        case let .pendingUserAuth(userCode, verificationURI, verificationURIComplete, expiresAt, intervalSeconds):
            try container.encode("pending_user_auth", forKey: .type)
            try container.encode(userCode, forKey: .userCode)
            try container.encode(verificationURI, forKey: .verificationURI)
            try container.encodeIfPresent(verificationURIComplete, forKey: .verificationURIComplete)
            try container.encode(expiresAt, forKey: .expiresAt)
            try container.encode(intervalSeconds, forKey: .intervalSeconds)
        case let .authorized(login, grantedScopes, orgState):
            try container.encode("authorized", forKey: .type)
            try container.encode(login, forKey: .login)
            try container.encode(grantedScopes, forKey: .grantedScopes)
            try container.encode(orgState, forKey: .orgState)
        case .expired:
            try container.encode("expired", forKey: .type)
        case .denied:
            try container.encode("denied", forKey: .type)
        case let .error(message):
            try container.encode("error", forKey: .type)
            try container.encode(message, forKey: .message)
        }
    }
}

/// Pure presentation model for the "GitHub account" subsection of the
/// external-tracker settings (OAuth device-flow design §6/§7/§8). Maps a
/// `GitHubAuthState` to the status line, icon, action buttons, pending
/// user-code prompt, and banner(s) the view renders — so the rendering
/// logic is unit-testable without a SwiftUI host, mirroring
/// `ExternalTrackerAttentionPresentation`.
struct GitHubAuthPresentation: Equatable {
    /// Action buttons offered in the section's main button row.
    enum Action: String, Equatable {
        case connect
        case cancel
        case disconnect
        case reauthorize
    }

    /// The user-code + verification-URL prompt shown while the engine polls.
    struct PendingPrompt: Equatable {
        let userCode: String
        /// Human-facing verification URL to display (the bare
        /// `verification_uri`).
        let verificationURL: String
        /// URL to open in the browser — `verification_uri_complete` when
        /// present (pre-fills the code), otherwise `verification_uri`.
        let openURL: String
    }

    /// An actionable banner surfacing org-approval / SSO / limited-scope /
    /// failure states (design §7, §8).
    struct Banner: Equatable {
        enum Kind: Equatable {
            case needsOrgApproval
            case needsSso
            case unknownOrg
            case limitedScopes
            case expired
            case denied
            case error
        }

        let kind: Kind
        let message: String
        /// When set, a button opens this URL in the browser.
        let actionURL: String?
        let actionLabel: String?
        /// When true the banner offers a "Re-check" button that re-runs the
        /// org/SSO probe via a `GitHubAuthStatus` request (design §7).
        let offersRecheck: Bool
    }

    /// Primary status line, e.g.
    /// "Connected as @octocat · scopes: repo, project".
    let statusLine: String
    /// SF Symbol for the status row.
    let statusIcon: String
    /// True while the engine is actively working (show a spinner).
    let isBusy: Bool
    /// True when the connect button restarts a finished/failed flow
    /// ("Start over") rather than starting fresh ("Connect").
    let connectIsRestart: Bool
    /// Buttons to offer in the main row, in display order.
    let actions: [Action]
    /// The pending user-code prompt, present only while polling.
    let pendingPrompt: PendingPrompt?
    /// Zero or more banners to render below the status line.
    let banners: [Banner]

    /// Scopes Boss requests via the device flow (design background / T753):
    /// `repo project`.
    static let requestedScopes = ["repo", "project"]

    static func forState(_ state: GitHubAuthState) -> GitHubAuthPresentation {
        switch state {
        case .disconnected:
            return GitHubAuthPresentation(
                statusLine: "Not connected. Sync uses your local gh login.",
                statusIcon: "person.crop.circle.badge.xmark",
                isBusy: false,
                connectIsRestart: false,
                actions: [.connect],
                pendingPrompt: nil,
                banners: []
            )
        case .requestingCode:
            return GitHubAuthPresentation(
                statusLine: "Requesting a device code from GitHub…",
                statusIcon: "hourglass",
                isBusy: true,
                connectIsRestart: false,
                actions: [.cancel],
                pendingPrompt: nil,
                banners: []
            )
        case let .pendingUserAuth(userCode, verificationURI, verificationURIComplete, _, _):
            return GitHubAuthPresentation(
                statusLine: "Waiting for you to authorize in the browser…",
                statusIcon: "hourglass",
                isBusy: true,
                connectIsRestart: false,
                actions: [.cancel],
                pendingPrompt: PendingPrompt(
                    userCode: userCode,
                    verificationURL: verificationURI,
                    openURL: verificationURIComplete ?? verificationURI
                ),
                banners: []
            )
        case let .authorized(login, grantedScopes, orgState):
            var banners: [Banner] = []
            switch orgState {
            case .ok:
                break
            case let .needsOrgApproval(requestURL):
                banners.append(Banner(
                    kind: .needsOrgApproval,
                    message: "Connected as @\(login), but the Boss app is not yet approved for this organization. An org owner must approve it before sync can read private issues.",
                    actionURL: requestURL,
                    actionLabel: "Open org settings",
                    offersRecheck: true
                ))
            case let .needsSso(ssoURL):
                banners.append(Banner(
                    kind: .needsSso,
                    message: "Your token needs SAML SSO authorization for this organization before sync can read private issues.",
                    actionURL: ssoURL,
                    actionLabel: "Authorize via SSO",
                    offersRecheck: true
                ))
            case .unknown:
                banners.append(Banner(
                    kind: .unknownOrg,
                    message: "Organization access has not been verified yet.",
                    actionURL: nil,
                    actionLabel: nil,
                    offersRecheck: true
                ))
            }
            if !hasRequestedScopes(grantedScopes) {
                banners.append(Banner(
                    kind: .limitedScopes,
                    message: "GitHub granted limited scopes (\(scopesText(grantedScopes))). Some issue-sync operations may fail; re-authorize to grant repo and project.",
                    actionURL: nil,
                    actionLabel: nil,
                    offersRecheck: false
                ))
            }
            return GitHubAuthPresentation(
                statusLine: "Connected as @\(login) · scopes: \(scopesText(grantedScopes))",
                statusIcon: "checkmark.seal.fill",
                isBusy: false,
                connectIsRestart: false,
                actions: [.reauthorize, .disconnect],
                pendingPrompt: nil,
                banners: banners
            )
        case .expired:
            return GitHubAuthPresentation(
                statusLine: "The device code expired before you finished.",
                statusIcon: "exclamationmark.triangle",
                isBusy: false,
                connectIsRestart: true,
                actions: [.connect],
                pendingPrompt: nil,
                banners: [Banner(
                    kind: .expired,
                    message: "Authorization timed out. Start over to request a new code.",
                    actionURL: nil,
                    actionLabel: nil,
                    offersRecheck: false
                )]
            )
        case .denied:
            return GitHubAuthPresentation(
                statusLine: "Authorization was denied.",
                statusIcon: "hand.raised",
                isBusy: false,
                connectIsRestart: true,
                actions: [.connect],
                pendingPrompt: nil,
                banners: [Banner(
                    kind: .denied,
                    message: "You declined the authorization request in the browser.",
                    actionURL: nil,
                    actionLabel: nil,
                    offersRecheck: false
                )]
            )
        case let .error(message):
            return GitHubAuthPresentation(
                statusLine: "Authorization failed.",
                statusIcon: "exclamationmark.triangle.fill",
                isBusy: false,
                connectIsRestart: true,
                actions: [.connect],
                pendingPrompt: nil,
                banners: [Banner(
                    kind: .error,
                    message: message,
                    actionURL: nil,
                    actionLabel: nil,
                    offersRecheck: false
                )]
            )
        }
    }

    /// Render the granted-scope list for the status line ("repo, project"
    /// or "none").
    static func scopesText(_ scopes: [String]) -> String {
        scopes.isEmpty ? "none" : scopes.joined(separator: ", ")
    }

    /// True when every requested scope (`repo`, `project`) was granted.
    static func hasRequestedScopes(_ granted: [String]) -> Bool {
        requestedScopes.allSatisfy { granted.contains($0) }
    }
}
