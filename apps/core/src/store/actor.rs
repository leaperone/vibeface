//! SQLite is owned by dedicated threads. Accepted calls run once even if their caller leaves.
use super::Store;
use anyhow::{Context, Result, anyhow};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

const CAPACITY: usize = 64;
const TIMING_SAMPLES: usize = 4096;

type Job = Box<dyn FnOnce(&mut Store) -> bool + Send>;
struct Request {
    operation: &'static str,
    admitted: Instant,
    admission_wait: Duration,
    job: Job,
}

#[derive(Clone, Debug)]
pub(crate) struct StoreTiming {
    pub operation: &'static str,
    pub admission_wait: Duration,
    pub queue_wait: Duration,
    pub execute: Duration,
}

/// Admission failure proves the closure never ran. After acceptance, a missing response means
/// the outcome is unknown; callers must reconcile persisted state instead of replaying effects.
#[derive(Debug)]
pub(crate) enum StoreCallError {
    NotAdmitted,
    OutcomeUnknown,
}
impl std::fmt::Display for StoreCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotAdmitted => "database is closed; operation was not admitted",
            Self::OutcomeUnknown => "database worker stopped; accepted operation outcome is unknown",
        })
    }
}
impl std::error::Error for StoreCallError {}

struct Lane {
    // There is exactly one sender. Holding this fair mutex through reserve preserves FIFO
    // admission and lets shutdown close the queue without outstanding sender clones.
    admission: Mutex<Option<mpsc::Sender<Request>>>,
    done: watch::Receiver<bool>,
    worker: StdMutex<Option<JoinHandle<()>>>,
    timings: Arc<StdMutex<VecDeque<StoreTiming>>>,
}
impl Lane {
    fn start(
        name: &'static str,
        open: impl FnOnce() -> Result<Store> + Send + 'static,
    ) -> Result<(Self, oneshot::Receiver<Result<()>>)> {
        let (sender, mut receiver) = mpsc::channel::<Request>(CAPACITY);
        let (ready_tx, ready) = oneshot::channel();
        let (done_tx, done) = watch::channel(false);
        let timings = Arc::new(StdMutex::new(VecDeque::new()));
        let samples = timings.clone();
        let worker = thread::Builder::new().name(name.into()).spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut store = match open() {
                    Ok(store) => store,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                while let Some(request) = receiver.blocking_recv() {
                    let started = Instant::now();
                    let queue_wait = started.duration_since(request.admitted);
                    let healthy = (request.job)(&mut store);
                    let execute = started.elapsed();
                    tracing::debug!(
                        target: "vibeface_core::store::actor",
                        operation = request.operation,
                        admission_us = request.admission_wait.as_micros() as u64,
                        queue_us = queue_wait.as_micros() as u64,
                        execute_us = execute.as_micros() as u64,
                        "database operation"
                    );
                    let mut samples = samples.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if samples.len() == TIMING_SAMPLES {
                        samples.pop_front();
                    }
                    samples.push_back(StoreTiming {
                        operation: request.operation,
                        admission_wait: request.admission_wait,
                        queue_wait,
                        execute,
                    });
                    if !healthy {
                        break;
                    }
                }
                // Store, including the process lock, is destroyed before completion is signalled.
            }));
            drop(receiver);
            if result.is_err() {
                tracing::error!(worker = name, "database worker panicked");
            }
            let _ = done_tx.send(true);
        })?;
        Ok((Self {
            admission: Mutex::new(Some(sender)),
            done,
            worker: StdMutex::new(Some(worker)),
            timings,
        }, ready))
    }

    async fn call<T: Send + 'static>(
        &self,
        operation: &'static str,
        f: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let requested = Instant::now();
        let (reply, response) = oneshot::channel();
        let admission = self.admission.lock().await;
        let sender = admission.as_ref().ok_or(StoreCallError::NotAdmitted)?;
        let permit = sender.reserve().await.map_err(|_| StoreCallError::NotAdmitted)?;
        let admitted = Instant::now();
        permit.send(Request {
            operation,
            admitted,
            admission_wait: admitted.duration_since(requested),
            job: Box::new(move |store| {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(store))) {
                    Ok(result) => {
                        let _ = reply.send(result);
                        true
                    }
                    Err(_) => {
                        let _ = reply.send(Err(StoreCallError::OutcomeUnknown.into()));
                        false
                    }
                }
            }),
        });
        drop(admission);
        response.await.map_err(|_| StoreCallError::OutcomeUnknown)?
    }

    async fn close(&self) {
        self.admission.lock().await.take();
    }

    async fn join(&self) -> Result<()> {
        let mut done = self.done.clone();
        while !*done.borrow_and_update() {
            done.changed().await.context("database worker completion channel closed")?;
        }
        let worker = self.worker.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(worker) = worker {
            worker.join().map_err(|_| anyhow!("database worker panicked during shutdown"))?;
        }
        Ok(())
    }
}
impl Drop for Lane {
    fn drop(&mut self) {
        self.admission.get_mut().take();
        // Explicit shutdown waits asynchronously. This fallback also owns and joins the thread
        // on failed startup or final drop, so reopening cannot race a detached database owner.
        if let Some(worker) = self.worker.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
            let _ = worker.join();
        }
    }
}

