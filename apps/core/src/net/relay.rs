//! Volunteer relays: checking a relay from outside, adopting the best open relays the mesh
//! announces, metering what each endpoint moves through this Node's relay, and the grants a
//! relay operator signs for paid access.
use super::{Net, RelayUrl, mesh};
use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// The header the relay front sets to its connection number; a client's own copy is removed.
pub const FRONT_HEADER: &str = "x-vibeface-front";
pub const GRANT_DOMAIN: &str = "relay-grant";
/// Adopted relays kept in the relay map.
pub const ADOPTED: usize = 8;
/// Consecutive failed checks that drop a relay.
const MAX_FAILS: i64 = 3;
/// Announced relays checked in one round, most recently seen first.
const CHECKS: usize = 32;
/// A relay checked this recently is not checked again in the next round.
pub const RECHECK: Duration = Duration::from_secs(240);
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// How much one check moves a relay's success score.
const SCORE_WEIGHT: f64 = 0.3;
const PROBE_NEIGHBORS: usize = 3;

fn now_ms() -> u64 {
    crate::runtime::now_ms() as u64
}

/// The relay tables of `mesh.sqlite3`, on a connection of their own.
pub struct RelayDb(Mutex<rusqlite::Connection>);

impl RelayDb {
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let db = crate::store::private_file_connection(path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(&format!("PRAGMA journal_mode=WAL;{}", mesh::table::SCHEMA))?;
        Ok(Arc::new(Self(Mutex::new(db))))
    }

    fn db(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.0.lock().expect("relay db")
    }

    /// Bytes `endpoint` has moved through this Node's relay, both ways.
    pub fn bytes(&self, endpoint: &str) -> Result<u64> {
        let bytes: Option<i64> = self
            .db()
            .query_row(
                "SELECT bytes_in + bytes_out FROM relay_usage WHERE endpoint_id = ?1",
                [endpoint],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                error => Err(error),
            })?;
        Ok(bytes.unwrap_or(0) as u64)
    }

    fn add_usage(&self, endpoint: &str, bytes_in: u64, bytes_out: u64, connections: u64) {
        let now = now_ms() as i64;
        if let Err(error) = self.db().execute(
            "INSERT INTO relay_usage(endpoint_id, bytes_in, bytes_out, connections, first_seen_at,
               last_seen_at) VALUES(?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(endpoint_id) DO UPDATE SET bytes_in = bytes_in + excluded.bytes_in,
               bytes_out = bytes_out + excluded.bytes_out,
               connections = connections + excluded.connections,
               last_seen_at = excluded.last_seen_at",
            rusqlite::params![
                endpoint,
                bytes_in as i64,
                bytes_out as i64,
                connections as i64,
                now
            ],
        ) {
            tracing::warn!(event = "relay.usage_failed", %error);
        }
    }

    /// Records one check of `url`, announced by `device`: its latency, or that it failed.
    pub fn record_check(&self, url: &str, device: &str, latency: Option<Duration>) -> Result<()> {
        let now = now_ms() as i64;
        let ok = latency.is_some();
        let latency = latency.map(|latency| latency.as_millis().min(i64::MAX as u128) as i64);
        // Success counts toward a score that starts halfway, so one check does not settle it.
        self.db().execute(
            "INSERT INTO mesh_relays(url, device_id, score, latency_ms, last_ok_at, last_fail_at,
               fails) VALUES(?1, ?2, 0.5 * (1 - ?6) + ?6 * ?3, ?4, ?5, ?7, ?8)
             ON CONFLICT(url) DO UPDATE SET device_id = excluded.device_id,
               score = score * (1 - ?6) + ?6 * ?3,
               latency_ms = coalesce(excluded.latency_ms, latency_ms),
               last_ok_at = coalesce(excluded.last_ok_at, last_ok_at),
               last_fail_at = coalesce(excluded.last_fail_at, last_fail_at),
               fails = CASE WHEN ?3 = 1.0 THEN 0 ELSE fails + 1 END",
            rusqlite::params![
                url,
                device,
                if ok { 1.0 } else { 0.0 },
                latency,
                ok.then_some(now),
                SCORE_WEIGHT,
                (!ok).then_some(now),
                i64::from(!ok),
            ],
        )?;
        Ok(())
    }

    /// When `url` was last checked, if it was.
    fn checked_at(&self, url: &str) -> Result<Option<u64>> {
        let at: Option<Option<i64>> = self
            .db()
            .query_row(
                "SELECT max(coalesce(last_ok_at, 0), coalesce(last_fail_at, 0)) FROM mesh_relays
                 WHERE url = ?1",
                [url],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                error => Err(error),
            })?;
        Ok(at.flatten().map(|at| at as u64))
    }

    /// The best `keep` of `urls`: ones that answered and have not failed three checks in a row,
    /// by success score over latency.
    pub fn best(&self, urls: &[String], keep: usize) -> Result<Vec<String>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT score, latency_ms, fails FROM mesh_relays WHERE url = ?1
             AND latency_ms IS NOT NULL",
        )?;
        let mut ranked = Vec::new();
        for url in urls {
            let row: Option<(f64, i64, i64)> = statement
                .query_row([url], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .map(Some)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    error => Err(error),
                })?;
            if let Some((score, latency, fails)) = row
                && fails < MAX_FAILS
            {
                ranked.push((score / (latency as f64 + 50.0), url.clone()));
            }
        }
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        Ok(ranked.into_iter().take(keep).map(|(_, url)| url).collect())
    }
}

