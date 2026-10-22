//! Counters for board invalidation traffic and the work it causes
//! (hub-and-spoke design §2.4).
//!
//! The `work invalidation fanout` tracing record says what was published, but
//! it is a log line: nothing counted pushes by kind, and nothing counted the
//! product-wide assemblies they cause. These counters make the runtime versus
//! structural split measurable on a live engine and in the invalidation
//! fixture: pushes by `(source, class)`, full board assemblies, and runtime
//! patches served.
//!
//! Names are keyed by source and class, so they are registered on first use
//! ([`Registry::counter_inc_by_dynamic`]) rather than as static handles;
//! "already registered" is the steady state, and no startup ordering exists to
//! get wrong.

use boss_protocol::InvalidationClass;

use crate::metrics::Registry;

/// Publishers a push can come from. Matches the `source` field of the
/// `work invalidation fanout` tracing record.
pub const SOURCE_MUTATION: &str = "mutation";
pub const SOURCE_BATCH_MUTATION: &str = "batch_mutation";
pub const SOURCE_EXECUTION: &str = "execution";

pub const FULL_ASSEMBLIES: &str = "board.assembly.full";
pub const RUNTIME_PATCHES: &str = "board.assembly.runtime_patch";
pub const RUNTIME_PATCH_CARDS: &str = "board.assembly.runtime_patch_cards";

/// Counter name for pushes from `source` of `class`.
pub fn push_counter(source: &str, class: InvalidationClass) -> String {
    format!("board.invalidation.{source}.{}", class.as_str())
}

/// Count one `WorkInvalidated` push (once per publish, not per subscriber).
pub fn record_push(registry: &Registry, source: &str, class: InvalidationClass) {
    registry.counter_inc_by_dynamic(
        &push_counter(source, class),
        "Board invalidation pushes published by this source with this class, since engine start.",
        1,
    );
}

/// Count one product-wide board assembly (`GetWorkTree` load that ran).
pub fn record_full_assembly(registry: &Registry) {
    registry.counter_inc_by_dynamic(
        FULL_ASSEMBLIES,
        "Product-wide board assemblies performed (single-flight loads that ran), since engine start.",
        1,
    );
}

/// Count one runtime patch served and the cards it re-read.
pub fn record_runtime_patch(registry: &Registry, cards: usize) {
    registry.counter_inc_by_dynamic(
        RUNTIME_PATCHES,
        "Runtime patches served in place of a board assembly, since engine start.",
        1,
    );
    registry.counter_inc_by_dynamic(
        RUNTIME_PATCH_CARDS,
        "Cards re-read across all runtime patches served, since engine start.",
        cards as u64,
    );
}

/// Pushes recorded for `(source, class)`, or zero.
pub fn pushes(registry: &Registry, source: &str, class: InvalidationClass) -> u64 {
    registry.counter_value(&push_counter(source, class)).unwrap_or(0)
}

/// Pushes recorded for `class` across every source, or zero.
pub fn pushes_of_class(registry: &Registry, class: InvalidationClass) -> u64 {
    [SOURCE_MUTATION, SOURCE_BATCH_MUTATION, SOURCE_EXECUTION]
        .into_iter()
        .map(|source| pushes(registry, source, class))
        .sum()
}

pub fn full_assemblies(registry: &Registry) -> u64 {
    registry.counter_value(FULL_ASSEMBLIES).unwrap_or(0)
}

pub fn runtime_patches(registry: &Registry) -> u64 {
    registry.counter_value(RUNTIME_PATCHES).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_pushes_by_source_and_class_and_assemblies_separately() {
        let registry = Registry::new();
        record_push(&registry, SOURCE_EXECUTION, InvalidationClass::Runtime);
        record_push(&registry, SOURCE_EXECUTION, InvalidationClass::Runtime);
        record_push(&registry, SOURCE_EXECUTION, InvalidationClass::Structural);
        record_push(&registry, SOURCE_MUTATION, InvalidationClass::Structural);
        record_full_assembly(&registry);
        record_runtime_patch(&registry, 3);
        record_runtime_patch(&registry, 1);

        assert_eq!(pushes(&registry, SOURCE_EXECUTION, InvalidationClass::Runtime), 2);
        assert_eq!(pushes(&registry, SOURCE_EXECUTION, InvalidationClass::Structural), 1);
        assert_eq!(pushes_of_class(&registry, InvalidationClass::Structural), 2);
        assert_eq!(pushes_of_class(&registry, InvalidationClass::Runtime), 2);
        assert_eq!(full_assemblies(&registry), 1);
        assert_eq!(runtime_patches(&registry), 2);
        assert_eq!(registry.counter_value(RUNTIME_PATCH_CARDS), Some(4));
        assert_eq!(pushes(&registry, SOURCE_BATCH_MUTATION, InvalidationClass::Runtime), 0);
    }
}
