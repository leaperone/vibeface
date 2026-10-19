use super::*;
use futures_util::FutureExt;
use std::sync::{
    Mutex as StdMutex, Weak,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

const MAX_SYNC_JOBS: usize = 128;
const BACKGROUND_JOBS: usize = MAX_SYNC_JOBS - 4;
const MAX_WAITERS: usize = 64;
const MAX_DESCRIPTOR_BYTES: usize = 32 * 1024 * 1024;
const METADATA_BYTES: usize = 64 * 1024 * 1024;
const BACKGROUND_METADATA_BYTES: usize = 48 * 1024 * 1024;
const RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadClass {
    Background,
    Proof,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum SyncKey {
    Inventory(String, String),
    Scope(String, String),
    Proof(String, String, String, String),
}

#[derive(Default)]
struct MetadataPool {
    total: AtomicUsize,
    background: AtomicUsize,
}

struct MetadataCharge {
    pool: Arc<MetadataPool>,
    bytes: usize,
    class: ReadClass,
}
impl MetadataCharge {
    fn grow(&mut self, bytes: usize) -> Result<()> {
        let mut extra = self.pool.charge(bytes, self.class)?;
        self.bytes += extra.bytes;
        extra.bytes = 0;
        Ok(())
    }
}
impl Drop for MetadataCharge {
    fn drop(&mut self) {
        self.pool.total.fetch_sub(self.bytes, Ordering::AcqRel);
        if self.class == ReadClass::Background {
            self.pool.background.fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}
impl MetadataPool {
    fn charge(self: &Arc<Self>, bytes: usize, class: ReadClass) -> Result<MetadataCharge> {
        if class == ReadClass::Background {
            self.background
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                    used.checked_add(bytes)
                        .filter(|next| *next <= BACKGROUND_METADATA_BYTES)
                })
                .map_err(|_| RequestFailure::Busy)?;
        }
        if self
            .total
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= METADATA_BYTES)
            })
            .is_err()
        {
            if class == ReadClass::Background {
                self.background.fetch_sub(bytes, Ordering::AcqRel);
            }
            return Err(RequestFailure::Busy.into());
        }
        Ok(MetadataCharge {
            pool: self.clone(),
            bytes,
            class,
        })
    }
}

pub(super) struct SyncSpec {
    key: SyncKey,
    pub(super) network: String,
    pub(super) scope: String,
    pub(super) sources: Vec<String>,
    pub(super) roots: Box<[String]>,
    pub(super) class: ReadClass,
    pub(super) event_budget: usize,
    once: bool,
    _charge: MetadataCharge,
}

#[derive(Clone)]
enum SyncFailure {
    Busy,
    Error(String),
}
impl SyncFailure {
    fn from_error(error: anyhow::Error) -> Self {
        if error.downcast_ref::<RequestFailure>() == Some(&RequestFailure::Busy) {
            Self::Busy
        } else {
            Self::Error(format!("{error:#}"))
        }
    }
    fn error(&self) -> anyhow::Error {
        match self {
            Self::Busy => RequestFailure::Busy.into(),
            Self::Error(error) => anyhow::anyhow!(error.clone()),
        }
    }
}

#[derive(Clone)]
pub(super) struct InventoryScope {
    pub scope: String,
    pub heads: Vec<String>,
    error: Option<String>,
}
enum SyncOutput {
    Inventory(Vec<InventoryScope>),
    Closure(ClosureSync),
}
struct SyncResult {
    output: std::result::Result<SyncOutput, SyncFailure>,
    _charge: MetadataCharge,
}
#[derive(Clone)]
enum SyncUpdate {
    Queued,
    Running,
    Finished(Arc<SyncResult>),
}

fn strings_bytes(strings: &Vec<String>) -> usize {
    strings.capacity() * std::mem::size_of::<String>()
        + strings.iter().map(String::capacity).sum::<usize>()
}
fn inventory_bytes(scopes: &Vec<InventoryScope>) -> usize {
    scopes.capacity() * std::mem::size_of::<InventoryScope>()
        + scopes
            .iter()
            .map(|scope| {
                scope.scope.capacity()
                    + strings_bytes(&scope.heads)
                    + scope.error.as_ref().map_or(0, String::capacity)
            })
            .sum::<usize>()
}

struct InventoryWork {
    scopes: Vec<InventoryScope>,
    next: usize,
    _charge: MetadataCharge,
}
struct Entry {
    inventory: Option<InventoryWork>,
    processed: usize,
    changed: bool,
    id: u64,
    revision: u64,
    source_cursor: usize,
    ready_at: tokio::time::Instant,
    spec: Arc<SyncSpec>,
    updates: watch::Sender<SyncUpdate>,
}
#[derive(Default)]
struct Queue {
    next_id: u64,
    entries: HashMap<SyncKey, Entry>,
    ready: VecDeque<SyncKey>,
    running: BTreeSet<u64>,
    started: bool,
}

#[derive(Default)]
struct ActiveReads {
    accounting: StdMutex<()>,
    total: AtomicUsize,
    background: AtomicUsize,
    bytes: AtomicUsize,
}
pub(super) struct ResponseLease {
    _bytes: Option<OwnedSemaphorePermit>,
    bytes: usize,
    active: Arc<ActiveReads>,
}
impl Drop for ResponseLease {
    fn drop(&mut self) {
        let _accounting = self
            .active
            .accounting
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        drop(self._bytes.take());
        let bytes = self.active.bytes.fetch_sub(self.bytes, Ordering::AcqRel) - self.bytes;
        tracing::debug!(
            bytes,
            limit = RESPONSE_BYTES,
            "history response quota released"
        );
    }
}
struct RpcLease {
    _global: Option<OwnedSemaphorePermit>,
    _source: Option<OwnedSemaphorePermit>,
    _background: Option<OwnedSemaphorePermit>,
    class: ReadClass,
    active: Arc<ActiveReads>,
    network: String,
    source: String,
}
impl Drop for RpcLease {
    fn drop(&mut self) {
        let _accounting = self
            .active
            .accounting
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        drop(self._source.take());
        drop(self._global.take());
        drop(self._background.take());
        let active = self.active.total.fetch_sub(1, Ordering::AcqRel) - 1;
        if self.class == ReadClass::Background {
            self.active.background.fetch_sub(1, Ordering::AcqRel);
        }
        tracing::debug!(network=%self.network, source=%self.source, ?self.class, active, source_active=0, "history rpc quota released");
    }
}

pub(super) struct HistoryPage {
    pub events: Vec<SignedEvent>,
    pub _reply: Option<HistoryReply>,
}
impl std::ops::Deref for HistoryPage {
    type Target = [SignedEvent];
    fn deref(&self) -> &Self::Target {
        &self.events
    }
}

