import AppKit
import CryptoKit
import SwiftUI

/// Bounded evidence kept in memory; never publishes changes or repairs caches.
@MainActor
final class BoardDiagnostics {
    struct Entry: Codable {
        let sequence: Int
        let uptime: TimeInterval
        let event: String
        let fields: [String: String]
    }

    private let salt = UUID().uuidString
    private var tokens: [String: String] = [:]
    private(set) var entries: [Entry] = []
    private var sequence = 0
    private var appliedRows: [Int: WorkTask] = [:]
    private let capacity: Int
    private let probes = NSMapTable<NSString, NSView>.strongToWeakObjects()
    private(set) var presented: [String: [[String: String]]] = [:]

    init(capacity: Int = 256) { self.capacity = max(1, capacity) }

    // Session-local tokens preserve relationships without exporting identifiers.
    func token(_ id: String?) -> String {
        guard let id else { return "none" }
        if let cached = tokens[id] { return cached }
        let value = SHA256.hash(data: Data((salt + id).utf8)).prefix(8)
            .map { String(format: "%02x", $0) }.joined()
        if tokens.count >= 2048 { tokens.removeAll(keepingCapacity: true) }
        tokens[id] = value
        return value
    }

    func record(_ event: String, _ fields: [String: String] = [:]) {
        sequence += 1
        entries.append(Entry(sequence: sequence, uptime: ProcessInfo.processInfo.systemUptime,
                             event: event, fields: fields))
        if entries.count > capacity {
            let removed = entries.removeFirst()
            appliedRows.removeValue(forKey: removed.sequence)
        }
    }

    func recordAppliedRow(_ task: WorkTask) {
        record("item-apply")
        appliedRows[sequence] = task
    }

    func exportEntries() -> [Entry] {
        entries.map { entry in
            guard let task = appliedRows[entry.sequence] else { return entry }
            return Entry(sequence: entry.sequence, uptime: entry.uptime,
                         event: entry.event, fields: row(task))
        }
    }

    func row(_ task: WorkTask) -> [String: String] {
        ["id": token(task.id), "project": token(task.projectID),
         "product": token(task.productID), "parent": token(task.parentTaskId),
         "kind": task.kind, "status": task.status,
         "deleted": String(task.deletedAt != nil),
         "revisionActive": String(task.hasInProgressRevision),
         "folded": String(task.isFoldedRevision),
         "column": task.boardColumn.rawValue,
         "reviewSection": task.reviewSection?.rawValue ?? "none"]
    }

    func sections(_ sections: [WorkBoardSection]) -> [[String: String]] {
        sections.map {
            ["section": token($0.id), "count": String($0.items.count),
             "cards": $0.items.map { token($0.id) }.joined(separator: ","),
             "collapsible": String($0.isCollapsible),
             "storedExpanded": String(WorkBoardSectionCollapse.isExpanded(
                sectionID: $0.id, defaultExpanded: $0.defaultExpanded))]
        }
    }

    func present(column: String, sections: [[String: String]]) {
        presented[column] = sections
        record("column-presented", ["column": column,
            "cards": sections.compactMap { $0["cards"] }.joined(separator: ",")])
    }

    func disappear(column: String) {
        presented.removeValue(forKey: column)
        record("column-disappeared", ["column": column])
    }

    func register(_ view: NSView, id: String) {
        probes.setObject(view, forKey: token(id) as NSString)
    }

    func unregister(_ view: NSView, id: String) {
        let key = token(id) as NSString
        if probes.object(forKey: key) === view { probes.removeObject(forKey: key) }
    }

    func geometry(board: NSView?) -> [[String: String]] {
        probes.keyEnumerator().allObjects.compactMap { key in
            guard let key = key as? NSString, let view = probes.object(forKey: key) else { return nil }
            return ["id": key as String, "attached": String(view.window != nil),
                    "hidden": String(view.isHiddenOrHasHiddenAncestor),
                    "frame": NSStringFromRect(view.convert(view.bounds, to: nil)),
                    "visibleRect": NSStringFromRect(view.convert(view.visibleRect, to: nil)),
                    "visibleInBoard": String(RevealCardViewport.isVisible(view: view, board: board))]
        }.sorted { $0["id", default: ""] < $1["id", default: ""] }
    }
}

