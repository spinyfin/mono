//! Session-local board reconciliation. Compare complete projections so related
//! cards, product moves and deletions cannot be missed by invalidation ID lists.
use crate::WorkTree;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkTreeUpdate {
    pub product_id: String,
    pub sequence: u64,
    /// None means an authoritative snapshot; otherwise apply only to this base.
    pub base_sequence: Option<u64>,
    pub changes: Value,
}

/// Bounded to one product per connection. Switching products or reconnecting
/// yields a snapshot. Responses are ordered; queue shedding is detected by the
/// client's base-sequence check and recovered with an explicit full request.
#[derive(Default)]
pub struct WorkTreeCache {
    sequence: u64,
    previous: Option<Value>,
}

impl WorkTreeCache {
    pub fn clear(&mut self) {
        self.previous = None;
    }

    pub fn update(&mut self, tree: &WorkTree) -> serde_json::Result<WorkTreeUpdate> {
        let next = serde_json::to_value(tree)?;
        Ok(self.update_value(next))
    }

    fn update_value(&mut self, next: Value) -> WorkTreeUpdate {
        let product_id = next["product"]["id"].as_str().expect("product id").to_owned();
        let previous = self
            .previous
            .as_ref()
            .filter(|old| old["product"]["id"] == next["product"]["id"]);
        let base_sequence = previous.map(|_| self.sequence);
        let changes = previous.map_or_else(|| next.clone(), |old| diff(old, &next));
        self.sequence += 1;
        self.previous = Some(next);
        WorkTreeUpdate {
            product_id,
            sequence: self.sequence,
            base_sequence,
            changes,
        }
    }
}

/// Keyed collections carry only changed records, plus the authoritative ID order
/// when membership/order changes. Omitted IDs are tombstones. Small unkeyed
/// values (product and dependency edges) are replaced only when changed.
fn diff(old: &Value, next: &Value) -> Value {
    let mut changes = Map::new();
    for (field, value) in next.as_object().expect("tree object") {
        if old.get(field) == Some(value) {
            continue;
        }
        let key = match field.as_str() {
            "tasks" | "chores" | "projects" | "ideas" => "id",
            "task_runtimes" => "work_item_id",
            _ => {
                changes.insert(field.clone(), json!({"replace": value}));
                continue;
            }
        };
        let rows = value.as_array().expect("collection");
        let before = old[field].as_array().expect("collection");
        let by_id: HashMap<_, _> = before.iter().map(|row| (row[key].as_str().expect("id"), row)).collect();
        let upserts: Vec<_> = rows
            .iter()
            .filter(|row| by_id.get(row[key].as_str().expect("id")) != Some(row))
            .collect();
        let order: Vec<_> = rows.iter().map(|row| &row[key]).collect();
        let old_order: Vec<_> = before.iter().map(|row| &row[key]).collect();
        let mut patch = json!({"upserts": upserts});
        if order != old_order {
            patch["order"] = json!(order);
        }
        changes.insert(field.clone(), patch);
    }
    Value::Object(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_include_related_records_removals_and_order_without_unchanged_briefs() {
        let mut cache = WorkTreeCache::default();
        let first = json!({"product":{"id":"one"}, "tasks":[
            {"id":"root","badge":"pending","description":"unchanged brief"},
            {"id":"revision","status":"active"}], "dependencies":[]});
        assert!(cache.update_value(first.clone()).base_sequence.is_none());
        let mut next = first;
        next["tasks"][0]["badge"] = json!("clean");
        next["tasks"].as_array_mut().unwrap().remove(1);
        next["dependencies"] = json!([{"dependent_id":"root","prerequisite_id":"other"}]);
        let patch = cache.update_value(next.clone());
        assert_eq!(patch.base_sequence, Some(1));
        assert_eq!(patch.changes["tasks"]["order"], json!(["root"]));
        assert_eq!(patch.changes["tasks"]["upserts"], next["tasks"]);
        assert_eq!(patch.changes["dependencies"]["replace"], next["dependencies"]);
        assert_eq!(cache.update_value(next.clone()).changes, json!({}));
        next["product"]["id"] = json!("two");
        assert!(cache.update_value(next.clone()).base_sequence.is_none());
        cache.clear();
        assert!(cache.update_value(next).base_sequence.is_none());
    }
}