/// What each endpoint moves through this Node's relay. The front numbers its connections and
/// counts their bytes; the relay names the endpoint behind a number once it admits it.
#[derive(Default)]
pub struct Usage {
    db: Option<Arc<RelayDb>>,
    next: AtomicU64,
    live: Mutex<HashMap<u64, Live>>,
}

struct Live {
    endpoint: Option<String>,
    counter: Arc<Counter>,
}

#[derive(Default)]
pub(super) struct Counter {
    pub(super) read: AtomicU64,
    pub(super) written: AtomicU64,
}

/// Flushes and forgets a front connection when it ends.
pub(super) struct FrontGuard(Arc<Usage>, u64);
impl Drop for FrontGuard {
    fn drop(&mut self) {
        self.0.close_front(self.1);
    }
}

impl Usage {
    pub fn new(db: Option<Arc<RelayDb>>) -> Arc<Self> {
        Arc::new(Self {
            db,
            ..Default::default()
        })
    }

    pub fn db(&self) -> Option<&Arc<RelayDb>> {
        self.db.as_ref()
    }

    pub(super) fn open_front(self: &Arc<Self>) -> (u64, Arc<Counter>, FrontGuard) {
        let front = self.next.fetch_add(1, Ordering::Relaxed);
        let counter = Arc::new(Counter::default());
        self.live.lock().expect("relay usage").insert(
            front,
            Live {
                endpoint: None,
                counter: counter.clone(),
            },
        );
        (front, counter, FrontGuard(self.clone(), front))
    }

    /// The relay admitted `request`: its front connection carries this endpoint from now on.
    pub fn bind(&self, request: &iroh_relay::server::ClientRequest) {
        let Some(front) = request
            .headers()
            .get(FRONT_HEADER)
            .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
        else {
            return;
        };
        let endpoint = super::device_id(&request.endpoint_id());
        let bound = match self.live.lock().expect("relay usage").get_mut(&front) {
            Some(live) if live.endpoint.is_none() => {
                live.endpoint = Some(endpoint.clone());
                true
            }
            _ => false,
        };
        if bound && let Some(db) = &self.db {
            db.add_usage(&endpoint, 0, 0, 1);
        }
    }

    fn take(&self, live: &Live) -> Option<(String, u64, u64)> {
        let endpoint = live.endpoint.clone()?;
        let read = live.counter.read.swap(0, Ordering::Relaxed);
        let written = live.counter.written.swap(0, Ordering::Relaxed);
        (read > 0 || written > 0).then_some((endpoint, read, written))
    }

