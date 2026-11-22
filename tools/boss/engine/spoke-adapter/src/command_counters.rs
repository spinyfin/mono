//! Per-op counts of [`crate::Hub::send_command`] calls, for tests that need to
//! observe "a command wait is armed" instead of sleeping. Compiled to nothing
//! outside tests and the `test-support` feature, so the production send path
//! pays no atomics or locks for them.

#[cfg(any(test, feature = "test-support"))]
mod enabled {
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub(crate) struct CommandCounters {
        waits: Mutex<HashMap<&'static str, usize>>,
        sends: Mutex<HashMap<&'static str, u64>>,
    }

    /// Decrements the in-flight count for its op when the wait ends.
    pub(crate) struct CommandWait<'a> {
        counters: &'a CommandCounters,
        op: &'static str,
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    impl CommandCounters {
        pub(crate) fn enter(&self, op: &'static str) -> CommandWait<'_> {
            *lock(&self.sends).entry(op).or_insert(0) += 1;
            *lock(&self.waits).entry(op).or_insert(0) += 1;
            CommandWait { counters: self, op }
        }

        pub(crate) fn waits(&self) -> usize {
            lock(&self.waits).values().sum()
        }

        pub(crate) fn sends(&self) -> u64 {
            lock(&self.sends).values().sum()
        }

        pub(crate) fn waits_for(&self, op: &str) -> usize {
            lock(&self.waits).get(op).copied().unwrap_or(0)
        }

        pub(crate) fn sends_for(&self, op: &str) -> u64 {
            lock(&self.sends).get(op).copied().unwrap_or(0)
        }
    }

    impl Drop for CommandWait<'_> {
        fn drop(&mut self) {
            if let Some(n) = lock(&self.counters.waits).get_mut(self.op) {
                *n = n.saturating_sub(1);
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) use enabled::CommandCounters;

#[cfg(not(any(test, feature = "test-support")))]
#[derive(Default)]
pub(crate) struct CommandCounters;

/// Stand-in for the in-flight guard; zero-sized and does nothing.
#[cfg(not(any(test, feature = "test-support")))]
pub(crate) struct CommandWait;

#[cfg(not(any(test, feature = "test-support")))]
impl CommandCounters {
    #[inline]
    pub(crate) fn enter(&self, _op: &'static str) -> CommandWait {
        CommandWait
    }
}
