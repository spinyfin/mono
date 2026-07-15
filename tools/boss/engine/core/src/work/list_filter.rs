//! Shared builder for the AND-ed list-filter queries the attempt-list
//! reads share (`conflict_resolutions`, `ci_remediations`,
//! `rebase_attempts`). Each caller supplies its own base `SELECT ... FROM
//! <table> WHERE 1=1` and row mapper; this type appends the optional
//! clauses and owns the boxed params so callers stop re-deriving the
//! `Vec<Box<dyn ToSql>>` -> `Vec<&dyn ToSql>` conversion by hand.
//!
//! Filter semantics are the ones the callers already documented and must
//! not drift: filters are AND-ed, an empty `statuses` slice means "any
//! status", and `limit = None` emits no `LIMIT` clause at all.

use anyhow::Result;
use rusqlite::{Connection, Row, ToSql};

/// A `SELECT` with its accumulated bind params, built clause by clause.
///
/// Builder methods are chained in SQL order — the emitted text depends on
/// call order, so callers append filters before `order_by_created_desc`
/// and `limit`.
pub(super) struct ListFilterQuery {
    sql: String,
    params: Vec<Box<dyn ToSql>>,
}

impl ListFilterQuery {
    /// Start from a base `SELECT <columns> FROM <table> WHERE 1=1`. The
    /// trailing `WHERE 1=1` is what lets every filter below append an
    /// unconditional ` AND ...`.
    pub(super) fn new(base: impl Into<String>) -> Self {
        Self {
            sql: base.into(),
            params: Vec::new(),
        }
    }

    /// Optional ` AND product_id = ?`.
    pub(super) fn filter_product_id(self, product_id: Option<&str>) -> Self {
        self.filter_eq("product_id", product_id)
    }

    /// Optional ` AND work_item_id = ?`.
    pub(super) fn filter_work_item_id(self, work_item_id: Option<&str>) -> Self {
        self.filter_eq("work_item_id", work_item_id)
    }

    fn filter_eq(mut self, column: &str, value: Option<&str>) -> Self {
        if let Some(v) = value {
            self.sql.push_str(&format!(" AND {column} = ?"));
            self.params.push(Box::new(v.to_owned()));
        }
        self
    }

    /// ` AND status IN (?,?,...)` for a non-empty slice; an empty slice
    /// emits nothing, which is the "any status" case.
    pub(super) fn filter_status_in(mut self, statuses: &[String]) -> Self {
        if !statuses.is_empty() {
            self.sql.push_str(" AND status IN (");
            for (idx, status) in statuses.iter().enumerate() {
                if idx > 0 {
                    self.sql.push(',');
                }
                self.sql.push('?');
                self.params.push(Box::new(status.clone()));
            }
            self.sql.push(')');
        }
        self
    }

    /// ` ORDER BY created_at DESC, id DESC` — freshest first, which is
    /// what every list CLI wants in its first row.
    pub(super) fn order_by_created_desc(mut self) -> Self {
        self.sql.push_str(" ORDER BY created_at DESC, id DESC");
        self
    }

    /// Optional ` LIMIT ?`; `None` returns every match.
    pub(super) fn limit(mut self, limit: Option<u32>) -> Self {
        if let Some(cap) = limit {
            self.sql.push_str(" LIMIT ?");
            self.params.push(Box::new(cap as i64));
        }
        self
    }

    /// Prepare, run, and collect every row through `mapper`. Takes
    /// `&self` so the statement borrow ends when this returns, leaving
    /// `conn` free for the caller's next use.
    pub(super) fn collect<T, F>(&self, conn: &Connection, mapper: F) -> Result<Vec<T>>
    where
        F: FnMut(&Row<'_>) -> rusqlite::Result<T>,
    {
        let mut stmt = conn.prepare(&self.sql)?;
        let refs: Vec<&dyn ToSql> = self.params.iter().map(|b| b.as_ref() as &dyn ToSql).collect();
        let rows = stmt.query_map(refs.as_slice(), mapper)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render the accumulated binds so a test can assert both their
    /// order and their values against the SQL's `?` positions.
    fn binds(q: &ListFilterQuery) -> Vec<String> {
        q.params.iter().map(|p| format!("{:?}", p.to_sql().unwrap())).collect()
    }

    /// Same rendering as [`binds`], for building an expectation without
    /// hard-coding rusqlite's `ToSqlOutput` debug encoding.
    fn bind_of(v: impl ToSql) -> String {
        format!("{:?}", v.to_sql().unwrap())
    }

    /// The shape `list_conflict_resolutions` / `list_ci_remediations`
    /// build: every filter present. Pins the emitted text because these
    /// callers' contract is that the refactor changed no SQL.
    #[test]
    fn every_clause_emits_in_sql_order_with_matching_binds() {
        let q = ListFilterQuery::new("SELECT id FROM t WHERE 1=1")
            .filter_product_id(Some("prod1"))
            .filter_work_item_id(Some("item1"))
            .filter_status_in(&["pending".to_string(), "running".to_string()])
            .order_by_created_desc()
            .limit(Some(5));

        assert_eq!(
            q.sql,
            "SELECT id FROM t WHERE 1=1 AND product_id = ? AND work_item_id = ? \
             AND status IN (?,?) ORDER BY created_at DESC, id DESC LIMIT ?"
        );
        assert_eq!(
            binds(&q),
            vec![
                bind_of("prod1"),
                bind_of("item1"),
                bind_of("pending"),
                bind_of("running"),
                bind_of(5_i64),
            ]
        );
    }

    /// `None` filters and an empty `statuses` slice mean "any" — they
    /// must emit no clause at all rather than a tautology, and bind
    /// nothing.
    #[test]
    fn absent_filters_emit_no_clauses() {
        let q = ListFilterQuery::new("SELECT id FROM t WHERE 1=1")
            .filter_product_id(None)
            .filter_work_item_id(None)
            .filter_status_in(&[])
            .order_by_created_desc()
            .limit(None);

        assert_eq!(q.sql, "SELECT id FROM t WHERE 1=1 ORDER BY created_at DESC, id DESC");
        assert!(binds(&q).is_empty());
    }

    /// The `rebase_attempts` shape: product + status only, no
    /// work_item_id filter and no LIMIT.
    #[test]
    fn rebase_attempts_shape_omits_work_item_and_limit() {
        let q = ListFilterQuery::new("SELECT id FROM rebase_attempts WHERE 1=1")
            .filter_product_id(Some("prod1"))
            .filter_status_in(&["failed".to_string()])
            .order_by_created_desc();

        assert_eq!(
            q.sql,
            "SELECT id FROM rebase_attempts WHERE 1=1 AND product_id = ? \
             AND status IN (?) ORDER BY created_at DESC, id DESC"
        );
        assert_eq!(binds(&q).len(), 2);
    }

    /// A single status must not emit a leading comma — the index guard
    /// in the `IN` loop is what the three call sites hand-rolled.
    #[test]
    fn single_status_emits_no_leading_comma() {
        let q = ListFilterQuery::new("SELECT id FROM t WHERE 1=1").filter_status_in(&["pending".to_string()]);
        assert_eq!(q.sql, "SELECT id FROM t WHERE 1=1 AND status IN (?)");
    }
}
