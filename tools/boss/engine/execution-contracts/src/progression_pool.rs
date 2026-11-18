//! Apply daily progression sizing without interrupting already claimed slots.
use super::*;

impl WorkerPool {
    pub async fn claim_progression_worker(&self, execution_id: &str, size: usize) -> Option<String> {
        assert_eq!(self.prefix, PROGRESSION_WORKER_ID_PREFIX);
        let size = size.clamp(1, self.hard_cap);
        let mut inner = self.inner.lock().await;
        // Only remove idle trailing slots: running workers keep their identity
        // and can release normally even after the desired size decreases.
        while inner.workers.len() > size && inner.workers.last().is_some_and(|w| w.execution_id.is_none()) {
            inner.workers.pop();
        }
        while inner.workers.len() < size {
            let index = inner.workers.len();
            inner.workers.push(WorkerSlot {
                worker_id: format!("{}{index}", self.prefix, index = index + 1),
                execution_id: None,
                last_workspace_id: None,
                working: false,
            });
        }
        if inner.workers.iter().filter(|w| w.execution_id.is_some()).count() >= size {
            return None;
        }
        let (index, selection) = select_claim_index(&inner.workers[..size], None)?;
        let worker = &mut inner.workers[index];
        worker.execution_id = Some(execution_id.to_owned());
        worker.working = false;
        log_pool_claim(&worker.worker_id, execution_id, selection);
        Some(worker.worker_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn shrinking_drains_claims_and_growth_reuses_stable_ids() {
        let pool = WorkerPool::new_progression(1);
        let first = pool.claim_progression_worker("one", 2).await.unwrap();
        let second = pool.claim_progression_worker("two", 2).await.unwrap();
        assert!(pool.claim_progression_worker("three", 1).await.is_none());
        pool.release_worker(&first, None).await;
        assert!(pool.claim_progression_worker("three", 1).await.is_none());
        pool.release_worker(&second, None).await;
        assert_eq!(pool.claim_progression_worker("three", 1).await, Some(first));
        assert_eq!(pool.claim_progression_worker("four", 2).await, Some(second));
    }
}
