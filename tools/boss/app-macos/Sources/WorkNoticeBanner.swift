import SwiftUI

/// Passive, dismissable chrome banner for `ChatViewModel.workErrorMessage`:
/// an engine `work_error` reply, a failed reveal, a broken doc pointer, or
/// any other failure report the app has no more specific surface for.
///
/// This replaced the former "Work Error" modal alert. A modal steals
/// keyboard focus from whatever the user is typing, blocks every other
/// control until acknowledged, and re-pops for each repeat of a message
/// the user can do nothing about (e.g. a busy/locked engine). The banner
/// sits in the titlebar accessory slot alongside the connection and
/// health banners (`ContentView.chromeBanners`): it never becomes key,
/// never takes first responder, and a newer message simply replaces the
/// text in place. Dismiss (or any action that clears
/// `workErrorMessage`) removes it.
///
/// See `tools/boss/app-macos/AGENTS.md` ("No modals for non-actionable
/// status or errors") for the rule this view exists to satisfy.
struct WorkNoticeBanner: View {
    let message: String
    let onDismiss: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
                .padding(.top, 2)
            Text(message)
                .font(.callout)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
            Button(action: onDismiss) {
                Image(systemName: "xmark")
                    .font(.callout.weight(.semibold))
            }
            .buttonStyle(.borderless)
            .controlSize(.small)
            .help("Dismiss this notice")
            .accessibilityLabel("Dismiss notice")
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.orange.opacity(0.12))
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("work-notice-banner")
    }
}
