//! Public bootstrap: a Discovery signs a catalogue of public Gateway and Relay pairs and issues
//! short-lived Relay grants bound to the device that asks. This module owns the public defaults,
//! the wire shapes and their verification, the Discovery-side issuer with its budgets, the
//! Relay-side admission memory, and the background client that finds, admits, registers and
//! renews. The rest of Core reads one state snapshot.
//!
//! A catalogue only says where to connect. Membership and task authority stay with the personal
//! network's signed history and the end-to-end identities on top of the relay.
use super::{RelayUrl, device_id, endpoint_id, relay_config, relay_url};
use crate::crypto::LocalIdentity;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::{Endpoint, EndpointId, endpoint::RelayStatus};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;

pub const CATALOG_DOMAIN: &str = "bootstrap-catalog";
pub const GRANT_DOMAIN: &str = "bootstrap-grant";
pub const REQUEST_DOMAIN: &str = "bootstrap-request";

pub const MAX_CATALOG_ENTRIES: usize = 4;
/// The whole signed catalogue response, as sent.
pub const MAX_SIGNED_CATALOG_BYTES: usize = 4096;
/// A grant string, the relay auth token a Node presents.
pub const MAX_GRANT_BYTES: usize = 1024;
/// The signed payload of an HTTPS grant request.
pub const MAX_REQUEST_BYTES: usize = 1024;
const MAX_URL_BYTES: usize = 256;
pub const DEFAULT_GRANT_TTL_MS: i64 = 30 * 60 * 1000;
pub const MAX_GRANT_TTL_MS: i64 = 60 * 60 * 1000;
/// Fixtures renew within seconds; nothing shorter is meaningful on a real network.
const MIN_GRANT_TTL_MS: i64 = 1000;
const CATALOG_TTL_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_CATALOG_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// The issuer signs a fresh catalogue once the cached one has used three quarters of its life.
const CATALOG_RESIGN_MS: i64 = CATALOG_TTL_MS / 4 * 3;
/// A Node fetches the catalogue again when the one it holds has this little life left.
const CATALOG_REFRESH_MS: i64 = 6 * 60 * 60 * 1000;
/// Tolerance for an issuer clock ahead of ours.
const SKEW_MS: i64 = 60_000;
/// How far a signed HTTPS request's timestamp may be from the issuer's clock.
const REQUEST_WINDOW_MS: i64 = 5 * 60 * 1000;
const BUDGET_WINDOW_MS: i64 = 60_000;
const DEVICE_GRANTS_PER_WINDOW: u32 = 6;
const IP_GRANTS_PER_WINDOW: u32 = 30;
const MAX_BUDGET_ENTRIES: usize = 4096;
const MAX_ADMITTED: usize = 4096;
/// How long the client waits for the UDP discovery session before it turns to HTTPS.
const UDP_WAIT: Duration = Duration::from_secs(3);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A relay denial right after a renewal waits for the backoff instead of renewing again.
const DENIED_DEBOUNCE_MS: i64 = 2000;

const PUBLIC_DEFAULTS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../config/public-bootstrap.json"
));

pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

/// The official service as `config/public-bootstrap.json` states it. The file is shared with
/// the web and mobile clients, so nothing here repeats its values.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicDefaults {
    pub issuer: String,
    pub udp_host: String,
    pub udp_port: u16,
    pub https_base: String,
}

impl PublicDefaults {
    pub fn load() -> Result<Self> {
        let defaults: Self =
            serde_json::from_str(PUBLIC_DEFAULTS).context("invalid public bootstrap defaults")?;
        vibeface_secure::validate_device_id(&defaults.issuer)?;
        ensure!(
            defaults.udp_port != 0 && !defaults.udp_host.is_empty(),
            "invalid public discovery address"
        );
        https_base(&defaults.https_base)?;
        Ok(defaults)
    }

    /// The `vibeface://<issuer>@<host>:<port>` form the UDP discovery protocol takes.
    pub fn discovery_url(&self) -> String {
        let host = if self.udp_host.contains(':') {
            format!("[{}]", self.udp_host)
        } else {
            self.udp_host.clone()
        };
        format!("vibeface://{}@{host}:{}", self.issuer, self.udp_port)
    }
}

/// Where a Node bootstraps from. `Off` is a product choice, never a fallback.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Bootstrap {
    /// No public infrastructure is contacted.
    #[default]
    Off,
    /// The official service from `config/public-bootstrap.json`.
    Public,
    /// A self-hosted Discovery pinned by the device ID in its URL, with an optional HTTPS
    /// bootstrap base; it never falls back to the official service.
    Custom {
        discovery: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        https: Option<String>,
    },
}

impl Bootstrap {
    pub fn is_off(&self) -> bool {
        matches!(self, Self::Off)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Public => "public",
            Self::Custom { .. } => "custom",
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.source().map(|_| ())
    }