    /// Writes what each named connection moved since the last flush.
    pub(super) fn flush(&self) {
        let pending: Vec<_> = self
            .live
            .lock()
            .expect("relay usage")
            .values()
            .filter_map(|live| self.take(live))
            .collect();
        self.write(pending);
    }

    fn close_front(&self, front: u64) {
        let live = self.live.lock().expect("relay usage").remove(&front);
        self.write(live.iter().filter_map(|live| self.take(live)).collect());
    }

    fn write(&self, pending: Vec<(String, u64, u64)>) {
        if let Some(db) = &self.db {
            for (endpoint, read, written) in pending {
                db.add_usage(&endpoint, read, written, 0);
            }
        }
    }
}

/// Reads the HTTP request head, at most 16 KiB, and returns it with the front's header in place
/// of any the client sent, followed by whatever came after the head.
pub(super) async fn read_head(
    client: &mut tokio::net::TcpStream,
    front: u64,
) -> Option<(Vec<u8>, usize)> {
    use tokio::io::AsyncReadExt as _;
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let read = client.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let received = buffer.len();
            return Some((rewrite_head(&buffer, end + 4, front), received));
        }
        if buffer.len() > 16 * 1024 {
            return None;
        }
    }
}

fn rewrite_head(bytes: &[u8], end: usize, front: u64) -> Vec<u8> {
    let head = &bytes[..end - 4];
    let mut lines = head
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    let mut out = Vec::with_capacity(bytes.len() + 32);
    out.extend_from_slice(lines.next().unwrap_or_default());
    out.extend_from_slice(format!("\r\n{FRONT_HEADER}: {front}\r\n").as_bytes());
    for line in lines {
        let name = line.split(|byte| *byte == b':').next().unwrap_or_default();
        if name
            .trim_ascii()
            .eq_ignore_ascii_case(FRONT_HEADER.as_bytes())
        {
            continue;
        }
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&bytes[end..]);
    out
}

/// Copies `from` to `to`, counting the bytes in `count`.
pub(super) async fn pump(
    mut from: impl tokio::io::AsyncRead + Unpin,
    mut to: impl tokio::io::AsyncWrite + Unpin,
    count: &AtomicU64,
) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let read = match from.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        if to.write_all(&buffer[..read]).await.is_err() {
            return;
        }
        count.fetch_add(read as u64, Ordering::Relaxed);
    }
}

/// A relay operator's permission for one endpoint to use its relay until `expires_at_ms`, for
/// at most `max_bytes` when set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayGrant {
    pub v: u8,
    /// The operator's device ID, which signs the grant.
    pub relay: String,
    /// The device ID the grant admits.
    pub endpoint: String,
    pub expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

impl RelayGrant {
    /// The token text: `base64url(payload) "." signature`, `sign` signing in [`GRANT_DOMAIN`].
    pub fn token(&self, sign: impl FnOnce(&[u8]) -> Result<String>) -> Result<String> {
        let payload = serde_json::to_vec(self)?;
        let signature = sign(&payload)?;
        Ok(format!("{}.{signature}", URL_SAFE_NO_PAD.encode(&payload)))
    }

    /// Whether `token` is a grant rather than an operator or invite token.
    pub fn is_grant(token: &str) -> bool {
        token.contains('.')
    }

    /// The grant `token` carries, if `relay` signed it for `endpoint` and it has not expired.
    pub fn open(token: &str, relay: &str, endpoint: &str, now_ms: u64) -> Result<Self> {
        ensure!(token.len() <= 4096, "relay grant too large");
        let (payload, signature) = token.split_once('.').context("not a relay grant")?;
        let payload = URL_SAFE_NO_PAD
            .decode(payload)
            .context("invalid relay grant")?;
        vibeface_secure::verify(relay, GRANT_DOMAIN, &payload, signature)
            .context("the relay grant is not this relay's")?;
        let grant: Self = serde_json::from_slice(&payload).context("invalid relay grant")?;
        ensure!(
            grant.v == 1 && grant.relay == relay,
            "the relay grant is not this relay's"
        );
        ensure!(
            grant.endpoint == endpoint,
            "the relay grant is for another device"
        );
        ensure!(now_ms < grant.expires_at_ms, "the relay grant has expired");
        Ok(grant)
    }
}

