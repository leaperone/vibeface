//! Resumable private prefixes. A blocking operation owns its writer and data-dir lock.
use super::*;
use crate::store::blob_transfer::{BlobTransfer, TransferPhase};
use std::{
    ffi::{CStr, CString},
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    sync::Weak,
};
use tokio::sync::OwnedMutexGuard;

const MAX_STAGING_BYTES: u64 = 4 * MAX_BLOB_BYTES as u64;
const MAX_STAGING_OBJECTS: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum BlobStep {
    Partial,
    Complete { changed: bool },
}
pub(super) struct BlobQuantum {
    pub remaining: usize,
}
impl BlobQuantum {
    pub fn new(chunks: usize) -> Self {
        Self { remaining: chunks }
    }
}
struct BlobChunk {
    bytes: Vec<u8>,
    response: HistoryReply,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct BlobKey {
    network: String,
    hash: String,
    size: u64,
}
impl BlobKey {
    fn new(network: &str, blob: &BlobRef) -> Result<Self> {
        BlobTransfer::key(blob)?;
        Ok(Self {
            network: network.into(),
            hash: blob.hash.clone(),
            size: blob.size,
        })
    }
    fn filename(&self) -> Result<CString> {
        Ok(CString::new(format!(
            "{}.part",
            crypto::digest(&serde_json::to_vec(&(
                &self.network,
                &self.hash,
                self.size
            ))?)
        ))?)
    }
}

#[derive(Default)]
struct CleanupCursor {
    after: Option<(String, String)>,
}

pub(super) struct BlobStaging {
    directory: File,
    writers: Mutex<HashMap<BlobKey, Weak<Mutex<()>>>>,
    reservation: Arc<Mutex<()>>,
    cleanup_pass: Arc<Mutex<CleanupCursor>>,
    _process_lock: Arc<File>,
}

struct BlobLease {
    key: BlobKey,
    staging: Arc<BlobStaging>,
    _guard: OwnedMutexGuard<()>,
}

struct BlobWriter {
    lease: BlobLease,
    part: File,
}

fn owned_fd(fd: libc::c_int) -> Result<File> {
    ensure!(
        fd >= 0,
        "private staging open failed: {}",
        std::io::Error::last_os_error()
    );
    // The successful open/openat call transfers this fresh descriptor exactly once.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn directory_entry(stream: *mut libc::DIR) -> Result<*mut libc::dirent> {
    // readdir distinguishes EOF from I/O failure through errno; stale errno is not meaningful.
    #[cfg(target_vendor = "apple")]
    unsafe {
        *libc::__error() = 0;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location() = 0;
    }
    let entry = unsafe { libc::readdir(stream) };
    if entry.is_null() {
        let error = std::io::Error::last_os_error();
        ensure!(
            error.raw_os_error() == Some(0),
            "read staging entries failed: {error}"
        );
    }
    Ok(entry)
}

impl BlobStaging {
    pub fn open(dir: &Path, process_lock: Arc<File>) -> Result<Arc<Self>> {
        let parent = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir)?;
        let metadata = parent.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "data directory is not private"
        );
        let name = CString::new("net1-blob-staging")?;
        let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
        if created != 0 {
            let error = std::io::Error::last_os_error();
            ensure!(
                error.kind() == std::io::ErrorKind::AlreadyExists,
                "create staging directory failed: {error}"
            );
        }
        let directory = owned_fd(unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        let child = directory.metadata()?;
        ensure!(
            child.is_dir() && child.uid() == metadata.uid() && child.mode() & 0o777 == 0o700,
            "staging directory has unexpected type, owner or permissions"
        );
        if created == 0 {
            parent.sync_all()?;
        }
        Ok(Arc::new(Self {
            directory,
            writers: Mutex::new(HashMap::new()),
            reservation: Arc::new(Mutex::new(())),
            cleanup_pass: Arc::new(Mutex::new(CleanupCursor::default())),
            _process_lock: process_lock,
        }))
    }
    async fn writer_lock(&self, key: &BlobKey) -> Arc<Mutex<()>> {
        let mut writers = self.writers.lock().await;
        writers.retain(|_, weak| weak.strong_count() > 0);
        if let Some(writer) = writers.get(key).and_then(Weak::upgrade) {
            return writer;
        }
        let writer = Arc::new(Mutex::new(()));
        writers.insert(key.clone(), Arc::downgrade(&writer));
        writer
    }
    async fn lease(self: &Arc<Self>, key: BlobKey) -> BlobLease {
        let guard = self.writer_lock(&key).await.lock_owned().await;
        BlobLease {
            key,
            staging: self.clone(),
            _guard: guard,
        }
    }
    async fn try_lease(self: &Arc<Self>, key: BlobKey) -> Option<BlobLease> {
        let guard = self.writer_lock(&key).await.try_lock_owned().ok()?;
        Some(BlobLease {
            key,
            staging: self.clone(),
            _guard: guard,
        })
    }
}