    /// The pinned issuer and its endpoints; `None` when off.
    pub fn source(&self) -> Result<Option<Source>> {
        match self {
            Self::Off => Ok(None),
            Self::Public => {
                let defaults = PublicDefaults::load()?;
                Ok(Some(Source {
                    issuer: defaults.issuer.clone(),
                    discovery: defaults.discovery_url(),
                    https: Some(https_base(&defaults.https_base)?),
                }))
            }
            Self::Custom { discovery, https } => {
                let server = super::discovery::Server::parse(discovery)?;
                let https = https.as_deref().map(https_base).transpose()?;
                Ok(Some(Source {
                    issuer: device_id(&server.id),
                    discovery: discovery.clone(),
                    https,
                }))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The Discovery's device ID; it signs the catalogue and every grant.
    pub issuer: String,
    /// Its UDP discovery URL.
    pub discovery: String,
    /// Its HTTPS bootstrap base, serving `/catalog` and `/grant`.
    pub https: Option<String>,
}

fn is_loopback_host(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

fn parse_http_url(value: &str) -> Result<reqwest::Url> {
    ensure!(value.len() <= MAX_URL_BYTES, "URL too long");
    let url = reqwest::Url::parse(value).context("invalid URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "URL must be HTTP(S) without credentials, query or fragment"
    );
    ensure!(
        url.scheme() == "https" || is_loopback_host(&url),
        "public addresses use HTTPS; plain HTTP is for loopback fixtures"
    );
    Ok(url)
}

/// `value` as a bare origin: no path, normalized as browsers serialize it.
pub fn origin(value: &str) -> Result<String> {
    let url = parse_http_url(value)?;
    ensure!(url.path() == "/", "origin must not carry a path");
    Ok(url.origin().ascii_serialization())
}

/// The origin a relay URL names, however iroh renders it; the audience grants are checked against.
pub fn relay_audience(relay: &RelayUrl) -> Result<String> {
    let url = reqwest::Url::parse(&relay.to_string()).context("invalid relay URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "relay URL must be HTTP(S)"
    );
    Ok(url.origin().ascii_serialization())
}

/// An HTTPS bootstrap base such as `https://app.example/bootstrap`, without a trailing slash.
pub fn https_base(value: &str) -> Result<String> {
    let url = parse_http_url(value)?;
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn is_nonce(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    /// The browser origin of a public entry.
    pub gateway_url: String,
    /// The relay that entry runs; the pair is published together.
    pub relay_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub version: u8,
    pub issuer: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub entries: Vec<CatalogEntry>,
}

/// `payload` is the catalogue's JSON text exactly as signed, so every client verifies the same
/// bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedCatalog {
    pub payload: String,
    pub signature: String,
}

impl SignedCatalog {
    /// The catalogue, if the pinned `issuer` signed it and it is current.
    pub fn open(&self, issuer: &str, now_ms: i64) -> Result<Catalog> {
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_SIGNED_CATALOG_BYTES,
            "catalogue too large"
        );
        let catalog: Catalog =
            serde_json::from_str(&self.payload).context("invalid catalogue payload")?;
        ensure!(catalog.version == 1, "unsupported catalogue version");
        ensure!(
            catalog.issuer == issuer,
            "catalogue signed by another issuer"
        );
        ensure!(
            catalog.issued_at_ms <= now_ms.saturating_add(SKEW_MS)
                && catalog.expires_at_ms > catalog.issued_at_ms
                && catalog.expires_at_ms <= catalog.issued_at_ms.saturating_add(MAX_CATALOG_TTL_MS),
            "catalogue has invalid timestamps"
        );
        ensure!(catalog.expires_at_ms > now_ms, "catalogue expired");
        ensure!(
            !catalog.entries.is_empty() && catalog.entries.len() <= MAX_CATALOG_ENTRIES,
            "catalogue names one to four entries"
        );
        for entry in &catalog.entries {
            origin(&entry.gateway_url)?;
            origin(&entry.relay_url)?;
        }
        vibeface_secure::verify(
            issuer,
            CATALOG_DOMAIN,
            self.payload.as_bytes(),
            &self.signature,
        )?;
        Ok(catalog)
    }
}

/// The claims a Relay grant carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub version: u8,
    pub issuer: String,
    /// The Node the grant is bound to; the relay compares it with the TLS-proven endpoint.
    pub device_id: String,
    pub relay_url: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub nonce: String,
}

/// `base64url(claims)` + `.` + `hex(signature)`: the string a Node presents as its relay auth
/// token. The signature covers the raw claim bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedGrant {
    token: String,
    claims: Vec<u8>,
    signature: String,
}

impl SignedGrant {
    pub fn parse(token: &str) -> Result<Self> {
        ensure!(token.len() <= MAX_GRANT_BYTES, "grant too large");
        let (claims, signature) = token.split_once('.').context("grant has no signature")?;
        ensure!(
            !claims.is_empty()
                && signature.len() == 128
                && signature.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid grant signature"
        );
        let claims = URL_SAFE_NO_PAD
            .decode(claims)
            .context("invalid grant encoding")?;
        Ok(Self {
            token: token.to_owned(),
            claims,
            signature: signature.to_owned(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.token
    }

    /// The claims as written, before anyone vouched for them.
    pub fn claims(&self) -> Result<Grant> {
        serde_json::from_slice(&self.claims).context("invalid grant claims")
    }

    /// The grant, if `issuer` signed it for `device` to use `relay` now.
    pub fn verify(
        &self,
        issuer: &str,
        device: &EndpointId,
        relay: &RelayUrl,
        now_ms: i64,
    ) -> Result<Grant> {
        let grant = self.claims()?;
        ensure!(grant.version == 1, "unsupported grant version");
        ensure!(grant.issuer == issuer, "grant signed by another issuer");
        ensure!(
            grant.device_id == device_id(device),
            "grant belongs to another device"
        );
        ensure!(
            origin(&grant.relay_url)? == relay_audience(relay)?,
            "grant is for another relay"
        );
        ensure!(is_nonce(&grant.nonce), "invalid grant nonce");
        ensure!(
            grant.issued_at_ms <= now_ms + SKEW_MS,
            "grant issued in the future"
        );
        ensure!(
            grant.expires_at_ms > grant.issued_at_ms
                && grant.expires_at_ms - grant.issued_at_ms <= MAX_GRANT_TTL_MS,
            "grant lifetime out of bounds"
        );
        ensure!(grant.expires_at_ms > now_ms, "grant expired");
        vibeface_secure::verify(issuer, GRANT_DOMAIN, &self.claims, &self.signature)?;
        Ok(grant)
    }
}

/// What a device asks the issuer for over HTTPS; the UDP path proves the device by its QUIC
/// connection instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRequest {
    pub device_id: String,
    pub relay_url: String,
    pub at_ms: i64,
    pub nonce: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRequest {
    pub payload: String,
    pub signature: String,
}

impl SignedRequest {
    pub fn sign(identity: &LocalIdentity, relay_url: &str, now_ms: i64) -> Result<Self> {
        let request = GrantRequest {
            device_id: identity.device_id()?,
            relay_url: origin(relay_url)?,
            at_ms: now_ms,
            nonce: crate::crypto::random_hex(16),
        };
        let payload = serde_json::to_string(&request)?;
        let signature = identity.sign_payload(REQUEST_DOMAIN, payload.as_bytes())?;
        Ok(Self { payload, signature })
    }

    /// The request, if the device it names signed it recently.
    pub fn open(&self, now_ms: i64) -> Result<GrantRequest> {
        ensure!(
            self.payload.len() <= MAX_REQUEST_BYTES && self.signature.len() == 128,
            "invalid grant request"
        );
        let request: GrantRequest =
            serde_json::from_str(&self.payload).context("invalid grant request")?;
        vibeface_secure::validate_device_id(&request.device_id)?;
        ensure!(
            (request.at_ms - now_ms).abs() <= REQUEST_WINDOW_MS,
            "grant request is stale"
        );
        ensure!(is_nonce(&request.nonce), "invalid grant request nonce");
        origin(&request.relay_url)?;
        vibeface_secure::verify(
            &request.device_id,
            REQUEST_DOMAIN,
            self.payload.as_bytes(),
            &self.signature,
        )?;
        Ok(request)
    }
}

/// What a discovery-only entry publishes and issues grants for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogConfig {
    pub entries: Vec<CatalogEntry>,
    /// Grant lifetime; 30 minutes unless set, at most an hour.
    #[serde(default = "default_grant_ttl_ms")]
    pub grant_ttl_ms: i64,
}

fn default_grant_ttl_ms() -> i64 {
    DEFAULT_GRANT_TTL_MS
}

impl CatalogConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.entries.is_empty() && self.entries.len() <= MAX_CATALOG_ENTRIES,
            "a catalogue names one to four entries"
        );
        for entry in &self.entries {
            origin(&entry.gateway_url)?;
            origin(&entry.relay_url)?;
        }
        ensure!(
            (MIN_GRANT_TTL_MS..=MAX_GRANT_TTL_MS).contains(&self.grant_ttl_ms),
            "grant lifetime must be between one second and one hour"
        );
        Ok(())
    }
}

/// Grants issued per key within the current window; the maps stay bounded.
#[derive(Default)]
struct Budget {
    devices: HashMap<String, (i64, u32)>,
    ips: HashMap<IpAddr, (i64, u32)>,
}

fn prune<K: Eq + Hash>(map: &mut HashMap<K, (i64, u32)>, now_ms: i64) -> Result<()> {
    if map.len() >= MAX_BUDGET_ENTRIES {
        map.retain(|_, (since, _)| now_ms - *since < BUDGET_WINDOW_MS);
        ensure!(
            map.len() < MAX_BUDGET_ENTRIES,
            "the issuer is busy; retry later"
        );
    }
    Ok(())
}

fn charge<K: Eq + Hash>(
    map: &mut HashMap<K, (i64, u32)>,
    key: K,
    now_ms: i64,
    limit: u32,
) -> Result<()> {
    let entry = map.entry(key).or_insert((now_ms, 0));
    if now_ms - entry.0 >= BUDGET_WINDOW_MS {
        *entry = (now_ms, 0);
    }
    ensure!(entry.1 < limit, "too many grant requests; retry later");
    entry.1 += 1;
    Ok(())
}

