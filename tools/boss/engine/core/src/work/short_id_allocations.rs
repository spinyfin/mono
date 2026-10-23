use super::*;

/// Shared per-product short-id read-modify-write. Reads the current
/// `next_value` from `table` for `product_id` (defaulting to 1 when no
/// row exists yet), writes back `next_value + 1`, and returns the value
/// just claimed. Must be called inside the same SQLite transaction as
/// the row insert; SQLite serialises writers in WAL mode, so two
/// concurrent inserts against the same product receive distinct ids.
///
/// `table` is a compile-time literal supplied by the three
/// `pub(crate)` wrappers below (never caller/user input), so
/// formatting it into the two SQL statements is not an injection
/// surface.
fn allocate_next_short_id(conn: &Connection, table: &str, product_id: &str) -> Result<i64> {
    let current: i64 = conn
        .query_row(
            &format!("SELECT next_value FROM {table} WHERE product_id = ?1"),
            [product_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(1);
    conn.execute(
        &format!(
            "INSERT INTO {table}(product_id, next_value) VALUES(?1, ?2)
             ON CONFLICT(product_id) DO UPDATE SET next_value = excluded.next_value"
        ),
        params![product_id, current + 1],
    )?;
    Ok(current)
}

/// Allocate the next per-product `short_id` for a new `tasks` or
/// `projects` row. Reads the current `next_value` from
/// `short_id_sequences` for `product_id`, defaulting to 1 if no row
/// exists yet, writes back `next_value + 1`, and returns the value
/// just claimed. Must be called inside the same SQLite transaction as
/// the row insert; SQLite serialises writers in WAL mode, so two
/// concurrent inserts against the same product receive distinct ids.
///
/// See `tools/boss/docs/designs/friendly-numeric-ids-for-work-items.md`
/// (Q3) for the reasoning behind the per-product scope and the
/// in-transaction read-modify-write pattern.
pub(crate) fn allocate_short_id(conn: &Connection, product_id: &str) -> Result<i64> {
    allocate_next_short_id(conn, "short_id_sequences", product_id)
}

/// Parallel to [`allocate_short_id`] for the `A<n>` automation namespace.
/// Reads and advances `automation_short_id_sequences` for `product_id`.
/// Must be called inside the same transaction as the `automations` row insert.
/// See `tools/boss/docs/designs/maintenance-tasks.md` §"Short-id namespace".
pub(crate) fn allocate_automation_short_id(conn: &Connection, product_id: &str) -> Result<i64> {
    allocate_next_short_id(conn, "automation_short_id_sequences", product_id)
}

/// Parallel to [`allocate_short_id`] for the attention-group `A<n>`
/// namespace. Reads and advances `attention_group_short_id_sequences`
/// for `product_id`. Must be called inside the same transaction as the
/// `attention_groups` row insert. Attention groups get their own dense
/// per-product counter (rather than sharing the tasks/projects sequence)
/// so the first group in a busy product is `A1`, not `A<large>`.
/// See `tools/boss/docs/designs/attentions.md` §"Schema and wire summary".
pub(crate) fn allocate_attention_group_short_id(conn: &Connection, product_id: &str) -> Result<i64> {
    allocate_next_short_id(conn, "attention_group_short_id_sequences", product_id)
}

/// Parallel to [`allocate_short_id`] for the product-decision `D<n>`
/// namespace. Reads and advances `decision_short_id_sequences` for
/// `product_id`. Must be called inside the same transaction as the
/// `product_decisions` row insert.
pub(crate) fn allocate_decision_short_id(conn: &Connection, product_id: &str) -> Result<i64> {
    allocate_next_short_id(conn, "decision_short_id_sequences", product_id)
}

/// Parallel to [`allocate_short_id`] for the idea `I<n>` namespace. Reads
/// and advances `idea_short_id_sequences` for `product_id`. Must be called
/// inside the same transaction as the `ideas` row insert (own dense
/// per-product counter, not the shared `short_id_sequences` tasks/chores/
/// projects use).
pub(crate) fn allocate_idea_short_id(conn: &Connection, product_id: &str) -> Result<i64> {
    allocate_next_short_id(conn, "idea_short_id_sequences", product_id)
}