impl BlobLease {
    fn open_part(&self) -> Result<File> {
        let name = self.key.filename()?;
        let part = owned_fd(unsafe {
            libc::openat(
                self.staging.directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
                0o600,
            )
        })?;
        let metadata = part.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == self.staging.directory.metadata()?.uid()
                && metadata.mode() & 0o777 == 0o600
                && metadata.nlink() == 1,
            "staging part has unexpected type, owner or permissions"
        );
        Ok(part)
    }
    async fn recover(self, offset: u64) -> Result<(BlobWriter, u64, u64)> {
        tokio::task::spawn_blocking(move || {
            let part = self.open_part()?;
            let length = part.metadata()?.len();
            let recovered = if length < offset { 0 } else { offset };
            if length != recovered {
                part.set_len(recovered)?;
            }
            part.sync_all()?;
            self.staging.directory.sync_all()?;
            let actual_length = part.metadata()?.len();
            ensure!(
                actual_length == recovered,
                "staging recovery length mismatch"
            );
            Ok((BlobWriter { lease: self, part }, recovered, actual_length))
        })
        .await?
    }
    async fn scan(
        self,
        reservation: OwnedMutexGuard<()>,
    ) -> Result<(Self, OwnedMutexGuard<()>, Vec<(Vec<u8>, u64)>)> {
        tokio::task::spawn_blocking(move || {
            let fd = owned_fd(unsafe {
                libc::openat(
                    self.staging.directory.as_raw_fd(),
                    c".".as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            })?;
            use std::os::fd::IntoRawFd;
            let raw = fd.into_raw_fd();
            let stream = unsafe { libc::fdopendir(raw) };
            if stream.is_null() {
                unsafe {
                    libc::close(raw);
                }
                bail!(
                    "open staging entries failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            struct Entries(*mut libc::DIR);
            impl Drop for Entries {
                fn drop(&mut self) {
                    unsafe {
                        libc::closedir(self.0);
                    }
                }
            }
            let entries = Entries(stream);
            let mut files = Vec::new();
            loop {
                let entry = directory_entry(entries.0)?;
                if entry.is_null() {
                    break;
                }
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                if name.to_bytes() == b"." || name.to_bytes() == b".." {
                    continue;
                }
                let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
                if unsafe {
                    libc::fstatat(
                        self.staging.directory.as_raw_fd(),
                        name.as_ptr(),
                        metadata.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                } != 0
                {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::NotFound {
                        continue;
                    }
                    return Err(error.into());
                }
                let metadata = unsafe { metadata.assume_init() };
                files.push((name.to_bytes().to_vec(), u64::try_from(metadata.st_size)?));
                ensure!(
                    files.len() <= MAX_STAGING_OBJECTS,
                    "staging object quota exceeded"
                );
            }
            Ok((self, reservation, files))
        })
        .await?
    }
    async fn unlink(
        self,
        pass: Option<OwnedMutexGuard<CleanupCursor>>,
    ) -> Result<(Option<OwnedMutexGuard<CleanupCursor>>, Result<Self>)> {
        tokio::task::spawn_blocking(move || {
            let result = (|| {
                let name = self.key.filename()?;
                // Validate an existing part without following links; absence is an idempotent cleanup.
                let descriptor = unsafe {
                    libc::openat(
                        self.staging.directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                    )
                };
                if descriptor >= 0 {
                    let file = owned_fd(descriptor)?;
                    let metadata = file.metadata()?;
                    ensure!(
                        metadata.is_file()
                            && metadata.uid() == self.staging.directory.metadata()?.uid()
                            && metadata.mode() & 0o777 == 0o600
                            && metadata.nlink() == 1,
                        "unsafe staging cleanup target"
                    );
                    if unsafe {
                        libc::unlinkat(self.staging.directory.as_raw_fd(), name.as_ptr(), 0)
                    } != 0
                    {
                        return Err(std::io::Error::last_os_error().into());
                    }
                } else {
                    let error = std::io::Error::last_os_error();
                    ensure!(
                        error.kind() == std::io::ErrorKind::NotFound,
                        "open cleanup target failed: {error}"
                    );
                }
                self.staging.directory.sync_all()?;
                test_boundary("unlinked");
                Ok(self)
            })();
            (pass, result)
        })
        .await
        .map_err(Into::into)
    }
}

impl BlobWriter {
    async fn write(
        mut self,
        offset: u64,
        bytes: Vec<u8>,
        response: Option<HistoryReply>,
    ) -> Result<(Self, u64, Option<HistoryReply>)> {
        tokio::task::spawn_blocking(move || {
            ensure!(
                self.part.metadata()?.len() == offset
                    && offset
                        .checked_add(bytes.len() as u64)
                        .is_some_and(|end| end <= self.lease.key.size),
                "staging write offset mismatch"
            );
            self.part.seek(SeekFrom::Start(offset))?;
            self.part.write_all(&bytes)?;
            self.part.sync_all()?;
            test_boundary("chunk_fsynced");
            let actual_length = self.part.metadata()?.len();
            Ok((self, actual_length, response))
        })
        .await?
    }
    async fn read(mut self) -> Result<(Self, Vec<u8>)> {
        tokio::task::spawn_blocking(move || {
            ensure!(
                self.part.metadata()?.len() == self.lease.key.size,
                "staging size mismatch"
            );
            self.part.seek(SeekFrom::Start(0))?;
            let mut bytes = vec![0; usize::try_from(self.lease.key.size)?];
            self.part.read_exact(&mut bytes)?;
            Ok((self, bytes))
        })
        .await?
    }
    async fn reset(self) -> Result<(Self, u64)> {
        tokio::task::spawn_blocking(move || {
            self.part.set_len(0)?;
            self.part.sync_all()?;
            let actual_length = self.part.metadata()?.len();
            Ok((self, actual_length))
        })
        .await?
    }
    fn into_lease(self) -> BlobLease {
        self.lease
    }
}

#[cfg(not(test))]
fn test_boundary(_: &str) {}

#[cfg(test)]
fn test_boundary(stage: &str) {
    if std::env::var("NET1_TEST_CRASH_STAGE").as_deref() == Ok(stage) {
        let path = std::env::var_os("NET1_TEST_CRASH_MARKER").expect("crash marker path");
        std::fs::write(path, stage).expect("write crash marker");
        unsafe {
            libc::raise(libc::SIGSTOP);
        }
    }
}

impl Runtime {
    fn authorize_blob_source(&self, store: &Store, network: &str, source: &str) -> Result<()> {
        ensure!(
            self.config.roles.contains(&NodeRole::Replica),
            "node capability is disabled"
        );
        let view = self.identities.public_state()?.project()?;
        ensure!(view.network_id == network, "wrong personal network");
        let local = self.identity.node_id()?;
        view.authorize_node(&local, Some(NodeRole::Replica))?;
        view.authorize_node(source, Some(NodeRole::Replica))?;
        store.authorize_node(network, &local, Some(NodeRole::Replica))?;
        store.authorize_node(network, source, Some(NodeRole::Replica))?;
        Ok(())
    }
    async fn cleanup_blob_lease(
        &self,
        lease: BlobLease,
        pass: Option<OwnedMutexGuard<CleanupCursor>>,
    ) -> (Option<OwnedMutexGuard<CleanupCursor>>, Result<bool>) {
        let expected = BlobRef {
            hash: lease.key.hash.clone(),
            size: lease.key.size,
        };
        match self
            .store
            .lock()
            .await
            .prepare_blob_cleanup(&lease.key.network, &expected)
        {
            Ok(true) => {}
            Ok(false) => return (pass, Ok(false)),
            Err(error) => return (pass, Err(error)),
        }
        let (pass, result) = match lease.unlink(pass).await {
            Ok(returned) => returned,
            Err(error) => return (None, Err(error)),
        };
        let lease = match result {
            Ok(lease) => lease,
            Err(error) => return (pass, Err(error)),
        };
        let result = self
            .store
            .lock()
            .await
            .delete_blob_cleanup(&lease.key.network, &expected);
        if result.is_ok() {
            tracing::debug!(network=%lease.key.network,hash=%expected.hash,size=expected.size,"history blob cleanup finished");
        }
        (pass, result.map(|()| true))
    }
    pub(super) async fn recover_blob_staging(&self, byte_budget: u64) {
        let Ok(_work) = self.maintenance.enter() else {
            return;
        };
        let Ok(mut pass) = self.blob_staging.cleanup_pass.clone().try_lock_owned() else {
            return;
        };
        let mut verified_bytes = 0;
        let mut wrapped = false;
        for _ in 0..4 {
            let row = {
                let after = pass
                    .after
                    .as_ref()
                    .map(|(network, key)| (network.as_str(), key.as_str()));
                self.store.lock().await.blob_transfers(after, 1)
            };
            let row = match row {
                Ok(rows) => rows.into_iter().next(),
                Err(error) => {
                    tracing::warn!(%error,"staging manifest enumeration failed");
                    break;
                }
            };
            let Some((network, key, raw)) = row else {
                if wrapped || pass.after.is_none() {
                    break;
                }
                pass.after = None;
                wrapped = true;
                continue;
            };
            let result = (|| -> Result<BlobTransfer> {
                let transfer: BlobTransfer = serde_json::from_slice(&raw)?;
                transfer.validate(&key)?;
                Ok(transfer)
            })();
            let transfer = match result {
                Ok(transfer) => transfer,
                Err(error) => {
                    pass.after = Some((network, key));
                    tracing::warn!(%error,"invalid staging manifest retained");
                    continue;
                }
            };
            if verified_bytes + transfer.blob.size > byte_budget {
                break;
            }
            pass.after = Some((network.clone(), key));
            let Ok(blob_key) = BlobKey::new(&network, &transfer.blob) else {
                continue;
            };
            let Some(lease) = self.blob_staging.try_lease(blob_key).await else {
                continue;
            };
            verified_bytes += transfer.blob.size;
            let (returned, result) = self.cleanup_blob_lease(lease, Some(pass)).await;
            if let Err(error) = result {
                tracing::warn!(network,hash=%transfer.blob.hash,%error,"staging cleanup pending");
            }
            if let Some(returned) = returned {
                pass = returned;
            } else {
                break;
            }
        }
    }
    async fn reserve_blob_transfer(
        &self,
        lease: BlobLease,
        blob: &BlobRef,
        source: &str,
    ) -> Result<(BlobLease, BlobTransfer)> {
        let reservation = self.blob_staging.reservation.clone().lock_owned().await;
        let (lease, _reservation, files) = lease.scan(reservation).await?;
        let mut store = self.store.lock().await;
        self.authorize_blob_source(&store, &lease.key.network, source)?;
        if let Some(transfer) = store.blob_transfer(&lease.key.network, blob)? {
            return Ok((lease, transfer));
        }
        let manifests = store.blob_transfers(None, 65)?;
        ensure!(
            manifests.len() < MAX_STAGING_OBJECTS,
            "staging object quota exceeded"
        );
        let mut bytes = 0_u64;
        let mut known = HashMap::new();
        for (network, key, raw) in &manifests {
            let transfer: BlobTransfer =
                serde_json::from_slice(raw).context("invalid staging manifest occupies quota")?;
            transfer.validate(key)?;
            bytes = bytes
                .checked_add(transfer.blob.size)
                .context("staging quota overflow")?;
            known.insert(
                BlobKey::new(network, &transfer.blob)?
                    .filename()?
                    .into_bytes(),
                transfer.blob.size,
            );
        }
        let mut count = manifests.len();
        for (filename, length) in files {
            if let Some(reserved) = known.get(&filename) {
                bytes = bytes
                    .checked_add(length.saturating_sub(*reserved))
                    .context("staging quota overflow")?;
            } else {
                count += 1;
                bytes = bytes
                    .checked_add(length)
                    .context("staging quota overflow")?;
            }
        }
        ensure!(
            count < MAX_STAGING_OBJECTS
                && bytes
                    .checked_add(blob.size)
                    .is_some_and(|bytes| bytes <= MAX_STAGING_BYTES),
            "staging quota exceeded"
        );
        let transfer = BlobTransfer::receiving(blob, source, 0);
        store.local_set(
            &lease.key.network,
            &BlobTransfer::key(blob)?,
            &serde_json::to_value(&transfer)?,
        )?;
        test_boundary("reserved");
        Ok((lease, transfer))
    }
    async fn read_blob_chunk(
        &self,
        network: &str,
        source: &str,
        blob: &BlobRef,
        offset: u64,
        limit: usize,
        class: ReadClass,
    ) -> Result<BlobChunk> {
        self.authorize_blob_source(&*self.store.lock().await, network, source)?;
        tracing::trace!(network,hash=%blob.hash,source,offset,limit,"history blob request");
        let value = self
            .read_history(
                network,
                source,
                json!({"type":"blob","hash":blob.hash,"offset":offset,"limit":limit}),
                class,
                limit * 2 + 1024,
            )
            .await?;
        self.authorize_blob_source(&*self.store.lock().await, network, source)?;
        ensure!(
            value.value["size"].as_u64() == Some(blob.size),
            "blob response size mismatch"
        );
        let encoded = value.value["data"].as_str().context("blob data required")?;
        ensure!(
            encoded.len() % 2 == 0 && encoded.len() <= limit * 2,
            "blob chunk too large"
        );
        let bytes = hex::decode(encoded)?;
        ensure!(
            offset <= blob.size
                && offset
                    .checked_add(bytes.len() as u64)
                    .is_some_and(|end| end <= blob.size)
                && (blob.size == 0 || !bytes.is_empty()),
            "invalid blob chunk"
        );
        if blob.size == 0 {
            ensure!(
                offset == 0 && bytes.is_empty() && crypto::digest(&bytes) == blob.hash,
                "invalid empty blob"
            );
        }
        Ok(BlobChunk {
            bytes,
            response: value,
        })
    }
    async fn commit_blob_progress(
        &self,
        network: &str,
        previous: &BlobTransfer,
        source: &str,
        offset: u64,
        part_length: u64,
    ) -> Result<BlobTransfer> {
        ensure!(
            part_length == offset,
            "durable part length does not match progress"
        );
        let next = BlobTransfer::receiving(&previous.blob, source, offset);
        let mut store = self.store.lock().await;
        self.authorize_blob_source(&store, network, source)?;
        store.advance_blob_transfer(network, &self.identity.node_id()?, previous, &next)?;
        test_boundary("offset_committed");
        tracing::trace!(network,hash=%previous.blob.hash,source,committed_offset=offset,part_length,"history blob committed");
        Ok(next)
    }
    pub(super) async fn history_blob(
        &self,
        network: &str,
        sources: &[HistorySource],
        blob: &BlobRef,
        class: ReadClass,
        quantum: &mut BlobQuantum,
    ) -> Result<BlobStep> {
        if quantum.remaining == 0 {
            return Ok(BlobStep::Partial);
        }
        let key = BlobKey::new(network, blob)?;
        let lease = self.blob_staging.lease(key).await;
        let (manifest, complete) = {
            let store = self.store.lock().await;
            (
                store.blob_transfer(network, blob)?,
                store.has_blob(network, blob)?,
            )
        };
        if complete {
            if manifest.is_some()
                && let (_, Err(error)) = self.cleanup_blob_lease(lease, None).await
            {
                tracing::warn!(network,hash=%blob.hash,%error,"staging cleanup pending");
            }
            return Ok(BlobStep::Complete { changed: false });
        }
        let (lease, mut transfer) = match manifest {
            Some(transfer) => {
                ensure!(
                    transfer.phase != TransferPhase::Cleanup,
                    "published blob damaged; preserving staging copy"
                );
                (lease, transfer)
            }
            None => {
                let source = sources
                    .first()
                    .context("blob unavailable from authorized replicas")?;
                self.reserve_blob_transfer(lease, blob, &source.node_id)
                    .await?
            }
        };
        let TransferPhase::Receiving {
            committed_offset,
            source_node_id,
        } = transfer.phase.clone()
        else {
            bail!("invalid receiving state");
        };
        let (mut writer, mut offset, part_length) = lease.recover(committed_offset).await?;
        if offset != committed_offset {
            let source = sources
                .iter()
                .find(|source| {
                    self.authorize_blob_source_for_current(network, &source.node_id)
                        .is_ok()
                })
                .map(|source| source.node_id.as_str())
                .unwrap_or(&source_node_id);
            transfer = self
                .commit_blob_progress(network, &transfer, source, offset, part_length)
                .await?;
        }
        let bound_source = match &transfer.phase {
            TransferPhase::Receiving { source_node_id, .. } => source_node_id.clone(),
            _ => unreachable!(),
        };
        tracing::debug!(network,hash=%blob.hash,size=blob.size,source=%bound_source,committed_offset=offset,part_length,"history blob opened");
        let mut ordered: Vec<_> = sources
            .iter()
            .filter(|source| !source.failed)
            .map(|source| source.node_id.as_str())
            .collect();
        if let Some(index) = ordered.iter().position(|source| *source == bound_source) {
            ordered.rotate_left(index);
        }
        let first_source = ordered.first().copied().unwrap_or(&bound_source);
        let mut empty_received = blob.size != 0;
        let mut busy = false;
        for source in ordered.iter().copied() {
            if blob.size == 0 {
                match self
                    .read_blob_chunk(network, source, blob, 0, 1, class)
                    .await
                {
                    Ok(chunk) => {
                        transfer = self
                            .commit_blob_progress(network, &transfer, source, 0, part_length)
                            .await?;
                        empty_received = true;
                        quantum.remaining -= 1;
                        drop(chunk);
                        break;
                    }
                    Err(error) => {
                        busy |=
                            error.downcast_ref::<RequestFailure>() == Some(&RequestFailure::Busy);
                        tracing::debug!(network,source,%error,"empty blob source unavailable; trying another authorized replica");
                        continue;
                    }
                }
            }
            while offset < blob.size {
                if quantum.remaining == 0 {
                    drop(writer);
                    return Ok(BlobStep::Partial);
                }
                let chunk = match self
                    .read_blob_chunk(network, source, blob, offset, 65536, class)
                    .await
                {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        busy |=
                            error.downcast_ref::<RequestFailure>() == Some(&RequestFailure::Busy);
                        tracing::debug!(network,source,%error,"blob source unavailable; prefix retained");
                        break;
                    }
                };
                let next = offset + chunk.bytes.len() as u64;
                let (returned, part_length, response) = writer
                    .write(offset, chunk.bytes, Some(chunk.response))
                    .await?;
                writer = returned;
                transfer = self
                    .commit_blob_progress(network, &transfer, source, next, part_length)
                    .await?;
                offset = next;
                quantum.remaining -= 1;
                drop(response);
            }
            if offset == blob.size {
                break;
            }
        }
        if busy && (offset != blob.size || !empty_received) {
            return Err(RequestFailure::Busy.into());
        }
        ensure!(
            offset == blob.size && empty_received,
            "blob unavailable from authorized replicas; prefix retained"
        );
        let (returned, bytes) = writer.read().await?;
        writer = returned;
        if crypto::digest(&bytes) != blob.hash {
            let (returned, part_length) = writer.reset().await?;
            writer = returned;
            let next = ordered
                .iter()
                .copied()
                .skip(1)
                .find(|source| {
                    *source != first_source
                        && self
                            .authorize_blob_source_for_current(network, source)
                            .is_ok()
                })
                .unwrap_or(first_source);
            let _ = self
                .commit_blob_progress(network, &transfer, next, 0, part_length)
                .await?;
            drop(writer);
            bail!("blob hash mismatch; prefix reset for another authorized source");
        }
        let mut source = match &transfer.phase {
            TransferPhase::Receiving { source_node_id, .. } => source_node_id.clone(),
            _ => unreachable!(),
        };
        if self
            .authorize_blob_source_for_current(network, &source)
            .is_err()
        {
            let last_offset = blob.size.saturating_sub(65536);
            let expected = &bytes[usize::try_from(last_offset)?..];
            let mut rebound = false;
            for candidate in ordered {
                if quantum.remaining == 0 {
                    drop(writer);
                    return Ok(BlobStep::Partial);
                }
                let chunk = match self
                    .read_blob_chunk(
                        network,
                        candidate,
                        blob,
                        last_offset,
                        expected.len().max(1),
                        class,
                    )
                    .await
                {
                    Ok(chunk) => {
                        quantum.remaining -= 1;
                        chunk
                    }
                    Err(error) => {
                        busy |=
                            error.downcast_ref::<RequestFailure>() == Some(&RequestFailure::Busy);
                        continue;
                    }
                };
                if chunk.bytes == expected {
                    self.commit_blob_progress(
                        network,
                        &transfer,
                        candidate,
                        offset,
                        bytes.len() as u64,
                    )
                    .await?;
                    source = candidate.into();
                    rebound = true;
                    break;
                }
            }
            if !rebound && busy {
                return Err(RequestFailure::Busy.into());
            }
            ensure!(
                rebound,
                "complete prefix has no currently authorized source; retained"
            );
        }
        let changed = {
            let mut store = self.store.lock().await;
            self.authorize_blob_source(&store, network, &source)?;
            store.publish_blob_transfer(
                network,
                &self.identity.node_id()?,
                blob,
                &source,
                &bytes,
            )?
        };
        test_boundary("published");
        tracing::debug!(network,hash=%blob.hash,size=blob.size,changed,"history blob published");
        if let (_, Err(error)) = self.cleanup_blob_lease(writer.into_lease(), None).await {
            tracing::warn!(network,hash=%blob.hash,%error,"staging cleanup pending");
        }
        Ok(BlobStep::Complete { changed })
    }
    fn authorize_blob_source_for_current(&self, network: &str, source: &str) -> Result<()> {
        let view = self.identities.public_state()?.project()?;
        ensure!(
            self.config.roles.contains(&NodeRole::Replica) && view.network_id == network,
            "replica network unavailable"
        );
        view.authorize_node(&self.identity.node_id()?, Some(NodeRole::Replica))?;
        view.authorize_node(source, Some(NodeRole::Replica))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &[u8] = b"durable prefix crash boundary";

    fn blob() -> BlobRef {
        BlobRef {
            hash: crypto::digest(BODY),
            size: BODY.len() as u64,
        }
    }

    async fn node() -> Result<(PathBuf, Runtime, String)> {
        let dir = super::super::tests::fixture_under("network-reliability")?;
        let node = Runtime::open(&dir).await?;
        let network = super::super::tests::conversation(&node).await?;
        Ok((dir, node, network))
    }

    async fn reserve(
        node: &Runtime,
        network: &str,
        blob: &BlobRef,
    ) -> Result<(BlobLease, BlobTransfer)> {
        let lease = node.blob_staging.lease(BlobKey::new(network, blob)?).await;
        node.reserve_blob_transfer(lease, blob, &node.identity.node_id()?)
            .await
    }

    fn quota_blob(label: &str, size: u64) -> BlobRef {
        BlobRef {
            hash: crypto::digest(label.as_bytes()),
            size,
        }
    }

    async fn save_manifest(
        node: &Runtime,
        network: &str,
        blob: &BlobRef,
        phase: TransferPhase,
    ) -> Result<()> {
        node.store.lock().await.local_set(
            network,
            &BlobTransfer::key(blob)?,
            &serde_json::to_value(BlobTransfer {
                version: 1,
                blob: blob.clone(),
                phase,
            })?,
        )
    }

    fn part_path(dir: &Path, network: &str, blob: &BlobRef) -> Result<PathBuf> {
        Ok(dir
            .join("net1-blob-staging")
            .join(BlobKey::new(network, blob)?.filename()?.to_str()?))
    }

    async fn reservation_error(node: &Runtime, network: &str, blob: &BlobRef) -> Result<String> {
        match reserve(node, network, blob).await {
            Err(error) => Ok(error.to_string()),
            Ok(_) => bail!("reservation unexpectedly succeeded"),
        }
    }

    #[tokio::test]
    async fn staging_quota_counts_all_networks_and_cleanup_at_exact_byte_limit() -> Result<()> {
        let (dir, node, network) = node().await?;
        let source = node.identity.node_id()?;
        let foreign = crypto::digest(b"former personal network");
        for index in 0..4 {
            let reserved = quota_blob(&format!("full-reservation-{index}"), MAX_BLOB_BYTES as u64);
            let phase = if index % 2 == 0 {
                TransferPhase::Receiving {
                    committed_offset: 0,
                    source_node_id: source.clone(),
                }
            } else {
                TransferPhase::Cleanup
            };
            save_manifest(
                &node,
                if index < 2 { &network } else { &foreign },
                &reserved,
                phase,
            )
            .await?;
        }
        let empty = quota_blob("", 0);
        let (lease, transfer) = reserve(&node, &network, &empty).await?;
        assert_eq!(transfer, BlobTransfer::receiving(&empty, &source, 0));
        drop(lease);
        let before = node.store.lock().await.blob_transfers(None, 65)?;
        assert_eq!(before.len(), 5);
        let extra = quota_blob("one byte over the global reservation", 1);
        assert_eq!(
            reservation_error(&node, &network, &extra).await?,
            "staging quota exceeded"
        );
        assert_eq!(node.store.lock().await.blob_transfers(None, 65)?, before);
        assert!(
            node.store
                .lock()
                .await
                .blob_transfer(&network, &extra)?
                .is_none()
        );
        assert_eq!(std::fs::read_dir(dir.join("net1-blob-staging"))?.count(), 0);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn staging_quota_counts_unknown_files_and_known_part_excess() -> Result<()> {
        for known in [false, true] {
            let (dir, node, network) = node().await?;
            let existing = quota_blob("existing sparse prefix", 1);
            let path = if known {
                save_manifest(
                    &node,
                    &network,
                    &existing,
                    TransferPhase::Receiving {
                        committed_offset: 0,
                        source_node_id: node.identity.node_id()?,
                    },
                )
                .await?;
                part_path(&dir, &network, &existing)?
            } else {
                dir.join("net1-blob-staging/unknown.part")
            };
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            file.write_all(b"keep")?;
            file.set_len(MAX_STAGING_BYTES - 1)?;
            drop(file);
            let last = quota_blob("last allowed byte", 1);
            let (lease, transfer) = reserve(&node, &network, &last).await?;
            assert_eq!(transfer.blob, last);
            drop(lease);
            let before = node.store.lock().await.blob_transfers(None, 65)?;
            let extra = quota_blob("excess byte", 1);
            assert_eq!(
                reservation_error(&node, &network, &extra).await?,
                "staging quota exceeded"
            );
            assert_eq!(node.store.lock().await.blob_transfers(None, 65)?, before);
            assert!(
                node.store
                    .lock()
                    .await
                    .blob_transfer(&network, &extra)?
                    .is_none()
            );
            assert_eq!(std::fs::metadata(&path)?.len(), MAX_STAGING_BYTES - 1);
            let mut prefix = [0_u8; 4];
            File::open(&path)?.read_exact(&mut prefix)?;
            assert_eq!(&prefix, b"keep");
            drop(node);
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn staging_object_quota_counts_empty_objects_and_unknown_files_once() -> Result<()> {
        for unknown in [false, true] {
            let (dir, node, network) = node().await?;
            let count = if unknown { 62 } else { 63 };
            for index in 0..count {
                let reserved = quota_blob(&format!("empty-reservation-{index}"), 0);
                save_manifest(
                    &node,
                    &network,
                    &reserved,
                    TransferPhase::Receiving {
                        committed_offset: 0,
                        source_node_id: node.identity.node_id()?,
                    },
                )
                .await?;
            }
            let matching = part_path(&dir, &network, &quota_blob("empty-reservation-0", 0))?;
            config::private_write(&matching, b"")?;
            let unknown_path = dir.join("net1-blob-staging/unknown-empty.part");
            if unknown {
                config::private_write(&unknown_path, b"")?;
            }
            let last = quota_blob("", 0);
            let (lease, transfer) = reserve(&node, &network, &last).await?;
            assert_eq!(transfer.blob, last);
            drop(lease);
            let before = node.store.lock().await.blob_transfers(None, 65)?;
            assert_eq!(before.len(), count + 1);
            let extra = quota_blob("sixty-fifth empty reservation", 0);
            assert_eq!(
                reservation_error(&node, &network, &extra).await?,
                if unknown {
                    "staging quota exceeded"
                } else {
                    "staging object quota exceeded"
                }
            );
            assert_eq!(node.store.lock().await.blob_transfers(None, 65)?, before);
            assert!(
                node.store
                    .lock()
                    .await
                    .blob_transfer(&network, &extra)?
                    .is_none()
            );
            assert_eq!(std::fs::metadata(&matching)?.len(), 0);
            if unknown {
                assert_eq!(std::fs::metadata(&unknown_path)?.len(), 0);
            }
            drop(node);
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn bad_manifest_bytes_block_reservation_but_do_not_block_cleanup_cursor() -> Result<()> {
        for bad in [b"not-json".as_slice(), b"\x80".as_slice()] {
            let (dir, node, network) = node().await?;
            let invalid = BlobRef {
                hash: "00".repeat(32),
                size: 1,
            };
            let invalid_key = BlobTransfer::key(&invalid)?;
            let invalid_part = part_path(&dir, &network, &invalid)?;
            config::private_write(&invalid_part, b"x")?;
            let complete = blob();
            assert!(invalid_key < BlobTransfer::key(&complete)?);
            node.store
                .lock()
                .await
                .put_blob(&network, &complete, BODY)?;
            save_manifest(&node, &network, &complete, TransferPhase::Cleanup).await?;
            let complete_part = part_path(&dir, &network, &complete)?;
            config::private_write(&complete_part, BODY)?;
            let connection = rusqlite::Connection::open(dir.join("node.sqlite3"))?;
            connection.execute(
                "INSERT INTO local_values(network_id,key,value) VALUES(?,?,CAST(? AS TEXT))",
                rusqlite::params![network, invalid_key, bad],
            )?;
            let extra = quota_blob("blocked by an invalid reservation", 1);
            assert_eq!(
                reservation_error(&node, &network, &extra).await?,
                "invalid staging manifest occupies quota"
            );
            node.recover_blob_staging(64).await;
            assert!(!complete_part.exists());
            assert!(
                node.store
                    .lock()
                    .await
                    .blob_transfer(&network, &complete)?
                    .is_none()
            );
            assert_eq!(
                node.store
                    .lock()
                    .await
                    .get_blob(&network, &complete.hash)?
                    .as_deref(),
                Some(BODY)
            );
            assert_eq!(std::fs::read(&invalid_part)?, b"x");
            let preserved: Vec<u8> = connection.query_row(
                "SELECT CAST(value AS BLOB) FROM local_values WHERE network_id=? AND key=?",
                rusqlite::params![network, invalid_key],
                |row| row.get(0),
            )?;
            assert_eq!(preserved, bad);
            assert_eq!(
                reservation_error(&node, &network, &extra).await?,
                "invalid staging manifest occupies quota"
            );
            assert!(
                node.store
                    .lock()
                    .await
                    .blob_transfer(&network, &extra)?
                    .is_none()
            );
            drop(connection);
            drop(node);
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn cleanup_budget_keeps_middle_large_object_for_the_next_pass() -> Result<()> {
        let (dir, node, network) = node().await?;
        let mut sides = [vec![0_u8; 32], vec![1_u8; 32]];
        sides.sort_by_key(|bytes| crypto::digest(bytes));
        let lower = crypto::digest(&sides[0]);
        let upper = crypto::digest(&sides[1]);
        let middle = (0_u8..=255)
            .map(|byte| vec![byte; 64])
            .find(|bytes| {
                let hash = crypto::digest(bytes);
                lower < hash && hash < upper
            })
            .context("small cleanup fixture has no ordered middle hash")?;
        let bodies = [sides[0].clone(), middle, sides[1].clone()];
        let mut objects = Vec::new();
        for bytes in &bodies {
            let complete = BlobRef {
                hash: crypto::digest(bytes),
                size: bytes.len() as u64,
            };
            node.store
                .lock()
                .await
                .put_blob(&network, &complete, bytes)?;
            save_manifest(&node, &network, &complete, TransferPhase::Cleanup).await?;
            let path = part_path(&dir, &network, &complete)?;
            config::private_write(&path, bytes)?;
            objects.push((complete, path));
        }
        let rows = node.store.lock().await.blob_transfers(None, 65)?;
        let ordered_sizes = rows
            .iter()
            .map(|(_, _, raw)| Ok(serde_json::from_slice::<BlobTransfer>(raw)?.blob.size))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(ordered_sizes, vec![32, 64, 32]);
        for expected in [
            [false, true, true],
            [false, false, true],
            [false, false, false],
        ] {
            node.recover_blob_staging(64).await;
            let mut manifests = Vec::new();
            for (index, (complete, path)) in objects.iter().enumerate() {
                assert_eq!(path.exists(), expected[index]);
                manifests.push(
                    node.store
                        .lock()
                        .await
                        .blob_transfer(&network, complete)?
                        .is_some(),
                );
                assert_eq!(
                    node.store
                        .lock()
                        .await
                        .get_blob(&network, &complete.hash)?
                        .as_deref(),
                    Some(bodies[index].as_slice())
                );
            }
            assert_eq!(manifests, expected);
        }
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn staging_recovers_only_committed_prefix_and_rejects_symlinks() -> Result<()> {
        let (dir, node, network) = node().await?;
        let blob = blob();
        let (lease, manifest) = reserve(&node, &network, &blob).await?;
        let (writer, offset, actual) = lease.recover(0).await?;
        assert_eq!((offset, actual), (0, 0));
        let split = 8;
        let (writer, length, _) = writer.write(0, BODY[..split].to_vec(), None).await?;
        let manifest = node
            .commit_blob_progress(
                &network,
                &manifest,
                &node.identity.node_id()?,
                split as u64,
                length,
            )
            .await?;
        let (writer, _, _) = writer
            .write(split as u64, BODY[split..].to_vec(), None)
            .await?;
        drop(writer);
        let key = BlobKey::new(&network, &blob)?;
        let lease = node.blob_staging.lease(key.clone()).await;
        let (mut writer, recovered, length) = lease.recover(split as u64).await?;
        assert_eq!((recovered, length), (split as u64, split as u64));
        writer.part.seek(SeekFrom::Start(0))?;
        let mut prefix = Vec::new();
        writer.part.read_to_end(&mut prefix)?;
        assert_eq!(prefix, BODY[..split]);
        writer.part.set_len(2)?;
        drop(writer);
        let lease = node.blob_staging.lease(key.clone()).await;
        let (writer, recovered, length) = lease.recover(split as u64).await?;
        assert_eq!(
            (recovered, length),
            (0, 0),
            "short files are truncated, never zero-filled to old offset"
        );
        let _ = node
            .commit_blob_progress(&network, &manifest, &node.identity.node_id()?, 0, length)
            .await?;
        drop(writer);
        let path = dir
            .join("net1-blob-staging")
            .join(key.filename()?.to_str()?);
        std::fs::remove_file(&path)?;
        let target = dir.join("untouched");
        std::fs::write(&target, b"private target must stay unchanged")?;
        std::os::unix::fs::symlink(&target, &path)?;
        let lease = node.blob_staging.lease(key).await;
        assert!(lease.recover(0).await.is_err());
        assert_eq!(
            std::fs::read(&target)?,
            b"private target must stay unchanged"
        );
        assert!(
            node.store
                .lock()
                .await
                .get_blob(&network, &blob.hash)?
                .is_none()
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn m5_revoked_complete_prefix_rebound_consumes_the_chunk_quantum() -> Result<()> {
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
        let mut replicas = Vec::new();
        for _ in 0..5 {
            let dir = super::super::tests::fixture_under("network-reliability")?;
            replicas.push(Arc::new(Runtime::open(&dir).await?));
        }
        replicas.sort_by_key(|node| node.identity.node_id().unwrap());
        let receiver_dir = super::super::tests::fixture_under("network-reliability")?;
        let receiver = Arc::new(Runtime::open(&receiver_dir).await?);
        let owner = LocalIdentity::generate();
        let retired = LocalIdentity::generate();
        let mut certificates = replicas
            .iter()
            .map(|node| node.identity.certificate(node.config.roles.clone()))
            .collect::<Result<Vec<_>>>()?;
        certificates.push(
            receiver
                .identity
                .certificate(receiver.config.roles.clone())?,
        );
        certificates.push(retired.certificate(vec![NodeRole::Replica])?);
        let membership = owner.sign_network_fixture(1, certificates.clone())?;
        let network = membership.genesis.network_id.clone();
        let blob = blob();
        for replica in &replicas {
            replica.install_membership(membership.clone()).await?;
            replica.store.lock().await.put_blob(&network, &blob, BODY)?;
        }
        receiver.install_membership(membership).await?;
        let lease = receiver
            .blob_staging
            .lease(BlobKey::new(&network, &blob)?)
            .await;
        let retired_id = retired.node_id()?;
        let (lease, manifest) = receiver
            .reserve_blob_transfer(lease, &blob, &retired_id)
            .await?;
        let (writer, _, _) = lease.recover(0).await?;
        let (writer, length, _) = writer.write(0, BODY.to_vec(), None).await?;
        receiver
            .commit_blob_progress(&network, &manifest, &retired_id, blob.size, length)
            .await?;
        drop(writer);
        let event = receiver.identity.sign_event(Event {
            version: 1,
            network_id: network.clone(),
            scope_id: "rebound".into(),
            author: receiver.identity.node_id()?,
            sequence: 1,
            previous: None,
            parents: vec![],
            created_at_ms: 1,
            body: EventBody::ConversationCreated {
                title: "rebound fairness".into(),
            },
            blobs: vec![blob.clone()],
        })?;
        assert_eq!(
            receiver
                .store
                .lock()
                .await
                .ingest_event(&network, &event)?
                .status,
            "pending"
        );
        certificates.last_mut().unwrap().revoked = true;
        let membership = owner.sign_network_fixture(2, certificates)?;
        for replica in &replicas {
            replica.install_membership(membership.clone()).await?;
        }
        receiver.install_membership(membership).await?;
        assert!(
            receiver
                .authorize_blob_source_for_current(&network, &retired_id)
                .is_err()
        );
        let requests = Arc::new(Mutex::new(Vec::<usize>::new()));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (entered, next_quantum) = oneshot::channel();
        let entered = Arc::new(Mutex::new(Some(entered)));
        let mut tasks = tokio::task::JoinSet::new();
        for (index, replica) in replicas.iter().enumerate() {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let url = format!("ws://{}/peer", listener.local_addr()?);
            let app = Router::new()
                .route("/peer", get(upgrade))
                .with_state(replica.hub.clone());
            tasks.spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            let hub = receiver.hub.clone();
            let scope = network.clone();
            tasks.spawn(async move {
                let _ = hub.connect(scope, url).await;
            });
            let observed = requests.clone();
            let gate = gate.clone();
            let entered = entered.clone();
            let mut inbound = replica
                .peer_rx
                .lock()
                .await
                .take()
                .context("source receiver")?;
            let responder = replica.clone();
            tasks.spawn(async move {
                while let Some(message) = inbound.recv().await {
                    if message.payload["type"] == "blob" {
                        let count = {
                            let mut requests = observed.lock().await;
                            requests.push(index);
                            requests.len()
                        };
                        if count == 5 {
                            if let Some(entered) = entered.lock().await.take() {
                                let _ = entered.send(());
                            }
                            gate.acquire().await.unwrap().forget();
                        }
                    }
                    let mut reply = responder.handle_peer(&message).await.unwrap();
                    if message.payload["type"] == "blob" && index < 4 {
                        reply["data"] = json!(hex::encode(vec![0; BODY.len()]));
                    }
                    let _ = responder.hub.respond(&message, reply).await;
                }
            });
        }
        tokio::time::timeout(Duration::from_secs(30), async {
            while receiver.hub.online(&network).await.len() < 5 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let sources = receiver.history_sources(&network, &[]).await?;
        let mut exhausted = BlobQuantum::new(0);
        assert_eq!(
            receiver
                .history_blob(&network, &sources, &blob, ReadClass::Proof, &mut exhausted)
                .await?,
            BlobStep::Partial
        );
        assert!(requests.lock().await.is_empty());
        let mut updates = receiver.events.subscribe();
        let node = receiver.clone();
        let scope = network.clone();
        let source_id = replicas[0].identity.node_id()?;
        let proof = tokio::spawn(async move {
            node.fetch_closure(&scope, "rebound", &source_id, &[event.hash])
                .await
        });
        tokio::time::timeout(Duration::from_secs(30), next_quantum)
            .await
            .context("rebound did not begin a second quantum")??;
        assert_eq!(&requests.lock().await[..4], &[0, 1, 2, 3]);
        {
            let store = receiver.store.lock().await;
            assert!(!store.has_blob(&network, &blob)?);
            assert!(
                matches!(store.blob_transfer(&network, &blob)?.unwrap().phase,
                TransferPhase::Receiving { committed_offset, source_node_id }
                if committed_offset == blob.size && source_node_id == retired_id)
            );
        }
        assert!(updates.try_recv().is_err());
        gate.add_permits(1);
        let result = tokio::time::timeout(Duration::from_secs(3), proof).await;
        if result.is_err() {
            let requests = requests.lock().await;
            println!(
                "NET1_M5 rebound_completion_timeout=true requests={} first_sources={:?}",
                requests.len(),
                &requests[..requests.len().min(16)]
            );
        }
        result
            .context("healthy fifth rebound source did not complete")???
            .require_complete()?;
        assert_eq!(*requests.lock().await, vec![0, 1, 2, 3, 1, 2, 3, 4]);
        assert_eq!(
            receiver
                .store
                .lock()
                .await
                .get_blob(&network, &blob.hash)?
                .unwrap(),
            BODY
        );
        assert!(
            receiver
                .store
                .lock()
                .await
                .blob_transfer(&network, &blob)?
                .is_none()
        );
        assert_eq!(updates.try_recv()?.payload["type"], "changed");
        assert!(updates.try_recv().is_err());
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        println!(
            "NET1_M5 revoked_complete_prefix_retained=true exhausted_quantum_no_read_or_publish=true wrong_authorized_tail_sources=4 healthy_fifth_source=true actual_coordinator_steps=2 successful_ranges_per_step=4,4 range_requests=8 unverified_source_not_bound=true no_changed_during_partial=true final_hash_verified=true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_blocking_writer_keeps_object_and_process_locks_until_io_ends() -> Result<()>
    {
        let dir = super::super::tests::fixture_under("network-reliability")?;
        let lock = Arc::new(config::lock_data_dir(&dir)?);
        let staging = BlobStaging::open(&dir, lock.clone())?;
        let key = BlobKey::new("network", &blob())?;
        let lease = staging.lease(key.clone()).await;
        let (started, wait_started) = std::sync::mpsc::sync_channel(1);
        let (finish, wait_finish) = std::sync::mpsc::sync_channel(1);
        let task = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let file = lease.open_part().unwrap();
                started.send(()).unwrap();
                wait_finish.recv().unwrap();
                file.sync_all().unwrap();
                drop(lease);
            })
            .await
            .unwrap();
        });
        tokio::task::spawn_blocking(move || wait_started.recv_timeout(Duration::from_secs(5)))
            .await??;
        task.abort();
        let _ = task.await;
        assert!(staging.try_lease(key.clone()).await.is_none());
        drop(lock);
        drop(staging);
        assert!(
            config::lock_data_dir(&dir).is_err(),
            "the cancelled caller must not release the data-dir lock"
        );
        finish.send(())?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(lock) = config::lock_data_dir(&dir) {
                drop(lock);
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "blocking writer did not release process lock"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn real_write_error_keeps_manifest_offset_and_never_publishes() -> Result<()> {
        let (dir, node, network) = node().await?;
        let blob = blob();
        let (lease, manifest) = reserve(&node, &network, &blob).await?;
        let (mut writer, _, _) = lease.recover(0).await?;
        // A genuine OS EBADF from write(), using a readonly descriptor of the real part.
        let name = writer.lease.key.filename()?;
        writer.part = owned_fd(unsafe {
            libc::openat(
                writer.lease.staging.directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        let error = match writer.write(0, BODY.to_vec(), None).await {
            Ok(_) => bail!("readonly writer unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error),
            Some(libc::EBADF)
        );
        println!("NET1_REAL_IO_ERROR {error:#}");
        assert_eq!(
            node.store.lock().await.blob_transfer(&network, &blob)?,
            Some(manifest)
        );
        assert!(
            node.store
                .lock()
                .await
                .get_blob(&network, &blob.hash)?
                .is_none()
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn cleanup_preserves_unique_part_when_complete_blob_is_corrupt_then_recovers()
    -> Result<()> {
        let (dir, node, network) = node().await?;
        let blob = blob();
        let (lease, manifest) = reserve(&node, &network, &blob).await?;
        let (writer, _, _) = lease.recover(0).await?;
        let (writer, length, _) = writer.write(0, BODY.to_vec(), None).await?;
        let _ = node
            .commit_blob_progress(
                &network,
                &manifest,
                &node.identity.node_id()?,
                blob.size,
                length,
            )
            .await?;
        node.store.lock().await.publish_blob_transfer(
            &network,
            &node.identity.node_id()?,
            &blob,
            &node.identity.node_id()?,
            BODY,
        )?;
        let path = dir
            .join("net1-blob-staging")
            .join(writer.lease.key.filename()?.to_str()?);
        drop(writer);
        let connection = rusqlite::Connection::open(dir.join("node.sqlite3"))?;
        connection.execute(
            "UPDATE blobs SET content=? WHERE network_id=? AND hash=?",
            rusqlite::params![vec![0_u8; BODY.len()], network, blob.hash],
        )?;
        node.recover_blob_staging(MAX_BLOB_BYTES as u64).await;
        assert_eq!(std::fs::read(&path)?, BODY);
        assert_eq!(
            node.store
                .lock()
                .await
                .blob_transfer(&network, &blob)?
                .unwrap()
                .phase,
            TransferPhase::Cleanup
        );
        node.store.lock().await.put_blob(&network, &blob, BODY)?;
        node.recover_blob_staging(MAX_BLOB_BYTES as u64).await;
        assert!(!path.exists());
        assert!(
            node.store
                .lock()
                .await
                .blob_transfer(&network, &blob)?
                .is_none()
        );
        assert_eq!(
            node.store
                .lock()
                .await
                .get_blob(&network, &blob.hash)?
                .unwrap(),
            BODY
        );
        drop(connection);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn blob_transfer_crash_child() -> Result<()> {
        let Some(dir) = std::env::var_os("NET1_TEST_CRASH_DIR") else {
            return Ok(());
        };
        let dir = PathBuf::from(dir);
        let node = Runtime::open(&dir).await?;
        let network = super::super::tests::conversation(&node).await?;
        let durability = node.store.lock().await.test_durability()?;
        std::fs::write(
            dir.join("durability.json"),
            serde_json::to_vec(
                &json!({"journal_mode":durability.0,"synchronous":durability.1,"network":network}),
            )?,
        )?;
        let blob = blob();
        let (lease, manifest) = reserve(&node, &network, &blob).await?;
        let (writer, _, _) = lease.recover(0).await?;
        let (writer, length, _) = writer.write(0, BODY.to_vec(), None).await?;
        let _ = node
            .commit_blob_progress(
                &network,
                &manifest,
                &node.identity.node_id()?,
                blob.size,
                length,
            )
            .await?;
        drop(writer);
        assert_eq!(
            node.history_blob(
                &network,
                &[],
                &blob,
                ReadClass::Proof,
                &mut BlobQuantum::new(4)
            )
            .await?,
            BlobStep::Complete { changed: true }
        );
        bail!("requested crash boundary was not reached")
    }

    #[tokio::test]
    async fn real_sigkill_boundaries_keep_fsync_offset_publish_and_cleanup_order() -> Result<()> {
        use std::os::unix::process::ExitStatusExt;
        let executable = std::env::current_exe()?;
        for stage in [
            "reserved",
            "chunk_fsynced",
            "offset_committed",
            "published",
            "unlinked",
        ] {
            let dir = super::super::tests::fixture_under("network-reliability")?;
            let marker = dir.join("crash-marker");
            let log = File::create(dir.join("child.log"))?;
            let mut child = std::process::Command::new(&executable)
                .args([
                    "--exact",
                    "runtime::blob_transfer::tests::blob_transfer_crash_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("NET1_TEST_CRASH_DIR", &dir)
                .env("NET1_TEST_CRASH_STAGE", stage)
                .env("NET1_TEST_CRASH_MARKER", &marker)
                .stdout(log.try_clone()?)
                .stderr(log)
                .spawn()?;
            let deadline = Instant::now() + Duration::from_secs(20);
            while !marker.exists() {
                if let Some(status) = child.try_wait()? {
                    bail!(
                        "crash child exited before {stage}: {status}; {}",
                        std::fs::read_to_string(dir.join("child.log"))?
                    );
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("crash child did not reach {stage}");
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            child.kill()?;
            assert_eq!(child.wait()?.signal(), Some(libc::SIGKILL));
            let durability: Value =
                serde_json::from_slice(&std::fs::read(dir.join("durability.json"))?)?;
            assert_eq!(durability["journal_mode"], "wal");
            let network = durability["network"].as_str().unwrap();
            let blob = blob();
            let connection = rusqlite::Connection::open(dir.join("node.sqlite3"))?;
            let raw: String = connection.query_row(
                "SELECT value FROM local_values WHERE network_id=? AND key=?",
                rusqlite::params![network, BlobTransfer::key(&blob)?],
                |row| row.get(0),
            )?;
            let manifest: BlobTransfer = serde_json::from_str(&raw)?;
            let count: u64 = connection.query_row(
                "SELECT count(*) FROM blobs WHERE network_id=? AND hash=?",
                rusqlite::params![network, blob.hash],
                |row| row.get(0),
            )?;
            let part = dir
                .join("net1-blob-staging")
                .join(BlobKey::new(network, &blob)?.filename()?.to_str()?);
            let length = std::fs::metadata(&part).ok().map(|metadata| metadata.len());
            match stage {
                "reserved" => {
                    assert_eq!(
                        manifest.phase,
                        BlobTransfer::receiving(&blob, &manifest_source(&manifest), 0).phase
                    );
                    assert_eq!(length, None);
                    assert_eq!(count, 0);
                }
                "chunk_fsynced" => {
                    assert!(matches!(
                        manifest.phase,
                        TransferPhase::Receiving {
                            committed_offset: 0,
                            ..
                        }
                    ));
                    assert_eq!(length, Some(blob.size));
                    assert_eq!(count, 0);
                }
                "offset_committed" => {
                    assert!(
                        matches!(manifest.phase,TransferPhase::Receiving { committed_offset,.. } if committed_offset==blob.size)
                    );
                    assert_eq!(length, Some(blob.size));
                    assert_eq!(count, 0);
                }
                "published" => {
                    assert_eq!(manifest.phase, TransferPhase::Cleanup);
                    assert_eq!(length, Some(blob.size));
                    assert_eq!(count, 1);
                }
                "unlinked" => {
                    assert_eq!(manifest.phase, TransferPhase::Cleanup);
                    assert_eq!(length, None);
                    assert_eq!(count, 1);
                }
                _ => unreachable!(),
            }
            println!(
                "NET1_CRASH_EVIDENCE {}",
                json!({"stage":stage,"signal":"SIGKILL","offset_or_phase":manifest.phase,"part_length":length,"complete_blob_count":count,"durability":durability})
            );
            drop(connection);
            let node = Runtime::open(&dir).await?;
            match stage {
                "reserved" | "chunk_fsynced" => {
                    let lease = node.blob_staging.lease(BlobKey::new(network, &blob)?).await;
                    let (writer, offset, length) = lease.recover(0).await?;
                    assert_eq!((offset, length), (0, 0));
                    drop(writer);
                    assert!(
                        node.store
                            .lock()
                            .await
                            .get_blob(network, &blob.hash)?
                            .is_none()
                    );
                }
                "offset_committed" => {
                    assert_eq!(
                        node.history_blob(
                            network,
                            &[],
                            &blob,
                            ReadClass::Proof,
                            &mut BlobQuantum::new(4)
                        )
                        .await?,
                        BlobStep::Complete { changed: true }
                    );
                    assert!(!part.exists());
                }
                _ => {
                    assert!(!part.exists());
                    assert!(
                        node.store
                            .lock()
                            .await
                            .blob_transfer(network, &blob)?
                            .is_none()
                    );
                }
            }
            if count == 1 || stage == "offset_committed" {
                assert_eq!(
                    node.store
                        .lock()
                        .await
                        .get_blob(network, &blob.hash)?
                        .unwrap(),
                    BODY
                );
            }
            drop(node);
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    fn manifest_source(manifest: &BlobTransfer) -> String {
        match &manifest.phase {
            TransferPhase::Receiving { source_node_id, .. } => source_node_id.clone(),
            _ => unreachable!(),
        }
    }
}