impl Budget {
    fn admit(&mut self, device: &str, ip: IpAddr, now_ms: i64) -> Result<()> {
        prune(&mut self.ips, now_ms)?;
        prune(&mut self.devices, now_ms)?;
        charge(&mut self.ips, ip, now_ms, IP_GRANTS_PER_WINDOW)?;
        charge(
            &mut self.devices,
            device.to_owned(),
            now_ms,
            DEVICE_GRANTS_PER_WINDOW,
        )
    }
}

/// The Discovery side: signs the catalogue and grants with the Discovery's own identity. It
/// keeps nothing but a catalogue cache and the grant budget, and shares nothing with any relay.
pub struct Issuer {
    identity: LocalIdentity,
    issuer: String,
    catalog: CatalogConfig,
    /// Relay origins of the catalogue, as grants name them.
    relays: Vec<String>,
    signed: Mutex<Option<(i64, SignedCatalog)>>,
    budget: Mutex<Budget>,
}

impl std::fmt::Debug for Issuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Issuer")
    }
}

impl Issuer {
    pub fn new(identity: LocalIdentity, catalog: CatalogConfig) -> Result<Self> {
        catalog.validate()?;
        let issuer = identity.device_id()?;
        let relays = catalog
            .entries
            .iter()
            .map(|entry| origin(&entry.relay_url))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            identity,
            issuer,
            catalog,
            relays,
            signed: Mutex::new(None),
            budget: Mutex::new(Budget::default()),
        })
    }

    pub fn device_id(&self) -> &str {
        &self.issuer
    }

    /// The signed catalogue, re-signed once the cached one has aged.
    pub fn catalog(&self, now_ms: i64) -> Result<SignedCatalog> {
        let mut signed = self.signed.lock().expect("catalogue cache");
        if let Some((issued_at_ms, cached)) = signed.as_ref()
            && now_ms - issued_at_ms < CATALOG_RESIGN_MS
            && now_ms >= *issued_at_ms
        {
            return Ok(cached.clone());
        }
        let entries = self
            .catalog
            .entries
            .iter()
            .map(|entry| {
                Ok(CatalogEntry {
                    gateway_url: origin(&entry.gateway_url)?,
                    relay_url: origin(&entry.relay_url)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let catalog = Catalog {
            version: 1,
            issuer: self.issuer.clone(),
            issued_at_ms: now_ms,
            expires_at_ms: now_ms + CATALOG_TTL_MS,
            entries,
        };
        let payload = serde_json::to_string(&catalog)?;
        let signature = self
            .identity
            .sign_payload(CATALOG_DOMAIN, payload.as_bytes())?;
        let fresh = SignedCatalog { payload, signature };
        ensure!(
            serde_json::to_vec(&fresh)?.len() <= MAX_SIGNED_CATALOG_BYTES,
            "catalogue too large"
        );
        *signed = Some((now_ms, fresh.clone()));
        Ok(fresh)
    }

    /// A grant for `device`, whose key the caller has already proven, to use `relay_url` from
    /// `ip`. Budgets count per device and per source address.
    pub fn grant(
        &self,
        device: &str,
        relay_url: &str,
        ip: IpAddr,
        now_ms: i64,
    ) -> Result<SignedGrant> {
        vibeface_secure::validate_device_id(device)?;
        let relay = origin(relay_url)?;
        ensure!(
            self.relays.contains(&relay),
            "the catalogue has no such relay"
        );
        self.budget
            .lock()
            .expect("grant budget")
            .admit(device, ip, now_ms)?;
        let grant = Grant {
            version: 1,
            issuer: self.issuer.clone(),
            device_id: device.to_owned(),
            relay_url: relay,
            issued_at_ms: now_ms,
            expires_at_ms: now_ms + self.catalog.grant_ttl_ms,
            nonce: crate::crypto::random_hex(16),
        };
        let claims = serde_json::to_vec(&grant)?;
        let signature = self.identity.sign_payload(GRANT_DOMAIN, &claims)?;
        SignedGrant::parse(&format!("{}.{signature}", URL_SAFE_NO_PAD.encode(&claims)))
    }

    /// The HTTPS path: the request itself proves the device.
    pub fn grant_signed(
        &self,
        request: &SignedRequest,
        ip: IpAddr,
        now_ms: i64,
    ) -> Result<SignedGrant> {
        let request = request.open(now_ms)?;
        self.grant(&request.device_id, &request.relay_url, ip, now_ms)
    }
}

/// The Relay side of a public entry that admits by grants. A verified grant admits the device it
/// names until it expires; the holder posts each renewal here, so a reconnect that still presents
/// the older token gets in by its TLS-proven identity. Nothing is persisted.
pub struct GrantAdmission {
    issuer: String,
    relay: RelayUrl,
    admitted: Mutex<HashMap<EndpointId, i64>>,
}

impl std::fmt::Debug for GrantAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GrantAdmission")
    }
}

impl GrantAdmission {
    pub fn new(issuer: String, relay: RelayUrl) -> Result<Self> {
        vibeface_secure::validate_device_id(&issuer)?;
        relay_audience(&relay)?;
        Ok(Self {
            issuer,
            relay,
            admitted: Mutex::new(HashMap::new()),
        })
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Verifies a grant its holder posted and remembers the device until the grant expires.
    pub fn register(&self, token: &str, now_ms: i64) -> Result<Grant> {
        let signed = SignedGrant::parse(token)?;
        let device = endpoint_id(&signed.claims()?.device_id)?;
        let grant = signed.verify(&self.issuer, &device, &self.relay, now_ms)?;
        self.remember(device, grant.expires_at_ms, now_ms)?;
        Ok(grant)
    }

    /// Whether the relay admits `device`, proven by TLS, presenting `token`.
    pub fn admits(&self, device: &EndpointId, token: Option<&str>, now_ms: i64) -> bool {
        if let Some(token) = token
            && let Ok(grant) = SignedGrant::parse(token)
                .and_then(|signed| signed.verify(&self.issuer, device, &self.relay, now_ms))
            && self.remember(*device, grant.expires_at_ms, now_ms).is_ok()
        {
            return true;
        }
        self.admitted
            .lock()
            .expect("relay admissions")
            .get(device)
            .is_some_and(|until| *until > now_ms)
    }

    pub fn admitted_until(&self, device: &EndpointId) -> Option<i64> {
        self.admitted
            .lock()
            .expect("relay admissions")
            .get(device)
            .copied()
    }

    fn remember(&self, device: EndpointId, until_ms: i64, now_ms: i64) -> Result<()> {
        let mut admitted = self.admitted.lock().expect("relay admissions");
        if let Some(entry) = admitted.get_mut(&device) {
            *entry = (*entry).max(until_ms);
            return Ok(());
        }
        if admitted.len() >= MAX_ADMITTED {
            admitted.retain(|_, until| *until > now_ms);
        }
        ensure!(
            admitted.len() < MAX_ADMITTED,
            "relay admission table is full"
        );
        admitted.insert(device, until_ms);
        Ok(())
    }

    /// Drops every admission, as a relay restart would.
    #[cfg(test)]
    pub(crate) fn forget_all(&self) {
        self.admitted.lock().expect("relay admissions").clear();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Udp,
    Https,
}

/// The entry this Node holds a current grant for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admitted {
    pub gateway_url: String,
    pub relay_url: String,
    pub grant_until_ms: i64,
    pub via: Transport,
}

/// The bootstrap snapshot the rest of Core and the status output read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BootstrapState {
    Off,
    /// No grant yet, since `since_ms`; `reason` is the last failure.
    Seeking {
        since_ms: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Admitted(Admitted),
    /// Renewal or registration is failing since `since_ms`; `admitted` is the grant still held.
    Degraded {
        since_ms: i64,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        admitted: Option<Admitted>,
    },
}

impl BootstrapState {
    /// The entry with a grant that is valid now; only it goes into the signed address record.
    pub fn admitted(&self, now_ms: i64) -> Option<&Admitted> {
        match self {
            Self::Admitted(admitted) => Some(admitted),
            Self::Degraded {
                admitted: Some(admitted),
                ..
            } => Some(admitted),
            _ => None,
        }
        .filter(|admitted| admitted.grant_until_ms > now_ms)
    }
}

/// A grant in hand and where it came from.
#[derive(Clone, Debug)]
pub struct Credential {
    pub relay: RelayUrl,
    pub gateway_url: String,
    pub token: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub via: Transport,
}

impl Credential {
    fn admitted(&self) -> Admitted {
        Admitted {
            gateway_url: self.gateway_url.clone(),
            relay_url: self.relay.to_string().trim_end_matches('/').to_owned(),
            grant_until_ms: self.expires_at_ms,
            via: self.via,
        }
    }
}

/// The HTTPS side of a bootstrap source.
#[derive(Clone)]
struct Https {
    client: reqwest::Client,
    base: String,
}

fn http_client(loopback: bool) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("vibeface-core/", env!("CARGO_PKG_VERSION")));
    if loopback {
        builder = builder.no_proxy();
    }
    builder.build().context("bootstrap HTTP client")
}

