//! Bounded SQLite group commits, with FIFO host queues and round-robin service.
//! Only transaction-local SQL belongs in a job: no network IO or nested locks.
use anyhow::{Result, anyhow};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

pub const MAX_BATCH_RECORDS: usize = 32;
pub const MAX_BATCH_BYTES: usize = 1024 * 1024;
pub const MAX_DELAY: Duration = Duration::from_micros(200);
const MAX_QUEUED: usize = 256;
const MAX_QUEUED_BYTES: usize = 4 * MAX_BATCH_BYTES;
const MAX_HOST_QUEUED: usize = 64;

type Completion = Box<dyn FnOnce(Option<&str>) + Send>;
type Write = Box<dyn FnOnce(&Transaction<'_>) -> Result<Completion> + Send>;

struct Job {
    bytes: usize,
    urgent: bool,
    queued: Instant,
    write: Write,
}

#[derive(Default)]
struct Queue {
    hosts: VecDeque<(String, VecDeque<Job>)>,
    count: usize,
    bytes: usize,
    closed: bool,
    prioritized_last: bool,
}

impl Queue {
    fn full(&self, host: &str, bytes: usize) -> bool {
        // A record larger than the byte cap is admitted alone into an empty
        // queue and commits as a batch of one, rather than being undeliverable.
        self.count >= MAX_QUEUED
            || (self.count > 0 && self.bytes + bytes > MAX_QUEUED_BYTES)
            || self.hosts.iter().any(|(h, q)| h == host && q.len() >= MAX_HOST_QUEUED)
    }

    fn push(&mut self, host: String, job: Job) {
        self.count += 1;
        self.bytes += job.bytes;
        if let Some((_, jobs)) = self.hosts.iter_mut().find(|(h, _)| *h == host) {
            jobs.push_back(job);
        } else {
            self.hosts.push_back((host, VecDeque::from([job])));
        }
    }

    fn ready(&self) -> bool {
        self.closed
            || self.count >= MAX_BATCH_RECORDS
            || self.bytes >= MAX_BATCH_BYTES
            || self.hosts.iter().any(|(_, q)| q.iter().any(|job| job.urgent))
    }

    fn deadline(&self) -> Option<Instant> {
        self.hosts
            .iter()
            .filter_map(|(_, q)| q.front().map(|j| j.queued + MAX_DELAY))
            .min()
    }

    fn take(&mut self) -> Vec<Job> {
        let mut batch = Vec::new();
        let mut bytes = 0;
        // Give one urgent host the first turn, then resume round-robin. Its
        // earlier numbered records must still precede the control result.
        // Alternate priority boosts with ordinary rotation. Otherwise a host
        // sending batch-sized urgent frames could monopolize every commit.
        let priority = (!self.prioritized_last)
            .then(|| {
                self.hosts
                    .iter()
                    .position(|(_, jobs)| jobs.front().is_some_and(|j| j.urgent))
            })
            .flatten();
        self.prioritized_last = priority.is_some();
        if let Some(index) = priority {
            let host = self.hosts.remove(index).unwrap();
            self.hosts.push_front(host);
        }
        while batch.len() < MAX_BATCH_RECORDS {
            let Some((host, mut jobs)) = self.hosts.pop_front() else {
                break;
            };
            if bytes + jobs.front().unwrap().bytes > MAX_BATCH_BYTES && !batch.is_empty() {
                self.hosts.push_front((host, jobs));
                break;
            }
            let job = jobs.pop_front().unwrap();
            bytes += job.bytes;
            self.bytes -= job.bytes;
            self.count -= 1;
            batch.push(job);
            if !jobs.is_empty() {
                self.hosts.push_back((host, jobs));
            }
        }
        batch
    }
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    changed: Condvar,
    committed_batches: AtomicU64,
}

struct Sender(Arc<Shared>);
impl Drop for Sender {
    fn drop(&mut self) {
        self.0.queue.lock().unwrap().closed = true;
        self.0.changed.notify_all();
    }
}

/// Dropping the last writer closes admission and drains already queued jobs.
#[derive(Clone)]
pub struct Writer(Arc<Sender>);

/// A successful value is published only after the outer transaction commits.
pub struct Receipt<T>(mpsc::Receiver<Result<T>>);

pub type ConnectionWork<'a> = Box<dyn FnOnce(&mut Connection) -> Result<()> + 'a>;

/// Lets the owner retain its connection-lock diagnostics and accounting.
pub trait ConnectionSource: Send + Sync {
    fn with_connection(&self, write: ConnectionWork<'_>) -> Result<()>;
}

impl ConnectionSource for Mutex<Connection> {
    fn with_connection(&self, write: ConnectionWork<'_>) -> Result<()> {
        let mut connection = self.lock().map_err(|_| anyhow!("ingress connection poisoned"))?;
        write(&mut connection)
    }
}

impl<T> Receipt<T> {
    pub fn wait(self) -> Result<T> {
        blocking(|| self.0.recv().map_err(|_| anyhow!("ingress writer stopped"))?)
    }
}

fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

impl Writer {
    pub fn new(connection: Arc<dyn ConnectionSource>) -> Result<Self> {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("ingress-writes".into())
            .spawn(move || {
                loop {
                    let batch = {
                        let mut q = worker.queue.lock().unwrap();
                        loop {
                            if q.closed && q.count == 0 {
                                return;
                            }
                            match q.deadline() {
                                Some(deadline) if q.ready() || Instant::now() >= deadline => break,
                                Some(deadline) => {
                                    q = worker
                                        .changed
                                        .wait_timeout(q, deadline.saturating_duration_since(Instant::now()))
                                        .unwrap()
                                        .0;
                                }
                                None => q = worker.changed.wait(q).unwrap(),
                            }
                        }
                        let batch = q.take();
                        worker.changed.notify_all();
                        batch
                    };
                    commit(connection.as_ref(), batch, &worker.committed_batches);
                }
            })?;
        Ok(Self(Arc::new(Sender(shared))))
    }

    pub fn committed_batches(&self) -> u64 {
        self.0.0.committed_batches.load(Ordering::Relaxed)
    }

    /// Backpressure is bounded by count and encoded bytes, globally and per
    /// host. A single record above the batch byte cap is never rejected here:
    /// it waits for an empty queue and commits alone, so size limits stay with
    /// the caller's per-host quota. Callers retain ownership on admission failure; never drop evidence.
    pub fn submit<T: Send + 'static>(
        &self,
        host: String,
        bytes: usize,
        urgent: bool,
        write: impl FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static,
    ) -> Result<Receipt<T>> {
        let (tx, rx) = mpsc::channel();
        let job = Job {
            bytes,
            urgent,
            queued: Instant::now(),
            write: Box::new(move |conn| {
                // Isolate a malformed host record without rolling back another
                // host's valid writes. Deferred constraints still fail the batch.
                conn.execute_batch("SAVEPOINT ingress_record")?;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write(conn)))
                    .unwrap_or_else(|_| Err(anyhow!("ingress write panicked")));
                if result.is_err() {
                    conn.execute_batch("ROLLBACK TO ingress_record")?;
                }
                conn.execute_batch("RELEASE ingress_record")?;
                Ok(Box::new(move |failure: Option<&str>| {
                    let result = match failure {
                        Some(error) => Err(anyhow!(error.to_owned())),
                        None => result,
                    };
                    let _ = tx.send(result);
                }) as Completion)
            }),
        };
        let shared = &self.0.0;
        blocking(|| {
            let mut q = shared.queue.lock().unwrap();
            while q.full(&host, bytes) && !q.closed {
                q = shared.changed.wait(q).unwrap();
            }
            anyhow::ensure!(!q.closed, "ingress writer closed");
            q.push(host, job);
            shared.changed.notify_all();
            Ok(Receipt(rx))
        })
    }
}

fn commit(connection: &dyn ConnectionSource, batch: Vec<Job>, committed: &AtomicU64) {
    let mut completions = Vec::new();
    let result = connection.with_connection(Box::new(|conn| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for job in batch {
            completions.push((job.write)(&tx)?);
        }
        tx.commit()?;
        committed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }));
    let error = result.err().map(|e| format!("ingress batch commit failed: {e:#}"));
    for complete in completions {
        complete(error.as_deref());
    }
}

#[cfg(test)]
mod tests;
