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

impl WorkTreeUpdate {
    /// Bound cold snapshot frames without dropping history or detail fields.
    /// Collections are split at record boundaries; a single oversized record
    /// is indivisible. Deltas retain their existing encoding.
    pub fn into_pages(self, target_bytes: usize) -> serde_json::Result<Vec<Self>> {
        if self.base_sequence.is_some() {
            return Ok(vec![self]);
        }
        let Value::Object(changes) = self.changes else {
            unreachable!("snapshot object")
        };
        let mut pages = Vec::new();
        let mut fields = Map::new();
        let mut bytes = 0;
        for (field, value) in changes {
            let (collection, records) = match value {
                Value::Array(rows) if !rows.is_empty() => (true, rows),
                value => (false, vec![value]),
            };
            for record in records {
                let size = serde_json::to_vec(&record)?.len() + field.len() + 8;
                if bytes > 0 && bytes + size > target_bytes {
                    pages.push(std::mem::take(&mut fields));
                    bytes = 0;
                }
                if collection {
                    fields
                        .entry(field.clone())
                        .or_insert_with(|| json!([]))
                        .as_array_mut()
                        .unwrap()
                        .push(record);
                } else {
                    fields.insert(field.clone(), record);
                }
                bytes += size;
            }
        }
        if !fields.is_empty() {
            pages.push(fields);
        }
        let count = pages.len();
        if count == 1 {
            return Ok(vec![Self {
                product_id: self.product_id,
                sequence: self.sequence,
                base_sequence: None,
                changes: Value::Object(pages.pop().unwrap()),
            }]);
        }
        Ok(pages
            .into_iter()
            .enumerate()
            .map(|(index, fields)| {
                let mut page = json!({"index": index, "count": count});
                page["fields"] = Value::Object(fields);
                Self {
                    product_id: self.product_id.clone(),
                    sequence: self.sequence,
                    base_sequence: None,
                    changes: Value::Object(Map::from_iter([("snapshot_page".into(), page)])),
                }
            })
            .collect())
    }

    pub fn is_last_page(&self) -> bool {
        let Some(page) = self.changes.get("snapshot_page") else {
            return true;
        };
        page["index"]
            .as_u64()
            .zip(page["count"].as_u64())
            .is_some_and(|(index, count)| index + 1 == count)
    }
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
            "tasks" | "chores" | "projects" | "ideas" | "attention_items" | "attention_groups"
            | "attention_members" => "id",
            "planner_runs" => "project_id",
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
    fn pages_preserve_every_record_and_collection_order() {
        let original = json!({"product": {"id": "one"}, "tasks": (0..200).map(|i| json!({"id": i, "description": "λ".repeat(100)})).collect::<Vec<_>>(), "chores": [], "dependencies": [{"dependent_id": "a"}], "task_runtimes": []});
        let update = WorkTreeUpdate {
            product_id: "one".into(),
            sequence: 1,
            base_sequence: None,
            changes: original.clone(),
        };
        let pages = update.into_pages(1024).unwrap();
        assert!(pages.len() > 1);
        let mut restored = Map::new();
        for (index, page) in pages.iter().enumerate() {
            assert_eq!(page.is_last_page(), index + 1 == pages.len());
            assert!(serde_json::to_vec(page).unwrap().len() < 1300);
            for (field, value) in page.changes["snapshot_page"]["fields"].as_object().unwrap() {
                match restored.get_mut(field) {
                    Some(Value::Array(rows)) => rows.extend(value.as_array().unwrap().iter().cloned()),
                    None => {
                        restored.insert(field.clone(), value.clone());
                    }
                    _ => panic!("duplicate scalar"),
                }
            }
        }
        assert_eq!(Value::Object(restored), original);
        let mut cache = WorkTreeCache::default();
        cache.update_value(original.clone());
        let delta = cache.update_value(original);
        assert_eq!(delta.into_pages(1).unwrap().len(), 1);
    }

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