async fn limited_body(
    response: reqwest::Response,
    limit: usize,
) -> Result<(reqwest::StatusCode, Vec<u8>)> {
    let status = response.status();
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= limit as u64),
        "bootstrap response too large"
    );
    let bytes = response.bytes().await?;
    ensure!(bytes.len() <= limit, "bootstrap response too large");
    Ok((status, bytes.to_vec()))
}

fn refusal(status: reqwest::StatusCode, body: &[u8]) -> anyhow::Error {
    let reason = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value["error"].as_str().map(str::to_owned))
        .unwrap_or_default();
    anyhow!("bootstrap service refused ({status}): {reason}")
}

impl Https {
    fn new(base: &str) -> Result<Self> {
        let loopback = reqwest::Url::parse(base).is_ok_and(|url| is_loopback_host(&url));
        Ok(Self {
            client: http_client(loopback)?,
            base: base.to_owned(),
        })
    }

    async fn catalog(&self) -> Result<SignedCatalog> {
        let response = self
            .client
            .get(format!("{}/catalog", self.base))
            .send()
            .await?;
        let (status, body) = limited_body(response, MAX_SIGNED_CATALOG_BYTES).await?;
        if !status.is_success() {
            return Err(refusal(status, &body));
        }
        serde_json::from_slice(&body).context("invalid catalogue response")
    }

    async fn grant(&self, identity: &LocalIdentity, relay_url: &str) -> Result<String> {
        let request = SignedRequest::sign(identity, relay_url, now_ms())?;
        let response = self
            .client
            .post(format!("{}/grant", self.base))
            .json(&request)
            .send()
            .await?;
        let (status, body) = limited_body(response, MAX_GRANT_BYTES + 256).await?;
        if !status.is_success() {
            return Err(refusal(status, &body));
        }
        let reply: serde_json::Value =
            serde_json::from_slice(&body).context("invalid grant response")?;
        reply["grant"]
            .as_str()
            .map(str::to_owned)
            .context("grant response names no grant")
    }
}

/// Tells the entry's relay about a grant, so a reconnect after the previous grant expired is
/// admitted by this device's identity. The gateway and relay belong to the same entry.
async fn register(gateway_url: &str, token: &str) -> Result<()> {
    let loopback = reqwest::Url::parse(gateway_url).is_ok_and(|url| is_loopback_host(&url));
    let response = http_client(loopback)?
        .post(format!(
            "{}/api/relay/grant",
            gateway_url.trim_end_matches('/')
        ))
        .json(&serde_json::json!({ "grant": token }))
        .send()
        .await?;
    let (status, body) = limited_body(response, 1024).await?;
    if !status.is_success() {
        return Err(refusal(status, &body));
    }
    Ok(())
}

fn credential(
    token: String,
    entry: &CatalogEntry,
    issuer: &str,
    device: &EndpointId,
    via: Transport,
) -> Result<Credential> {
    let relay = relay_url(&entry.relay_url)?;
    let grant = SignedGrant::parse(&token)?.verify(issuer, device, &relay, now_ms())?;
    Ok(Credential {
        relay,
        gateway_url: entry.gateway_url.clone(),
        token,
        issued_at_ms: grant.issued_at_ms,
        expires_at_ms: grant.expires_at_ms,
        via,
    })
}

/// One grant for another endpoint of this device, over HTTPS: `vibeface join` uses it before the
/// Node's own network runs. The first catalogue entry that grants wins.
pub async fn admit_once(identity: &LocalIdentity, source: &Source) -> Result<Credential> {
    let base = source
        .https
        .as_deref()
        .context("the bootstrap source has no HTTPS endpoint")?;
    let https = Https::new(base)?;
    let catalog = https.catalog().await?.open(&source.issuer, now_ms())?;
    let device = endpoint_id(&identity.device_id()?)?;
    let mut failures = Vec::new();
    for entry in &catalog.entries {
        match https.grant(identity, &entry.relay_url).await {
            Ok(token) => {
                return credential(token, entry, &source.issuer, &device, Transport::Https);
            }
            Err(error) => failures.push(format!("{}: {error:#}", entry.relay_url)),
        }
    }
    bail!("no catalogue entry granted access: {}", failures.join("; "))
}

/// The background client of one endpoint; other modules read its state.
pub struct Bootstrapper {
    state: watch::Receiver<BootstrapState>,
    credential: Arc<Mutex<Option<Credential>>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for Bootstrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Bootstrapper")
    }
}

impl Drop for Bootstrapper {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl Bootstrapper {
    pub(super) fn off() -> Self {
        let (_, state) = watch::channel(BootstrapState::Off);
        Self {
            state,
            credential: Arc::new(Mutex::new(None)),
            task: None,
        }
    }

    /// Starts bootstrapping `endpoint` from `source`. `discovery` is the UDP session with the
    /// source, when the endpoint keeps one; `relays` is the endpoint's set of known relay URLs,
    /// so a relay learned later never replaces the granted configuration with a bare one.
    pub(super) fn spawn(
        endpoint: Endpoint,
        identity: LocalIdentity,
        source: Source,
        discovery: Option<super::discovery::DiscoveryClient>,
        relays: Arc<tokio::sync::Mutex<HashSet<RelayUrl>>>,
    ) -> Result<Self> {
        let (sender, state) = watch::channel(BootstrapState::Seeking {
            since_ms: now_ms(),
            reason: None,
        });
        let credential = Arc::new(Mutex::new(None));
        let https = source.https.as_deref().map(Https::new).transpose()?;
        let worker = Worker {
            endpoint,
            identity,
            source,
            discovery,
            https,
            relays,
            state: sender,
            credential: credential.clone(),
        };
        Ok(Self {
            state,
            credential,
            task: Some(tokio::spawn(worker.run())),
        })
    }