pub(super) struct HistoryReply {
    pub value: Value,
    pub _lease: ResponseLease,
    _rpc: RpcLease,
}

pub(super) struct SyncCoordinator {
    queue: StdMutex<Queue>,
    changed: Notify,
    metadata: Arc<MetadataPool>,
    rpc: Arc<Semaphore>,
    background_rpc: Arc<Semaphore>,
    response: Arc<Semaphore>,
    sources: StdMutex<HashMap<(String, String), Weak<Semaphore>>>,
    active: Arc<ActiveReads>,
    inventory_cursor: AtomicUsize,
}
impl Default for SyncCoordinator {
    fn default() -> Self {
        Self {
            queue: StdMutex::new(Queue::default()),
            changed: Notify::new(),
            metadata: Arc::new(MetadataPool::default()),
            rpc: Arc::new(Semaphore::new(4)),
            background_rpc: Arc::new(Semaphore::new(3)),
            response: Arc::new(Semaphore::new(RESPONSE_BYTES)),
            sources: StdMutex::new(HashMap::new()),
            active: Arc::new(ActiveReads::default()),
            inventory_cursor: AtomicUsize::new(0),
        }
    }
}

impl SyncCoordinator {
    fn make_spec(
        &self,
        key: SyncKey,
        sources: Vec<String>,
        mut roots: Vec<String>,
        event_budget: usize,
        once: bool,
    ) -> Result<Arc<SyncSpec>> {
        roots.sort();
        roots.dedup();
        let (network, scope, class) = match &key {
            SyncKey::Inventory(network, _) => (network.as_str(), "", ReadClass::Background),
            SyncKey::Scope(network, scope) => {
                (network.as_str(), scope.as_str(), ReadClass::Background)
            }
            SyncKey::Proof(network, scope, ..) => {
                (network.as_str(), scope.as_str(), ReadClass::Proof)
            }
        };
        ensure!(
            matches!(key, SyncKey::Inventory(..)) || (!scope.is_empty() && scope.len() <= 512),
            "invalid history scope"
        );
        let bytes = std::mem::size_of::<SyncSpec>()
            + network.len()
            + scope.len()
            + sources
                .iter()
                .map(|source| std::mem::size_of::<String>() + source.capacity())
                .sum::<usize>()
            + roots
                .iter()
                .map(|root| std::mem::size_of::<String>() + root.capacity())
                .sum::<usize>()
            + (sources.capacity() - sources.len()) * std::mem::size_of::<String>()
            + 4096;
        ensure!(
            bytes <= MAX_DESCRIPTOR_BYTES,
            "sync descriptor exceeds bounded storage"
        );
        let charge = self.metadata.charge(bytes, class)?;
        Ok(Arc::new(SyncSpec {
            network: network.into(),
            scope: scope.into(),
            key,
            sources,
            roots: roots.into_boxed_slice(),
            class,
            event_budget,
            once,
            _charge: charge,
        }))
    }

    fn register(
        &self,
        queue: &mut Queue,
        spec: Arc<SyncSpec>,
    ) -> Result<watch::Receiver<SyncUpdate>> {
        if let Some(entry) = queue.entries.get_mut(&spec.key) {
            ensure!(
                entry.updates.receiver_count() < MAX_WAITERS,
                RequestFailure::Busy
            );
            if matches!(spec.key, SyncKey::Proof(..)) {
                ensure!(
                    entry.spec.roots == spec.roots,
                    "sync roots digest collision"
                );
            } else if matches!(spec.key, SyncKey::Scope(..)) {
                let mut roots = entry.spec.roots.to_vec();
                roots.extend(spec.roots.iter().cloned());
                roots.sort();
                roots.dedup();
                let mut sources = entry.spec.sources.clone();
                for source in &spec.sources {
                    if !sources.contains(source) {
                        sources.push(source.clone());
                    }
                }
                if roots.as_slice() != entry.spec.roots.as_ref() || sources != entry.spec.sources {
                    entry.spec = self.make_spec(
                        spec.key.clone(),
                        sources,
                        roots,
                        entry.spec.event_budget,
                        entry.spec.once,
                    )?;
                    entry.revision += 1;
                }
            }
            tracing::debug!(network=%spec.network, scope=%spec.scope, flight_id=entry.id, waiters=entry.updates.receiver_count()+1,
                kind=?spec.key, "history sync flight joined");
            return Ok(entry.updates.subscribe());
        }
        let inventories = queue
            .entries
            .keys()
            .filter(|key| matches!(key, SyncKey::Inventory(..)))
            .count();
        ensure!(
            !matches!(spec.key, SyncKey::Inventory(..)) || inventories < 4,
            RequestFailure::Busy
        );
        let background = queue
            .entries
            .values()
            .filter(|entry| entry.spec.class == ReadClass::Background)
            .count();
        ensure!(
            queue.entries.len() < MAX_SYNC_JOBS
                && (spec.class == ReadClass::Proof || background < BACKGROUND_JOBS),
            RequestFailure::Busy
        );
        queue.next_id += 1;
        let id = queue.next_id;
        let key = spec.key.clone();
        let (updates, receiver) = watch::channel(SyncUpdate::Queued);
        tracing::debug!(network=%spec.network, scope=%spec.scope, flight_id=id, jobs=queue.entries.len()+1,
            limit=MAX_SYNC_JOBS, metadata_bytes=self.metadata.total.load(Ordering::Acquire), kind=?spec.key, "history sync flight created");
        queue.entries.insert(
            key.clone(),
            Entry {
                inventory: None,
                processed: 0,
                changed: false,
                id,
                revision: 0,
                source_cursor: 0,
                ready_at: tokio::time::Instant::now(),
                spec,
                updates,
            },
        );
        queue.ready.push_back(key);
        Ok(receiver)
    }

    fn try_join(
        &self,
        key: &SyncKey,
        sources: &[String],
        roots: &[String],
    ) -> Result<Option<watch::Receiver<SyncUpdate>>> {
        let queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        self.join_existing(&queue, key, sources, roots)
    }

