//! Finds member Nodes on the LAN over mDNS, so a new DHCP address does not cost the direct path.
//! Adverts carry only the rotating tag from `vibeface_secure::lan`, and what they say is a
//! dialing hint: QUIC TLS still proves the device.
use mdns_sd::{IfKind, ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use vibeface_secure::lan::{SERVICE, TXT_KEY, hour, lan_tag, lan_tag_matches};

/// Anyone on the LAN can advertise; these bound what each dial checks and tries.
const MAX_ADVERTS: usize = 64;
const MAX_ADDRS: usize = 8;

pub(super) type Wake = Box<dyn Fn(&str) + Send + Sync>;

pub(super) struct Lan {
    daemon: ServiceDaemon,
    service: String,
    device: String,
    port: u16,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    own: Option<Own>,
    /// The other member Nodes of `own.network`.
    members: Vec<String>,
    /// Adverts by full service name. They are matched to members when used, so one seen
    /// before this Node learned of its sender still counts.
    seen: HashMap<String, Advert>,
}

/// The network this Node is in and the advert naming it there.
struct Own {
    network: String,
    hour: u64,
    fullname: String,
}

struct Advert {
    tag: String,
    addrs: Vec<SocketAddr>,
}

impl Lan {
    /// `wake` hears which member Node an advert names, as soon as it appears or changes.
    pub(super) fn start(device: String, port: u16, wake: Wake) -> mdns_sd::Result<Self> {
        let lan = Self {
            daemon: ServiceDaemon::new()?,
            service: format!("{SERVICE}.local."),
            device,
            port,
            state: Arc::default(),
        };
        lan.daemon
            .disable_interface(vec![IfKind::LoopbackV4, IfKind::LoopbackV6])?;
        let events = lan.daemon.browse(&lan.service)?;
        tokio::spawn(watch(events, lan.state.clone(), wake));
        Ok(lan)
    }

    /// Advertises this Node in `network` under the current hour's tag; `None` withdraws it.
    pub(super) fn refresh(&self, network: Option<&str>, members: &[String]) {
        let hour = hour(now_ms());
        let mut state = self.state.lock().expect("LAN state");
        state.members = members.to_vec();
        let current = state
            .own
            .as_ref()
            .map(|own| (own.network.as_str(), own.hour));
        if current == network.map(|network| (network, hour)) {
            return;
        }
        if let Some(own) = state.own.take() {
            let _ = self.daemon.unregister(&own.fullname);
        }
        let Some(network) = network else { return };
        let tag = lan_tag(network, &self.device, hour);
        // The host name is the tag too, so the advert does not carry the machine's name.
        let advert = ServiceInfo::new(
            &self.service,
            &tag,
            &format!("{tag}.local."),
            (),
            self.port,
            &[(TXT_KEY, tag.as_str())][..],
        )
        .and_then(|info| self.daemon.register(info.enable_addr_auto()));
        if let Err(error) = advert {
            tracing::debug!(%error, "this Node is not advertised on the LAN this hour");
        }
        state.own = Some(Own {
            network: network.to_owned(),
            hour,
            fullname: format!("{tag}.{}", self.service),
        });
    }

    pub(super) fn hints(&self, device: &str) -> Vec<SocketAddr> {
        self.state
            .lock()
            .expect("LAN state")
            .hints(device, now_ms())
    }
}

impl Drop for Lan {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

async fn watch(events: mdns_sd::Receiver<ServiceEvent>, state: Arc<Mutex<State>>, wake: Wake) {
    while let Ok(event) = events.recv_async().await {
        match event {
            ServiceEvent::ServiceResolved(service) => {
                let Some(advert) = Advert::parse(&service) else {
                    continue;
                };
                let now = now_ms();
                let member = state
                    .lock()
                    .expect("LAN state")
                    .learn(service.fullname, advert, now);
                if let Some(member) = member {
                    tracing::debug!(%member, "member Node found on the LAN");
                    wake(&member);
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                state.lock().expect("LAN state").seen.remove(&fullname);
            }
            _ => {}
        }
    }
}

impl Advert {
    fn parse(service: &ResolvedService) -> Option<Self> {
        let addrs = service
            .addresses
            .iter()
            .map(ScopedIp::to_ip_addr)
            // Dialing a link-local IPv6 address needs its interface, which a hint does not keep.
            .filter(|ip| !matches!(ip, IpAddr::V6(ip) if ip.is_unicast_link_local()))
            .take(MAX_ADDRS)
            .map(|ip| SocketAddr::new(ip, service.port))
            .collect();
        Some(Self {
            tag: service.get_property_val_str(TXT_KEY)?.to_owned(),
            addrs,
        })
    }
}

impl State {
    /// Keeps `advert`; returns the member Node it names, if any.
    fn learn(&mut self, fullname: String, advert: Advert, now_ms: u64) -> Option<String> {
        if self.seen.len() >= MAX_ADVERTS && !self.seen.contains_key(&fullname) {
            return None;
        }
        let member = self.own.as_ref().and_then(|own| {
            self.members
                .iter()
                .find(|member| lan_tag_matches(&advert.tag, &own.network, member, now_ms))
                .cloned()
        });
        self.seen.insert(fullname, advert);
        member
    }

    /// A tag stops matching an hour after its own, which retires adverts nobody withdrew.
    fn hints(&self, device: &str, now_ms: u64) -> Vec<SocketAddr> {
        let Some(own) = &self.own else {
            return Vec::new();
        };
        self.seen
            .values()
            .filter(|advert| lan_tag_matches(&advert.tag, &own.network, device, now_ms))
            .flat_map(|advert| advert.addrs.iter().copied())
            .collect()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_MS: u64 = 3_600_000;

    fn advert(network: &str, device: &str, hour: u64, port: u16) -> Advert {
        Advert {
            tag: lan_tag(network, device, hour),
            addrs: vec![SocketAddr::from(([192, 168, 1, 20], port))],
        }
    }

    #[test]
    fn adverts_hint_member_addresses_until_withdrawn_or_stale() {
        let now = 100 * HOUR_MS;
        let mut state = State::default();
        assert!(
            state
                .learn("b0".into(), advert("net", "b", 100, 1), now)
                .is_none()
        );
        state.own = Some(Own {
            network: "net".into(),
            hour: 100,
            fullname: String::new(),
        });
        state.members = vec!["b".into(), "c".into()];
        assert_eq!(
            state.hints("b", now),
            [SocketAddr::from(([192, 168, 1, 20], 1))],
            "an advert seen before the network was known still counts"
        );
        assert!(
            state
                .learn("a".into(), advert("net", "a", 100, 2), now)
                .is_none()
        );
        assert!(
            state
                .learn("x".into(), advert("net-b", "c", 100, 3), now)
                .is_none()
        );
        assert_eq!(
            state.learn("c".into(), advert("net", "c", 99, 4), now),
            Some("c".into())
        );
        assert_eq!(
            state.hints("c", now),
            [SocketAddr::from(([192, 168, 1, 20], 4))]
        );
        assert!(state.hints("c", now + 2 * HOUR_MS).is_empty());
        state.seen.remove("c");
        assert!(state.hints("c", now).is_empty());
    }
}
