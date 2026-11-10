//! SQLite is owned by dedicated threads. Accepted calls run once even if their caller leaves.
use super::Store;
use crate::identity::NetworkState;
use std::sync::atomic::{AtomicBool, Ordering};
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

type ClientMembership = (u64, Option<Arc<NetworkState>>);

struct MembershipPublication {
    dirty: Arc<AtomicBool>,
    authority: watch::Sender<ClientMembership>,
    changed: watch::Sender<u64>,
}
impl MembershipPublication {
    fn refresh(&self, store: &Store) -> Result<()> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let state = if store.has_network()? { Some(store.public_state()?) } else { None };
        let previous = self.authority.borrow().clone();
        if previous.1.as_deref() != state.as_ref() {
            let generation = previous.0.checked_add(1).context("membership generation overflow")?;
            self.authority.send_replace((generation, state.map(Arc::new)));
            self.changed.send_replace(generation);
        } else {
            // A timed recovery can change the effective directory without changing its history.
            self.changed.send_modify(|_| {});
        }
        Ok(())
    }

    fn invalidate(&self) {
        let generation = self.authority.borrow().0.saturating_add(1);
        self.authority.send_replace((generation, None));
        self.changed.send_replace(generation);
    }
}

pub struct StoreActor {
    writer: Lane,
    reader: Option<Lane>,
    publication: Option<Arc<MembershipPublication>>,
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
        Ok(Self { writer, reader: Some(reader), publication: None })
    }

    #[cfg(test)]
    pub(crate) fn from_store(store: Store) -> Result<Self> {
        let (writer, ready) = Lane::start("test-store-writer", move || Ok(store))?;
        // The supplied Store is already initialized; spawning needs no blocking handshake.
        drop(ready);
        Ok(Self { writer, reader: None, publication: None })
    }

    /// The hook marks possible membership changes; it never queries SQLite recursively.
    /// Publication happens after each accepted closure finishes, before its reply is released.
    pub(crate) async fn publish_membership_to(
        &mut self,
        authority: watch::Sender<ClientMembership>,
        changed: watch::Sender<u64>,
        log_wakeup: Arc<tokio::sync::Notify>,
    ) -> Result<()> {
        let publication = Arc::new(MembershipPublication {
            dirty: Arc::new(AtomicBool::new(true)), authority, changed,
        });
        let worker_publication = publication.clone();
        self.writer.call("initialize_membership_snapshot", move |store| {
            let dirty = worker_publication.dirty.clone();
            store.connection.update_hook(Some(move |_, _: &str, table: &str, _| {
                if table == "log_heads" { log_wakeup.notify_one(); }
                if table == "networks" || table == "identity_meta" {
                    dirty.store(true, Ordering::Release);
                }
            }));
            worker_publication.refresh(store)
        }).await?;
        self.publication = Some(publication);
        Ok(())
    }

    pub async fn call<T: Send + 'static>(
        &self,
        operation: &'static str,
        f: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let Some(publication) = self.publication.clone() else {
            return self.writer.call(operation, f).await;
        };
        self.writer.call(operation, move |store| {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(store)));
            let published = publication.refresh(store);
            if published.is_err() { publication.invalidate(); }
            match result {
                Ok(result) => { published?; result }
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }).await
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
    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_queued_write_is_drained_before_shutdown_returns() -> Result<()> {
        let (directory, actor) = fixture().await?;
        let (entered, running) = oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let blocking_actor = actor.clone();
        let blocking = tokio::spawn(async move {
            blocking_actor.call("held_writer", move |_| {
                let _ = entered.send(());
                held.recv()?;
                Ok(())
            }).await
        });
        tokio::time::timeout(Duration::from_secs(2), running).await??;
        let queued_actor = actor.clone();
        let queued = tokio::spawn(async move {
            queued_actor.call("cancelled_queued_write", |store| {
                store.connection.execute("INSERT INTO identity_meta(key,value) VALUES('queued_once','1')
                    ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1", [])?;
                Ok(())
            }).await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let accepted = actor.writer.admission.lock().await.as_ref()
                    .is_some_and(|sender| sender.capacity() == CAPACITY - 1);
                if accepted { break; }
                tokio::task::yield_now().await;
            }
        }).await?;
        queued.abort();
        assert!(queued.await.unwrap_err().is_cancelled());
        let shutting_actor = actor.clone();
        let shutdown = tokio::spawn(async move { shutting_actor.shutdown().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while actor.writer.admission.lock().await.is_some() {
                tokio::task::yield_now().await;
            }
        }).await?;
        assert!(!shutdown.is_finished(), "shutdown must wait for accepted work");
        let error = actor.call("late_call", |_| Ok(())).await.unwrap_err();
        assert!(matches!(error.downcast_ref::<StoreCallError>(), Some(StoreCallError::NotAdmitted)));
        release.send(())?;
        blocking.await??;
        shutdown.await??;
        let lock = Arc::new(crate::config::lock_data_dir(&directory)?);
        let reopened = StoreActor::open(directory.join("node.sqlite3"), lock, |_| Ok(())).await?;
        let count: String = reopened.call("queued_count", |store| {
            Ok(store.connection.query_row("SELECT value FROM identity_meta WHERE key='queued_once'", [], |row| row.get(0))?)
        }).await?;
        assert_eq!(count, "1");
        reopened.shutdown().await?;
        drop(reopened);
        drop(actor);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_panicked_writer_reports_unknown_and_cannot_reopen_itself() -> Result<()> {
        let (directory, actor) = fixture().await?;
        let error = actor.call::<()>("panic_after_effect", |store| {
            store.connection.execute("INSERT INTO identity_meta(key,value) VALUES('before_panic','committed')", [])?;
            panic!("synthetic database operation panic")
        }).await.unwrap_err();
        assert!(matches!(error.downcast_ref::<StoreCallError>(), Some(StoreCallError::OutcomeUnknown)));
        actor.shutdown().await?;
        assert!(actor.call("after_panic", |_| Ok(())).await.is_err());
        let lock = Arc::new(crate::config::lock_data_dir(&directory)?);
        let reopened = StoreActor::open(directory.join("node.sqlite3"), lock, |_| Ok(())).await?;
        let value: String = reopened.read("read_effect", |store| {
            Ok(store.connection.query_row("SELECT value FROM identity_meta WHERE key='before_panic'", [], |row| row.get(0))?)
        }).await?;
        assert_eq!(value, "committed");
        reopened.shutdown().await?;
        drop(reopened);
        drop(actor);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn membership_publication_changes_generation_only_for_committed_history() -> Result<()> {
        let (directory, actor) = fixture().await?;
        let mut actor = Arc::try_unwrap(actor).ok().context("unshared fixture actor")?;
        let authority = watch::Sender::new((0, None));
        let changed = watch::Sender::new(0);
        actor.publish_membership_to(authority.clone(), changed.clone(), Arc::new(tokio::sync::Notify::new())).await?;
        let identity = crate::crypto::LocalIdentity::generate();
        let state = identity.sign_network_fixture(1, vec![identity.device_id()?])?;
        let installed = state.clone();
        actor.call("install_history", move |store| store.install_state(installed)).await?;
        assert_eq!(authority.borrow().0, 1);
        assert_eq!(*changed.borrow(), 1);
        assert!(authority.borrow().1.as_deref() == Some(&state));
        let repeated = state.clone();
        actor.call("same_history", move |store| store.install_state(repeated)).await?;
        assert_eq!(authority.borrow().0, 1, "identical sync must not invalidate in-flight calls");
        let error = actor.call::<()>("rolled_back_history", |store| {
            store.connection.execute_batch("BEGIN; DELETE FROM networks; ROLLBACK;")?;
            Err(anyhow!("synthetic rolled-back operation"))
        }).await.unwrap_err();
        assert!(error.to_string().contains("rolled-back"));
        assert_eq!(authority.borrow().0, 1);
        assert!(authority.borrow().1.as_deref() == Some(&state));
        actor.shutdown().await?;
        drop(actor);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

}

#[cfg(test)]
mod performance {
    use super::*;
    use crate::{crypto::LocalIdentity, identity::NetworkState, model::*};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn signed_fixture(size: usize, cascade_group: Option<usize>) -> Result<(String, NetworkState, Vec<SignedEvent>, Vec<SignedEvent>, Vec<SignedEvent>)> {
        let identity = LocalIdentity::generate();
        let author = identity.device_id()?;
        let network = identity.fixture_network_id()?;
        let dependency_author = LocalIdentity::generate();
        let dependency_id = dependency_author.device_id()?;
        let state = identity.sign_network_fixture(1, vec![author.clone(), dependency_id.clone()])?;
        let mut dependencies: Vec<SignedEvent> = Vec::new();
        let admin = identity.fixture_admin();
        let mut events: Vec<SignedEvent> = Vec::new();
        let mut sign = |body: EventBody, deps: Vec<String>| -> Result<()> {
            events.push(identity.sign_event(Event {
                version: WIRE_VERSION, network_id: network.clone(), author: author.clone(),
                seq: events.len() as u64 + 1, prev: events.last().map(|event| event.hash.clone()),
                deps, scope_id: "c".into(), created_at_ms: events.len() as i64 + 1,
                body, blobs: vec![],
            })?);
            Ok(())
        };
        sign(EventBody::ConversationCreated { title: "Synthetic actor history".into() }, vec![])?;
        sign(admin.attest(&network, "c", EventBody::ConversationOwner {
            scheduler_node: author.clone(), epoch: 1, attestation: None, cause: None,
        })?, vec![])?;
        sign(admin.attest(&network, "c", EventBody::BotCreated {
            bot_id: "bot".into(), name: "Synthetic".into(), instructions: "fixture".into(),
            project: None, attestation: None, cause: None,
        })?, vec![])?;
        sign(admin.attest(&network, "c", EventBody::InputPosted {
            message_id: "seed".into(), target: crate::workspace::InputTarget::BotMain { bot_id: "bot".into() },
            intent: crate::workspace::InputIntent::Continue, sequence: 1, expected_revision: None,
            text: "fixture".into(), attachments: vec![], report_results: Some(false), attestation: None,
        })?, vec![])?;
        for index in 0..size {
            let deps = if cascade_group.is_some_and(|group| index % group == 0) {
                let dependency = dependency_author.sign_event(Event {
                    version: WIRE_VERSION, network_id: network.clone(), author: dependency_id.clone(),
                    seq: dependencies.len() as u64 + 1, prev: dependencies.last().map(|event| event.hash.clone()),
                    deps: vec![], scope_id: format!("dependency-{}", dependencies.len()),
                    created_at_ms: index as i64 + 1,
                    body: EventBody::ConversationCreated { title: "Synthetic dependency".into() }, blobs: vec![],
                })?;
                let hash = dependency.hash.clone();
                dependencies.push(dependency);
                vec![hash]
            } else { vec![] };
            sign(EventBody::MessagePosted {
                message_id: format!("assistant-{index}"), role: Role::Assistant, text: "synthetic reply".into(),
                attachments: vec![], report_results: None, in_reply_to: Some("seed".into()),
                status: Some(ReplyStatus::Completed), answers: None, attestation: None, failure: None,
            }, deps)?;
        }
        let workload = events.split_off(4);
        Ok((network, state, events, workload, dependencies))
    }

    fn report(label: &str, mut values: Vec<u128>) {
        values.sort_unstable();
        if values.is_empty() { eprintln!("metric={label} samples=0"); return; }
        let percentile = |p: usize| values[(values.len() - 1) * p / 100];
        eprintln!("metric={label} samples={} p50_ns={} p95_ns={} p99_ns={} max_ns={}",
            values.len(), percentile(50), percentile(95), percentile(99), values[values.len() - 1]);
    }

    /// Synthetic data only; signing is outside measured intervals. Algorithm baseline comparisons
    /// are in store::tests::performance_tests::signed_history_scale using the same signed bodies.
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "signed 1k/10k/100k actor ingestion with timer, snapshots and stop admission"]
    async fn signed_ingest_concurrency() -> Result<()> {
        let sizes = std::env::var("VIBEFACE_PERF_SIZES").unwrap_or_else(|_| "1000,10000,100000".into());
        for size in sizes.split(',').map(str::parse::<usize>) {
            let size = size?;
            for page in [64, 256, 1024] {
                for cascade in [false, true] {
                    let (network, state, prelude, events, dependencies) = tokio::task::spawn_blocking(
                        move || signed_fixture(size, cascade.then_some(page * 4)),
                    ).await??;
                    for repeat in 0..3 {
                        let directory = std::env::temp_dir().join(format!("vibeface-actor-perf-{}", uuid::Uuid::new_v4()));
                        crate::config::private_dir(&directory)?;
                        let lock = Arc::new(crate::config::lock_data_dir(&directory)?);
                        let seed_network = network.clone(); let state = state.clone(); let prelude = prelude.clone();
                        let actor = Arc::new(StoreActor::open(directory.join("node.sqlite3"), lock, move |store| {
                            store.install_trust_bundle(&seed_network, &state)?;
                            let result = store.ingest_events(&seed_network, &prelude)?;
                            anyhow::ensure!(result.statuses.iter().all(|status| *status == IngestStatus::Accepted), "synthetic prelude rejected");
                            super::super::log::test_metrics::reset();
                            Ok(())
                        }).await?);
                        let stop = Arc::new(AtomicBool::new(false));
                        let timer_stop = stop.clone();
                        let heartbeat = tokio::spawn(async move {
                            let mut samples = Vec::new();
                            let mut timer = tokio::time::interval(Duration::from_millis(10));
                            while !timer_stop.load(Ordering::Relaxed) {
                                let scheduled = timer.tick().await;
                                samples.push(tokio::time::Instant::now().saturating_duration_since(scheduled).as_nanos());
                            }
                            samples
                        });
                        let reader = actor.clone(); let read_stop = stop.clone(); let read_network = network.clone();
                        let snapshots = tokio::spawn(async move {
                            let mut samples = Vec::new();
                            while !read_stop.load(Ordering::Relaxed) {
                                let network = read_network.clone(); let started = Instant::now();
                                reader.read("benchmark_snapshot", move |store| store.heads(&network)).await?;
                                samples.push(started.elapsed().as_nanos());
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                            Ok::<_, anyhow::Error>(samples)
                        });
                        let stopper = actor.clone(); let admission_stop = stop.clone();
                        let admissions = tokio::spawn(async move {
                            let mut samples = Vec::new();
                            while !admission_stop.load(Ordering::Relaxed) {
                                let started = Instant::now();
                                stopper.call("benchmark_stop_admission", |store| {
                                    store.hold_stop_request(&crate::work::StopRequest {
                                        work_id: "synthetic-stop".into(), conversation: "c".into(), target: "synthetic-target".into(),
                                        requested_at_ms: 1, deadline_ms: i64::MAX, timed_out: false,
                                    })
                                }).await?;
                                samples.push(started.elapsed().as_nanos());
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                            Ok::<_, anyhow::Error>(samples)
                        });
                        let started = Instant::now(); let mut timings = Vec::new();
                        // A missing event from the second author holds at most 4096 events.
                        // Its one-event page releases a cascade larger than a normal ingest page.
                        for (group_index, group) in events.chunks(if cascade { page * 4 } else { page }).enumerate() {
                            let batches: Vec<_> = if cascade {
                                group.chunks(page).chain(std::iter::once(std::slice::from_ref(&dependencies[group_index]))).collect()
                            } else { vec![group] };
                            for batch in batches {
                                let network = network.clone(); let batch = batch.to_vec();
                                actor.call("benchmark_signed_ingest", move |store| {
                                    let ingested = store.ingest_events(&network, &batch)?;
                                    anyhow::ensure!(ingested.statuses.iter().all(|status| matches!(status,
                                        IngestStatus::Accepted | IngestStatus::Pending)), "synthetic ingest rejected");
                                    Ok(())
                                }).await?;
                                timings.extend(actor.take_timings());
                            }
                        }
                        let elapsed = started.elapsed();
                        stop.store(true, Ordering::Relaxed);
                        let heartbeats = heartbeat.await?;
                        let snapshot_samples = snapshots.await??;
                        let admission_samples = admissions.await??;
                        let phases = actor.call("benchmark_phases", |_| Ok(super::super::log::test_metrics::snapshot())).await?;
                        actor.shutdown().await?;
                        timings.extend(actor.take_timings());
                        eprintln!("workload=actor_signed_assistant size={size} page={page} cascade={cascade} repeat={repeat} elapsed_ns={} events_per_second={}", elapsed.as_nanos(), size as f64 / elapsed.as_secs_f64());
                        report("timer_drift", heartbeats);
                        report("snapshot_roundtrip", snapshot_samples);
                        report("stop_admission", admission_samples);
                        for operation in ["benchmark_signed_ingest", "benchmark_snapshot", "benchmark_stop_admission"] {
                            let selected: Vec<_> = timings.iter().filter(|sample| sample.operation == operation).collect();
                            report(&format!("{operation}.admission_wait"), selected.iter().map(|sample| sample.admission_wait.as_nanos()).collect());
                            report(&format!("{operation}.queue_wait"), selected.iter().map(|sample| sample.queue_wait.as_nanos()).collect());
                            report(&format!("{operation}.execute"), selected.iter().map(|sample| sample.execute.as_nanos()).collect());
                        }
                        report("ingest", phases.ingest_ns); report("validation", phases.validation_ns);
                        report("recompute", phases.recompute_ns); report("projection_rebuild", phases.rebuild_ns);
                        #[cfg(unix)] {
                            let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
                            // getrusage initializes the complete structure on success.
                            if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } == 0 {
                                let usage = unsafe { usage.assume_init() };
                                let bytes = if cfg!(target_os = "macos") { usage.ru_maxrss as u64 } else { usage.ru_maxrss as u64 * 1024 };
                                eprintln!("process_peak_resident_bytes={bytes}");
                            }
                        }
                        drop(actor);
                        std::fs::remove_dir_all(directory)?;
                    }
                }
            }
        }
        Ok(())
    }
}
