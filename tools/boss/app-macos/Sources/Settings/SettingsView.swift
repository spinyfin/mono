import SwiftUI
import UpdateCore

/// macOS Settings window for Boss (opened via Boss → Settings… or ⌘,).
///
/// Reads current values from the engine at appear time and writes back
/// through `SetSetting` RPCs so settings live in engine state rather
/// than `UserDefaults`. Different machines each carry their own
/// `state.db` and therefore their own independent settings.
struct SettingsView: View {
    @EnvironmentObject private var chatModel: ChatViewModel
    @EnvironmentObject private var updateModel: UpdateModel

    var body: some View {
        TabView {
            WorkerSettingsPane()
                .tabItem {
                    Label("Workers", systemImage: "person.2")
                }
            EngineConfigPane()
                .tabItem {
                    Label("Engine", systemImage: "gearshape")
                }
            HostRegistryPane()
                .tabItem {
                    Label("Hosts", systemImage: "server.rack")
                }
            FeatureFlagsViewer()
                .tabItem {
                    Label("Feature Flags", systemImage: "flag")
                }
            TrunkSettingsPane()
                .tabItem {
                    Label("Trunk", systemImage: "arrow.triangle.merge")
                }
            UpdateSettingsView(model: updateModel)
                .tabItem {
                    Label("Updates", systemImage: "arrow.down.circle")
                }
        }
        .environmentObject(chatModel)
        .onAppear {
            chatModel.refreshSettings()
            // Engine health is fetched on every reconnect, but the
            // user may open Settings against a long-lived session
            // where the API-key state changed (a restart with a new
            // env var). Re-poll on appear so the pane shows the
            // current truth, not a snapshot from minutes ago.
            chatModel.refreshEngineHealth()
            chatModel.refreshDriverTrafficSplit()
            // Cheap: the engine serves its cache unless the TTL has expired,
            // so this is not three provider calls per Settings open. It is
            // fire-and-forget either way — the window never waits on it.
            chatModel.refreshDriverQuota()
        }
        .frame(minWidth: 560, minHeight: 400)
    }
}

/// "Engine" pane — engine-side configuration health.
/// Renders the same issues the chrome banner shows, plus the raw
/// `ANTHROPIC_API_KEY` presence bit so the user can confirm at a
/// glance the engine sees the env var. Also surfaces the
/// Keychain-backed override added in #735 so launching from
/// Finder/Spotlight no longer requires a launchd plist or shell-
/// inherited env.
private struct EngineConfigPane: View {
    @EnvironmentObject private var chatModel: ChatViewModel

    /// SecureField draft — never persisted, never inspected by other
    /// state. Cleared on save so a typed-then-cancelled value doesn't
    /// linger in memory longer than the pane is open.
    @State private var apiKeyDraft: String = ""

    /// Mirror of `APIKeyStore.readAnthropicApiKey() != nil` so the UI
    /// can render "stored" / "not stored" without re-querying the
    /// Keychain on every redraw. Refreshed on appear and after every
    /// save / clear.
    @State private var hasStoredApiKey: Bool = APIKeyStore.readAnthropicApiKey() != nil

    /// User-visible error message from the last save / clear attempt.
    /// `nil` means the last action succeeded (or none has happened).
    @State private var apiKeyError: String?

    /// Transient status line shown after a successful save / clear.
    @State private var apiKeyStatus: String?

    /// Destination for the Settings HelpLinks: the reference detail that the
    /// one-sentence captions leave out. Points at the doc on main rather than a
    /// bundled copy, so there is one source of truth for it.
    private var settingsHelpURL: URL {
        URL(string: "https://github.com/spinyfin/mono/blob/main/tools/boss/docs/driver-traffic-and-api-key-settings.md")!
    }

