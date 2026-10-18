//! A cursor stays on its worker stack and never borrows the main write connection.
use super::*;
use std::{
    collections::HashMap,
    sync::mpsc::{self, SyncSender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_READERS: usize = 4;
const MAX_PER_ORIGIN: usize = 2;
const MAX_READER_BYTES: u64 = 128 * 1024 * 1024;
const IDLE: Duration = Duration::from_secs(30);
const MAX_AGE: Duration = Duration::from_secs(300);
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Metadata {
    rowid: i64,
    pub size: u64,
}

pub(super) fn metadata(
    connection: &Connection,
    network: &str,
    hash: &str,
) -> Result<Option<Metadata>> {
    let row: Option<(i64, i64, i64, String)> = connection
        .query_row(
            "SELECT rowid,size,length(content),typeof(content) FROM blobs WHERE network_id=? AND hash=?",
            params![network, hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    row.map(|(rowid, size, length, kind)| {
        ensure!(
            kind == "blob" && size >= 0 && size == length && size <= MAX_BLOB_BYTES as i64,
            "stored blob size mismatch or limit exceeded"
        );
        Ok(Metadata {
            rowid,
            size: size as u64,
        })
    })
    .transpose()
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub(super) struct ReaderKey {
    network: String,
    origin: String,
    hash: String,
    rowid: i64,
    size: u64,
}
impl ReaderKey {
    pub(super) fn new(network: &str, origin: &str, hash: &str, metadata: Metadata) -> Self {
        Self {
            network: network.into(),
            origin: origin.into(),
            hash: hash.into(),
            rowid: metadata.rowid,
            size: metadata.size,
        }
    }
}

struct RangeRequest {
    offset: usize,
    count: usize,
    reply: SyncSender<std::result::Result<Vec<u8>, String>>,
}

struct ReaderWorker {
    requests: Option<SyncSender<RangeRequest>>,
    join: Option<JoinHandle<()>>,
}

impl ReaderWorker {
    fn request(&mut self, offset: usize, count: usize, timeout: Duration) -> Result<Vec<u8>> {
        let (reply, receiver) = mpsc::sync_channel(1);
        let sent = self
            .requests
            .as_ref()
            .context("blob range reader closing")?
            .try_send(RangeRequest {
                offset,
                count,
                reply,
            });
        if let Err(error) = sent {
            self.close();
            bail!("blob range request unavailable: {error}");
        }
        match receiver.recv_timeout(timeout) {
            Ok(Ok(bytes)) => Ok(bytes),
            result => {
                self.close();
                match result {
                    Ok(Err(error)) => bail!("{error}"),
                    Err(error) => bail!("blob range reply unavailable; reader closing: {error}"),
                    Ok(Ok(_)) => unreachable!(),
                }
            }
        }
    }
    fn close(&mut self) {
        self.requests.take();
    }
    fn finished(&self) -> bool {
        self.join.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for ReaderWorker {
    fn drop(&mut self) {
        self.close();
        if let Some(join) = self.join.take()
            && join.join().is_err()
        {
            tracing::warn!("blob range worker panicked while closing");
        }
    }
}

#[derive(Default)]
pub(super) struct BlobReaders {
    workers: HashMap<ReaderKey, ReaderWorker>,
}

impl BlobReaders {
    fn reap(&mut self) {
        self.workers.retain(|_, worker| !worker.finished());
    }

    pub fn retire_network(&mut self, network: &str) {
        self.workers.retain(|key, _| key.network != network);
    }

    pub fn retire_blob(&mut self, network: &str, hash: &str) {
        self.workers
            .retain(|key, _| key.network != network || key.hash != hash);
    }

    pub fn read(
        &mut self,
        path: &Path,
        process_lock: &Arc<std::fs::File>,
        key: ReaderKey,
        offset: usize,
        count: usize,
    ) -> Result<Vec<u8>> {
        self.reap();
        if !self.workers.contains_key(&key) {
            let per_origin = self
                .workers
                .keys()
                .filter(|existing| existing.origin == key.origin)
                .count();
            let bytes: u64 = self.workers.keys().map(|key| key.size).sum();
            ensure!(
                self.workers.len() < MAX_READERS
                    && per_origin < MAX_PER_ORIGIN
                    && bytes
                        .checked_add(key.size)
                        .is_some_and(|bytes| bytes <= MAX_READER_BYTES),
                "blob range readers busy"
            );
            let (requests, receiver) = mpsc::sync_channel(1);
            let path = path.to_owned();
            let worker_key = key.clone();
            let process_lock = process_lock.clone();
            let join = thread::Builder::new()
                .name("blob-range".into())
                .spawn(move || {
                    let _process_lock = process_lock;
                    // Initialization errors are delivered to the first caller, with the same owned reply.
                    let initialized = open_reader(&path, &worker_key);
                    match initialized {
                        Ok(connection) => {
                            let result = serve(&connection, &worker_key, receiver, IDLE, MAX_AGE);
                            if let Err(error) = result {
                                tracing::debug!(error=%error, "blob range worker stopped");
                            }
                            if let Err((connection, error)) = connection.close() {
                                tracing::warn!(error=%error, "blob reader connection close failed");
                                drop(connection);
                            }
                        }
                        Err(error) => {
                            if let Ok(request) = receiver.recv_timeout(REPLY_TIMEOUT) {
                                let _ = request.reply.try_send(Err(format!(
                                    "blob reader initialization failed: {error:#}"
                                )));
                            }
                        }
                    }
                })?;
            self.workers.insert(
                key.clone(),
                ReaderWorker {
                    requests: Some(requests),
                    join: Some(join),
                },
            );
        }
        let worker = self.workers.get_mut(&key).context("blob reader missing")?;
        worker.request(offset, count, REPLY_TIMEOUT)
    }
}

fn open_reader(path: &Path, key: &ReaderKey) -> Result<Connection> {
    use rusqlite::OpenFlags;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA cache_size=-64; PRAGMA mmap_size=0;")?;
    ensure!(
        metadata(&connection, &key.network, &key.hash)?
            == Some(Metadata {
                rowid: key.rowid,
                size: key.size,
            }),
        "blob reader metadata changed"
    );
    Ok(connection)
}

fn serve(
    connection: &Connection,
    key: &ReaderKey,
    receiver: mpsc::Receiver<RangeRequest>,
    idle: Duration,
    max_age: Duration,
) -> Result<()> {
    let blob = connection.blob_open(rusqlite::MAIN_DB, "blobs", "content", key.rowid, true)?;
    let result =
        (|| {
            ensure!(blob.len() as u64 == key.size, "blob reader length changed");
            let opened = Instant::now();
            let mut last_used = opened;
            loop {
                let now = Instant::now();
                let remaining = idle
                    .saturating_sub(now.duration_since(last_used))
                    .min(max_age.saturating_sub(now.duration_since(opened)));
                if remaining.is_zero() {
                    break;
                }
                let request = match receiver.recv_timeout(remaining) {
                    Ok(request) => request,
                    Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                        break;
                    }
                };
                if opened.elapsed() >= max_age || last_used.elapsed() >= idle {
                    let _ = request
                        .reply
                        .try_send(Err("blob range reader expired".into()));
                    break;
                }
                let read = (|| {
                    ensure!(
                        request.count <= 65536
                            && request
                                .offset
                                .checked_add(request.count)
                                .is_some_and(|end| end <= blob.len()),
                        "invalid blob reader range"
                    );
                    let mut bytes = vec![0; request.count];
                    blob.read_at_exact(&mut bytes, request.offset)?;
                    Ok(bytes)
                })();
                let failed = read.is_err();
                if request
                    .reply
                    .try_send(read.map_err(|error: anyhow::Error| {
                        format!("blob range read failed: {error:#}")
                    }))
                    .is_err()
                    || failed
                {
                    break;
                }
                last_used = Instant::now();
            }
            Ok(())
        })();
    let close = blob.close();
    if let Err(error) = close {
        tracing::warn!(error=%error, "blob cursor close failed");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_metadata_rejects_non_blob_content_before_allocating() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch("CREATE TABLE blobs(network_id TEXT,hash TEXT,size INTEGER,content BLOB); INSERT INTO blobs VALUES('n','h',4,X'01020304');")?;
        assert_eq!(metadata(&connection, "n", "h")?.unwrap().size, 4);
        connection.execute("UPDATE blobs SET content='abcd'", [])?;
        assert!(
            metadata(&connection, "n", "h").is_err(),
            "same-length TEXT is not a stored blob"
        );
        connection.execute("UPDATE blobs SET content=X'01020304'", [])?;
        assert_eq!(metadata(&connection, "n", "h")?.unwrap().size, 4);
        Ok(())
    }

    #[test]
    fn timed_out_readers_remain_charged_until_join_and_full_channels_close() -> Result<()> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/network-reliability")
            .join(format!("reader-closing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let lock = Arc::new(std::fs::File::create(dir.join("process-lock"))?);
        let mut readers = BlobReaders::default();
        let mut finishers = Vec::new();
        for index in 0..2 {
            let key = ReaderKey {
                network: "n".into(),
                origin: format!("origin-{index}"),
                hash: format!("hash-{index}"),
                rowid: index,
                size: 64 * 1024 * 1024,
            };
            let (requests, receiver) = mpsc::sync_channel::<RangeRequest>(1);
            let (finish, finished) = mpsc::sync_channel(1);
            let guard = lock.clone();
            let join = thread::spawn(move || {
                let _guard = guard;
                if let Ok(request) = receiver.recv() {
                    finished.recv().unwrap();
                    assert!(
                        request.reply.try_send(Ok(vec![7])).is_err(),
                        "timed-out reply cannot be reused"
                    );
                }
            });
            readers.workers.insert(
                key.clone(),
                ReaderWorker {
                    requests: Some(requests),
                    join: Some(join),
                },
            );
            assert!(
                readers
                    .workers
                    .get_mut(&key)
                    .unwrap()
                    .request(0, 1, Duration::from_millis(5))
                    .is_err()
            );
            assert!(readers.workers[&key].requests.is_none());
            assert!(!readers.workers[&key].finished());
            finishers.push(finish);
        }
        let another = ReaderKey {
            network: "n".into(),
            origin: "new-origin".into(),
            hash: "new".into(),
            rowid: 3,
            size: 1,
        };
        assert!(
            readers
                .read(&dir.join("not-opened.sqlite3"), &lock, another, 0, 1)
                .unwrap_err()
                .to_string()
                .contains("busy")
        );
        assert_eq!(readers.workers.len(), 2);
        for finish in finishers {
            finish.send(())?;
        }
        // Retiring really joins both threads; this is when their reservations and locks disappear.
        readers.retire_network("n");
        assert!(readers.workers.is_empty());
        assert_eq!(Arc::strong_count(&lock), 1);
        let (requests, receiver) = mpsc::sync_channel(1);
        let (reply, _) = mpsc::sync_channel(1);
        requests.send(RangeRequest {
            offset: 0,
            count: 1,
            reply,
        })?;
        let (finish, finished) = mpsc::sync_channel(1);
        let join = thread::spawn(move || {
            finished.recv().unwrap();
            drop(receiver);
        });
        let mut worker = ReaderWorker {
            requests: Some(requests),
            join: Some(join),
        };
        assert!(worker.request(0, 1, Duration::from_millis(5)).is_err());
        assert!(worker.requests.is_none());
        assert!(!worker.finished());
        finish.send(())?;
        drop(worker);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn readonly_cursor_pins_a_read_transaction_and_releases_wal_after_close() -> Result<()> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/network-reliability")
            .join(format!("reader-wal-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("node.sqlite3");
        let writer = Connection::open(&path)?;
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE blobs(network_id TEXT,hash TEXT,size INTEGER,content BLOB); INSERT INTO blobs VALUES('n','h',4,X'01020304'); PRAGMA wal_checkpoint(TRUNCATE);")?;
        let key = ReaderKey {
            network: "n".into(),
            origin: "o".into(),
            hash: "h".into(),
            rowid: 1,
            size: 4,
        };
        let reader = open_reader(&path, &key)?;
        let blob = reader.blob_open(rusqlite::MAIN_DB, "blobs", "content", 1, true)?;
        let state =
            || unsafe { rusqlite::ffi::sqlite3_txn_state(reader.handle(), c"main".as_ptr()) };
        assert_eq!(state(), rusqlite::ffi::SQLITE_TXN_READ);
        assert!(
            reader.is_autocommit(),
            "autocommit=true does not prove the absence of a read transaction"
        );
        for _ in 0..4 {
            writer.execute("INSERT INTO blobs VALUES('n','other',1,X'05')", [])?;
        }
        let pinned: (i64, i64, i64) =
            writer.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        assert!(pinned.1 > pinned.2);
        blob.close()?;
        assert_eq!(state(), rusqlite::ffi::SQLITE_TXN_NONE);
        drop(reader);
        let drained: (i64, i64, i64) =
            writer.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        assert_eq!(drained, (0, 0, 0));
        println!(
            "NET1_WAL_EVIDENCE {}",
            serde_json::json!({"reader_open_txn":"READ","autocommit_while_read":true,"pinned_checkpoint":pinned,"after_close_txn":"NONE","drained_checkpoint":drained})
        );
        drop(writer);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn idle_and_busy_readers_end_at_request_boundaries_and_close_the_snapshot() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch("CREATE TABLE blobs(network_id TEXT,hash TEXT,size INTEGER,content BLOB); INSERT INTO blobs VALUES('n','h',4,X'01020304');")?;
        let key = ReaderKey {
            network: "n".into(),
            origin: "o".into(),
            hash: "h".into(),
            rowid: 1,
            size: 4,
        };
        for hot in [false, true] {
            let (sender, receiver) = mpsc::sync_channel::<RangeRequest>(1);
            let caller = thread::spawn(move || {
                if hot {
                    loop {
                        let (reply, received) = mpsc::sync_channel(1);
                        if sender
                            .send(RangeRequest {
                                offset: 1,
                                count: 2,
                                reply,
                            })
                            .is_err()
                        {
                            break;
                        }
                        match received.recv() {
                            Ok(Ok(bytes)) => assert_eq!(bytes, [2, 3]),
                            _ => break,
                        }
                    }
                } else {
                    thread::sleep(Duration::from_millis(30));
                    assert!(
                        sender
                            .send(RangeRequest {
                                offset: 0,
                                count: 1,
                                reply: mpsc::sync_channel(1).0
                            })
                            .is_err()
                    );
                }
            });
            serve(
                &connection,
                &key,
                receiver,
                Duration::from_millis(10),
                Duration::from_millis(20),
            )?;
            caller.join().unwrap();
            assert!(connection.is_autocommit());
        }
        Ok(())
    }
}