    fn join_existing(
        &self,
        queue: &Queue,
        key: &SyncKey,
        sources: &[String],
        roots: &[String],
    ) -> Result<Option<watch::Receiver<SyncUpdate>>> {
        let Some(entry) = queue.entries.get(key) else {
            return Ok(None);
        };
        let included = match key {
            SyncKey::Inventory(..) => true,
            SyncKey::Proof(..) => {
                ensure!(
                    entry.spec.roots.as_ref() == roots,
                    "sync roots digest collision"
                );
                true
            }
            SyncKey::Scope(..) => {
                roots
                    .iter()
                    .all(|root| entry.spec.roots.binary_search(root).is_ok())
                    && sources
                        .iter()
                        .all(|source| entry.spec.sources.contains(source))
            }
        };
        if !included {
            return Ok(None);
        }
        ensure!(
            entry.updates.receiver_count() < MAX_WAITERS,
            RequestFailure::Busy
        );
        tracing::debug!(network=%entry.spec.network,scope=%entry.spec.scope,flight_id=entry.id,
            waiters=entry.updates.receiver_count()+1,kind=?key,"history sync flight joined");
        Ok(Some(entry.updates.subscribe()))
    }

    fn submit(
        self: &Arc<Self>,
        node: &Arc<Runtime>,
        spec: Arc<SyncSpec>,
    ) -> Result<watch::Receiver<SyncUpdate>> {
        let _work = node.maintenance.enter()?;
        let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        let receiver = self.register(&mut queue, spec)?;
        if !queue.started {
            queue.started = true;
            let scheduler = self.clone();
            let runtime = Arc::downgrade(node);
            tokio::spawn(async move {
                scheduler.run(runtime).await;
            });
        }
        drop(queue);
        self.changed.notify_one();
        Ok(receiver)
    }

    fn finish(&self, key: &SyncKey, output: Result<SyncOutput>) {
        let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        let Some(entry) = queue.entries.remove(key) else {
            return;
        };
        queue.running.remove(&entry.id);
        let mut output = output.map_err(SyncFailure::from_error);
        if let Ok(SyncOutput::Closure(report)) = &mut output {
            report.processed = report.processed.saturating_add(entry.processed);
            report.changed |= entry.changed;
        }
        let bytes = match &output {
            Ok(SyncOutput::Closure(report)) => {
                std::mem::size_of::<ClosureSync>()
                    + report.scope_id.capacity()
                    + report.target.capacity()
                    + strings_bytes(&report.missing.events)
                    + strings_bytes(&report.missing.conflicts)
                    + strings_bytes(&report.errors)
                    + report.missing.blobs.capacity() * std::mem::size_of::<BlobRef>()
                    + report
                        .missing
                        .blobs
                        .iter()
                        .map(|blob| blob.hash.capacity())
                        .sum::<usize>()
            }
            Ok(SyncOutput::Inventory(scopes)) => inventory_bytes(scopes),
            Err(SyncFailure::Error(error)) => error.capacity(),
            Err(SyncFailure::Busy) => 0,
        } + std::mem::size_of::<SyncResult>()
            + 64;
        let class = entry.spec.class;
        drop(entry.spec);
        let (output, charge) = match self.metadata.charge(bytes, class) {
            Ok(charge) => (output, charge),
            Err(_) => (
                Err(SyncFailure::Busy),
                self.metadata.charge(0, class).expect("zero charge"),
            ),
        };
        entry
            .updates
            .send_replace(SyncUpdate::Finished(Arc::new(SyncResult {
                output,
                _charge: charge,
            })));
        tracing::debug!(
            flight_id = entry.id,
            jobs = queue.entries.len(),
            "history sync flight finished"
        );
    }

