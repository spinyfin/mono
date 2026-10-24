//! Compact global decision feed. Reuses the work mutation publishers so
//! every question producer and every withdrawal path share the same channel.
use super::*;
use boss_protocol::OperatorDecisionNotification;

pub(super) const TOPIC: &str = "work.decisions";

fn snapshot(db: &WorkDb) -> anyhow::Result<Vec<OperatorDecisionNotification>> {
    let conn = db.connect()?;
    let mut stmt = conn.prepare(
        "SELECT q.id, json_extract(q.question_json, '$.text'), t.id, t.product_id,
                CASE WHEN t.short_id IS NULL THEN t.name
                     ELSE '#' || t.short_id || ': ' || t.name END
         FROM operator_questions q JOIN tasks t ON t.id = q.work_item_id
         WHERE q.status = 'open' AND t.deleted_at IS NULL ORDER BY q.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(OperatorDecisionNotification {
            question_id: row.get(0)?,
            text: row.get(1)?,
            work_item_id: row.get(2)?,
            product_id: row.get(3)?,
            label: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

impl TopicBroker {
    pub(super) async fn refresh_decisions(&self, db: &WorkDb, force: bool) {
        // Serialize reads with publication: an older snapshot must never land
        // after a newer one and resurrect a closed question's reminder.
        let mut previous = self.decision_snapshot.lock().await;
        if !self.inner.lock().await.sessions_by_topic.contains_key(TOPIC) {
            return;
        }
        let questions = match snapshot(db) {
            Ok(questions) => questions,
            Err(err) => {
                tracing::error!(?err, "failed to refresh decision notifications");
                return;
            }
        };
        if !force && previous.as_ref() == Some(&questions) {
            return;
        }
        *previous = Some(questions.clone());
        self.publish(
            TOPIC,
            FrontendEventEnvelope::push(FrontendEvent::OperatorDecisionSnapshot { questions }),
        )
        .await;
    }
}