extension ChatViewModel {
    /// Read existing stores and caches, without warming, invalidating, fetching,
    /// or forcing layout. A capture must preserve the discrepancy it observes.
    func boardDiagnosticData() throws -> Data {
        let trace = boardDiagnostics
        let productID = currentSelectedProductID
        let rows = (tasksByProjectID.values.flatMap { $0 }
            + choresByProductID.values.flatMap { $0 }
            + productLevelTasksByProductID.values.flatMap { $0 }
            + productLevelRevisionsByProductID.values.flatMap { $0 })
            .filter { $0.productID == productID }
        let events = try JSONSerialization.jsonObject(with: JSONEncoder().encode(trace.exportEntries()))
        let snapshot: [String: Any] = [
            "schema": 1, "events": events,
            "appVersion": Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "unknown",
            "product": trace.token(productID),
            "projects": selectedProjectFilterIDs.sorted().map { trace.token($0) },
            "projectRows": (projectsByProductID[productID ?? ""] ?? []).map {
                ["id": trace.token($0.id), "status": $0.status]
            },
            "searchActive": isWorkSearchActive, "choresOnly": filterToChoresOnly,
            "includeChores": includeChores, "showBlockedOnly": showBlockedOnly,
            "showArchivedProjects": showArchivedProjects,
            "grouping": workBoardGrouping.rawValue,
            "navigation": String(describing: navigationMode),
            "navigatorMode": String(describing: navigator.mode),
            "rows": rows.sorted { $0.id < $1.id }.map { trace.row($0) },
            "cachedVisible": cachedVisibleItems.map { $0.map { trace.token($0.id) } } as Any? ?? NSNull(),
            "cachedItems": Dictionary(uniqueKeysWithValues: cachedItemsByColumn.map {
                ($0.key.rawValue, $0.value.map { trace.token($0.id) })
            }),
            "cachedSections": Dictionary(uniqueKeysWithValues: cachedSectionsByColumn.map {
                ($0.key.rawValue, trace.sections($0.value))
            }),
            "presentedSections": trace.presented,
            "boardVisibleRect": revealBoardViewport.map {
                NSStringFromRect($0.convert($0.visibleRect, to: nil))
            } ?? "unmounted",
            "geometry": trace.geometry(board: revealBoardViewport),
        ]
        return try JSONSerialization.data(withJSONObject: snapshot, options: [.prettyPrinted, .sortedKeys])
    }

    func copyBoardDiagnostics() {
        do {
            let data = try boardDiagnosticData()
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(String(decoding: data, as: UTF8.self), forType: .string)
        } catch {
            workErrorMessage = "Could not copy board diagnostics: \(error.localizedDescription)"
        }
    }
}

/// Records delivery of the section values shared by the header and list.
/// Instantiation is not proof of visible pixels; geometry is separate evidence.
struct BoardDiagnosticColumn: ViewModifier {
    let trace: BoardDiagnostics
    let column: String
    let sections: [WorkBoardSection]

    private struct Identity: Equatable {
        let id: String
        let cards: [String]
        let collapsible: Bool
        let defaultExpanded: Bool
    }

    func body(content: Content) -> some View {
        content
            .onAppear { trace.present(column: column, sections: trace.sections(sections)) }
            .onChange(of: sections.map {
                Identity(id: $0.id, cards: $0.items.map(\.id),
                         collapsible: $0.isCollapsible, defaultExpanded: $0.defaultExpanded)
            }) { _, _ in trace.present(column: column, sections: trace.sections(sections)) }
            .onDisappear { trace.disappear(column: column) }
    }
}