    async fn run(self: Arc<Self>, runtime: Weak<Runtime>) {
        let mut running = tokio::task::JoinSet::new();
        loop {
            let notified = self.changed.notified();
            let mut inventories_done = Vec::new();
            let mut next_ready = tokio::time::Instant::now() + Duration::from_secs(5);
            {
                let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
                let attempts = queue.ready.len();
                for _ in 0..attempts {
                    if running.len() == 4 {
                        break;
                    }
                    let Some(key) = queue.ready.pop_front() else {
                        break;
                    };
                    let Some(mut entry) = queue.entries.remove(&key) else {
                        continue;
                    };
                    if entry.ready_at > tokio::time::Instant::now() {
                        next_ready = next_ready.min(entry.ready_at);
                        queue.entries.insert(key.clone(), entry);
                        queue.ready.push_back(key);
                        continue;
                    }
                    if let Some(mut inventory) = entry.inventory.take() {
                        if let Some(scope) = inventory.scopes.get(inventory.next) {
                            let child = scope;
                            queue.entries.insert(key.clone(), entry);
                            let parent = &queue.entries[&key];
                            let child_key =
                                SyncKey::Scope(parent.spec.network.clone(), child.scope.clone());
                            let result = if child.error.is_some() {
                                Ok(())
                            } else {
                                match self.join_existing(
                                    &queue,
                                    &child_key,
                                    &parent.spec.sources,
                                    &child.heads,
                                ) {
                                    Ok(Some(_)) => Ok(()),
                                    Ok(None) => self
                                        .make_spec(
                                            child_key,
                                            parent.spec.sources.clone(),
                                            child.heads.clone(),
                                            1024,
                                            false,
                                        )
                                        .and_then(|spec| {
                                            self.register(&mut queue, spec).map(|_| ())
                                        }),
                                    Err(error) => Err(error),
                                }
                            };
                            entry = queue
                                .entries
                                .remove(&key)
                                .expect("inventory owner retained");
                            match result {
                                Ok(()) => inventory.next += 1,
                                Err(error)
                                    if error.downcast_ref::<RequestFailure>()
                                        == Some(&RequestFailure::Busy) => {}
                                Err(error) => {
                                    let error = format!("{error:#}");
                                    if inventory._charge.grow(error.capacity()).is_ok() {
                                        inventory.scopes[inventory.next].error = Some(error);
                                        inventory.next += 1;
                                    }
                                }
                            }
                            entry.inventory = Some(inventory);
                            queue.entries.insert(key.clone(), entry);
                            queue.ready.push_back(key);
                        } else {
                            inventories_done.push((key.clone(), inventory.scopes));
                            queue.entries.insert(key, entry);
                        }
                        continue;
                    }
                    queue.entries.insert(key.clone(), entry);
                    let entry = &queue.entries[&key];
                    let background = queue
                        .entries
                        .values()
                        .filter(|entry| {
                            entry.spec.class == ReadClass::Background
                                && queue.running.contains(&entry.id)
                        })
                        .count();
                    if entry.spec.class == ReadClass::Background && background >= 3 {
                        queue.ready.push_back(key);
                        continue;
                    }
                    let spec = entry.spec.clone();
                    let id = entry.id;
                    let revision = entry.revision;
                    let cursor = entry.source_cursor;
                    entry.updates.send_replace(SyncUpdate::Running);
                    queue.running.insert(id);
                    let runtime = runtime.clone();
                    running.spawn(async move {
                        let result = std::panic::AssertUnwindSafe(async {
                            let node = runtime.upgrade().context("sync Runtime stopped")?;
                            let _work = node.maintenance.enter()?;
                            node.run_sync_step(spec, cursor).await
                        })
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("sync step panicked")));
                        (key, revision, result)
                    });
                }
                if running.is_empty() && queue.ready.is_empty() && inventories_done.is_empty() {
                    queue.started = false;
                    return;
                }
            }
            for (key, scopes) in inventories_done {
                self.finish(&key, Ok(SyncOutput::Inventory(scopes)));
            }
            if running.is_empty()
                && !self
                    .queue
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .ready
                    .is_empty()
            {
                next_ready = next_ready.min(tokio::time::Instant::now() + Duration::from_millis(5));
            }
            tokio::select! {
                done=running.join_next(), if !running.is_empty() => {
                    let Some(Ok((key,revision,result)))=done else { continue };
                    match result {
                        Ok(StepOutput::Yield(report)) => {
                            {
                            let mut queue=self.queue.lock().unwrap_or_else(|error| error.into_inner());
                            if let Some(entry)=queue.entries.get_mut(&key) {
                                entry.processed=entry.processed.saturating_add(report.processed);
                                entry.changed |= report.changed;
                                entry.source_cursor=entry.source_cursor.wrapping_add(1);
                                entry.ready_at=tokio::time::Instant::now()+Duration::from_millis(1);
                                let id=entry.id; entry.updates.send_replace(SyncUpdate::Queued);
                                tracing::debug!(flight_id=id,scope=%report.scope_id,processed=report.processed,
                                    missing_events=report.missing.events.len(),missing_blobs=report.missing.blobs.len(),"history sync flight yielded");
                                queue.running.remove(&id); queue.ready.push_back(key);
                            }
                            }
                            tokio::task::yield_now().await;
                        }
                        Ok(StepOutput::Done(SyncOutput::Inventory(scopes))) => {
                            let bytes=inventory_bytes(&scopes);
                            match self.metadata.charge(bytes,ReadClass::Background) {
                                Ok(charge)=>{
                                    let mut queue=self.queue.lock().unwrap_or_else(|error|error.into_inner());
                                    if let Some(entry)=queue.entries.get_mut(&key){
                                        entry.inventory=Some(InventoryWork{scopes,next:0,_charge:charge});
                                        let id=entry.id;queue.running.remove(&id);queue.ready.push_back(key);
                                    }
                                }
                                Err(error)=>self.finish(&key,Err(error)),
                            }
                        }
                        Ok(StepOutput::Done(output)) => {
                            let retry={
                                let mut queue=self.queue.lock().unwrap_or_else(|error| error.into_inner());
                                if let Some(entry)=queue.entries.get_mut(&key) {
                                    if entry.revision != revision {
                                        if let SyncOutput::Closure(report) = &output {
                                            entry.processed=entry.processed.saturating_add(report.processed);
                                            entry.changed |= report.changed;
                                        }
                                        let id=entry.id;queue.running.remove(&id);queue.ready.push_back(key.clone());true
                                    } else { false }
                                } else { false }
                            };
                            if !retry { self.finish(&key,Ok(output)); }
                        }
                        Err(error) if error.downcast_ref::<RequestFailure>()==Some(&RequestFailure::Busy)=>{
                            let mut queue=self.queue.lock().unwrap_or_else(|error|error.into_inner());
                            if let Some(entry)=queue.entries.get_mut(&key) {
                                entry.ready_at=tokio::time::Instant::now()+Duration::from_millis(1);
                                let id=entry.id;entry.updates.send_replace(SyncUpdate::Queued);
                                queue.running.remove(&id);queue.ready.push_back(key);
                            }
                        }
                        Err(error) => self.finish(&key,Err(error)),
                    }
                }
                _=notified => {},
                _=tokio::time::sleep_until(next_ready)=>{},
            }
        }
    }

    fn reserve_read(
        &self,
        network: &str,
        source: &str,
        class: ReadClass,
        bound: usize,
    ) -> Result<(RpcLease, ResponseLease)> {
        ensure!(
            bound <= crate::peer::MAX_PACKET_BYTES,
            "history response reservation too large"
        );
        let semaphore = {
            let mut sources = self
                .sources
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            sources.retain(|_, source| source.strong_count() != 0);
            let key = (network.to_owned(), source.to_owned());
            let semaphore = sources
                .get(&key)
                .and_then(Weak::upgrade)
                .unwrap_or_else(|| Arc::new(Semaphore::new(1)));
            sources.insert(key, Arc::downgrade(&semaphore));
            semaphore
        };
        let _accounting = self
            .active
            .accounting
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let source_permit = semaphore
            .try_acquire_owned()
            .map_err(|_| RequestFailure::Busy)?;
        let background = if class == ReadClass::Background {
            Some(
                self.background_rpc
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| RequestFailure::Busy)?,
            )
        } else {
            None
        };
        let global = self
            .rpc
            .clone()
            .try_acquire_owned()
            .map_err(|_| RequestFailure::Busy)?;
        let bytes = self
            .response
            .clone()
            .try_acquire_many_owned(bound.try_into()?)
            .map_err(|_| RequestFailure::Busy)?;
        let active = self.active.total.fetch_add(1, Ordering::AcqRel) + 1;
        if class == ReadClass::Background {
            self.active.background.fetch_add(1, Ordering::AcqRel);
        }
        let held = self.active.bytes.fetch_add(bound, Ordering::AcqRel) + bound;
        tracing::debug!(
            network,
            source,
            ?class,
            active,
            source_active = 1,
            bytes = held,
            rpc_limit = 4,
            source_limit = 1,
            byte_limit = RESPONSE_BYTES,
            "history rpc quota acquired"
        );
        Ok((
            RpcLease {
                _global: Some(global),
                _source: Some(source_permit),
                _background: background,
                class,
                active: self.active.clone(),
                network: network.into(),
                source: source.into(),
            },
            ResponseLease {
                _bytes: Some(bytes),
                bytes: bound,
                active: self.active.clone(),
            },
        ))
    }
}

enum StepOutput {
    Done(SyncOutput),
    Yield(ClosureSync),
}

async fn wait(mut updates: watch::Receiver<SyncUpdate>) -> Result<Arc<SyncResult>> {
    loop {
        if let SyncUpdate::Finished(result) = updates.borrow().clone() {
            return Ok(result);
        }
        updates
            .changed()
            .await
            .context("sync producer stopped without a result")?;
    }
}

impl Runtime {
    fn submit_sync(
        self: &Arc<Self>,
        key: SyncKey,
        sources: Vec<String>,
        roots: Vec<String>,
        budget: usize,
        once: bool,
    ) -> Result<watch::Receiver<SyncUpdate>> {
        let _work = self.maintenance.enter()?;
        if let Some(receiver) = self.sync.try_join(&key, &sources, &roots)? {
            return Ok(receiver);
        }
        let spec = self.sync.make_spec(key, sources, roots, budget, once)?;
        self.sync.clone().submit(self, spec)
    }