/// Checks `url` as a device elsewhere would use it: `/generate_204` answers 204 and a relay
/// handshake with a throwaway key is admitted. Returns how long `/generate_204` took. A relay on
/// a private or loopback address is refused unless `allow_private`.
pub async fn check_relay(url: &str, allow_private: bool) -> Result<Duration> {
    let relay = super::relay_url(url)?;
    let parsed = reqwest::Url::parse(url)?;
    let host = parsed.host_str().context("the relay has no host")?;
    let port = parsed
        .port_or_known_default()
        .context("the relay has no port")?;
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.trim_matches(['[', ']']), port))
        .await
        .context("the relay does not resolve")?
        .collect();
    let address = *addresses.first().context("the relay does not resolve")?;
    ensure!(
        allow_private
            || (mesh::public_host(&parsed)
                && addresses
                    .iter()
                    .all(|address| crate::fetch::public(address.ip()))),
        "the relay is on a private network"
    );
    let mut client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(CHECK_TIMEOUT);
    if let Some(domain) = parsed.domain() {
        // Connect to the address checked above, not to a second lookup's answer.
        client = client.resolve(domain, address);
    }
    let started = Instant::now();
    let status = client
        .build()?
        .get(parsed.join("/generate_204")?)
        .send()
        .await
        .context("the relay is unreachable")?
        .status();
    let latency = started.elapsed();
    ensure!(
        status == reqwest::StatusCode::NO_CONTENT,
        "the relay answered {status} to /generate_204"
    );
    let tls = iroh_relay::tls::CaTlsConfig::default()
        .client_config(iroh_relay::tls::default_provider())
        .context("relay TLS is unavailable")?;
    let handshake = iroh_relay::client::ClientBuilder::new(
        relay,
        iroh::SecretKey::generate(),
        iroh::dns::DnsResolver::new(),
    )
    .tls_client_config(tls);
    let connected = tokio::time::timeout(CHECK_TIMEOUT, handshake.connect())
        .await
        .context("the relay handshake timed out")?
        .context("the relay refused a handshake")?;
    drop(connected);
    Ok(latency)
}

/// Asks up to three mesh neighbors to check `url` from outside; passes when one could use it.
pub async fn probe_through(mesh: &mesh::Mesh, url: &str) -> Result<()> {
    let neighbors = mesh.neighbors();
    ensure!(
        !neighbors.is_empty(),
        "no mesh neighbor can check the relay yet"
    );
    let request = json!({"type":"probe_relay","url":url});
    let mut failure = None;
    for neighbor in neighbors.iter().take(PROBE_NEIGHBORS) {
        match mesh.request(neighbor, &request).await {
            Ok(reply) if reply["ok"] == true => return Ok(()),
            Ok(_) => failure = Some(anyhow::anyhow!("the neighbor could not check the relay")),
            Err(error) => failure = Some(error),
        }
    }
    Err(failure
        .unwrap_or_else(|| anyhow::anyhow!("no neighbor checked the relay"))
        .context("a neighbor could not reach the relay"))
}

/// An open relay another Node announces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub url: String,
    pub device_id: String,
}

/// The open relays `entries` announce, most recently seen first, other than `own_device`'s and
/// `own_relay`, and other than ones on private hosts unless `allow_private`.
pub fn candidates(
    entries: &[mesh::table::Entry],
    own_device: &str,
    own_relay: Option<&RelayUrl>,
    allow_private: bool,
) -> Vec<Candidate> {
    let mut entries: Vec<&mesh::table::Entry> = entries
        .iter()
        .filter(|entry| entry.record.device_id != own_device)
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_seen_ms));
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for entry in entries {
        for role in &entry.record.roles {
            let mesh::record::Role::Relay {
                url,
                access: mesh::record::Access::Open,
                ..
            } = role
            else {
                continue;
            };
            let Ok(parsed) = super::relay_url(url) else {
                continue;
            };
            let public = reqwest::Url::parse(url).is_ok_and(|url| mesh::public_host(&url));
            if Some(&parsed) == own_relay
                || !(allow_private || public)
                || !seen.insert(parsed.clone())
            {
                continue;
            }
            found.push(Candidate {
                url: parsed.to_string(),
                device_id: entry.record.device_id.clone(),
            });
        }
    }
    found.truncate(CHECKS);
    found
}

