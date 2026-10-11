import SwiftUI
import UpdateCore

/// Chrome banner shown after the relaunch watchdog rolled a self-update back:
/// "Update to X failed: <reason>; rolled back to Y". Persistent (not a toast) until
/// the user dismisses it or retries, so a rollback is never silent.
///
/// "Retry Update" is the deliberate path that clears the failed version's blocklist
/// entry and stages it again (``UpdateModel/retryBlockedUpdate(_:)``); background
/// polling never re-downloads a blocklisted version on its own.
struct UpdateRollbackBanner: View {
    let record: RollbackRecord
    let onRetry: () -> Void
    let onDismiss: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "arrow.uturn.backward.circle.fill")
                .foregroundStyle(.white)
                .padding(.top, 2)
            VStack(alignment: .leading, spacing: 2) {
                Text(record.noticeText)
                    .font(.callout.weight(.semibold))
                    .foregroundStyle(.white)
                    .fixedSize(horizontal: false, vertical: true)
                if !record.detail.isEmpty {
                    Text(record.detail)
                        .font(.caption)
                        .foregroundStyle(.white.opacity(0.85))
                        .fixedSize(horizontal: false, vertical: true)
                }
                Text("Boss \(record.version) is blocked from installing automatically. Retry to download and install it again; details are in the update log.")
                    .font(.caption)
                    .foregroundStyle(.white.opacity(0.85))
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
            Button("Retry Update", action: onRetry)
                .buttonStyle(.bordered)
                .tint(.white)
            Button("Dismiss", action: onDismiss)
                .buttonStyle(.bordered)
                .tint(.white)
        }
        .padding(8)
        .background(Color.orange.opacity(0.85))
        .accessibilityIdentifier("update-rollback-banner")
    }
}
