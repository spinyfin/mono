import SwiftUI

/// "Trunk" Settings pane — lets the operator provision the org-level Trunk
/// API token from the app instead of the CLI (`boss engine trunk
/// set-token`). See the Trunk merge-queue integration design's "Auth: the
/// Trunk org API token" section
/// (`tools/boss/docs/designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`).
///
/// Thin RPC client over the engine's existing `TrunkSetToken`/`TrunkStatus`
/// handlers (`tools/boss/engine/core/src/app/trunk_auth.rs`) — the token is
/// persisted to the OS Keychain engine-side; this pane never stores or logs
/// it itself. Mirrors `EngineConfigPane`'s ANTHROPIC_API_KEY control.
struct TrunkSettingsPane: View {
    @EnvironmentObject private var chatModel: ChatViewModel

    /// SecureField draft — never persisted, cleared on save.
    @State private var tokenDraft: String = ""

    var body: some View {
        Form {
            Section {
                HStack(spacing: 6) {
                    Image(systemName: statusIcon)
                        .foregroundStyle(statusColor)
                    Text("Trunk API Token")
                        .font(.body.weight(.medium))
                    Spacer()
                    Text(statusLabel)
                        .foregroundStyle(.secondary)
                }

                Text(
                    "Required for products set to \"Trunk merge queue\" in their product " +
                    "settings. Org-level — one token covers every Trunk-queue product. " +
                    "Provisioned here or via `boss engine trunk set-token`."
                )
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

                if let validityLabel {
                    HStack(spacing: 6) {
                        Image(systemName: validityIcon)
                            .foregroundStyle(validityColor)
                        Text(validityLabel)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }

                VStack(alignment: .leading, spacing: 8) {
                    SecureField("Trunk org API token", text: $tokenDraft)
                        .textFieldStyle(.roundedBorder)
                    HStack(spacing: 8) {
                        Button("Save") {
                            saveToken()
                        }
                        .disabled(tokenDraft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                        Button("Refresh Status") {
                            chatModel.refreshTrunkStatus()
                        }
                    }
                }
                .padding(.top, 4)
            } header: {
                Text("Trunk Merge Queue")
            }
        }
        .formStyle(.grouped)
        .padding()
        .onAppear {
            chatModel.refreshTrunkStatus()
        }
    }

    private var statusIcon: String {
        switch chatModel.trunkTokenConfigured {
        case true: return "checkmark.circle.fill"
        case false: return "exclamationmark.triangle.fill"
        case nil: return "questionmark.circle"
        }
    }

    private var statusColor: Color {
        switch chatModel.trunkTokenConfigured {
        case true: return .green
        case false: return .orange
        case nil: return .secondary
        }
    }

    private var statusLabel: String {
        switch chatModel.trunkTokenConfigured {
        case true:
            if let source = chatModel.trunkTokenSource {
                return "Configured (\(source))"
            }
            return "Configured"
        case false:
            return "Not configured"
        case nil:
            return "Checking…"
        }
    }

    /// Live `getQueue` smoke-check outcome, shown under the presence/source
    /// status when available. `nil` (label omitted) when the token isn't
    /// configured yet, since presence is already covered by `statusLabel`.
    private var validityLabel: String? {
        guard chatModel.trunkTokenConfigured == true else { return nil }
        if let queueCheck = chatModel.trunkTokenQueueCheck {
            return queueCheck.ok ? "Queue check: \(queueCheck.detail)" : "Queue check failed: \(queueCheck.detail)"
        }
        if let note = chatModel.trunkTokenNote {
            return note
        }
        return nil
    }

    private var validityIcon: String {
        if let queueCheck = chatModel.trunkTokenQueueCheck {
            return queueCheck.ok ? "checkmark.circle.fill" : "xmark.circle.fill"
        }
        return "info.circle"
    }

    private var validityColor: Color {
        if let queueCheck = chatModel.trunkTokenQueueCheck {
            return queueCheck.ok ? .green : .red
        }
        return .secondary
    }

    private func saveToken() {
        let trimmed = tokenDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        chatModel.setTrunkToken(trimmed)
        tokenDraft = ""
    }
}
