//! Keep completed snapshots only while overlapping reads need them.
//! Hold the mutex through enqueue, never through database work.
//! Reads get a per-view sequence before querying. If a later-started read has
//! already replied, reuse its snapshot under the older request's correlation.
//! No query is retried, and idle views retain no snapshot or sequence state.
use std::collections::HashMap;

pub(super) struct ReadFreshness<T> {
    requests: HashMap<String, (String, u64)>,
    views: HashMap<String, View<T>>,
}

struct View<T> {
    active: usize,
    generation: u64,
    completed: u64,
    latest: Option<T>,
}

impl<T> Default for ReadFreshness<T> {
    fn default() -> Self {
        Self {
            requests: HashMap::new(),
            views: HashMap::new(),
        }
    }
}

impl<T: Clone> ReadFreshness<T> {
    pub(super) fn begin(&mut self, request: &str, key: String) {
        let view = self.views.entry(key.clone()).or_insert(View {
            active: 0,
            generation: 0,
            completed: 0,
            latest: None,
        });
        view.active += 1;
        view.generation += 1;
        self.requests.insert(request.into(), (key, view.generation));
    }

    pub(super) fn cancel(&mut self, request: &str) {
        if let Some((key, _)) = self.requests.remove(request) {
            let view = self.views.get_mut(&key).expect("active view");
            view.active -= 1;
            if view.active == 0 {
                self.views.remove(&key);
            }
        }
    }

    pub(super) fn finish(&mut self, request: &str, value: T) -> T {
        let Some((key, generation)) = self.requests.get(request) else {
            return value;
        };
        let view = self.views.get_mut(key).expect("active view");
        let value = if *generation < view.completed {
            view.latest.clone().expect("completed snapshot")
        } else {
            value
        };
        view.completed = view.completed.max(*generation);
        if view.active > 1 {
            view.latest = Some(value.clone());
        }
        self.cancel(request);
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_completion_preserves_latest_view_without_retaining_idle_views() {
        let mut reads = ReadFreshness::default();
        for id in 0..10 {
            reads.begin(&id.to_string(), "products".into());
        }
        reads.begin("unrelated", "projects".into());
        for id in (0..10).rev() {
            assert_eq!(reads.finish(&id.to_string(), id), 9);
        }
        assert_eq!(reads.finish("unrelated", 1), 1);
        assert!(reads.views.is_empty());
        assert!(reads.requests.is_empty());
    }

    #[test]
    fn newer_completion_can_advance_an_older_completed_snapshot() {
        let mut reads = ReadFreshness::default();
        reads.begin("old", "products".into());
        reads.begin("new", "products".into());
        assert_eq!(reads.finish("old", 1), 1);
        assert_eq!(reads.finish("new", 2), 2);
        assert!(reads.views.is_empty());
    }
}