    pub(super) async fn read_history(
        &self,
        network: &str,
        source: &str,
        payload: Value,
        class: ReadClass,
        bound: usize,
    ) -> Result<HistoryReply> {
        self.store
            .lock()
            .await
            .authorize_node(network, source, Some(NodeRole::Replica))?;
        let (rpc, lease) = self.sync.reserve_read(network, source, class, bound)?;
        let value = tokio::time::timeout(
            Duration::from_secs(5),
            self.raw_peer_request(network, source, payload),
        )
        .await
        .map_err(|_| RequestFailure::Unknown)??;
        ensure!(
            serde_json::to_vec(&value)?.len() <= bound,
            "history response exceeds reserved wire bytes"
        );
        self.store
            .lock()
            .await
            .authorize_node(network, source, Some(NodeRole::Replica))?;
        Ok(HistoryReply {
            value,
            _lease: lease,
            _rpc: rpc,
        })
    }

    async fn run_sync_step(
        self: &Arc<Self>,
        spec: Arc<SyncSpec>,
        cursor: usize,
    ) -> Result<StepOutput> {
        self.membership_state().await?;
        match &spec.key {
            SyncKey::Inventory(_, source) => {
                let reply = self
                    .read_history(
                        &spec.network,
                        source,
                        json!({"type":"inventory"}),
                        ReadClass::Background,
                        crate::peer::MAX_PACKET_BYTES,
                    )
                    .await?;
                let state: NetworkState =
                    serde_json::from_value(reply.value["membership"].clone())?;
                ensure!(
                    state.genesis.network_id == spec.network,
                    "wrong membership network"
                );
                self.install_membership(state).await?;
                let mut scopes = Vec::new();
                for scope in reply.value["scopes"]
                    .as_array()
                    .context("invalid inventory")?
                {
                    let scope_id = field(scope, "scope_id").map(String::from);
                    let heads = serde_json::from_value::<Vec<String>>(scope["heads"].clone());
                    let error = scope_id
                        .as_ref()
                        .err()
                        .map(ToString::to_string)
                        .or_else(|| heads.as_ref().err().map(ToString::to_string));
                    scopes.push(InventoryScope {
                        scope: scope_id.unwrap_or_else(|_| "<invalid>".into()),
                        heads: heads.unwrap_or_default(),
                        error,
                    });
                }
                Ok(StepOutput::Done(SyncOutput::Inventory(scopes)))
            }
            SyncKey::Scope(..) | SyncKey::Proof(..) => {
                let (report, progress) = self.sync_step_core(&spec, cursor).await?;
                if !spec.once && report.state == SyncState::Partial && progress {
                    Ok(StepOutput::Yield(report))
                } else {
                    Ok(StepOutput::Done(SyncOutput::Closure(report)))
                }
            }
        }
    }

    fn enqueue_scope(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        sources: Vec<String>,
        mut roots: Vec<String>,
        once: bool,
        budget: usize,
    ) -> Result<watch::Receiver<SyncUpdate>> {
        ensure!(
            !scope.is_empty() && scope.len() <= 512,
            "invalid history scope"
        );
        roots.sort();
        roots.dedup();
        for root in &roots {
            ensure!(
                root.len() == 64 && hex::decode(root)?.len() == 32,
                "invalid history hash"
            );
        }
        self.submit_sync(
            SyncKey::Scope(network.into(), scope.into()),
            sources,
            roots,
            budget,
            once,
        )
    }

    pub(super) async fn request_scope_sync(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        roots: Vec<String>,
    ) -> Result<()> {
        self.require_role(network, NodeRole::Replica).await?;
        let sources = self
            .history_sources(network, &[])
            .await?
            .into_iter()
            .map(|source| source.node_id)
            .collect::<Vec<_>>();
        if !sources.is_empty() {
            self.enqueue_scope(network, scope, sources, roots, false, 1024)?;
        }
        Ok(())
    }

    pub(super) async fn enqueue_sync_all(self: &Arc<Self>, network: &str) -> Result<()> {
        self.require_role(network, NodeRole::Replica).await?;
        let mut sources = self.history_sources(network, &[]).await?;
        if !sources.is_empty() {
            let length = sources.len();
            let cursor = self.sync.inventory_cursor.fetch_add(4, Ordering::AcqRel);
            sources.rotate_left(cursor % length);
        }
        for source in sources.into_iter().take(4) {
            let _ = self.submit_sync(
                SyncKey::Inventory(network.into(), source.node_id.clone()),
                vec![source.node_id],
                vec![],
                1024,
                true,
            );
        }
        Ok(())
    }

    pub async fn sync_all(self: &Arc<Self>, network: &str) -> Result<SyncReport> {
        let _work = self.maintenance.enter()?;
        self.require_role(network, NodeRole::Replica).await?;
        let sources = self.history_sources(network, &[]).await?;
        let count = sources.len();
        let mut scopes = Vec::new();
        let mut unreachable_sources = Vec::new();
        for source in sources {
            match self.sync_from(network, &source.node_id).await {
                Ok(reports) => scopes.extend(reports),
                Err(_) => unreachable_sources.push(source.node_id),
            }
        }
        let state = if count == 0 || unreachable_sources.len() == count {
            SyncState::Unreachable
        } else if !unreachable_sources.is_empty()
            || scopes
                .iter()
                .any(|scope| scope.state != SyncState::Complete)
        {
            SyncState::Partial
        } else {
            SyncState::Complete
        };
        Ok(SyncReport {
            state,
            scopes,
            unreachable_sources,
        })
    }

    pub(super) async fn sync_from(
        self: &Arc<Self>,
        network: &str,
        target: &str,
    ) -> Result<Vec<ClosureSync>> {
        let updates = self.submit_sync(
            SyncKey::Inventory(network.into(), target.into()),
            vec![target.into()],
            vec![],
            1024,
            true,
        )?;
        let result = wait(updates).await?;
        let scopes = match &result.output {
            Ok(SyncOutput::Inventory(scopes)) => scopes,
            Err(error) => return Err(error.error()),
            _ => bail!("invalid inventory result"),
        };
        let mut reports = Vec::new();
        for scope in scopes {
            let result = if let Some(error) = &scope.error {
                Err(anyhow::anyhow!(error.clone()))
            } else {
                match self.enqueue_scope(
                    network,
                    &scope.scope,
                    vec![target.into()],
                    scope.heads.clone(),
                    false,
                    1024,
                ) {
                    Ok(updates) => match wait(updates).await {
                        Ok(result) => match &result.output {
                            Ok(SyncOutput::Closure(report)) => Ok(report.clone()),
                            Err(error) => Err(error.error()),
                            _ => Err(anyhow::anyhow!("invalid scope result")),
                        },
                        Err(error) => Err(error),
                    },
                    Err(error) => Err(error),
                }
            };
            let mut report = result.unwrap_or_else(|error| ClosureSync {
                scope_id: scope.scope.clone(),
                target: target.into(),
                state: SyncState::Partial,
                missing: MissingDependencies::default(),
                processed: 0,
                changed: false,
                errors: vec![format!("{error:#}")],
            });
            report.target = target.into();
            reports.push(report);
        }
        Ok(reports)
    }

