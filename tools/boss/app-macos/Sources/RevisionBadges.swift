import AppKit
import os.log
import SwiftUI
import UpdateCore

struct RevisionBadge: View {
    let seq: Int

    var body: some View {
        HStack(spacing: 3) {
            Text("⟳")
                .font(.caption.weight(.semibold))
            Text("R\(seq)")
                .font(.system(.caption, design: .monospaced).weight(.semibold))
        }
        .foregroundStyle(.white)
        .padding(.horizontal, 6)
        .padding(.vertical, 2)
        .background(Color.accentColor.opacity(0.75), in: Capsule())
        .accessibilityLabel("Revision \(seq)")
    }
}

/// Discriminates the engine-triggered origin of a revision task from its
/// `created_via` field. `nil` when the revision is operator- or comment-driven.
/// Design: `tools/boss/docs/designs/unify-pr-remediation-on-revisions.md` Q2.

enum EngineRevisionOrigin {
    case mergeConflict
    case ciFix

    init?(createdVia: String) {
        if createdVia.hasPrefix("merge-conflict:") {
            self = .mergeConflict
        } else if createdVia.hasPrefix("ci-fix:") {
            self = .ciFix
        } else {
            return nil
        }
    }

    var label: String {
        switch self {
        case .mergeConflict: return "conflict fix"
        case .ciFix: return "CI fix"
        }
    }

    var helpText: String {
        switch self {
        case .mergeConflict: return "Engine-triggered revision: auto-generated to resolve a merge conflict."
        case .ciFix: return "Engine-triggered revision: auto-generated to fix a CI failure."
        }
    }

    var accessibilityLabel: String {
        switch self {
        case .mergeConflict: return "Engine-triggered conflict fix"
        case .ciFix: return "Engine-triggered CI fix"
        }
    }
}

/// Subtle chrome indicating an engine-triggered revision (merge-conflict or
/// CI-fix origin). Shown inline with [[RevisionBadge]] on revision cards
/// whose `created_via` matches one of the engine-trigger prefixes.

struct EngineRevisionBadge: View {
    let origin: EngineRevisionOrigin

    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "gear")
                .font(.caption2)
            Text(origin.label)
                .font(.caption.weight(.medium))
        }
        .foregroundStyle(.secondary)
        .padding(.horizontal, 5)
        .padding(.vertical, 2)
        .background(Color.secondary.opacity(0.12), in: Capsule())
        .help(origin.helpText)
        .accessibilityLabel(origin.accessibilityLabel)
    }
}

/// One rollup line in a Review-lane parent card for an in-review revision.
/// Shows `⟳ R<n>  <description truncated>  ↗` linking to the parent PR.

struct RevisionRollupLine: View {
    let revision: WorkTask

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            if let seq = revision.revisionSeq {
                Text("⟳ R\(seq)")
                    .font(.system(.caption2, design: .monospaced).weight(.semibold))
                    .foregroundStyle(Color.accentColor)
            }
            Text(revision.name)
                .font(.caption2)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.tail)
            Spacer(minLength: 0)
            if let prURL = revision.revisionParentPrUrl,
               let url = URL(string: prURL) {
                Link(destination: url) {
                    Image(systemName: "arrow.up.right")
                        .font(.caption2)
                        .foregroundStyle(Color.accentColor)
                }
                .buttonStyle(.plain)
                .help("Revision \(revision.revisionSeq.map { "R\($0)" } ?? ""): \(revision.name)")
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel({
            let seqLabel = revision.revisionSeq.map { "Revision \($0)" } ?? "Revision"
            return "\(seqLabel): \(revision.name)"
        }())
    }
}

/// Per-project "open the design doc" affordance for the kanban
/// project-section header. Icon variant is keyed off
/// [[ProjectDesignDocState]] (hidden / plain doc icon / warning glyph),
/// click handler is the engine-resolved open dispatch on
/// [[ChatViewModel.openProjectDesignDoc(_:)]]. The view stays empty
/// when no state has been resolved yet so cards don't flash a stale
/// affordance while the first `ResolveProjectDesignDoc` is in flight.
