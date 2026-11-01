use super::*;

/// Stage a board refresh in the transaction that changed classification
/// inputs, including callers with no frontend mutation handler.
pub(crate) fn stage_task_board_change(pending: &mut PendingEvents, conn: &Connection, id: &str) -> Result<()> {
    if let Some(task) = query_task(conn, id)? {
        pending.push(boss_event_bus::Event::WorkItemChanged {
            product_id: task.product_id,
            task_id: task.id,
        });
    }
    Ok(())
}
