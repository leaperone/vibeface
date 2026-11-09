//! The two `vibeface://` links.
//!
//! A node link says who a Node is and where it was last reachable:
//! `vibeface://node/<device-id>?addr=<host>:<port>&relay=<URL>`, with up to four `addr` and at
//! most one `relay`. A Node shares its own, and a bootstrap node is given as one.
//!
//! An invite link joins a personal network: `vibeface://join/vibeface1…`, whose
//! [`Ticket`](crate::join::Ticket) carries the entry's node link and a one-time secret.

use crate::{Result, decode32, validate_device_id};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    net::{IpAddr, SocketAddr},
};

pub const NODE_LINK: &str = "vibeface://node/";
pub const JOIN_LINK: &str = "vibeface://join/";
pub const MAX_ADDRS: usize = 4;
const MAX_HOST: usize = 253;
const MAX_RELAY: usize = 256;
const MAX_LINK: usize = 2048;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Host {
    Ip(IpAddr),
    /// A lowercase DNS name.
    Name(String),
}

/// A UDP address of a Node: an IP address, or a DNS name resolved when dialing.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Addr {
    pub host: Host,
    pub port: u16,
}

impl From<SocketAddr> for Addr {
    fn from(address: SocketAddr) -> Self {
        Self {
            host: Host::Ip(address.ip()),
            port: address.port(),
        }
    }
}

impl Addr {
    /// `1.2.3.4:4790`, `[2001:db8::1]:4790` or `node.example:4790`.
    pub fn parse(text: &str) -> Result<Self> {
        if let Ok(address) = text.parse::<SocketAddr>() {
            let addr = Self::from(address);
            addr.check()?;
            return Ok(addr);
        }
        let (host, port) = text.rsplit_once(':').ok_or("an address is <host>:<port>")?;
        let addr = Self {
            host: Host::Name(host.to_owned()),
            port: port.parse().map_err(|_| "an address is <host>:<port>")?,
        };
        addr.check()?;
        Ok(addr)
    }

    fn check(&self) -> Result<()> {
        if self.port == 0 {
            return Err("an address needs a port".into());
        }
        if let Host::Name(name) = &self.host {
            let valid = (1..=MAX_HOST).contains(&name.len())
                && name.bytes().any(|b| b.is_ascii_lowercase())
                && name.split('.').all(|label| {
                    !label.is_empty()
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                });
            if !valid {
                return Err("an address host is an IP address or a lowercase DNS name".into());
            }
        }
        Ok(())
    }
}

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Host::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]:{}", self.port),
            Host::Ip(IpAddr::V4(ip)) => write!(f, "{ip}:{}", self.port),
            Host::Name(name) => write!(f, "{name}:{}", self.port),
        }
    }
}

/// Who a Node is and where to dial it. The addresses and relay are hints; the QUIC handshake
/// proves the device.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeLink {
    pub id: [u8; 32],
    pub addrs: Vec<Addr>,
    pub relay: Option<String>,
}

impl NodeLink {
    pub fn new(device_id: &str, addrs: Vec<Addr>, relay: Option<String>) -> Result<Self> {
        validate_device_id(device_id)?;
        let link = Self {
            id: decode32(device_id)?,
            addrs,
            relay,
        };
        link.check()?;
        Ok(link)
    }

    pub fn device_id(&self) -> String {
        hex::encode(self.id)
    }

    /// Whether the link says where the Node is, not only who.
    pub fn has_hints(&self) -> bool {
        !self.addrs.is_empty() || self.relay.is_some()
    }

    pub fn check(&self) -> Result<()> {
        validate_device_id(&self.device_id())?;
        if self.addrs.len() > MAX_ADDRS {
            return Err(format!("a node link has at most {MAX_ADDRS} addresses").into());
        }
        for addr in &self.addrs {
            addr.check()?;
        }
        if let Some(relay) = &self.relay {
            let rest = relay
                .strip_prefix("https://")
                .or_else(|| relay.strip_prefix("http://"))
                .filter(|rest| !rest.is_empty() && !rest.starts_with('/'));
            let plain = relay
                .bytes()
                .all(|b| b.is_ascii_graphic() && !b"&#?%\"<>\\`{}|^".contains(&b));
            if rest.is_none() || !plain || relay.len() > MAX_RELAY {
                return Err("a node link's relay is a plain http(s) URL".into());
            }
        }
        Ok(())
    }