    pub fn state(&self) -> BootstrapState {
        self.state.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<BootstrapState> {
        self.state.clone()
    }

    /// The entry whose grant is valid now.
    pub fn admitted(&self) -> Option<Admitted> {
        self.state.borrow().admitted(now_ms()).cloned()
    }

    /// The relay and grant this endpoint holds, for another endpoint of the same device.
    pub fn credential(&self) -> Option<(RelayUrl, String)> {
        self.credential
            .lock()
            .expect("bootstrap credential")
            .as_ref()
            .filter(|credential| credential.expires_at_ms > now_ms())
            .map(|credential| (credential.relay.clone(), credential.token.clone()))
    }

    pub async fn wait_admitted(&self, timeout: Duration) -> Result<Admitted> {
        let mut state = self.state.clone();
        tokio::time::timeout(timeout, async {
            loop {
                let admitted = state.borrow_and_update().admitted(now_ms()).cloned();
                if let Some(admitted) = admitted {
                    return Ok(admitted);
                }
                state.changed().await.context("bootstrap stopped")?;
            }
        })
        .await
        .context("public bootstrap did not admit this Node in time")?
    }
}

enum Pause {
    Due,
    /// The relay denied the token the endpoint presents.
    Denied(String),
}

struct Worker {
    endpoint: Endpoint,
    identity: LocalIdentity,
    source: Source,
    discovery: Option<super::discovery::DiscoveryClient>,
    https: Option<Https>,
    relays: Arc<tokio::sync::Mutex<HashSet<RelayUrl>>>,
    state: watch::Sender<BootstrapState>,
    credential: Arc<Mutex<Option<Credential>>>,
}

fn jittered_renewal(credential: &Credential, now_ms: i64) -> Duration {
    let ttl = (credential.expires_at_ms - credential.issued_at_ms).max(MIN_GRANT_TTL_MS);
    let jitter = (rand::random::<u64>() % (ttl as u64 / 10).max(1)) as i64;
    let at = credential.issued_at_ms + ttl / 2 + jitter;
    Duration::from_millis((at - now_ms).clamp(200, ttl) as u64)
}

impl Worker {
    async fn run(self) {
        let mut backoff = BACKOFF_MIN;
        let mut catalog: Option<Catalog> = None;
        let mut current: Option<Credential> = None;
        let mut trouble_since: Option<i64> = None;
        let mut relay_status = self.endpoint.home_relay_status();
        while !self.endpoint.is_closed() {
            let now = now_ms();
            if current
                .as_ref()
                .is_some_and(|credential| credential.expires_at_ms <= now)
            {
                current = None;
                *self.credential.lock().expect("bootstrap credential") = None;
                self.state.send_replace(BootstrapState::Degraded {
                    since_ms: *trouble_since.get_or_insert(now),
                    reason: "the relay grant expired before it was renewed".into(),
                    admitted: None,
                });
            }
            if catalog
                .as_ref()
                .is_none_or(|catalog| catalog.expires_at_ms <= now + CATALOG_REFRESH_MS)
            {
                match self.fetch_catalog().await {
                    Ok(fresh) => catalog = Some(fresh),
                    Err(error) if catalog.is_none() => {
                        self.report(
                            format!("catalogue unavailable: {error:#}"),
                            current.as_ref(),
                            &mut trouble_since,
                        );
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                        continue;
                    }
                    // The catalogue in hand stays good until it expires.
                    Err(error) => tracing::debug!(%error, "catalogue refresh pending"),
                }
            }
            let Some(known) = catalog.as_ref() else {
                continue;
            };
            let outcome = self.admit(known, current.as_ref()).await;
            match outcome {
                Ok(credential) => {
                    backoff = BACKOFF_MIN;
                    trouble_since = None;
                    let last_renewal_ms = now_ms();
                    *self.credential.lock().expect("bootstrap credential") =
                        Some(credential.clone());
                    let admitted = credential.admitted();
                    tracing::info!(
                        event = "bootstrap.admitted",
                        gateway = %admitted.gateway_url,
                        relay = %admitted.relay_url,
                        via = ?admitted.via,
                        "public entry admitted this Node"
                    );
                    self.state.send_replace(BootstrapState::Admitted(admitted));
                    let delay = jittered_renewal(&credential, now_ms());
                    let relay = credential.relay.clone();
                    current = Some(credential);
                    if let Pause::Denied(reason) =
                        self.pause(delay, &mut relay_status, &relay).await
                    {
                        tracing::warn!(event = "bootstrap.relay_denied", %reason, "the relay denied this Node's grant; renewing");
                        if now_ms() - last_renewal_ms < DENIED_DEBOUNCE_MS {
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(BACKOFF_MAX);
                        }
                    }
                }
                Err(error) => {
                    self.report(format!("{error:#}"), current.as_ref(), &mut trouble_since);
                    catalog = None;
                    let wait = match &current {
                        Some(credential) => backoff.min(Duration::from_millis(
                            (credential.expires_at_ms - now_ms()).max(200) as u64,
                        )),
                        None => backoff,
                    };
                    tokio::time::sleep(wait).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    }

    fn report(
        &self,
        reason: String,
        current: Option<&Credential>,
        trouble_since: &mut Option<i64>,
    ) {
        let since_ms = *trouble_since.get_or_insert_with(now_ms);
        tracing::warn!(event = "bootstrap.pending", %reason, "public bootstrap is pending");
        let state = match current {
            Some(credential) => BootstrapState::Degraded {
                since_ms,
                reason,
                admitted: Some(credential.admitted()),
            },
            None => match &*self.state.borrow() {
                BootstrapState::Degraded { admitted, .. } => BootstrapState::Degraded {
                    since_ms,
                    reason,
                    admitted: admitted.clone(),
                },
                _ => BootstrapState::Seeking {
                    since_ms,
                    reason: Some(reason),
                },
            },
        };
        self.state.send_replace(state);
    }

    /// Sleeps for `delay`, unless the relay reports that it denied this endpoint's token.
    async fn pause<W>(&self, delay: Duration, status: &mut W, relay: &RelayUrl) -> Pause
    where
        W: iroh::Watcher<Value = Vec<RelayStatus>>,
    {
        let deadline = tokio::time::sleep(delay);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return Pause::Due,
                changed = status.updated() => match changed {
                    Ok(statuses) => {
                        if let Some(reason) = statuses
                            .iter()
                            .filter(|status| status.url() == relay)
                            .find_map(|status| status.auth_denied_reason().map(str::to_owned))
                        {
                            return Pause::Denied(reason);
                        }
                    }
                    Err(_) => return Pause::Due,
                },
            }
        }
    }

    async fn fetch_catalog(&self) -> Result<Catalog> {
        let udp = match &self.discovery {
            Some(discovery) => discovery.catalog(UDP_WAIT).await,
            None => Err(anyhow!("no UDP discovery session")),
        };
        let signed = match (udp, &self.https) {
            (Ok(signed), _) => signed,
            (Err(udp), Some(https)) => https
                .catalog()
                .await
                .map_err(|error| anyhow!("UDP: {udp:#}; HTTPS: {error:#}"))?,
            (Err(udp), None) => return Err(udp),
        };
        signed.open(&self.source.issuer, now_ms())
    }

    async fn request_grant(&self, entry: &CatalogEntry) -> Result<Credential> {
        let device = self.endpoint.id();
        let udp = match &self.discovery {
            Some(discovery) => discovery.grant(&entry.relay_url, UDP_WAIT).await,
            None => Err(anyhow!("no UDP discovery session")),
        };
        let (token, via) = match (udp, &self.https) {
            (Ok(token), _) => (token, Transport::Udp),
            (Err(udp), Some(https)) => (
                https
                    .grant(&self.identity, &entry.relay_url)
                    .await
                    .map_err(|error| anyhow!("UDP: {udp:#}; HTTPS: {error:#}"))?,
                Transport::Https,
            ),
            (Err(udp), None) => return Err(udp),
        };
        credential(token, entry, &self.source.issuer, &device, via)
    }

    /// Prefers the current entry, then tries the others until an entry registers its lease.
    async fn admit(&self, catalog: &Catalog, current: Option<&Credential>) -> Result<Credential> {
        let mut entries: Vec<_> = catalog.entries.iter().collect();
        if let Some(current) = current {
            let audience = relay_audience(&current.relay)?;
            if let Some(index) = entries
                .iter()
                .position(|entry| origin(&entry.relay_url).is_ok_and(|relay| relay == audience))
            {
                let preferred = entries.remove(index);
                entries.insert(0, preferred);
            }
        }
        let mut failures = Vec::new();
        for entry in entries {
            match self.request_grant(entry).await {
                Ok(credential) => {
                    match register(&credential.gateway_url, &credential.token).await {
                        Ok(()) => {
                            self.install(&credential).await;
                            return Ok(credential);
                        }
                        Err(error) => failures.push(format!("{}: {error:#}", entry.gateway_url)),
                    }
                }
                Err(error) => failures.push(format!("{}: {error:#}", entry.relay_url)),
            }
        }
        bail!(
            "no catalogue entry registered access: {}",
            failures.join("; ")
        )
    }

    /// Puts the granted relay into the endpoint's relay map, replacing any bare copy.
    async fn install(&self, credential: &Credential) {
        let mut relays = self.relays.lock().await;
        relays.insert(credential.relay.clone());
        let _previous = self
            .endpoint
            .insert_relay(
                credential.relay.clone(),
                Arc::new(relay_config(
                    credential.relay.clone(),
                    Some(credential.token.clone()),
                )),
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(relay: &str, gateway: &str) -> CatalogConfig {
        CatalogConfig {
            entries: vec![CatalogEntry {
                gateway_url: gateway.into(),
                relay_url: relay.into(),
            }],
            grant_ttl_ms: DEFAULT_GRANT_TTL_MS,
        }
    }

    fn ip() -> IpAddr {
        "203.0.113.7".parse().unwrap()
    }

    #[test]
    fn public_defaults_come_from_the_shared_file() -> Result<()> {
        let file: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../config/public-bootstrap.json"),
        )?)?;
        let defaults = PublicDefaults::load()?;
        assert_eq!(defaults.issuer, file["issuer"].as_str().unwrap());
        assert_eq!(defaults.https_base, file["https_base"].as_str().unwrap());
        let source = Bootstrap::Public.source()?.unwrap();
        assert_eq!(source.issuer, defaults.issuer);
        assert_eq!(
            source.https.as_deref(),
            Some(defaults.https_base.trim_end_matches('/'))
        );
        let server = super::super::discovery::Server::parse(&source.discovery)?;
        assert_eq!(device_id(&server.id), defaults.issuer);
        assert_eq!(server.host, file["udp_host"].as_str().unwrap());
        assert_eq!(u64::from(server.port), file["udp_port"].as_u64().unwrap());
        assert!(Bootstrap::Off.source()?.is_none());
        assert_eq!(
            serde_json::to_value(Bootstrap::default())?,
            serde_json::json!({"mode":"off"})
        );
        Ok(())
    }

    #[test]
    fn custom_sources_pin_the_discovery_identity_and_reject_bad_https() -> Result<()> {
        let id = LocalIdentity::generate().device_id()?;
        let custom = Bootstrap::Custom {
            discovery: format!("vibeface://{id}@disc.example:4792"),
            https: Some("https://disc.example/bootstrap/".into()),
        };
        let source = custom.source()?.unwrap();
        assert_eq!(source.issuer, id);
        assert_eq!(
            source.https.as_deref(),
            Some("https://disc.example/bootstrap")
        );
        for https in [
            "http://disc.example/bootstrap",
            "https://user:pw@disc.example/bootstrap",
            "https://disc.example/bootstrap?x=1",
            "https://disc.example/bootstrap#f",
            "ftp://disc.example",
        ] {
            let bad = Bootstrap::Custom {
                discovery: format!("vibeface://{id}@disc.example:4792"),
                https: Some(https.into()),
            };
            assert!(bad.validate().is_err(), "{https}");
        }
        assert!(
            Bootstrap::Custom {
                discovery: "https://disc.example".into(),
                https: None,
            }
            .validate()
            .is_err()
        );
        assert_eq!(
            Bootstrap::Custom {
                discovery: format!("vibeface://{id}@disc.example:4792"),
                https: Some("http://127.0.0.1:8080/bootstrap".into()),
            }
            .source()?
            .unwrap()
            .https
            .as_deref(),
            Some("http://127.0.0.1:8080/bootstrap")
        );
        Ok(())
    }

    #[test]
    fn origins_are_normalized_and_bounded() {
        assert_eq!(
            origin("https://App.Example:443/").unwrap(),
            "https://app.example"
        );
        assert_eq!(
            origin("http://127.0.0.1:3340").unwrap(),
            "http://127.0.0.1:3340"
        );
        for bad in [
            "http://app.example",
            "https://app.example/path",
            "https://u:p@app.example",
            "https://app.example/?q",
            "https://app.example/#f",
            "wss://app.example",
        ] {
            assert!(origin(bad).is_err(), "{bad}");
        }
        assert!(origin(&format!("https://{}.example", "a".repeat(300))).is_err());
        let relay: RelayUrl = "https://app.example/".parse().unwrap();
        assert_eq!(relay_audience(&relay).unwrap(), "https://app.example");
    }

    #[test]
    fn catalogues_open_only_when_current_and_signed_by_the_pinned_issuer() -> Result<()> {
        let identity = LocalIdentity::generate();
        let issuer = Issuer::new(
            identity.clone(),
            config("https://relay.example", "https://app.example"),
        )?;
        let now = now_ms();
        let signed = issuer.catalog(now)?;
        let catalog = signed.open(&identity.device_id()?, now)?;
        assert_eq!(catalog.entries.len(), 1);
        assert_eq!(catalog.entries[0].relay_url, "https://relay.example");
        assert_eq!(
            issuer.catalog(now + 1000)?,
            signed,
            "the cache serves a young catalogue"
        );
        assert_ne!(issuer.catalog(now + CATALOG_RESIGN_MS + 1)?, signed);
        let other = LocalIdentity::generate().device_id()?;
        assert!(signed.open(&other, now).is_err(), "another pinned issuer");
        assert!(
            signed
                .open(&identity.device_id()?, now + CATALOG_TTL_MS + 1)
                .is_err(),
            "expired"
        );
        let mut tampered = signed.clone();
        tampered.payload = tampered.payload.replace("relay.example", "evil.example");
        assert!(tampered.open(&identity.device_id()?, now).is_err());
        let mut forged = signed.clone();
        forged.signature =
            LocalIdentity::generate().sign_payload(CATALOG_DOMAIN, forged.payload.as_bytes())?;
        assert!(forged.open(&identity.device_id()?, now).is_err());
        let claims = Catalog {
            version: 1,
            issuer: other.clone(),
            issued_at_ms: now,
            expires_at_ms: now + 1000,
            entries: catalog.entries.clone(),
        };
        let payload = serde_json::to_string(&claims)?;
        let relabelled = SignedCatalog {
            signature: identity.sign_payload(CATALOG_DOMAIN, payload.as_bytes())?,
            payload,
        };
        assert!(
            relabelled.open(&identity.device_id()?, now).is_err(),
            "payload naming another issuer"
        );
        let mut many = config("https://relay.example", "https://app.example");
        many.entries = (0..5)
            .map(|i| CatalogEntry {
                gateway_url: format!("https://app{i}.example"),
                relay_url: format!("https://relay{i}.example"),
            })
            .collect();
        assert!(Issuer::new(identity.clone(), many).is_err());
        assert!(
            Issuer::new(
                identity.clone(),
                config("http://relay.example", "https://app.example")
            )
            .is_err()
        );
        let huge = SignedCatalog {
            payload: "x".repeat(MAX_SIGNED_CATALOG_BYTES),
            signature: "0".repeat(128),
        };
        assert!(huge.open(&identity.device_id()?, now).is_err());
        Ok(())
    }

    #[test]
    fn grants_bind_issuer_device_relay_and_time() -> Result<()> {
        let identity = LocalIdentity::generate();
        let issuer = Issuer::new(
            identity.clone(),
            config("https://relay.example", "https://app.example"),
        )?;
        let device = LocalIdentity::generate().device_id()?;
        let endpoint = endpoint_id(&device)?;
        let relay: RelayUrl = "https://relay.example".parse().unwrap();
        let now = now_ms();
        let signed = issuer.grant(&device, "https://relay.example/", ip(), now)?;
        assert!(signed.as_str().len() <= MAX_GRANT_BYTES);
        let grant = signed.verify(&identity.device_id()?, &endpoint, &relay, now)?;
        assert_eq!(grant.device_id, device);
        assert_eq!(grant.relay_url, "https://relay.example");
        assert_eq!(
            grant.expires_at_ms - grant.issued_at_ms,
            DEFAULT_GRANT_TTL_MS
        );
        let reparsed = SignedGrant::parse(signed.as_str())?;
        assert_eq!(reparsed, signed);
        assert!(
            signed
                .verify(
                    &LocalIdentity::generate().device_id()?,
                    &endpoint,
                    &relay,
                    now
                )
                .is_err(),
            "another issuer"
        );
        let stranger = endpoint_id(&LocalIdentity::generate().device_id()?)?;
        assert!(
            signed
                .verify(&identity.device_id()?, &stranger, &relay, now)
                .is_err(),
            "another device"
        );
        let elsewhere: RelayUrl = "https://other.example".parse().unwrap();
        assert!(
            signed
                .verify(&identity.device_id()?, &endpoint, &elsewhere, now)
                .is_err(),
            "another relay"
        );
        assert!(
            signed
                .verify(
                    &identity.device_id()?,
                    &endpoint,
                    &relay,
                    now + DEFAULT_GRANT_TTL_MS
                )
                .is_err(),
            "expired"
        );
        assert!(
            signed
                .verify(&identity.device_id()?, &endpoint, &relay, now - SKEW_MS - 1)
                .is_err(),
            "issued in the future"
        );
        let (claims, signature) = signed.as_str().split_once('.').unwrap();
        let mut forged = serde_json::from_slice::<Grant>(&URL_SAFE_NO_PAD.decode(claims)?)?;
        forged.expires_at_ms += 60_000;
        let forged = format!(
            "{}.{signature}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged)?)
        );
        assert!(
            SignedGrant::parse(&forged)?
                .verify(&identity.device_id()?, &endpoint, &relay, now)
                .is_err(),
            "tampered"
        );
        let mut long = serde_json::from_slice::<Grant>(&URL_SAFE_NO_PAD.decode(claims)?)?;
        long.expires_at_ms = long.issued_at_ms + MAX_GRANT_TTL_MS + 1;
        let long_claims = serde_json::to_vec(&long)?;
        let long_token = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&long_claims),
            identity.sign_payload(GRANT_DOMAIN, &long_claims)?
        );
        assert!(
            SignedGrant::parse(&long_token)?
                .verify(&identity.device_id()?, &endpoint, &relay, now)
                .is_err(),
            "a lifetime beyond an hour fails even with a valid signature"
        );
        let unencodable = format!("!.{}", "0".repeat(128));
        let oversized = "a".repeat(MAX_GRANT_BYTES + 1);
        for malformed in [
            "",
            "abc",
            "abc.def",
            unencodable.as_str(),
            oversized.as_str(),
        ] {
            assert!(SignedGrant::parse(malformed).is_err(), "{malformed}");
        }
        assert!(
            issuer
                .grant(&device, "https://other.example", ip(), now)
                .is_err(),
            "relay outside the catalogue"
        );
        assert!(
            issuer
                .grant("not-a-device", "https://relay.example", ip(), now)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn issuer_budgets_are_per_device_and_per_address_and_bounded() -> Result<()> {
        let identity = LocalIdentity::generate();
        let issuer = Issuer::new(
            identity,
            config("https://relay.example", "https://app.example"),
        )?;
        let now = now_ms();
        let device = LocalIdentity::generate().device_id()?;
        for _ in 0..DEVICE_GRANTS_PER_WINDOW {
            issuer.grant(&device, "https://relay.example", ip(), now)?;
        }
        assert!(
            issuer
                .grant(&device, "https://relay.example", ip(), now)
                .is_err()
        );
        issuer.grant(
            &device,
            "https://relay.example",
            ip(),
            now + BUDGET_WINDOW_MS,
        )?;
        let other: IpAddr = "198.51.100.9".parse()?;
        for _ in 0..IP_GRANTS_PER_WINDOW {
            let device = LocalIdentity::generate().device_id()?;
            issuer.grant(&device, "https://relay.example", other, now)?;
        }
        assert!(
            issuer
                .grant(
                    &LocalIdentity::generate().device_id()?,
                    "https://relay.example",
                    other,
                    now
                )
                .is_err()
        );
        let mut budget = Budget::default();
        for i in 0..MAX_BUDGET_ENTRIES {
            let address = IpAddr::V4(std::net::Ipv4Addr::from(0x0a00_0000u32 + i as u32));
            budget.admit(&format!("{i:064x}"), address, now)?;
        }
        assert!(
            budget.admit("ff".repeat(32).as_str(), ip(), now).is_err(),
            "a full table refuses"
        );
        budget.admit("ff".repeat(32).as_str(), ip(), now + BUDGET_WINDOW_MS)?;
        Ok(())
    }

    #[test]
    fn signed_requests_prove_the_device_within_a_window() -> Result<()> {
        let identity = LocalIdentity::generate();
        let now = now_ms();
        let request = SignedRequest::sign(&identity, "https://relay.example", now)?;
        let opened = request.open(now + 1000)?;
        assert_eq!(opened.device_id, identity.device_id()?);
        assert!(request.open(now + REQUEST_WINDOW_MS + 1).is_err(), "stale");
        let mut tampered = request.clone();
        tampered.payload = tampered.payload.replace("relay.example", "other.example");
        assert!(tampered.open(now).is_err());
        let mut forged = request.clone();
        forged.signature =
            LocalIdentity::generate().sign_payload(REQUEST_DOMAIN, forged.payload.as_bytes())?;
        assert!(forged.open(now).is_err());
        let huge = SignedRequest {
            payload: "x".repeat(MAX_REQUEST_BYTES + 1),
            signature: "0".repeat(128),
        };
        assert!(huge.open(now).is_err());
        let issuer = Issuer::new(
            LocalIdentity::generate(),
            config("https://relay.example", "https://app.example"),
        )?;
        let grant = issuer.grant_signed(&request, ip(), now)?;
        assert_eq!(grant.claims()?.device_id, identity.device_id()?);
        Ok(())
    }

    #[test]
    fn relay_admission_remembers_registered_renewals_until_they_expire() -> Result<()> {
        let discovery = LocalIdentity::generate();
        let mut short = config("http://127.0.0.1:3340", "http://127.0.0.1:4790");
        short.grant_ttl_ms = 5000;
        let issuer = Issuer::new(discovery.clone(), short)?;
        let relay: RelayUrl = "http://127.0.0.1:3340".parse().unwrap();
        let admission = GrantAdmission::new(discovery.device_id()?, relay.clone())?;
        let node = LocalIdentity::generate();
        let endpoint = endpoint_id(&node.device_id()?)?;
        let now = now_ms();
        assert!(!admission.admits(&endpoint, None, now));
        let first = issuer.grant(&node.device_id()?, "http://127.0.0.1:3340", ip(), now)?;
        assert!(admission.admits(&endpoint, Some(first.as_str()), now));
        assert!(
            admission.admits(&endpoint, None, now + 4000),
            "a verified connect is remembered"
        );
        assert!(
            !admission.admits(&endpoint, Some(first.as_str()), now + 5000),
            "expired token, nothing registered"
        );
        let stranger = endpoint_id(&LocalIdentity::generate().device_id()?)?;
        assert!(
            !admission.admits(&stranger, Some(first.as_str()), now),
            "a grant does not admit another device"
        );
        let renewed = issuer.grant(
            &node.device_id()?,
            "http://127.0.0.1:3340",
            ip(),
            now + 3000,
        )?;
        let registered = admission.register(renewed.as_str(), now + 3000)?;
        assert_eq!(registered.expires_at_ms, now + 8000);
        assert_eq!(admission.admitted_until(&endpoint), Some(now + 8000));
        assert!(
            admission.admits(&endpoint, Some(first.as_str()), now + 6000),
            "a reconnect with the stale token gets in on the registered renewal"
        );
        assert!(!admission.admits(&endpoint, Some(first.as_str()), now + 8000));
        assert!(
            admission.register(renewed.as_str(), now + 9000).is_err(),
            "an expired grant cannot be registered"
        );
        let wrong_issuer = Issuer::new(
            LocalIdentity::generate(),
            config("http://127.0.0.1:3340", "http://127.0.0.1:4790"),
        )?;
        let foreign = wrong_issuer.grant(&node.device_id()?, "http://127.0.0.1:3340", ip(), now)?;
        assert!(admission.register(foreign.as_str(), now).is_err());
        assert!(!admission.admits(&endpoint, Some(foreign.as_str()), now + 8001));
        let elsewhere = GrantAdmission::new(
            discovery.device_id()?,
            "http://127.0.0.1:9999".parse().unwrap(),
        )?;
        assert!(
            !elsewhere.admits(&endpoint, Some(renewed.as_str()), now + 3000),
            "another relay's audience"
        );
        admission.forget_all();
        assert!(!admission.admits(&endpoint, None, now + 3000));
        Ok(())
    }

    #[test]
    fn relay_admission_refuses_new_identity_when_full_and_preserves_leases() -> Result<()> {
        let discovery = LocalIdentity::generate();
        let issuer = Issuer::new(
            discovery.clone(),
            config("https://relay.example", "https://app.example"),
        )?;
        let admission = GrantAdmission::new(
            discovery.device_id()?,
            "https://relay.example".parse().unwrap(),
        )?;
        let now = now_ms();
        let first = LocalIdentity::generate().device_id()?;
        let first_device = endpoint_id(&first)?;
        let first_token = issuer.grant(&first, "https://relay.example", ip(), now)?;
        let first_grant = admission.register(first_token.as_str(), now)?;
        for _ in 1..MAX_ADMITTED {
            let device = endpoint_id(&LocalIdentity::generate().device_id()?)?;
            admission.remember(device, first_grant.expires_at_ms, now)?;
        }
        let before = admission.admitted.lock().unwrap().clone();
        let newcomer = endpoint_id(&LocalIdentity::generate().device_id()?)?;
        let token = issuer.grant(&device_id(&newcomer), "https://relay.example", ip(), now)?;
        assert!(admission.register(token.as_str(), now).is_err());
        assert!(!admission.admits(&newcomer, Some(token.as_str()), now));
        assert_eq!(*admission.admitted.lock().unwrap(), before);
        let later = now + 1000;
        let token = issuer.grant(&first, "https://relay.example", ip(), later)?;
        let renewed = admission.register(token.as_str(), later)?;
        assert!(renewed.expires_at_ms > first_grant.expires_at_ms);
        admission.register(first_token.as_str(), later)?;
        assert_eq!(
            admission.admitted_until(&first_device),
            Some(renewed.expires_at_ms)
        );
        assert!(admission.admits(&first_device, None, later));
        assert_eq!(admission.admitted.lock().unwrap().len(), MAX_ADMITTED);
        Ok(())
    }

    #[test]
    fn relay_admission_reuses_slots_after_expiry() -> Result<()> {
        let discovery = LocalIdentity::generate();
        let issuer = Issuer::new(
            discovery.clone(),
            config("https://relay.example", "https://app.example"),
        )?;
        let admission = GrantAdmission::new(
            discovery.device_id()?,
            "https://relay.example".parse().unwrap(),
        )?;
        let now = now_ms();
        let first = endpoint_id(&LocalIdentity::generate().device_id()?)?;
        admission.remember(first, now + 1000, now)?;
        for _ in 1..MAX_ADMITTED {
            let device = endpoint_id(&LocalIdentity::generate().device_id()?)?;
            admission.remember(device, now + 1000, now)?;
        }
        let replacement = endpoint_id(&LocalIdentity::generate().device_id()?)?;
        let later = now + 2000;
        let token = issuer.grant(
            &device_id(&replacement),
            "https://relay.example",
            ip(),
            later,
        )?;
        let grant = admission.register(token.as_str(), later)?;
        assert_eq!(admission.admitted.lock().unwrap().len(), 1);
        assert!(admission.admitted_until(&first).is_none());
        assert_eq!(
            admission.admitted_until(&replacement),
            Some(grant.expires_at_ms)
        );
        assert!(admission.admits(&replacement, None, later));
        Ok(())
    }

    #[test]
    fn state_exposes_only_grants_valid_now() {
        let admitted = Admitted {
            gateway_url: "https://app.example".into(),
            relay_url: "https://relay.example".into(),
            grant_until_ms: 10_000,
            via: Transport::Udp,
        };
        let state = BootstrapState::Admitted(admitted.clone());
        assert_eq!(state.admitted(9_999), Some(&admitted));
        assert!(state.admitted(10_000).is_none());
        let degraded = BootstrapState::Degraded {
            since_ms: 1,
            reason: "renewal failed".into(),
            admitted: Some(admitted.clone()),
        };
        assert_eq!(degraded.admitted(5_000), Some(&admitted));
        assert!(
            BootstrapState::Seeking {
                since_ms: 1,
                reason: None
            }
            .admitted(0)
            .is_none()
        );
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["state"], "admitted");
        assert_eq!(json["gateway_url"], "https://app.example");
        assert_eq!(
            serde_json::from_value::<BootstrapState>(json).unwrap(),
            state
        );
    }