pub struct StoreActor {
    writer: Lane,
    reader: Option<Lane>,
}
impl StoreActor {
    pub(crate) async fn open(
        path: PathBuf,
        process_lock: Arc<std::fs::File>,
        initialize: impl FnOnce(&mut Store) -> Result<()> + Send + 'static,
    ) -> Result<Self> {
        let writer_path = path.clone();
        let reader_lock = process_lock.clone();
        let (writer, ready) = Lane::start("store-writer", move || {
            let mut store = Store::open(writer_path)?;
            store.attach_process_lock(process_lock);
            initialize(&mut store)?;
            Ok(store)
        })?;
        ready.await.context("database writer failed during startup")??;
        let (reader, ready) = Lane::start("store-reader", move || {
            let mut store = Store::open_readonly(path)?;
            store.attach_process_lock(reader_lock);
            Ok(store)
        })?;
        ready.await.context("database reader failed during startup")??;
        Ok(Self { writer, reader: Some(reader) })
    }

    #[cfg(test)]
    pub(crate) fn from_store(store: Store) -> Result<Self> {
        let (writer, ready) = Lane::start("test-store-writer", move || Ok(store))?;
        // The supplied Store is already initialized; spawning needs no blocking handshake.
        drop(ready);
        Ok(Self { writer, reader: None })
    }

    pub async fn call<T: Send + 'static>(
        &self,
        operation: &'static str,
        f: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.writer.call(operation, f).await
    }

    /// A display-only snapshot of committed WAL data. Admission/authorization decisions use call.
    pub async fn read<T: Send + 'static>(
        &self,
        operation: &'static str,
        f: impl FnOnce(&Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let reader = self.reader.as_ref().context("this test Store has no snapshot reader")?;
        reader.call(operation, move |store| {
            let transaction = store.connection.unchecked_transaction()?;
            let result = f(store);
            transaction.rollback()?;
            result
        }).await
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.writer.close().await;
        if let Some(reader) = &self.reader {
            reader.close().await;
        }
        self.writer.join().await?;
        if let Some(reader) = &self.reader {
            reader.join().await?;
        }
        Ok(())
    }

    pub(crate) fn take_timings(&self) -> Vec<StoreTiming> {
        let mut samples: Vec<_> = self.writer.timings.lock().unwrap_or_else(std::sync::PoisonError::into_inner).drain(..).collect();
        if let Some(reader) = &self.reader {
            samples.extend(reader.timings.lock().unwrap_or_else(std::sync::PoisonError::into_inner).drain(..));
        }
        samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> Result<(PathBuf, Arc<StoreActor>)> {
        let directory = std::env::temp_dir().join(format!("vibeface-store-actor-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&directory)?;
        let lock = Arc::new(crate::config::lock_data_dir(&directory)?);
        let actor = StoreActor::open(directory.join("node.sqlite3"), lock, |_| Ok(())).await?;
        Ok((directory, Arc::new(actor)))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn held_writer_allows_timer_and_committed_snapshot() -> Result<()> {
        let (directory, actor) = fixture().await?;
        let (entered, running) = oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let writer = actor.clone();
        let task = tokio::spawn(async move {
            writer.call("held_transaction", move |store| {
                let transaction = store.connection.transaction()?;
                transaction.execute("INSERT INTO identity_meta(key,value) VALUES('actor_test','committed')", [])?;
                let _ = entered.send(());
                held.recv()?;
                transaction.commit()?;
                Ok(())
            }).await
        });
        tokio::time::timeout(Duration::from_secs(2), running).await??;
        let mut heartbeat = tokio::time::interval(Duration::from_millis(10));
        tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..3 { heartbeat.tick().await; }
            actor.read("committed_snapshot", |store| {
                let count: i64 = store.connection.query_row(
                    "SELECT count(*) FROM identity_meta WHERE key='actor_test'", [], |row| row.get(0),
                )?;
                assert_eq!(count, 0, "snapshot must not expose the uncommitted writer row");
                Ok(())
            }).await
        }).await??;
        release.send(())?;
        task.await??;
        assert_eq!(actor.read("committed_result", |store| {
            Ok(store.connection.query_row(
                "SELECT value FROM identity_meta WHERE key='actor_test'", [], |row| row.get::<_, String>(0),
            )?)
        }).await?, "committed");
        actor.shutdown().await?;
        drop(actor);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_an_accepted_write_executes_it_once() -> Result<()> {
        let (directory, actor) = fixture().await?;
        let (entered, running) = oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let writer = actor.clone();
        let task = tokio::spawn(async move {
            writer.call("accepted_cancelled_write", move |store| {
                let _ = entered.send(());
                held.recv()?;
                store.connection.execute(
                    "INSERT INTO identity_meta(key,value) VALUES('actor_test','1')
                     ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1", [],
                )?;
                Ok(())
            }).await
        });
        tokio::time::timeout(Duration::from_secs(2), running).await??;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        release.send(())?;
        let count: String = actor.call("accepted_write_barrier", |store| {
            Ok(store.connection.query_row(
                "SELECT value FROM identity_meta WHERE key='actor_test'", [], |row| row.get(0),
            )?)
        }).await?;
        assert_eq!(count, "1");
        actor.shutdown().await?;
        drop(actor);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_closes_admission_and_releases_the_process_lock() -> Result<()> {
        let (directory, actor) = fixture().await?;
        assert!(crate::config::lock_data_dir(&directory).is_err());
        actor.shutdown().await?;
        let error = actor.call("closed_write", |_| Ok(())).await.unwrap_err();
        assert!(matches!(error.downcast_ref::<StoreCallError>(), Some(StoreCallError::NotAdmitted)));
        let lock = Arc::new(crate::config::lock_data_dir(&directory)?);
        let reopened = StoreActor::open(directory.join("node.sqlite3"), lock, |_| Ok(())).await?;
        reopened.shutdown().await?;
        actor.shutdown().await?;
        drop(reopened);
        drop(actor);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}
