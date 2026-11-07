import Foundation

/// Selection history is local; opening the switcher never waits for the engine.
struct WorkSelectionHistory {
    static let storageKey = "boss.work.selectionHistory"
    private struct Snapshot: Codable {
        var products: [String] = []
        var projects: [String: [String]] = [:]
    }
    private let defaults: UserDefaults
    private var snapshot: Snapshot

    init(defaults: UserDefaults) {
        self.defaults = defaults
        snapshot = defaults.data(forKey: Self.storageKey)
            .flatMap { try? JSONDecoder().decode(Snapshot.self, from: $0) } ?? Snapshot()
    }

    mutating func useProduct(_ id: String) {
        snapshot.products.removeAll { $0 == id }
        snapshot.products.insert(id, at: 0)
        save()
    }

    mutating func useProjects(_ ids: Set<String>, productID: String) {
        guard !ids.isEmpty else { return }
        var history = snapshot.projects[productID] ?? []
        history.removeAll { ids.contains($0) }
        history.insert(contentsOf: ids.sorted(), at: 0)
        snapshot.projects[productID] = history
        save()
    }

    mutating func pruneProducts(validIDs: Set<String>) {
        snapshot.products.removeAll { !validIDs.contains($0) }
        snapshot.projects = snapshot.projects.filter { validIDs.contains($0.key) }
        save()
    }

    mutating func forgetProduct(_ id: String) {
        snapshot.products.removeAll { $0 == id }
        snapshot.projects.removeValue(forKey: id)
        save()
    }

    mutating func forgetProject(_ id: String, productID: String) {
        snapshot.projects[productID]?.removeAll { $0 == id }
        save()
    }

    /// Only prune products with a loaded snapshot: an unvisited product's
    /// missing work tree does not mean that its projects were deleted.
    mutating func pruneProjects(productID: String, validIDs: Set<String>) {
        snapshot.projects[productID]?.removeAll { !validIDs.contains($0) }
        save()
    }

    func products(fallback: [String], selected: String?) -> [String] {
        ranked(snapshot.products, fallback: fallback, selected: Set([selected].compactMap { $0 }))
    }

    func projects(productID: String, fallback: [String], selected: Set<String>) -> [String] {
        ranked(snapshot.projects[productID] ?? [], fallback: fallback, selected: selected)
    }

    private func ranked(_ history: [String], fallback: [String], selected: Set<String>) -> [String] {
        let valid = Set(fallback)
        var seen: Set<String> = []
        let ordered = (history + fallback).filter { valid.contains($0) && seen.insert($0).inserted }
        // Keep every active filter, even if a multi-selection exceeds seven.
        let active = selected.intersection(valid)
        var included = active
        for id in ordered where included.count < max(7, active.count) { included.insert(id) }
        return ordered.filter { included.contains($0) }
    }

    private func save() {
        if let data = try? JSONEncoder().encode(snapshot) { defaults.set(data, forKey: Self.storageKey) }
    }
}

extension ChatViewModel {
    var recentWorkProducts: [WorkProduct] {
        let fallback = activeProducts.sorted { ($0.name.localizedLowercase, $0.id) < ($1.name.localizedLowercase, $1.id) }
        return selectionHistory.products(fallback: fallback.map(\.id), selected: selectedWorkProductID)
            .compactMap { id in fallback.first { $0.id == id } }
    }

    var recentWorkProjects: [WorkProject] {
        guard let productID = selectedWorkProductID else { return [] }
        let fallback = allProjectsForSelectedProduct.filter { $0.status != "archived" }
            .sorted { ($0.name.localizedLowercase, $0.id) < ($1.name.localizedLowercase, $1.id) }
        return selectionHistory.projects(productID: productID, fallback: fallback.map(\.id), selected: selectedProjectFilterIDs)
            .compactMap { id in fallback.first { $0.id == id } }
    }

    var workSwitcherLabel: String {
        let product = selectedProduct?.name ?? "Choose product"
        return hasProjectFilters ? "\(product) · \(projectFilterDescription)" : product
    }
}