/// Checks the candidates not checked in the last few minutes with `check`, records the results,
/// and keeps the best eight of the candidates in `net`'s relay map in place of those adopted
/// before. Returns the adopted URLs.
pub async fn adopt<F, Fut>(
    net: &Net,
    db: &RelayDb,
    candidates: Vec<Candidate>,
    check: F,
) -> Result<Vec<String>>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<Duration>>,
{
    let due_before = now_ms().saturating_sub(RECHECK.as_millis() as u64);
    let mut due = Vec::new();
    for candidate in &candidates {
        if db
            .checked_at(&candidate.url)?
            .is_none_or(|at| at <= due_before)
        {
            due.push(candidate.clone());
        }
    }
    let results =
        futures_util::future::join_all(due.iter().map(|candidate| check(candidate.url.clone())))
            .await;
    for (candidate, result) in due.iter().zip(results) {
        if let Err(error) = &result {
            tracing::debug!(event = "relay.check_failed", url = %candidate.url, %error);
        }
        db.record_check(&candidate.url, &candidate.device_id, result.ok())?;
    }
    let urls: Vec<String> = candidates
        .into_iter()
        .map(|candidate| candidate.url)
        .collect();
    let best = db.best(&urls, ADOPTED)?;
    let adopted = best
        .iter()
        .filter_map(|url| super::relay_url(url).ok())
        .collect();
    net.set_adopted(adopted).await;
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    fn dir(label: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/relay-tests")
            .join(format!("{label}-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn the_front_replaces_a_header_the_client_sent() {
        let request =
            b"GET /relay HTTP/1.1\r\nHost: r\r\nX-Vibeface-Front : 9\r\nUpgrade: websocket\r\n\r\nrest";
        let end = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert_eq!(
            String::from_utf8(rewrite_head(request, end, 3)).unwrap(),
            "GET /relay HTTP/1.1\r\nx-vibeface-front: 3\r\nHost: r\r\nUpgrade: websocket\r\n\r\nrest"
        );
    }

    #[test]
    fn a_grant_admits_its_endpoint_until_it_expires() -> Result<()> {
        let operator = crate::crypto::LocalIdentity::generate();
        let relay = operator.device_id()?;
        let endpoint = crate::crypto::LocalIdentity::generate().device_id()?;
        let other = crate::crypto::LocalIdentity::generate().device_id()?;
        let grant = RelayGrant {
            v: 1,
            relay: relay.clone(),
            endpoint: endpoint.clone(),
            expires_at_ms: 2_000,
            max_bytes: Some(1 << 20),
        };
        let token = grant.token(|payload| operator.sign_payload(GRANT_DOMAIN, payload))?;
        assert!(RelayGrant::is_grant(&token));
        assert!(!RelayGrant::is_grant(&"ab".repeat(32)));
        assert_eq!(RelayGrant::open(&token, &relay, &endpoint, 1_999)?, grant);
        let refused = |token: &str, relay: &str, endpoint: &str, now: u64| {
            RelayGrant::open(token, relay, endpoint, now)
                .unwrap_err()
                .to_string()
        };
        assert!(refused(&token, &relay, &endpoint, 2_000).contains("expired"));
        assert!(refused(&token, &relay, &other, 1_000).contains("another device"));
        assert!(refused(&token, &other, &endpoint, 1_000).contains("not this relay's"));
        // Signed by someone else for this relay.
        let forger = crate::crypto::LocalIdentity::generate();
        let forged = grant.token(|payload| forger.sign_payload(GRANT_DOMAIN, payload))?;
        assert!(refused(&forged, &relay, &endpoint, 1_000).contains("not this relay's"));
        // The operator's signature in another domain.
        let elsewhere = grant.token(|payload| operator.sign_payload("address", payload))?;
        assert!(refused(&elsewhere, &relay, &endpoint, 1_000).contains("not this relay's"));
        // A payload changed after signing.
        let (_, signature) = token.split_once('.').unwrap();
        let longer = RelayGrant {
            expires_at_ms: u64::MAX,
            ..grant.clone()
        };
        let tampered = format!(
            "{}.{signature}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&longer)?)
        );
        assert!(refused(&tampered, &relay, &endpoint, 1_000).contains("not this relay's"));
        Ok(())
    }

    /// Admits anyone and names the endpoint behind each front connection, as the Node's does.
    #[derive(Debug)]
    struct Metered(Arc<Usage>);
    impl iroh_relay::server::AccessControl for Metered {
        async fn on_connect(
            &self,
            request: &iroh_relay::server::ClientRequest,
        ) -> iroh_relay::server::Access {
            self.0.bind(request);
            iroh_relay::server::Access::Allow
        }
    }

    impl std::fmt::Debug for Usage {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Usage")
        }
    }

    async fn metered_relay(usage: Arc<Usage>) -> Result<(super::super::RelayServer, String)> {
        let relay = super::super::spawn_relay(
            "127.0.0.1:0".parse()?,
            Arc::new(Metered(usage.clone())),
            None,
            NonZeroUsize::new(8),
            usage,
        )
        .await?;
        let url = format!("http://{}", relay.addr);
        Ok((relay, url))
    }

    /// A relay handshake through the front is written to `relay_usage` under the endpoint's
    /// device ID once the connection ends; a plain HTTP check names no endpoint.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_front_counts_each_endpoints_bytes() -> Result<()> {
        let dir = dir("usage");
        let db = RelayDb::open(&dir.join("mesh.sqlite3"))?;
        let usage = Usage::new(Some(db.clone()));
        let (relay, url) = metered_relay(usage.clone()).await?;
        check_relay(&url, true).await?;
        let key = iroh::SecretKey::generate();
        let endpoint = super::super::device_id(&key.public());
        let tls = iroh_relay::tls::CaTlsConfig::default()
            .client_config(iroh_relay::tls::default_provider())?;
        let client = iroh_relay::client::ClientBuilder::new(
            super::super::relay_url(&url)?,
            key,
            iroh::dns::DnsResolver::new(),
        )
        .tls_client_config(tls)
        .connect()
        .await?;
        drop(client);
        let row = || -> Result<Option<(i64, i64, i64)>> {
            let db = db.db();
            let mut statement = db.prepare(
                "SELECT bytes_in, bytes_out, connections FROM relay_usage WHERE endpoint_id = ?1",
            )?;
            let mut rows = statement.query([&endpoint])?;
            Ok(match rows.next()? {
                Some(row) => Some((row.get(0)?, row.get(1)?, row.get(2)?)),
                None => None,
            })
        };
        let counted = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some((read, written, connections)) = row()?
                    && read > 0
                    && written > 0
                {
                    return Ok::<_, anyhow::Error>(connections);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("the front never wrote the endpoint's bytes")??;
        assert_eq!(counted, 1);
        assert_eq!(
            db.db()
                .query_row("SELECT count(*) FROM relay_usage", [], |row| row
                    .get::<_, i64>(0))?,
            2,
            "the check's handshake and the client; its /generate_204 request names no endpoint"
        );
        assert!(db.bytes(&endpoint)? > 0);
        relay.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_relay_check_needs_a_live_relay_on_an_allowed_address() -> Result<()> {
        let (relay, url) = metered_relay(Usage::new(None)).await?;
        check_relay(&url, true).await?;
        let private = check_relay(&url, false).await.unwrap_err().to_string();
        assert!(private.contains("private network"), "{private}");
        relay.shutdown().await;
        assert!(
            check_relay(&url, true).await.is_err(),
            "a stopped relay fails"
        );
        // An HTTP server that is not a relay answers /generate_204 with something else.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                    .await;
            }
        });
        let other = check_relay(&format!("http://{addr}"), true)
            .await
            .unwrap_err()
            .to_string();
        assert!(other.contains("/generate_204"), "{other}");
        server.abort();
        Ok(())
    }

    /// A Node answers `probe_relay` by checking the relay itself, once a minute per device.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_neighbor_probes_a_relay_from_outside() -> Result<()> {
        let dir = dir("probe");
        let config = super::super::NetworkConfig {
            bind: Some("127.0.0.1:0".parse()?),
            ..Default::default()
        };
        let secret = crate::crypto::LocalIdentity::generate()
            .secret_key_hex
            .clone();
        let net = Net::bind(&secret, &config, 0, vec![]).await?;
        let mesh_config = mesh::MeshConfig {
            enabled: true,
            network: "probe".into(),
            bootstrap: vec![],
            allow_private: true,
        };
        net.start_mesh(&mesh_config, &dir.join("mesh.sqlite3"))
            .await?;
        let mesh = net.mesh().context("no mesh")?;
        let (relay, url) = metered_relay(Usage::new(None)).await?;
        let stranger = || {
            super::super::endpoint_id(
                &crate::crypto::LocalIdentity::generate()
                    .device_id()
                    .unwrap(),
            )
            .unwrap()
        };
        let asker = stranger();
        let probe = json!({"type":"probe_relay","url":url});
        let reply = mesh.answer(asker, probe.clone()).await?;
        assert_eq!(reply["ok"], true);
        let again = mesh.answer(asker, probe.clone()).await.unwrap_err();
        assert!(again.to_string().contains("once a minute"), "{again}");
        relay.shutdown().await;
        assert!(
            mesh.answer(stranger(), probe).await.is_err(),
            "a relay that stopped fails the probe"
        );
        assert!(
            probe_through(&mesh, &url).await.is_err(),
            "without a neighbor nothing is announced"
        );
        net.close().await;
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    fn entry(
        identity: &vibeface_secure::Identity,
        seen: u64,
        roles: Vec<mesh::record::Role>,
    ) -> mesh::table::Entry {
        let record = mesh::record::MeshRecord {
            v: 1,
            network: "lab".into(),
            device_id: identity.device_id(),
            seq: now_ms(),
            ttl_s: mesh::record::DEFAULT_TTL_S,
            addrs: vec![],
            relay_url: None,
            reachable: mesh::record::Reachable::Unknown,
            roles,
        };
        let signed = mesh::record::SignedMeshRecord::sign(&record, |payload| {
            Ok(identity.sign(mesh::record::DOMAIN, payload)?)
        })
        .unwrap();
        mesh::table::Entry {
            signed,
            record,
            last_seen_ms: seen,
        }
    }

    fn relay_role(url: &str, access: mesh::record::Access) -> mesh::record::Role {
        mesh::record::Role::Relay {
            url: url.into(),
            access,
            rate: 16384,
            max_clients: 128,
            offer: None,
        }
    }

    #[test]
    fn candidates_are_other_nodes_open_relays_on_allowed_hosts() -> Result<()> {
        use mesh::record::Access;
        let own = vibeface_secure::Identity::generate()?;
        let a = vibeface_secure::Identity::generate()?;
        let b = vibeface_secure::Identity::generate()?;
        let entries = vec![
            entry(
                &own,
                9,
                vec![relay_role("https://own.example", Access::Open)],
            ),
            entry(
                &a,
                1,
                vec![
                    relay_role("https://a.example", Access::Open),
                    relay_role("http://127.0.0.1:3340", Access::Open),
                ],
            ),
            entry(
                &b,
                2,
                vec![
                    relay_role("https://paid.example", Access::Paid),
                    relay_role("https://token.example", Access::Token),
                    relay_role("https://b.example", Access::Open),
                    relay_role("https://mine.example", Access::Open),
                ],
            ),
        ];
        let mine = super::super::relay_url("https://mine.example")?;
        let urls = |allow_private| {
            candidates(&entries, &own.device_id(), Some(&mine), allow_private)
                .into_iter()
                .map(|candidate| candidate.url)
                .collect::<Vec<_>>()
        };
        assert_eq!(urls(false), ["https://b.example/", "https://a.example/"]);
        assert_eq!(
            urls(true),
            [
                "https://b.example/",
                "https://a.example/",
                "http://127.0.0.1:3340/"
            ]
        );
        Ok(())
    }

    /// The relays `net` keeps in its relay map.
    async fn in_map(net: &Net) -> HashSet<String> {
        let pinned = net.relays.lock().await;
        let adopted = net.adopted.lock().await;
        pinned.union(&adopted).map(ToString::to_string).collect()
    }

    /// Ten relays answer; the eight fastest go into the relay map. A relay that fails three
    /// checks in a row or is no longer announced leaves it, and a configured relay stays.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_best_eight_relays_are_adopted() -> Result<()> {
        let dir = dir("adopt");
        let db = RelayDb::open(&dir.join("mesh.sqlite3"))?;
        let pinned = "http://127.0.0.1:1/".to_owned();
        let config = super::super::NetworkConfig {
            bind: Some("127.0.0.1:0".parse()?),
            relays: vec![super::super::Relay {
                url: pinned.clone(),
                token: None,
            }],
            ..Default::default()
        };
        let secret = crate::crypto::LocalIdentity::generate()
            .secret_key_hex
            .clone();
        let net = Net::bind(&secret, &config, 0, vec![]).await?;
        let url = |port: u16| format!("http://127.0.0.1:{}/", 10 + port);
        let device = crate::crypto::LocalIdentity::generate().device_id()?;
        let candidate = |url: String| Candidate {
            url,
            device_id: device.clone(),
        };
        let mut all: Vec<Candidate> = (0..10).map(|port| candidate(url(port))).collect();
        all.push(candidate(pinned.clone()));
        let failing = Arc::new(Mutex::new(HashSet::<String>::new()));
        let check = |url: String| {
            let failing = failing.clone();
            async move {
                anyhow::ensure!(!failing.lock().unwrap().contains(&url), "down");
                // Lower ports answer faster; the configured relay is slowest.
                let port: u64 = url
                    .trim_end_matches('/')
                    .rsplit(':')
                    .next()
                    .unwrap()
                    .parse()?;
                Ok(Duration::from_millis(if port == 1 {
                    900
                } else {
                    port * 10
                }))
            }
        };
        let adopted = adopt(&net, &db, all.clone(), check).await?;
        assert_eq!(adopted, (0..8).map(url).collect::<Vec<_>>());
        let map = in_map(&net).await;
        assert!((0..8).all(|port| map.contains(&url(port))));
        assert!(!map.contains(&url(8)) && !map.contains(&url(9)));
        assert!(map.contains(&pinned));

        // Relay 0 goes down and relay 1 stops being announced; the next two step in.
        failing.lock().unwrap().insert(url(0));
        all.retain(|candidate| candidate.url != url(1));
        let recheck = || {
            db.db()
                .execute(
                    "UPDATE mesh_relays SET last_ok_at = 0, last_fail_at = 0",
                    [],
                )
                .unwrap()
        };
        for _ in 0..3 {
            recheck();
            adopt(&net, &db, all.clone(), check).await?;
        }
        let map = in_map(&net).await;
        assert!(!map.contains(&url(0)) && !map.contains(&url(1)));
        assert!(map.contains(&url(8)) && map.contains(&url(9)));
        assert!(
            adopt(&net, &db, vec![candidate(url(0))], check)
                .await?
                .is_empty(),
            "three failures in a row drop a relay even with nothing better"
        );
        let failed_once = candidate(url(2));
        failing.lock().unwrap().insert(url(2));
        recheck();
        assert_eq!(
            adopt(&net, &db, vec![failed_once], check).await?,
            [url(2)],
            "one failure lowers a relay's score but keeps it"
        );
        // Adopting nothing leaves the configured relay in place.
        adopt(&net, &db, vec![], check).await?;
        let map = in_map(&net).await;
        assert!(map.contains(&pinned) && !map.contains(&url(2)));
        assert!(
            net.adopted.lock().await.is_empty(),
            "a configured relay is never adopted"
        );
        net.close().await;
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }
}
