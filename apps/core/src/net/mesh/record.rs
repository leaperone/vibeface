// A Node's signed public record on the mesh. The delivery probe includes this file and pins its
// SHA-256, so it uses only anyhow, serde, serde_json, vibeface_secure and std.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::net::{Ipv6Addr, SocketAddr};

pub const DOMAIN: &str = "mesh-v1";
pub const MAX_PAYLOAD: usize = 1024;
pub const DEFAULT_TTL_S: u32 = 1800;
/// How far past this clock a record's `seq` may be.
pub const MAX_AHEAD_MS: u64 = 5 * 60 * 1000;
const MAX_ADDRS: usize = 8;
const MAX_ROLES: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedMeshRecord {
    /// The record's JSON text exactly as signed.
    pub payload: String,
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshRecord {
    pub v: u8,
    pub network: String,
    pub device_id: String,
    /// Milliseconds since the Unix epoch when signed; a device's later record has a larger one.
    pub seq: u64,
    pub ttl_s: u32,
    /// Public addresses first, then local ones; never another device's.
    pub addrs: Vec<SocketAddr>,
    /// The device's home relay.
    pub relay_url: Option<String>,
    pub reachable: Reachable,
    pub roles: Vec<Role>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reachable {
    Public,
    Nat,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Role {
    Relay {
        url: String,
        access: Access,
        rate: u32,
        max_clients: u32,
        #[serde(default)]
        offer: Option<Offer>,
    },
    /// An HTTPS origin that forwards `/api/client` to any device.
    Gateway { url: String },
    Service {
        service: ServiceKind,
        #[serde(default)]
        offer: Option<Offer>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Open,
    Token,
    Paid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    Delivery,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub asset: String,
    pub price: String,
    pub escrow: String,
    pub grant_endpoint: String,
}

impl SignedMeshRecord {
    pub fn sign(record: &MeshRecord, sign: impl FnOnce(&[u8]) -> Result<String>) -> Result<Self> {
        record.check()?;
        let payload = serde_json::to_string(record)?;
        ensure!(payload.len() <= MAX_PAYLOAD, "mesh record is too large");
        let signature = sign(payload.as_bytes())?;
        Ok(Self { payload, signature })
    }

    /// The record, if its device signed it for `network` and it is current at `now_ms`.
    pub fn open(&self, network: &str, now_ms: u64) -> Result<MeshRecord> {
        let record = MeshRecord::parse(&self.payload)?;
        ensure!(
            record.network == network,
            "mesh record belongs to another network"
        );
        ensure!(now_ms < record.expires_at(), "mesh record has expired");
        ensure!(
            record.seq <= now_ms.saturating_add(MAX_AHEAD_MS),
            "mesh record is dated ahead of this clock"
        );
        vibeface_secure::verify(
            &record.device_id,
            DOMAIN,
            self.payload.as_bytes(),
            &self.signature,
        )?;
        Ok(record)
    }
}

impl MeshRecord {
    /// The record in `payload`, unsigned and undated: only for records this Node already checked.
    pub fn parse(payload: &str) -> Result<Self> {
        ensure!(payload.len() <= MAX_PAYLOAD, "mesh record is too large");
        let record: Self = serde_json::from_str(payload).context("invalid mesh record")?;
        record.check()?;
        Ok(record)
    }

    pub fn expires_at(&self) -> u64 {
        self.seq.saturating_add(u64::from(self.ttl_s) * 1000)
    }

    fn check(&self) -> Result<()> {
        ensure!(self.v == 1, "unsupported mesh record version");
        valid_network(&self.network)?;
        vibeface_secure::validate_device_id(&self.device_id)?;
        ensure!(
            (60..=3600).contains(&self.ttl_s),
            "mesh record lifetime is out of range"
        );
        ensure!(
            self.addrs.len() <= MAX_ADDRS && self.roles.len() <= MAX_ROLES,
            "mesh record lists too many addresses or roles"
        );
        if let Some(url) = &self.relay_url {
            relay_url(url)?;
        }
        for role in &self.roles {
            match role {
                Role::Relay { url, .. } => relay_url(url)?,
                Role::Gateway { url } => gateway_url(url)?,
                Role::Service { .. } => {}
            }
        }
        Ok(())
    }
}

/// `^[a-z0-9][a-z0-9-]{0,31}$`
pub fn valid_network(name: &str) -> Result<()> {
    ensure!(
        (1..=32).contains(&name.len())
            && name.bytes().enumerate().all(|(at, b)| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || (at > 0 && b == b'-')
            }),
        "a mesh network name is up to 32 lowercase letters, digits and dashes, not starting with a dash"
    );
    Ok(())
}

/// `http(s)://host[:port][/path]`, without credentials, query or fragment.
pub fn relay_url(url: &str) -> Result<()> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .context("a relay URL is HTTP(S)")?;
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    host_port(authority)?;
    ensure!(
        path.bytes()
            .all(|b| b.is_ascii_graphic() && b != b'?' && b != b'#'),
        "a relay URL has no query or fragment"
    );
    Ok(())
}

/// `https://host[:port]`, an origin.
pub fn gateway_url(url: &str) -> Result<()> {
    let authority = url
        .strip_prefix("https://")
        .context("a gateway is an HTTPS origin")?;
    host_port(authority.strip_suffix('/').unwrap_or(authority))
}

fn host_port(authority: &str) -> Result<()> {
    let port = match authority.strip_prefix('[') {
        Some(rest) => {
            let (ip, port) = rest.split_once(']').context("invalid URL host")?;
            ip.parse::<Ipv6Addr>().context("invalid URL host")?;
            port
        }
        None => {
            let (host, port) = authority.split_at(authority.find(':').unwrap_or(authority.len()));
            ensure!(
                !host.is_empty()
                    && host
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'),
                "invalid URL host"
            );
            port
        }
    };
    if !port.is_empty() {
        let digits = port.strip_prefix(':').context("invalid URL port")?;
        ensure!(
            !digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit())
                && digits.parse::<u16>().is_ok_and(|port| port != 0),
            "invalid URL port"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn record(identity: &vibeface_secure::Identity, seq: u64) -> MeshRecord {
        MeshRecord {
            v: 1,
            network: "vibeface".into(),
            device_id: identity.device_id(),
            seq,
            ttl_s: DEFAULT_TTL_S,
            addrs: vec!["203.0.113.7:4790".parse().unwrap()],
            relay_url: Some("https://relay.example/".into()),
            reachable: Reachable::Public,
            roles: vec![Role::Gateway {
                url: "https://node.example:4792".into(),
            }],
        }
    }

    fn signed(identity: &vibeface_secure::Identity, record: &MeshRecord) -> SignedMeshRecord {
        SignedMeshRecord::sign(record, |payload| Ok(identity.sign(DOMAIN, payload)?)).unwrap()
    }

    #[test]
    fn only_current_records_signed_for_this_network_open() {
        let identity = vibeface_secure::Identity::generate().unwrap();
        let valid = signed(&identity, &record(&identity, NOW));
        assert_eq!(valid.open("vibeface", NOW).unwrap(), record(&identity, NOW));

        let expired = valid.open("vibeface", NOW + u64::from(DEFAULT_TTL_S) * 1000);
        assert!(expired.unwrap_err().to_string().contains("expired"));

        let ahead = signed(&identity, &record(&identity, NOW + MAX_AHEAD_MS + 1));
        assert!(
            ahead
                .open("vibeface", NOW)
                .unwrap_err()
                .to_string()
                .contains("ahead")
        );

        let other = vibeface_secure::Identity::generate().unwrap();
        let forged = SignedMeshRecord {
            signature: other.sign(DOMAIN, valid.payload.as_bytes()).unwrap(),
            ..valid.clone()
        };
        assert!(forged.open("vibeface", NOW).is_err());

        let mut large = record(&identity, NOW);
        large.addrs = vec!["[2001:db8::1]:4790".parse().unwrap(); 8];
        large.roles = vec![
            Role::Relay {
                url: format!("https://{}.example/", "r".repeat(200)),
                access: Access::Open,
                rate: 1,
                max_clients: 1,
                offer: None,
            };
            4
        ];
        assert!(SignedMeshRecord::sign(&large, |_| Ok(String::new())).is_err());
        let oversize = SignedMeshRecord {
            payload: serde_json::to_string(&large).unwrap(),
            signature: identity.sign(DOMAIN, b"").unwrap(),
        };
        assert!(
            oversize
                .open("vibeface", NOW)
                .unwrap_err()
                .to_string()
                .contains("too large")
        );

        assert!(
            valid
                .open("other", NOW)
                .unwrap_err()
                .to_string()
                .contains("another network")
        );
    }

    #[test]
    fn urls_and_network_names_keep_their_shape() {
        for good in [
            "http://relay.example",
            "https://10.0.0.1:3340/",
            "http://[::1]:3340/relay",
        ] {
            relay_url(good).unwrap();
        }
        for bad in [
            "ftp://relay.example",
            "https://user@relay.example",
            "https://relay.example/?a",
            "https://relay.example#x",
            "https://:80",
            "https://relay.example:0",
        ] {
            assert!(relay_url(bad).is_err(), "{bad}");
        }
        gateway_url("https://node.example:4792").unwrap();
        gateway_url("https://node.example/").unwrap();
        for bad in [
            "http://node.example",
            "https://node.example/api",
            "https://node.example?x",
        ] {
            assert!(gateway_url(bad).is_err(), "{bad}");
        }
        valid_network("vibeface").unwrap();
        valid_network("lab-2").unwrap();
        for bad in ["", "-lab", "Lab", "lab_2", &"a".repeat(33)] {
            assert!(valid_network(bad).is_err(), "{bad}");
        }
    }
}