    #[test]
    fn wasm_catalogue_vector_verifies_in_rust() -> Result<()> {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/bootstrap-catalog.json"
        )))?;
        let signed: SignedCatalog = serde_json::from_value(fixture["catalog"].clone())?;
        let issuer = fixture["issuer"].as_str().expect("fixture issuer");
        let now = fixture["now_ms"].as_i64().expect("fixture time");
        let catalog = signed.open(issuer, now)?;
        assert_eq!(catalog.entries.len(), 1);
        assert_eq!(
            catalog.entries[0].gateway_url,
            "https://gateway.fixture.test"
        );
        assert_eq!(catalog.entries[0].relay_url, "https://relay.fixture.test");
        Ok(())
    }

    #[test]
    fn renewal_falls_between_half_and_six_tenths_of_the_lifetime() {
        let now = now_ms();
        let credential = Credential {
            relay: "https://relay.example".parse().unwrap(),
            gateway_url: "https://app.example".into(),
            token: String::new(),
            issued_at_ms: now,
            expires_at_ms: now + 60_000,
            via: Transport::Https,
        };
        for _ in 0..32 {
            let delay = jittered_renewal(&credential, now).as_millis() as i64;
            assert!((30_000..36_000).contains(&delay), "{delay}");
        }
        assert_eq!(
            credential.admitted().relay_url,
            "https://relay.example",
            "the status shows the relay as an origin"
        );
    }
}