    pub(super) async fn fetch_closure(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        target: &str,
        wanted: &[String],
    ) -> Result<ClosureSync> {
        let mut roots = wanted.to_vec();
        roots.sort();
        roots.dedup();
        if roots.is_empty() {
            roots = self.store.lock().await.heads(network, scope)?;
        }
        for root in &roots {
            ensure!(
                root.len() == 64 && hex::decode(root)?.len() == 32,
                "invalid history hash"
            );
        }
        let digest = crypto::digest(&serde_json::to_vec(&roots)?);
        let updates = self.submit_sync(
            SyncKey::Proof(network.into(), scope.into(), target.into(), digest),
            vec![target.into()],
            roots,
            1024,
            false,
        )?;
        let result = wait(updates).await?;
        match &result.output {
            Ok(SyncOutput::Closure(report)) => Ok(report.clone()),
            Err(error) => Err(error.error()),
            _ => bail!("invalid proof result"),
        }
    }

    #[cfg(test)]
    pub(super) async fn fetch_scope(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        target: &str,
        wanted: &[String],
    ) -> Result<ClosureSync> {
        self.fetch_closure_batch(network, scope, target, wanted, 1024)
            .await
    }
    #[cfg(test)]
    pub(super) async fn fetch_closure_batch(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        target: &str,
        wanted: &[String],
        budget: usize,
    ) -> Result<ClosureSync> {
        let updates = self.enqueue_scope(
            network,
            scope,
            vec![target.into()],
            wanted.to_vec(),
            true,
            budget,
        )?;
        let result = wait(updates).await?;
        match &result.output {
            Ok(SyncOutput::Closure(report)) => Ok(report.clone()),
            Err(error) => Err(error.error()),
            _ => bail!("invalid scope result"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m5_descriptor_accepts_the_largest_legal_roots_representation() -> Result<()> {
        let scheduler = SyncCoordinator::default();
        let count = (vibeface_secure::MAX_APPLICATION_MESSAGE - 1) / 67;
        let roots = (0..count)
            .map(|index| format!("{index:064x}"))
            .collect::<Vec<_>>();
        let spec = scheduler.make_spec(
            SyncKey::Proof(
                "network".into(),
                "chat".into(),
                "source".into(),
                "digest".into(),
            ),
            vec!["source".into()],
            roots,
            1024,
            false,
        )?;
        assert_eq!(spec.roots.len(), count);
        assert!(spec._charge.bytes > 2 * 1024 * 1024);
        assert!(spec._charge.bytes < MAX_DESCRIPTOR_BYTES);
        assert_eq!(
            scheduler.metadata.total.load(Ordering::Acquire),
            spec._charge.bytes
        );
        let descriptor_bytes = spec._charge.bytes;
        drop(spec);
        assert_eq!(scheduler.metadata.total.load(Ordering::Acquire), 0);
        println!(
            "NET1_M5 largest_16mib_wire_roots_fit_descriptor=true roots={count} descriptor_bytes={descriptor_bytes} descriptor_limit={MAX_DESCRIPTOR_BYTES} no_arbitrary_heads_limit=true"
        );
        Ok(())
    }

    #[test]
    fn m5_retained_errors_and_spare_capacity_remain_charged() -> Result<()> {
        let scheduler = SyncCoordinator::default();
        let key = SyncKey::Inventory("network".into(), "source".into());
        let spec = scheduler.make_spec(key.clone(), vec!["source".into()], vec![], 1024, true)?;
        let updates = scheduler.register(&mut scheduler.queue.lock().unwrap(), spec)?;
        scheduler.finish(&key, Err(anyhow::anyhow!("e".repeat(128 * 1024))));
        let result = match updates.borrow().clone() {
            SyncUpdate::Finished(result) => result,
            _ => panic!("missing finished error"),
        };
        assert!(result._charge.bytes >= 128 * 1024);
        assert_eq!(
            scheduler.metadata.total.load(Ordering::Acquire),
            result._charge.bytes
        );
        drop(updates);
        drop(result);
        assert_eq!(scheduler.metadata.total.load(Ordering::Acquire), 0);
        let compact = scheduler.make_spec(
            SyncKey::Scope("network".into(), "chat".into()),
            vec!["source".into()],
            vec![],
            1024,
            false,
        )?;
        let mut sources = Vec::with_capacity(64);
        sources.push("source".into());
        let spare = scheduler.make_spec(
            SyncKey::Scope("network".into(), "chat".into()),
            sources,
            vec![],
            1024,
            false,
        )?;
        assert_eq!(
            spare._charge.bytes - compact._charge.bytes,
            63 * std::mem::size_of::<String>()
        );
        drop(compact);
        drop(spare);
        let key = SyncKey::Scope("network".into(), "chat".into());
        let sources = vec!["source".into()];
        let roots = vec!["1".repeat(64), "0".repeat(64), "1".repeat(64)];
        let spec = scheduler.make_spec(key.clone(), sources.clone(), roots.clone(), 1024, false)?;
        assert_eq!(spec.roots.as_ref(), &["0".repeat(64), "1".repeat(64)]);
        let mut queue = scheduler.queue.lock().unwrap();
        let first = scheduler.register(&mut queue, spec)?;
        let fill = scheduler.metadata.charge(
            METADATA_BYTES - scheduler.metadata.total.load(Ordering::Acquire),
            ReadClass::Proof,
        )?;
        let position = queue.ready.front().cloned();
        let joined = scheduler
            .join_existing(&queue, &key, &sources, &roots)?
            .context("included inventory child joins at full metadata")?;
        assert_eq!(queue.entries.len(), 1);
        assert_eq!(queue.entries[&key].revision, 0);
        assert_eq!(queue.ready.front().cloned(), position);
        assert_eq!(
            scheduler.metadata.total.load(Ordering::Acquire),
            METADATA_BYTES
        );
        drop(joined);
        drop(first);
        drop(fill);
        queue.entries.clear();
        queue.ready.clear();
        drop(queue);
        assert_eq!(scheduler.metadata.total.load(Ordering::Acquire), 0);
        let mut heads = Vec::with_capacity(32);
        heads.push("0".repeat(64));
        let mut scopes = Vec::with_capacity(8);
        let error = String::with_capacity(8192);
        scopes.push(InventoryScope {
            scope: "chat".into(),
            heads,
            error: Some(error),
        });
        assert!(
            inventory_bytes(&scopes)
                >= 8192
                    + 32 * std::mem::size_of::<String>()
                    + 8 * std::mem::size_of::<InventoryScope>()
        );
        let mut charge = scheduler
            .metadata
            .charge(inventory_bytes(&scopes), ReadClass::Background)?;
        let before = scheduler.metadata.total.load(Ordering::Acquire);
        let error = "invalid descriptor".repeat(4096);
        charge.grow(error.capacity())?;
        scopes[0].error = Some(error);
        assert_eq!(
            scheduler.metadata.total.load(Ordering::Acquire),
            before + scopes[0].error.as_ref().unwrap().capacity()
        );
        drop(charge);
        assert_eq!(scheduler.metadata.total.load(Ordering::Acquire), 0);
        println!(
            "NET1_M5 retained_128k_error_charged=true inventory_error_capacity_and_vector_spare_charged=true source_spare_capacity_charged=true inventory_existing_child_joins_at_full_metadata=true final_receiver_drop_releases_charge=true"
        );
        Ok(())
    }

    #[test]
    fn m5_history_quota_reserves_proof_and_holds_bytes_after_rpc_release() -> Result<()> {
        let scheduler = SyncCoordinator::default();
        let mut ordinary = Vec::new();
        for index in 0..3 {
            ordinary.push(scheduler.reserve_read(
                "network",
                &format!("source-{index}"),
                ReadClass::Background,
                crate::peer::MAX_PACKET_BYTES,
            )?);
        }
        assert!(
            scheduler
                .reserve_read("network", "source-3", ReadClass::Background, 1024)
                .is_err()
        );
        assert!(
            scheduler
                .reserve_read("network", "source-0", ReadClass::Proof, 1024)
                .is_err()
        );
        let (proof, response) = scheduler.reserve_read(
            "network",
            "source-3",
            ReadClass::Proof,
            crate::peer::MAX_PACKET_BYTES,
        )?;
        assert_eq!(scheduler.active.total.load(Ordering::Acquire), 4);
        assert_eq!(
            scheduler.active.bytes.load(Ordering::Acquire),
            RESPONSE_BYTES
        );
        drop(proof);
        assert_eq!(scheduler.active.total.load(Ordering::Acquire), 3);
        assert!(
            scheduler
                .reserve_read("network", "source-4", ReadClass::Proof, 1024)
                .is_err()
        );
        drop(response);
        let fourth = scheduler.reserve_read(
            "network",
            "source-4",
            ReadClass::Proof,
            crate::peer::MAX_PACKET_BYTES,
        )?;
        assert_eq!(scheduler.active.total.load(Ordering::Acquire), 4);
        drop(fourth);
        drop(ordinary);
        assert_eq!(scheduler.active.total.load(Ordering::Acquire), 0);
        assert_eq!(scheduler.active.bytes.load(Ordering::Acquire), 0);
        assert_eq!(scheduler.response.available_permits(), RESPONSE_BYTES);
        println!(
            "NET1_M5 history_rpc_limit=4 background_limit=3 per_source_limit=1 response_limit=8388608 proof_reserved_positive=true response_resident_after_rpc_drop=true all_permits_released=true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn m5_real_noise_scope_join_cancelled_proof_quantum_and_inventory_window() -> Result<()> {
        use axum::{
            Router,
            extract::{State, WebSocketUpgrade},
            routing::get,
        };
        async fn upgrade(
            State(hub): State<Arc<PeerHub>>,
            ws: WebSocketUpgrade,
        ) -> impl axum::response::IntoResponse {
            ws.on_upgrade(|socket| async move {
                let _ = hub.accept(socket).await;
            })
        }
        let source_dir = crate::runtime::tests::fixture_under("network-reliability")?;
        let receiver_dir = crate::runtime::tests::fixture_under("network-reliability")?;
        let source = Arc::new(Runtime::open(&source_dir).await?);
        let receiver = Arc::new(Runtime::open(&receiver_dir).await?);
        let owner = LocalIdentity::generate();
        let membership = owner.sign_network_fixture(
            1,
            vec![
                source.identity.certificate(source.config.roles.clone())?,
                receiver
                    .identity
                    .certificate(receiver.config.roles.clone())?,
            ],
        )?;
        let network = membership.genesis.network_id.clone();
        source.install_membership(membership.clone()).await?;
        receiver.install_membership(membership).await?;
        let source_id = source.identity.node_id()?;
        let content = vec![0x43; 640 * 1024];
        let blob = BlobRef {
            hash: crypto::digest(&content),
            size: content.len() as u64,
        };
        let (head, ready_root) = {
            let mut store = source.store.lock().await;
            store.put_blob(&network, &blob, &content)?;
            let created = store.append_event(
                &source.identity,
                &network,
                "chat",
                EventBody::ConversationCreated {
                    title: "m5 live scope".into(),
                },
                vec![],
                1,
            )?;
            let message = store.append_event(
                &source.identity,
                &network,
                "chat",
                EventBody::MessagePosted {
                    message_id: "m5-message".into(),
                    role: "user".into(),
                    text: "local pending attachment".into(),
                    scheduler_node: None,
                },
                vec![blob.clone()],
                2,
            )?;
            let ready_root = created.hash.clone();
            receiver
                .store
                .lock()
                .await
                .ingest_events(&network, &[created, message.clone()])?;
            (message.hash, ready_root)
        };
        let mut tasks = tokio::task::JoinSet::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}/peer", listener.local_addr()?);
        let router = Router::new()
            .route("/peer", get(upgrade))
            .with_state(source.hub.clone());
        tasks.spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let hub = receiver.hub.clone();
        let scope = network.clone();
        tasks.spawn(async move {
            let _ = hub.connect(scope, url).await;
        });
        let gate = Arc::new(Semaphore::new(0));
        let first_blob = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let requests = Arc::new(Mutex::new(Vec::<(String, u64)>::new()));
        let mut inbound = source
            .peer_rx
            .lock()
            .await
            .take()
            .context("missing source receiver")?;
        let responder = source.clone();
        let observed = requests.clone();
        let server_gate = gate.clone();
        let (entered, blob_entered) = oneshot::channel();
        tasks.spawn(async move {
            let mut entered = Some(entered);
            while let Some(message) = inbound.recv().await {
                if message.payload["type"] == "blob" {
                    observed.lock().await.push((
                        message.payload["hash"].as_str().unwrap().into(),
                        message.payload["offset"].as_u64().unwrap(),
                    ));
                    if first_blob.swap(false, Ordering::AcqRel) {
                        if let Some(entered) = entered.take() {
                            let _ = entered.send(());
                        }
                        server_gate.acquire().await.unwrap().forget();
                    }
                }
                let mut reply = responder
                    .handle_peer(&message)
                    .await
                    .unwrap_or_else(|error| json!({"error":format!("{error:#}")}));
                if message.payload["type"] == "inventory" {
                    let scopes = reply["scopes"].as_array_mut().unwrap();
                    for index in 0..128 {
                        scopes.insert(
                            index,
                            json!({"scope_id":format!("empty-{index:03}"),"heads":[]}),
                        );
                    }
                }
                let _ = responder.hub.respond(&message, reply).await;
            }
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !receiver.hub.online(&network).await.contains(&source_id) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let cancelled_node = receiver.clone();
        let cancelled_network = network.clone();
        let cancelled_source = source_id.clone();
        let cancelled_head = head.clone();
        let cancelled = tokio::spawn(async move {
            cancelled_node
                .fetch_closure(
                    &cancelled_network,
                    "chat",
                    &cancelled_source,
                    &[cancelled_head],
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), blob_entered).await??;
        println!("M5_STAGE first proof blob entered");
        let checkpoint = format!("net1:v1:closure:{}", crypto::digest(b"chat"));
        assert!(
            receiver
                .store
                .lock()
                .await
                .local_value(&network, &checkpoint)?
                .is_none()
        );
        let remaining_metadata =
            METADATA_BYTES - receiver.sync.metadata.total.load(Ordering::Acquire);
        let metadata_fill = receiver
            .sync
            .metadata
            .charge(remaining_metadata, ReadClass::Proof)?;
        let survivor_node = receiver.clone();
        let survivor_network = network.clone();
        let survivor_source = source_id.clone();
        let survivor_head = head.clone();
        let survivor = tokio::spawn(async move {
            survivor_node
                .fetch_closure(
                    &survivor_network,
                    "chat",
                    &survivor_source,
                    &[survivor_head],
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let joined = receiver
                    .sync
                    .queue
                    .lock()
                    .unwrap()
                    .entries
                    .values()
                    .any(|entry| {
                        matches!(entry.spec.key, SyncKey::Proof(..))
                            && entry.updates.receiver_count() == 2
                    });
                if joined {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(
            receiver.sync.metadata.total.load(Ordering::Acquire),
            METADATA_BYTES
        );
        drop(metadata_fill);
        let independent = tokio::time::timeout(
            Duration::from_secs(1),
            receiver.fetch_closure(&network, "chat", &source_id, &[ready_root]),
        )
        .await??;
        independent.require_complete()?;
        assert!(!survivor.is_finished());
        assert!(!cancelled.is_finished());
        assert_eq!(requests.lock().await.len(), 1);
        assert_eq!(
            receiver
                .sync
                .queue
                .lock()
                .unwrap()
                .entries
                .keys()
                .filter(|key| matches!(key, SyncKey::Proof(..)))
                .count(),
            1
        );
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        let access = Access {
            network_id: network.clone(),
            human: true,
            conversation_id: None,
            manager: None,
        };
        for _ in 0..3 {
            let snapshot = tokio::time::timeout(
                Duration::from_secs(1),
                receiver.conversation_snapshot(&access, &json!({"conversation_id":"chat"})),
            )
            .await??;
            assert!(!snapshot["missing"]["blobs"].as_array().unwrap().is_empty());
        }
        {
            let queue = receiver.sync.queue.lock().unwrap();
            assert_eq!(
                queue
                    .entries
                    .keys()
                    .filter(|key| matches!(key, SyncKey::Scope(..)))
                    .count(),
                1
            );
            assert_eq!(
                queue
                    .entries
                    .keys()
                    .filter(|key| matches!(key, SyncKey::Proof(..)))
                    .count(),
                1
            );
        }
        assert_eq!(requests.lock().await.len(), 1);
        println!("M5_STAGE snapshots returned; release first blob");
        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(10), survivor)
            .await??
            .context("m5 survivor proof")?
            .require_complete()?;
        println!("M5_STAGE proof complete");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !receiver.sync.queue.lock().unwrap().entries.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(
            receiver
                .store
                .lock()
                .await
                .get_blob(&network, &blob.hash)?
                .unwrap(),
            content
        );
        let offsets = requests
            .lock()
            .await
            .iter()
            .filter(|(hash, _)| hash == &blob.hash)
            .map(|(_, offset)| *offset)
            .collect::<Vec<_>>();
        assert_eq!(
            offsets,
            (0..10).map(|chunk| chunk * 65536).collect::<Vec<_>>()
        );
        assert!(
            receiver
                .store
                .lock()
                .await
                .local_value(&network, &checkpoint)?
                .unwrap()["frontier_generation"]
                .is_u64()
        );
        source.store.lock().await.append_event(
            &source.identity,
            &network,
            "scope-128",
            EventBody::ConversationCreated {
                title: "real signed tail scope".into(),
            },
            vec![],
            3,
        )?;
        println!("M5_STAGE inventory starting");
        let reports = tokio::time::timeout(
            Duration::from_secs(25),
            receiver.sync_from(&network, &source_id),
        )
        .await?
        .context("m5 inventory traversal")?;
        println!("M5_STAGE inventory complete");
        assert_eq!(reports.len(), 130);
        assert!(
            reports
                .iter()
                .all(|report| report.state == SyncState::Complete)
        );
        assert!(
            receiver
                .store
                .lock()
                .await
                .scopes(&network)?
                .contains(&"scope-128".to_owned())
        );
        assert_eq!(receiver.sync.active.total.load(Ordering::Acquire), 0);
        assert_eq!(receiver.sync.active.bytes.load(Ordering::Acquire), 0);
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        println!(
            "NET1_M5 real_noise=true same_scope_producer=1 proof_exact_roots_join=true different_exact_roots_complete_while_first_proof_blob_blocked=true join_at_full_metadata_capacity=true cancelled_waiter_did_not_cancel_producer=true proof_did_not_write_scope_checkpoint=true conversation_read_returned_while_blob_response_blocked=true blob_ranges=10 quantum_release_progress=true wire_inventory_scopes=130 legal_empty_scope_fixture=128 actual_signed_tail_scope_completed_without_background_tick=true"
        );
        Ok(())
    }
}
