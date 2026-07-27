//! `product_decisions` persistence: product-scoped wontfix / decided
//! records. Design: retire-the-coordinator-s-memory §T-B2-decision.
//!
//! These are deliberately **not** a `TaskStatus` — `cancelled` already
//! covers terminal-without-delivery work items. A decision is durable
//! product knowledge that should surface when filing near work.

use super::*;

/// Column list shared by every `product_decisions` SELECT. Order must match
/// [`map_decision`].
const DECISION_COLUMNS: &str = "id, short_id, product_id, kind, status, title, body, keywords, \
     related_work_item_id, superseded_by, created_by, created_via, created_at, updated_at";

fn map_decision(row: &Row<'_>) -> rusqlite::Result<Decision> {
    let kind_raw: String = row.get(3)?;
    let status_raw: String = row.get(4)?;
    let kind = kind_raw
        .parse::<DecisionKind>()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, e.into()))?;
    let status = status_raw
        .parse::<DecisionStatus>()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, e.into()))?;
    Ok(Decision {
        id: row.get(0)?,
        short_id: row.get(1)?,
        product_id: row.get(2)?,
        kind,
        status,
        title: row.get(5)?,
        body: row.get(6)?,
        keywords: row.get::<_, Option<String>>(7)?.filter(|s| !s.is_empty()),
        related_work_item_id: row.get::<_, Option<String>>(8)?.filter(|s| !s.is_empty()),
        superseded_by: row.get::<_, Option<String>>(9)?.filter(|s| !s.is_empty()),
        created_by: row.get(10)?,
        created_via: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

fn query_decision(conn: &Connection, id: &str) -> Result<Option<Decision>> {
    let sql = format!("SELECT {DECISION_COLUMNS} FROM product_decisions WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt.query_row([id], map_decision).optional()?;
    Ok(row)
}

impl WorkDb {
    /// Insert a new `active` product decision and return the row.
    pub fn create_decision(&self, input: CreateDecisionInput) -> Result<Decision> {
        if input.title.trim().is_empty() {
            bail!("decision title may not be empty");
        }
        if input.body.trim().is_empty() {
            bail!("decision body may not be empty");
        }
        if input.created_by.trim().is_empty() {
            bail!("decision created_by may not be empty");
        }

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_product_exists(&tx, &input.product_id)?;

        if let Some(ref work_item_id) = input.related_work_item_id {
            let work_item_id = work_item_id.trim();
            if !work_item_id.is_empty() {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND deleted_at IS NULL)",
                    [work_item_id],
                    |row| row.get(0),
                )?;
                if !exists {
                    bail!("unknown work item: {work_item_id}");
                }
            }
        }

        let id = next_id("dec");
        let now = now_string();
        let short_id = allocate_decision_short_id(&tx, &input.product_id)?;
        let created_via = input
            .created_via
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| CREATED_VIA_UNKNOWN.to_owned());
        let keywords = input.keywords.filter(|s| !s.trim().is_empty());
        let related_work_item_id = input.related_work_item_id.filter(|s| !s.trim().is_empty());

        tx.execute(
            "INSERT INTO product_decisions
                 (id, short_id, product_id, kind, status, title, body, keywords,
                  related_work_item_id, superseded_by, created_by, created_via,
                  created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, ?8, NULL, ?9, ?10, ?11, ?11)",
            params![
                id,
                short_id,
                input.product_id,
                input.kind.as_str(),
                input.title.trim(),
                input.body.trim(),
                keywords,
                related_work_item_id,
                input.created_by.trim(),
                created_via,
                now,
            ],
        )?;

        let decision = query_decision(&tx, &id)?.with_context(|| format!("missing decision after insert: {id}"))?;
        tx.commit()?;
        Ok(decision)
    }

    /// Fetch a single decision by canonical id.
    pub fn get_decision(&self, id: &str) -> Result<Option<Decision>> {
        let conn = self.connect()?;
        query_decision(&conn, id)
    }

    /// Fetch a decision by per-product `D<n>` short id.
    pub fn get_decision_by_short_id(&self, product_id: &str, short_id: i64) -> Result<Option<Decision>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;
        let sql = format!(
            "SELECT {DECISION_COLUMNS} FROM product_decisions \
             WHERE product_id = ?1 AND short_id = ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let row = stmt.query_row(params![product_id, short_id], map_decision).optional()?;
        Ok(row)
    }

    /// List decisions for a product, newest first. By default only
    /// `active` rows; `include_inactive` also returns superseded/revoked.
    pub fn list_decisions(&self, product_id: &str, include_inactive: bool) -> Result<Vec<Decision>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let mut sql = format!("SELECT {DECISION_COLUMNS} FROM product_decisions WHERE product_id = ?1");
        if !include_inactive {
            sql.push_str(" AND status = 'active'");
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([product_id], map_decision)?;
        collect_rows(rows)
    }

    /// Mark an active decision as `revoked`. Already-revoked is a no-op
    /// success. Superseded decisions cannot be revoked (revoke the
    /// successor, or leave the chain alone).
    pub fn revoke_decision(&self, id: &str) -> Result<Decision> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let existing = query_decision(&tx, id)?.with_context(|| format!("unknown decision: {id}"))?;
        match existing.status {
            DecisionStatus::Revoked => {
                tx.commit()?;
                return Ok(existing);
            }
            DecisionStatus::Superseded => {
                bail!("cannot revoke a superseded decision ({id}); revoke the successor instead");
            }
            DecisionStatus::Active => {}
        }
        let now = now_string();
        tx.execute(
            "UPDATE product_decisions
             SET status = 'revoked', superseded_by = NULL, updated_at = ?1
             WHERE id = ?2",
            params![now, id],
        )?;
        let updated = query_decision(&tx, id)?.with_context(|| format!("missing decision after revoke: {id}"))?;
        tx.commit()?;
        Ok(updated)
    }

    /// Mark `id` as `superseded` by `successor_id`. Both must be on the
    /// same product; the successor must be `active`; the predecessor must
    /// currently be `active`.
    pub fn supersede_decision(&self, id: &str, successor_id: &str) -> Result<Decision> {
        if id == successor_id {
            bail!("a decision cannot supersede itself");
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let existing = query_decision(&tx, id)?.with_context(|| format!("unknown decision: {id}"))?;
        if existing.status != DecisionStatus::Active {
            bail!(
                "only an active decision can be superseded ({} is {})",
                id,
                existing.status.as_str()
            );
        }
        let successor = query_decision(&tx, successor_id)?
            .with_context(|| format!("unknown successor decision: {successor_id}"))?;
        if successor.product_id != existing.product_id {
            bail!("successor decision must be on the same product");
        }
        if successor.status != DecisionStatus::Active {
            bail!(
                "successor decision must be active ({} is {})",
                successor_id,
                successor.status.as_str()
            );
        }
        let now = now_string();
        tx.execute(
            "UPDATE product_decisions
             SET status = 'superseded', superseded_by = ?1, updated_at = ?2
             WHERE id = ?3",
            params![successor_id, now, id],
        )?;
        let updated = query_decision(&tx, id)?.with_context(|| format!("missing decision after supersede: {id}"))?;
        tx.commit()?;
        Ok(updated)
    }
}
