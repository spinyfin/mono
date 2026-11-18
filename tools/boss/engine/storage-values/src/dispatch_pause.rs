//! Persisted dispatch-pause metadata keys and the pure rules for reading
//! them back, shared by the coordinator (restore at boot) and work-db
//! (review-pool admission) so the two cannot disagree on what a stored pause
//! holds.

use boss_protocol::DispatchPauseScope;

/// Metadata key for the global dispatch-pause flag. `"1"` = paused, `"0"` or
/// absent = running.
pub const METADATA_KEY_DISPATCH_PAUSED: &str = "dispatch_paused";
/// Metadata key storing the origin of the current pause (`operator` or
/// `breaker`).
pub const METADATA_KEY_DISPATCH_PAUSE_ORIGIN: &str = "dispatch_paused_origin";
/// Metadata key storing the scope of the current pause. Absent (a pause
/// persisted before this key existed) resolves to the origin's default scope;
/// an unrecognised value resolves to the stricter `all_spawns`; see
/// [`resolve_persisted_pause_scope`].
pub const METADATA_KEY_DISPATCH_PAUSE_SCOPE: &str = "dispatch_paused_scope";

/// The persisted origin string written for an operator-initiated pause.
pub const PERSISTED_ORIGIN_OPERATOR: &str = "operator";

/// True when the persisted origin string names an operator pause. Anything
/// else, including absent, is treated as a breaker pause, the conservative
/// reading that does not exempt reviews.
pub fn persisted_origin_is_operator(origin: Option<&str>) -> bool {
    origin == Some(PERSISTED_ORIGIN_OPERATOR)
}

/// Admission's exemption for reviews and critical main-pool work.
pub fn exempts_work(overridable: bool, review: bool, critical_main: bool) -> bool {
    overridable && (review || critical_main)
}

/// The scope a pause of the given persisted origin holds when no scope was
/// stored: operator pauses exempt reviews (`new_work`); anything else holds
/// everything (`all_spawns`).
pub fn default_scope_for_persisted_origin(origin: Option<&str>) -> DispatchPauseScope {
    if persisted_origin_is_operator(origin) {
        DispatchPauseScope::NewWork
    } else {
        DispatchPauseScope::AllSpawns
    }
}

/// Resolve the persisted scope of a pause. Absent restores as the origin's
/// default; a present-but-unrecognised value restores as the stricter
/// `all_spawns`.
pub fn resolve_persisted_pause_scope(scope: Option<&str>, origin: Option<&str>) -> DispatchPauseScope {
    match scope {
        Some(raw) => DispatchPauseScope::from_persisted(raw),
        None => default_scope_for_persisted_origin(origin),
    }
}

/// True when an operator pause with a review-holding scope (`all_spawns`) is
/// persisted. Breaker pauses are deliberately excluded: they are transient
/// and self-healing, so they neither defer admission nor release existing
/// never-started reservations.
pub fn operator_pause_holds_reviews(paused: Option<&str>, origin: Option<&str>, scope: Option<&str>) -> bool {
    paused == Some("1")
        && persisted_origin_is_operator(origin)
        && resolve_persisted_pause_scope(scope, origin).holds_reviews()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_scope_resolves_from_origin() {
        assert_eq!(
            resolve_persisted_pause_scope(None, Some("operator")),
            DispatchPauseScope::NewWork
        );
        assert_eq!(
            resolve_persisted_pause_scope(None, Some("breaker")),
            DispatchPauseScope::AllSpawns
        );
        assert_eq!(resolve_persisted_pause_scope(None, None), DispatchPauseScope::AllSpawns);
    }

    #[test]
    fn unrecognised_scope_is_all_spawns_for_any_origin() {
        assert_eq!(
            resolve_persisted_pause_scope(Some("garbage"), Some("operator")),
            DispatchPauseScope::AllSpawns
        );
        assert_eq!(
            resolve_persisted_pause_scope(Some(""), Some("operator")),
            DispatchPauseScope::AllSpawns
        );
    }

    #[test]
    fn only_operator_all_spawns_pause_holds_reviews() {
        assert!(operator_pause_holds_reviews(
            Some("1"),
            Some("operator"),
            Some("all_spawns")
        ));
        assert!(!operator_pause_holds_reviews(
            Some("1"),
            Some("operator"),
            Some("new_work")
        ));
        assert!(!operator_pause_holds_reviews(Some("1"), Some("operator"), None));
        assert!(!operator_pause_holds_reviews(
            Some("1"),
            Some("breaker"),
            Some("all_spawns")
        ));
        assert!(!operator_pause_holds_reviews(Some("1"), Some("breaker"), None));
        assert!(!operator_pause_holds_reviews(
            Some("0"),
            Some("operator"),
            Some("all_spawns")
        ));
        assert!(!operator_pause_holds_reviews(
            None,
            Some("operator"),
            Some("all_spawns")
        ));
    }
}