    var body: some View {
        Form {
            Section {
                HStack(spacing: 6) {
                    Image(systemName: chatModel.engineAnthropicApiKeyPresent
                          ? "checkmark.circle.fill"
                          : "exclamationmark.triangle.fill")
                        .foregroundStyle(chatModel.engineAnthropicApiKeyPresent ? .green : .orange)
                    Text("ANTHROPIC_API_KEY")
                        .font(.body.weight(.medium))
                    Spacer()
                    Text(engineKeyStatusLabel)
                        .foregroundStyle(.secondary)
                }
                if !chatModel.engineAnthropicApiKeyPresent {
                    Text("Live summaries need a key — paste one below, or export ANTHROPIC_API_KEY and relaunch Boss.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }

                VStack(alignment: .leading, spacing: 8) {
                    SecureField("sk-ant-…", text: $apiKeyDraft)
                        .textFieldStyle(.roundedBorder)
                        .disabled(chatModel.isRestartingEngine)
                    HStack(spacing: 8) {
                        Button(hasStoredApiKey ? "Save & restart engine" : "Save") {
                            saveApiKey()
                        }
                        .disabled(apiKeyDraft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                                  || chatModel.isRestartingEngine)
                        if hasStoredApiKey {
                            Button("Clear stored key") {
                                clearApiKey()
                            }
                            .disabled(chatModel.isRestartingEngine)
                        }
                        if chatModel.isRestartingEngine {
                            ProgressView()
                                .controlSize(.small)
                            Text("Restarting engine…")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    }
                    Text("Saving stores the key and restarts the engine, overriding any ANTHROPIC_API_KEY already in its environment.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    if let apiKeyError {
                        Text(apiKeyError)
                            .font(.caption)
                            .foregroundStyle(.red)
                            .fixedSize(horizontal: false, vertical: true)
                    } else if let apiKeyStatus {
                        Text(apiKeyStatus)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .padding(.top, 4)
            } header: {
                Text("Required Configuration")
            }

            Section {
                DriverTrafficSplitRow()
            } header: {
                Text("Driver Traffic")
            } footer: {
                HStack(alignment: .top) {
                    Text("Sets the percentage of new work sent to each driver; PR reviews and automation always use their own pinned driver, not this split.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Spacer()
                    HelpLink(destination: settingsHelpURL)
                }
            }

            DriverQuotaSection()

            if !chatModel.engineHealthIssues.isEmpty {
                Section {
                    ForEach(chatModel.engineHealthIssues) { issue in
                        VStack(alignment: .leading, spacing: 4) {
                            HStack(spacing: 6) {
                                Image(systemName: issue.severity == "error"
                                      ? "exclamationmark.octagon.fill"
                                      : "exclamationmark.triangle.fill")
                                    .foregroundStyle(issue.severity == "error" ? .red : .orange)
                                Text(issue.title)
                                    .font(.body.weight(.medium))
                            }
                            Text(issue.body)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        .padding(.vertical, 2)
                    }
                } header: {
                    Text("Health Issues")
                }
            }
        }
        .formStyle(.grouped)
        .padding()
        .onAppear {
            // Refresh the Keychain-backed state on every appear so the
            // "Stored" indicator reflects edits that happened outside
            // this pane (another session, manual Keychain Access edit).
            hasStoredApiKey = APIKeyStore.readAnthropicApiKey() != nil
        }
    }

    /// Combined label for the presence row. Distinguishes "the engine
    /// currently sees a key" (the runtime truth) from "we have a key
    /// stored that will be applied at the next engine launch" (the
    /// Settings truth). The two diverge for a brief window after Save
    /// while the engine is restarting.
    private var engineKeyStatusLabel: String {
        if chatModel.engineAnthropicApiKeyPresent {
            return hasStoredApiKey ? "Detected (from Settings)" : "Detected"
        }
        return hasStoredApiKey ? "Stored — restart engine to apply" : "Not set"
    }

    private func saveApiKey() {
        let trimmed = apiKeyDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            apiKeyError = "API key cannot be empty."
            apiKeyStatus = nil
            return
        }
        do {
            try APIKeyStore.saveAnthropicApiKey(trimmed)
            apiKeyDraft = ""
            hasStoredApiKey = true
            apiKeyError = nil
            apiKeyStatus = "Saved. Restarting engine to apply…"
            // Bounce the engine so the freshly-stored key is injected
            // into its env on the next spawn (see
            // EngineProcessController.launchDetached).
            chatModel.restartEngine()
        } catch {
            apiKeyError = (error as? LocalizedError)?.errorDescription
                ?? error.localizedDescription
            apiKeyStatus = nil
        }
    }

    private func clearApiKey() {
        do {
            try APIKeyStore.clearAnthropicApiKey()
            hasStoredApiKey = false
            apiKeyError = nil
            apiKeyStatus = "Cleared. Restarting engine so summarization falls back to env / disabled."
            chatModel.restartEngine()
        } catch {
            apiKeyError = (error as? LocalizedError)?.errorDescription
                ?? error.localizedDescription
            apiKeyStatus = nil
        }
    }
}

/// The driver traffic split control in the "Engine" Settings tab: a
/// proportional bar plus one stepper per driver.
///
/// Three interdependent values that must total 100 do not decompose into
/// three independent steppers — the operator would spend their time fighting
/// a sum constraint. They are not modelled as independent here either: each
/// stepper moves ONE share and `DriverTrafficSplit.adjusting(_:to:)` makes
/// the other two absorb the difference (Claude gives way first — it is the
/// engine's default driver, so it holds the traffic nobody has deliberately
/// claimed). The sum invariant therefore holds at every intermediate step,
/// which means:
///
/// - there is no "apply" button and no rejected intermediate state;
/// - every valid split is reachable by repeated single-share edits,
///   including any one or two drivers at 0 — raising a driver to 100 drains
///   the other two to zero in donor order, and raising one to 20 then
///   another to 80 zeroes the third;
/// - the engine's hard "shares must sum to exactly 100" rejection is a
///   backstop against a hand-written RPC, not something this UI can trip.
///
/// Reads/writes `chatModel.driverTrafficSplit`, which mirrors the engine's
/// persisted `state.db` value (fetched on Settings appear via
/// `refreshDriverTrafficSplit`).
private struct DriverTrafficSplitRow: View {
    @EnvironmentObject private var chatModel: ChatViewModel

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            DriverTrafficSplitBar(split: chatModel.driverTrafficSplit)
            ForEach(DriverSlug.allCases, id: \.self) { driver in
                Stepper(
                    value: Binding(
                        get: { chatModel.driverTrafficSplit.share(for: driver) },
                        set: { chatModel.setDriverTrafficShare(driver, to: $0) }
                    ),
                    in: 0...100,
                    step: 5
                ) {
                    HStack(spacing: 8) {
                        Circle()
                            .fill(driver.tint)
                            .frame(width: 8, height: 8)
                        Text(driver.displayName)
                            .frame(minWidth: 64, alignment: .leading)
                        Text("\(chatModel.driverTrafficSplit.share(for: driver))%")
                            .font(.body.monospacedDigit())
                            .frame(minWidth: 48, alignment: .leading)
                    }
                }
                .accessibilityLabel("\(driver.displayName) traffic share")
            }
        }
        .padding(.vertical, 2)
    }
}

/// The split rendered as one 100%-wide bar, segments in the same
/// `codex | claude | grok` order the engine lays its bucket line out in, so
/// the picture matches the mechanism. A driver at 0 has a zero-width segment
/// — visibly absent, which is the point.
private struct DriverTrafficSplitBar: View {
    let split: DriverTrafficSplit

    var body: some View {
        GeometryReader { geo in
            HStack(spacing: 0) {
                ForEach(DriverSlug.allCases, id: \.self) { driver in
                    Rectangle()
                        .fill(driver.tint)
                        .frame(width: geo.size.width * CGFloat(split.share(for: driver)) / 100)
                }
            }
        }
        .frame(height: 8)
        .clipShape(RoundedRectangle(cornerRadius: 4))
        .accessibilityElement()
        .accessibilityLabel("Driver traffic split")
        .accessibilityValue(
            DriverSlug.allCases
                .map { "\($0.displayName) \(split.share(for: $0)) percent" }
                .joined(separator: ", ")
        )
    }
}

extension DriverSlug {
    /// Segment/legend colour. Distinct hues only — nothing here encodes
    /// preference or health, and the percentages are always spelled out
    /// alongside, so colour is never the sole carrier of the value.
    var tint: Color {
        switch self {
        case .codex: return .purple
        case .claude: return .accentColor
        case .grok: return .orange
        }
    }
}

/// "Workers" pane — worker defaults grouped by concern.
private struct WorkerSettingsPane: View {
    @EnvironmentObject private var chatModel: ChatViewModel
    @State private var showCoordinatorResetConfirm = false

    private var prSettings: [EngineSetting] {
        chatModel.engineSettings.filter { $0.key == "default_pr_draft_mode" }
    }

    private var permissionModeSetting: EngineSetting? {
        chatModel.engineSettings.first { $0.key == "workers.non_opus_permission_mode" }
    }

    private var coordinatorSettings: [EngineSetting] {
        chatModel.engineSettings.filter { $0.key == "coordinator.direct_developer_mode" }
    }

    private var tmuxHostingSetting: EngineSetting? {
        chatModel.engineSettings.first { $0.key == "workers.tmux_hosting" }
    }

    var body: some View {
        Form {
            if chatModel.engineSettings.isEmpty {
                Section {
                    ProgressView("Loading…")
                        .frame(maxWidth: .infinity, alignment: .center)
                        .padding()
                }
            } else {
                Section {
                    ForEach(prSettings) { setting in
                        SettingToggleRow(setting: setting) { enabled in
                            chatModel.setEngineSetting(key: setting.key, enabled: enabled)
                        }
                    }
                } header: {
                    Text("PR Conventions")
                }
                if let setting = permissionModeSetting {
                    Section {
                        PermissionModePickerRow(setting: setting) { enabled in
                            chatModel.setEngineSetting(key: setting.key, enabled: enabled)
                        }
                    } header: {
                        Text("Workers")
                    }
                }
                if let setting = tmuxHostingSetting {
                    Section {
                        SettingToggleRow(setting: setting) { enabled in
                            chatModel.setEngineSetting(key: setting.key, enabled: enabled)
                        }
                    } header: {
                        Text("Session Hosting")
                    } footer: {
                        Text(
                            "Applies to worker panes only (review, automation, interactive) — the " +
                            "coordinator's own session is always tmux-hosted regardless of this setting."
                        )
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    }
                }
                Section {
                    ForEach(coordinatorSettings) { setting in
                        SettingToggleRow(setting: setting) { enabled in
                            chatModel.setEngineSetting(key: setting.key, enabled: enabled)
                        }
                    }
                    Button(role: .destructive) {
                        showCoordinatorResetConfirm = true
                    } label: {
                        Text("Reset Coordinator Session…")
                    }
                    .disabled(chatModel.attachedCoordinatorSpawnToken == nil)
                } header: {
                    Text("Coordinator")
                } footer: {
                    Text(
                        "Ends the coordinator's current session and starts a fresh one with the " +
                        "currently installed claude binary and instructions."
                    )
                    .font(.caption)
                    .foregroundStyle(.secondary)
                }
            }
        }
        .formStyle(.grouped)
        .padding()
        .confirmationDialog(
            CoordinatorResetCopy.title,
            isPresented: $showCoordinatorResetConfirm,
            titleVisibility: .visible
        ) {
            Button("Reset", role: .destructive) {
                chatModel.resetCoordinator()
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(CoordinatorResetCopy.message)
        }
    }
}

/// Confirmation copy shared by every entry point into the coordinator reset
/// flow (Settings ▸ Workers, and the update-available banner) so the two
/// dialogs can never drift apart in wording.
enum CoordinatorResetCopy {
    static let title = "Reset the coordinator?"
    static let message =
        "This permanently ends the coordinator's current conversation and discards its context — " +
        "this cannot be undone. A fresh session starts immediately with the current claude binary " +
        "and instructions."
}

private struct SettingToggleRow: View {
    let setting: EngineSetting
    let onToggle: (Bool) -> Void

    var body: some View {
        Toggle(isOn: Binding(
            get: { setting.enabled },
            set: { onToggle($0) }
        )) {
            VStack(alignment: .leading, spacing: 3) {
                Text(labelText(for: setting.key))
                    .font(.body)
                Text(setting.description)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .toggleStyle(.switch)
        .padding(.vertical, 2)
    }

    private func labelText(for key: String) -> String {
        switch key {
        case "default_pr_draft_mode":
            return "Default new PRs to draft mode"
        case "coordinator.direct_developer_mode":
            return "Direct Boss developer mode"
        case "workers.tmux_hosting":
            return "Host workers in tmux"
        default:
            return key
        }
    }
}

/// Segmented picker for the two-value `workers.non_opus_permission_mode` setting.
/// `false` (default) = --dangerously-skip-permissions (personal laptop).
/// `true` = --permission-mode auto (corp laptop).
private struct PermissionModePickerRow: View {
    let setting: EngineSetting
    let onChange: (Bool) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Permission mode for Sonnet/Haiku workers")
                .font(.body)
            Text(setting.description)
                .font(.caption)
                .foregroundStyle(.secondary)
            Picker("", selection: Binding(
                get: { setting.enabled },
                set: { onChange($0) }
            )) {
                Text("Skip permissions (default)").tag(false)
                Text("Auto mode").tag(true)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
        }
        .padding(.vertical, 2)
    }
}