    pub fn parse(text: &str) -> Result<Self> {
        const FORMAT: &str =
            "a node link is vibeface://node/<device-id>?addr=<host>:<port>&relay=<URL>";
        let text = text.trim();
        if text.len() > MAX_LINK {
            return Err("node link is too long".into());
        }
        let rest = text.strip_prefix(NODE_LINK).ok_or(FORMAT)?;
        let (device, query) = match rest.split_once('?') {
            Some((device, query)) => (device, Some(query)),
            None => (rest, None),
        };
        let mut addrs = Vec::new();
        let mut relay = None;
        for pair in query.into_iter().flat_map(|query| query.split('&')) {
            match pair.split_once('=').ok_or(FORMAT)? {
                ("addr", value) => addrs.push(Addr::parse(value)?),
                ("relay", value) if relay.is_none() => relay = Some(value.to_owned()),
                _ => return Err(FORMAT.into()),
            }
        }
        Self::new(device, addrs, relay)
    }
}

impl fmt::Display for NodeLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{NODE_LINK}{}", self.device_id())?;
        let mut separator = '?';
        for addr in &self.addrs {
            write!(f, "{separator}addr={addr}")?;
            separator = '&';
        }
        if let Some(relay) = &self.relay {
            write!(f, "{separator}relay={relay}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;

    #[test]
    fn node_links_round_trip_their_hints() {
        let id = Identity::generate().unwrap().device_id();
        let text = format!(
            "vibeface://node/{id}?addr=203.0.113.5:4790&addr=[2001:db8::1]:4790&addr=node.example:4792&relay=https://relay.example:8443"
        );
        let link = NodeLink::parse(&text).unwrap();
        assert_eq!(link.device_id(), id);
        assert_eq!(
            link.addrs,
            [
                Addr::from("203.0.113.5:4790".parse::<SocketAddr>().unwrap()),
                Addr::from("[2001:db8::1]:4790".parse::<SocketAddr>().unwrap()),
                Addr {
                    host: Host::Name("node.example".into()),
                    port: 4792
                },
            ]
        );
        assert_eq!(link.relay.as_deref(), Some("https://relay.example:8443"));
        assert_eq!(link.to_string(), text);
        let bare = NodeLink::parse(&format!("vibeface://node/{id}")).unwrap();
        assert!(!bare.has_hints());
        assert_eq!(bare.to_string(), format!("vibeface://node/{id}"));
    }

    #[test]
    fn node_links_reject_other_shapes_and_unbounded_hints() {
        let id = Identity::generate().unwrap().device_id();
        let addr = "addr=203.0.113.5:4790";
        for bad in [
            format!("vibeface://{id}@203.0.113.5:4790"),
            format!("vibeface://join/{id}"),
            format!("vibeface://node/{id}/"),
            format!("vibeface://node/{id}?"),
            format!("vibeface://node/{}", "z".repeat(64)),
            format!("vibeface://node/{id}?{addr}&{addr}&{addr}&{addr}&{addr}"),
            format!("vibeface://node/{id}?addr=203.0.113.5"),
            format!("vibeface://node/{id}?addr=203.0.113.5:0"),
            format!("vibeface://node/{id}?addr=1.2.3:4790"),
            format!("vibeface://node/{id}?addr=Node.Example:4790"),
            format!("vibeface://node/{id}?addr=bad_host:4790"),
            format!("vibeface://node/{id}?relay=ftp://relay.example"),
            format!("vibeface://node/{id}?relay=http%3A%2F%2Frelay.example"),
            format!("vibeface://node/{id}?relay=https://a&relay=https://b"),
            format!(
                "vibeface://node/{id}?relay=https://{}.example",
                "r".repeat(300)
            ),
            format!("vibeface://node/{id}?other=1"),
            format!("vibeface://node/{id}#x"),
        ] {
            assert!(NodeLink::parse(&bad).is_err(), "{bad}");
        }
    }
}
