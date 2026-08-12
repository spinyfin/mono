//! Shared work-item id selector grammar and clap surface marker.
//!
//! Both `boss` and `bossctl` accept the same selector forms for work items.
//! The pure parser lives here so neither binary reimplements the grammar,
//! and so a surface-enumeration test can discover every id-accepting
//! clap argument by the shared [`WORK_ITEM_ID_VALUE_NAME`] marker.
//!
//! Authoritative **resolution** (DB lookup, ambiguity, product scope)
//! lives in the engine (`WorkDb::resolve_work_item_ref` /
//! `GetWorkItem`). Clients parse with this module, then hand the
//! selector to that choke point — they must not invent a second
//! resolution path.

/// clap `value_name` stamped on every argument that accepts a work-item
/// id (primary `task_…` / `proj_…`, friendly `T<n>` / `P<n>` / `#n` /
/// bare `n`, or cross-product `slug/n`).
///
/// The surface-enumeration regression test walks each binary's clap
/// command tree and treats every arg with this value_name as an
/// id-accepting surface that must route through the shared resolver.
pub const WORK_ITEM_ID_VALUE_NAME: &str = "WORK_ITEM_ID";

/// Stable marker substring in engine messages for short-id **ambiguity**.
/// Built into the engine error text and matched by clients that need to
/// discriminate ambiguity from not-found without depending on free-form
/// English (a reworded message that drops this marker is a deliberate
/// protocol break).
pub const WORK_ITEM_ID_AMBIGUOUS_MARKER: &str = "short id is ambiguous";

/// Stable marker substring in engine messages for **not-found**
/// resolution failures (friendly form or typed primary id with no row).
/// Clients that want "no match" fall-through (e.g. agent verbs that
/// then list live workers) match this and must not treat it as hard
/// failure when a non-work-item interpretation is still possible.
pub const WORK_ITEM_ID_NOT_FOUND_MARKER: &str = "no matching work item";

/// Parsed form of a task/chore/project selector.
///
/// Priority order:
/// 1. `#42` / `42` / `T42` / `t42` / `P7` / `p7` → short id
/// 2. `boss/42` / `boss/#42` → cross-product short id
/// 3. `task_…` / `proj_…` / `prod_…` → primary id (typed)
/// 4. anything else → opaque (slug / execution id / pass-through)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkItemSelector {
    /// `42` / `#42` / `T42` / `P7` — short id; product may still be needed
    /// when the number is not globally unique.
    ShortId(i64),
    /// `boss/42` or `boss/#42` — short id in the named product slug.
    ProductShortId { product_slug: String, n: i64 },
    /// `task_…` / `proj_…` / `prod_…` — primary engine id.
    PrimaryId(String),
    /// Slug, execution id, or other selector; fall through to existing
    /// resolution (or hard-error at the call site).
    Other(String),
}

/// True when `s` looks like a typed engine work-item id (`prod_` /
/// `proj_` / `task_`). Chores share the `task_` prefix.
pub fn is_typed_work_item_id(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("prod_") || s.starts_with("proj_") || s.starts_with("task_")
}

/// True when `s` is a friendly short-id form the shared resolver must
/// resolve (not pass through as a literal primary / execution id).
///
/// Used by surfaces like `bossctl dispatch diagnose` that also accept
/// execution ids: short-id forms must hard-error when unresolvable,
/// while opaque forms may fall through to execution-id lookup.
pub fn is_friendly_work_item_selector(s: &str) -> bool {
    matches!(
        parse_work_item_selector(s),
        WorkItemSelector::ShortId(_) | WorkItemSelector::ProductShortId { .. }
    )
}

/// Canonical `T{n}` form used on the wire to `GetWorkItem` when the
/// caller has a bare short id and no product scope. The engine's
/// shared resolver accepts this form (and handles ambiguity).
pub fn short_id_wire_form(n: i64) -> String {
    format!("T{n}")
}

