//! Volunteer relays: checking a relay from outside, adopting the best open relays the mesh
//! announces, metering what each endpoint moves through this Node's relay, and the grants a
//! relay operator signs for paid access.
use super::{Net, RelayUrl, mesh};
use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
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
/// Announced relays taken into one round: those with the longest record of passed checks,
/// then others drawn at random from the whole table.
const CHECKS: usize = 32;
/// Relays of one network prefix taken into a round, and adopted.
const PER_PREFIX_CHECKED: usize = 2;
const PER_PREFIX_ADOPTED: usize = 1;
/// A relay checked this recently is not checked again in the next round.
pub const RECHECK: Duration = Duration::from_secs(240);
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// A relay's history counts its passed checks up to this; a failed check halves it.
const MAX_HISTORY: i64 = 288;
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
        let db = Arc::new(Self(Mutex::new(db)));
        db.prune()?;
        Ok(db)
    }

    /// Forgets relays not checked, and endpoints not seen, in the last 30 days.
    pub fn prune(&self) -> Result<()> {
        let before = now_ms().saturating_sub(mesh::table::KEEP_MS) as i64;
        let db = self.db();
        db.execute(
            "DELETE FROM mesh_relays
             WHERE max(coalesce(last_ok_at, 0), coalesce(last_fail_at, 0)) < ?1",
            [before],
        )?;
        db.execute("DELETE FROM relay_usage WHERE last_seen_at < ?1", [before])?;
        Ok(())
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

    /// Records one check of `url`, announced by `device`: its latency and the address it was
    /// reached on, or that it failed.
    pub fn record_check(
        &self,
        url: &str,
        device: &str,
        passed: Option<(Duration, IpAddr)>,
    ) -> Result<()> {
        let now = now_ms() as i64;
        let ok = passed.is_some();
        let latency = passed.map(|(latency, _)| latency.as_millis().min(i64::MAX as u128) as i64);
        let ip = passed.map(|(_, ip)| ip.to_canonical().to_string());
        self.db().execute(
            "INSERT INTO mesh_relays(url, device_id, history, latency_ms, ip, last_ok_at,
               last_fail_at, fails) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(url) DO UPDATE SET device_id = excluded.device_id,
               history = CASE WHEN ?3 = 1 THEN min(history + 1, ?9) ELSE history / 2 END,
               latency_ms = coalesce(excluded.latency_ms, latency_ms),
               ip = coalesce(excluded.ip, ip),
               last_ok_at = coalesce(excluded.last_ok_at, last_ok_at),
               last_fail_at = coalesce(excluded.last_fail_at, last_fail_at),
               fails = CASE WHEN ?3 = 1 THEN 0 ELSE fails + 1 END",
            rusqlite::params![
                url,
                device,
                i64::from(ok),
                latency,
                ip,
                ok.then_some(now),
                (!ok).then_some(now),
                i64::from(!ok),
                MAX_HISTORY,
            ],
        )?;
        Ok(())
    }

    /// What earlier checks found about `url`, if it was checked.
    fn known(&self, url: &str) -> Result<Option<Known>> {
        self.db()
            .query_row(
                "SELECT max(coalesce(last_ok_at, 0), coalesce(last_fail_at, 0)), history, ip,
                   latency_ms, fails
                 FROM mesh_relays WHERE url = ?1",
                [url],
                |row| {
                    Ok(Known {
                        checked_at: row.get::<_, i64>(0)? as u64,
                        history: row.get(1)?,
                        ip: row
                            .get::<_, Option<String>>(2)?
                            .and_then(|ip| ip.parse().ok()),
                        latency_ms: row.get(3)?,
                        fails: row.get(4)?,
                    })
                },
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                error => Err(error),
            })
            .map_err(Into::into)
    }

    /// The best `keep` of `urls`: ones that answered and have not failed three checks in a
    /// row, by their history of passed checks, then latency, one a network prefix.
    pub fn best(&self, urls: &[String], keep: usize) -> Result<Vec<String>> {
        let mut ranked = Vec::new();
        for url in urls {
            if let Some(known) = self.known(url)?
                && let (Some(latency), Some(ip)) = (known.latency_ms, known.ip)
                && known.fails < MAX_FAILS
            {
                ranked.push((known.history, latency, prefix(ip), url.clone()));
            }
        }
        ranked.sort_by_key(|(history, latency, _, _)| (std::cmp::Reverse(*history), *latency));
        let mut taken: HashMap<Prefix, usize> = HashMap::new();
        Ok(ranked
            .into_iter()
            .filter(|(_, _, prefix, _)| {
                let count = taken.entry(*prefix).or_default();
                *count += 1;
                *count <= PER_PREFIX_ADOPTED
            })
            .take(keep)
            .map(|(_, _, _, url)| url)
            .collect())
    }
}