/// Parse `s` into a [`WorkItemSelector`].
pub fn parse_work_item_selector(s: &str) -> WorkItemSelector {
    let s = s.trim();
    // Cross-product form: "boss/42" or "boss/#42"
    if let Some(slash) = s.find('/') {
        let product_slug = &s[..slash];
        let rest = s[slash + 1..].trim_start_matches('#');
        if !product_slug.is_empty()
            && let Ok(n) = rest.parse::<i64>()
            && n > 0
        {
            return WorkItemSelector::ProductShortId {
                product_slug: product_slug.to_owned(),
                n,
            };
        }
    }
    // `#42` form (explicit friendly-id prefix)
    if let Some(rest) = s.strip_prefix('#')
        && let Ok(n) = rest.parse::<i64>()
        && n > 0
    {
        return WorkItemSelector::ShortId(n);
    }
    // `T42` / `t42` / `P7` / `p12` — friendly kanban id.
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        if (first == b'T' || first == b't' || first == b'P' || first == b'p')
            && let Ok(n) = s[1..].parse::<i64>()
            && n > 0
        {
            return WorkItemSelector::ShortId(n);
        }
    }
    // Plain integer → short id
    if let Ok(n) = s.parse::<i64>()
        && n > 0
    {
        return WorkItemSelector::ShortId(n);
    }
    if is_typed_work_item_id(s) {
        return WorkItemSelector::PrimaryId(s.to_owned());
    }
    WorkItemSelector::Other(s.to_owned())
}

/// Extract the numeric short id when `s` is a friendly form (`T42`,
/// `P7`, `#42`, bare `42`). Returns `None` for primary ids and opaque
/// selectors. Does not perform DB resolution.
pub fn parse_short_id_number(s: &str) -> Option<i64> {
    match parse_work_item_selector(s) {
        WorkItemSelector::ShortId(n) => Some(n),
        WorkItemSelector::ProductShortId { n, .. } => Some(n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_t_and_p_prefix() {
        assert_eq!(parse_work_item_selector("T42"), WorkItemSelector::ShortId(42));
        assert_eq!(parse_work_item_selector("t42"), WorkItemSelector::ShortId(42));
        assert_eq!(parse_work_item_selector("P7"), WorkItemSelector::ShortId(7));
        assert_eq!(parse_work_item_selector("p9"), WorkItemSelector::ShortId(9));
    }

    #[test]
    fn parses_hash_and_bare() {
        assert_eq!(parse_work_item_selector("#42"), WorkItemSelector::ShortId(42));
        assert_eq!(parse_work_item_selector("42"), WorkItemSelector::ShortId(42));
    }

    #[test]
    fn parses_cross_product() {
        assert_eq!(
            parse_work_item_selector("boss/42"),
            WorkItemSelector::ProductShortId {
                product_slug: "boss".into(),
                n: 42,
            }
        );
        assert_eq!(
            parse_work_item_selector("boss/#42"),
            WorkItemSelector::ProductShortId {
                product_slug: "boss".into(),
                n: 42,
            }
        );
    }

    #[test]
    fn parses_primary_and_rejects_zero() {
        assert!(matches!(
            parse_work_item_selector("task_18ae0000_1"),
            WorkItemSelector::PrimaryId(_)
        ));
        assert!(matches!(parse_work_item_selector("T0"), WorkItemSelector::Other(_)));
        assert!(matches!(parse_work_item_selector("Tabc"), WorkItemSelector::Other(_)));
    }

    #[test]
    fn friendly_selector_predicate() {
        assert!(is_friendly_work_item_selector("T42"));
        assert!(is_friendly_work_item_selector("1135"));
        assert!(is_friendly_work_item_selector("#1135"));
        assert!(is_friendly_work_item_selector("boss/42"));
        assert!(!is_friendly_work_item_selector("task_abc"));
        assert!(!is_friendly_work_item_selector("exec_abc"));
    }
}