struct Known {
    checked_at: u64,
    history: i64,
    ip: Option<IpAddr>,
    latency_ms: Option<i64>,
    fails: i64,
}

/// A relay host's network: its IPv4 /24 or IPv6 /48. Many relays in one network are likely one
/// operator's, so they count as one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Prefix {
    V4([u8; 3]),
    V6([u16; 3]),
}

fn prefix(ip: IpAddr) -> Prefix {
    match ip.to_canonical() {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            Prefix::V4([a, b, c])
        }
        IpAddr::V6(ip) => {
            let [a, b, c, ..] = ip.segments();
            Prefix::V6([a, b, c])
        }
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

/// Answers every lookup with the one address a check already resolved and vetted, so the relay
/// handshake connects there and not to a second lookup's answer.
#[derive(Debug)]
struct Pinned(IpAddr);

impl iroh::dns::Resolver for Pinned {
    fn lookup_ipv4(
        &self,
        _host: String,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<iroh::dns::BoxIter<Ipv4Addr>, iroh::dns::DnsError>,
    > {
        let ip = match self.0 {
            IpAddr::V4(ip) => Some(ip),
            IpAddr::V6(_) => None,
        };
        Box::pin(async move { Ok(Box::new(ip.into_iter()) as iroh::dns::BoxIter<_>) })
    }

    fn lookup_ipv6(
        &self,
        _host: String,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<iroh::dns::BoxIter<Ipv6Addr>, iroh::dns::DnsError>,
    > {
        let ip = match self.0 {
            IpAddr::V6(ip) => Some(ip),
            IpAddr::V4(_) => None,
        };
        Box::pin(async move { Ok(Box::new(ip.into_iter()) as iroh::dns::BoxIter<_>) })
    }

    fn lookup_txt(
        &self,
        _host: String,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<iroh::dns::BoxIter<iroh::dns::TxtRecordData>, iroh::dns::DnsError>,
    > {
        Box::pin(async { Ok(Box::new(std::iter::empty()) as iroh::dns::BoxIter<_>) })
    }

    fn clear_cache(&self) {}

    fn reset(&self) -> Box<dyn iroh::dns::Resolver> {
        Box::new(Self(self.0))
    }
}

/// Checks `url` as a device elsewhere would use it: `/generate_204` answers 204 and a relay
/// handshake with a throwaway key is admitted, both on the one address resolved and vetted.
/// Returns how long `/generate_204` took and that address. A relay on a private or loopback
/// address is refused unless `allow_private`.
pub async fn check_relay(url: &str, allow_private: bool) -> Result<(Duration, IpAddr)> {
    let relay = super::relay_url(url)?;
    let parsed = reqwest::Url::parse(url)?;
    let host = parsed.host_str().context("the relay has no host")?;
    let port = parsed
        .port_or_known_default()
        .context("the relay has no port")?;
    let addresses: Vec<SocketAddr> = tokio::time::timeout(
        CHECK_TIMEOUT,
        tokio::net::lookup_host((host.trim_matches(['[', ']']), port)),
    )
    .await
    .context("the relay did not resolve in time")?
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
        .connect_timeout(CHECK_TIMEOUT)
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
        iroh::dns::DnsResolver::custom(Pinned(address.ip())),
    )
    .tls_client_config(tls);
    let connected = tokio::time::timeout(CHECK_TIMEOUT, handshake.connect())
        .await
        .context("the relay handshake timed out")?
        .context("the relay refused a handshake")?;
    drop(connected);
    Ok((latency, address.ip()))
}

/// Asks up to three mesh neighbors to check `url` from outside; passes when one could use it.
pub async fn probe_through(mesh: &mesh::Mesh, url: &str) -> Result<()> {
    let neighbors = mesh.neighbors();
    ensure!(
        !neighbors.is_empty(),
        "no mesh neighbor can check the relay yet"
    );
    let request = mesh::Request::ProbeRelay { url: url.into() };
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

/// The open relays `entries` announce, other than `own_device`'s and `own_relay`, and other
/// than ones on private hosts unless `allow_private`.
pub fn candidates(
    entries: &[mesh::table::Entry],
    own_device: &str,
    own_relay: Option<&RelayUrl>,
    allow_private: bool,
) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.record.device_id != own_device)
    {
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
    found
}

/// The candidates one round takes, at most 32: those with the longest history of passed checks
/// first, then the rest in random order, at most two of any network prefix known from an
/// earlier check or the URL's address. Each comes with whether it is due for a check.
fn sample(db: &RelayDb, candidates: Vec<Candidate>) -> Result<Vec<(Candidate, bool)>> {
    use rand::seq::SliceRandom as _;
    let due_before = now_ms().saturating_sub(RECHECK.as_millis() as u64);
    let mut known = Vec::new();
    let mut fresh = Vec::new();
    for candidate in candidates {
        match db.known(&candidate.url)? {
            Some(info) if info.history > 0 => known.push((candidate, Some(info))),
            info => fresh.push((candidate, info)),
        }
    }
    known.sort_by_key(|(_, info)| std::cmp::Reverse(info.as_ref().map_or(0, |info| info.history)));
    fresh.shuffle(&mut rand::thread_rng());
    let mut taken: HashMap<Prefix, usize> = HashMap::new();
    let mut sampled = Vec::new();
    for (candidate, info) in known.into_iter().chain(fresh) {
        if sampled.len() == CHECKS {
            break;
        }
        let ip = info.as_ref().and_then(|info| info.ip).or_else(|| {
            reqwest::Url::parse(&candidate.url)
                .ok()?
                .host_str()?
                .trim_matches(['[', ']'])
                .parse()
                .ok()
        });
        if let Some(ip) = ip {
            let count = taken.entry(prefix(ip)).or_default();
            if *count == PER_PREFIX_CHECKED {
                continue;
            }
            *count += 1;
        }
        let due = info.is_none_or(|info| info.checked_at <= due_before);
        sampled.push((candidate, due));
    }
    Ok(sampled)
}

/// Checks the sampled candidates not checked in the last few minutes with `check`, records the
/// results, and keeps the best eight of them in `net`'s relay map in place of those adopted
/// before. Returns the adopted URLs.
pub async fn adopt<F, Fut>(
    net: &Net,
    db: &RelayDb,
    candidates: Vec<Candidate>,
    check: F,
) -> Result<Vec<String>>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<(Duration, IpAddr)>>,
{
    let sampled = sample(db, candidates)?;
    let due: Vec<&Candidate> = sampled
        .iter()
        .filter(|(_, due)| *due)
        .map(|(candidate, _)| candidate)
        .collect();
    let results =
        futures_util::future::join_all(due.iter().map(|candidate| check(candidate.url.clone())))
            .await;
    for (candidate, result) in due.iter().zip(results) {
        if let Err(error) = &result {
            tracing::debug!(event = "relay.check_failed", url = %candidate.url, %error);
        }
        db.record_check(&candidate.url, &candidate.device_id, result.ok())?;
    }
    let urls: Vec<String> = sampled
        .into_iter()
        .map(|(candidate, _)| candidate.url)
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
        let boot = crate::crypto::LocalIdentity::generate().device_id()?;
        let mesh_config = mesh::MeshConfig {
            enabled: true,
            network: "probe".into(),
            bootstrap: vec![format!(
                "vibeface://node/{boot}?relay=http://boot.example:3340"
            )],
            allow_private: true,
            advertise: vec![],
        };
        net.start_mesh(&mesh_config, &dir.join("mesh.sqlite3"))
            .await?;
        let mesh = net.mesh().context("no mesh")?;
        assert_eq!(
            mesh.bootstrap_relays(),
            [super::super::relay_url("http://boot.example:3340")?],
            "a bootstrap node's relay is pinned"
        );
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
        let probe = mesh::Request::ProbeRelay { url: url.clone() };
        let reply = mesh.answer(asker, probe.clone()).await?;
        assert_eq!(reply["ok"], true);
        let again = mesh.answer(asker, probe.clone()).await.unwrap_err();
        assert!(again.to_string().contains("once a minute"), "{again}");
        relay.shutdown().await;
        let stopped = mesh.answer(stranger(), probe).await.unwrap_err();
        assert_eq!(
            stopped.to_string(),
            "the relay check failed",
            "a relay that stopped fails the probe, and the asker learns no more"
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
            first_seen_ms: seen,
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
        assert_eq!(urls(false), ["https://a.example/", "https://b.example/"]);
        assert_eq!(
            urls(true),
            [
                "https://a.example/",
                "http://127.0.0.1:3340/",
                "https://b.example/"
            ]
        );
        Ok(())
    }

    /// The relays `net` keeps in its relay map.
    async fn in_map(net: &Net) -> HashSet<String> {
        let pinned = net.relays.lock().await;
        let bootstrap = net.bootstrap.lock().await;
        let adopted = net.adopted.lock().await;
        pinned
            .iter()
            .chain(bootstrap.iter())
            .chain(adopted.iter())
            .map(ToString::to_string)
            .collect()
    }

    /// Clears when each relay was checked, so the next round checks them all again.
    fn recheck(db: &RelayDb) {
        db.db()
            .execute(
                "UPDATE mesh_relays SET last_ok_at = 0, last_fail_at = 0",
                [],
            )
            .unwrap();
    }

    /// The number in a test relay's host name, `relay-<n>.example`.
    fn relay_number(url: &str) -> u64 {
        url.trim_start_matches("http://relay-")
            .split('.')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    async fn test_net(pinned: &str) -> Result<Net> {
        let config = super::super::NetworkConfig {
            bind: Some("127.0.0.1:0".parse()?),
            relays: vec![super::super::Relay {
                url: pinned.into(),
                token: None,
            }],
            ..Default::default()
        };
        let secret = crate::crypto::LocalIdentity::generate()
            .secret_key_hex
            .clone();
        Net::bind(&secret, &config, 0, vec![]).await
    }

    /// Ten relays answer, each in a network of its own; the eight fastest go into the relay map.
    /// A relay that fails three checks in a row or is no longer announced leaves it, and a
    /// configured relay stays.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_best_eight_relays_are_adopted() -> Result<()> {
        let dir = dir("adopt");
        let db = RelayDb::open(&dir.join("mesh.sqlite3"))?;
        let pinned = "http://127.0.0.1:1/".to_owned();
        let net = test_net(&pinned).await?;
        let url = |n: u64| format!("http://relay-{n}.example/");
        let device = crate::crypto::LocalIdentity::generate().device_id()?;
        let candidate = |url: String| Candidate {
            url,
            device_id: device.clone(),
        };
        let mut all: Vec<Candidate> = (0..10).map(|n| candidate(url(n))).collect();
        all.push(candidate(pinned.clone()));
        let failing = Arc::new(Mutex::new(HashSet::<String>::new()));
        let check = |url: String| {
            let failing = failing.clone();
            async move {
                anyhow::ensure!(!failing.lock().unwrap().contains(&url), "down");
                // Lower numbers answer faster; the configured relay is slowest.
                if url.starts_with("http://127.0.0.1") {
                    return Ok((Duration::from_millis(900), IpAddr::from([127, 0, 0, 1])));
                }
                let n = relay_number(&url);
                Ok((
                    Duration::from_millis(10 + n * 10),
                    IpAddr::from([203, 0, n as u8, 1]),
                ))
            }
        };
        let adopted = adopt(&net, &db, all.clone(), check).await?;
        assert_eq!(adopted, (0..8).map(url).collect::<Vec<_>>());
        let map = in_map(&net).await;
        assert!((0..8).all(|n| map.contains(&url(n))));
        assert!(!map.contains(&url(8)) && !map.contains(&url(9)));
        assert!(map.contains(&pinned));

        // Relay 0 goes down and relay 1 stops being announced; the next two step in.
        failing.lock().unwrap().insert(url(0));
        all.retain(|candidate| candidate.url != url(1));
        for _ in 0..3 {
            recheck(&db);
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
        recheck(&db);
        assert_eq!(
            adopt(&net, &db, vec![failed_once], check).await?,
            [url(2)],
            "one failure lowers a relay's history but keeps it"
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

    /// Seven relays in networks of their own have served for a while. Two hundred faster relays
    /// appear, all in one /24: they take one place in the relay map at most, and two of a
    /// round's checks once their network is known. The configured relay and the bootstrap
    /// node's relay stay whatever the ranking.
    #[tokio::test(flavor = "multi_thread")]
    async fn relays_in_one_network_cannot_crowd_out_the_others() -> Result<()> {
        let dir = dir("eclipse");
        let db = RelayDb::open(&dir.join("mesh.sqlite3"))?;
        let pinned = "http://127.0.0.1:1/".to_owned();
        let net = test_net(&pinned).await?;
        let boot = super::super::relay_url("http://relay-99.example/")?;
        net.set_bootstrap_relays(vec![boot.clone()]).await;
        let url = |n: u64| format!("http://relay-{n}.example/");
        let sybil = |n: u64| format!("http://relay-{}.example/", 1000 + n);
        let device = crate::crypto::LocalIdentity::generate().device_id()?;
        let candidate = |url: String| Candidate {
            url,
            device_id: device.clone(),
        };
        let checked = Arc::new(Mutex::new(Vec::<String>::new()));
        let check = |url: String| {
            let checked = checked.clone();
            async move {
                checked.lock().unwrap().push(url.clone());
                let n = relay_number(&url);
                Ok(if n >= 1000 {
                    (
                        Duration::from_millis(1),
                        IpAddr::from([198, 51, 100, n as u8]),
                    )
                } else {
                    (
                        Duration::from_millis(100),
                        IpAddr::from([203, 0, n as u8, 1]),
                    )
                })
            }
        };
        let honest: Vec<Candidate> = (0..7).map(|n| candidate(url(n))).collect();
        for _ in 0..5 {
            recheck(&db);
            adopt(&net, &db, honest.clone(), check).await?;
        }
        let mut all = honest.clone();
        all.push(candidate(boot.to_string()));
        all.extend((0..200).map(|n| candidate(sybil(n))));
        for round in 0..10 {
            recheck(&db);
            checked.lock().unwrap().clear();
            let adopted = adopt(&net, &db, all.clone(), check).await?;
            let checked = std::mem::take(&mut *checked.lock().unwrap());
            assert!(checked.len() <= CHECKS);
            let sybils_adopted = adopted
                .iter()
                .filter(|url| relay_number(url) >= 1000)
                .count();
            assert!(sybils_adopted <= 1, "round {round}: {adopted:?}");
            assert!(
                (0..7).all(|n| adopted.contains(&url(n))),
                "round {round}: {adopted:?}"
            );
            let map = in_map(&net).await;
            assert!(map.contains(&pinned) && map.contains(&boot.to_string()));
            // Once checked, the sybils' network is known: of those, two a round at most.
            let known_sybils = checked
                .iter()
                .filter(|url| relay_number(url) >= 1000)
                .filter(|url| {
                    db.known(url)
                        .unwrap()
                        .is_some_and(|known| known.history > 1)
                })
                .count();
            assert!(known_sybils <= 2, "round {round}: {known_sybils}");
        }
        assert!(
            !net.adopted.lock().await.contains(&boot),
            "the bootstrap node's relay is pinned, not adopted"
        );
        net.close().await;
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn a_relay_network_is_its_ipv4_24_or_ipv6_48() {
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        assert_eq!(prefix(ip("198.51.100.1")), prefix(ip("198.51.100.250")));
        assert_ne!(prefix(ip("198.51.100.1")), prefix(ip("198.51.101.1")));
        assert_eq!(
            prefix(ip("::ffff:198.51.100.1")),
            prefix(ip("198.51.100.9"))
        );
        assert_eq!(
            prefix(ip("2001:db8:1::1")),
            prefix(ip("2001:db8:1:ffff::1"))
        );
        assert_ne!(prefix(ip("2001:db8:1::1")), prefix(ip("2001:db8:2::1")));
    }
}
