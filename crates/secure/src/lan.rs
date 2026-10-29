//! The tag a Node advertises over mDNS. Members know the network ID and can tell which Node a
//! tag names; anyone else on the LAN sees a value that changes every hour. A tag only hints
//! where to dial: QUIC TLS still proves the device.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// mDNS service type every Node advertises.
pub const SERVICE: &str = "_vibeface._udp";
pub const TXT_KEY: &str = "t";
const HOUR_MS: u64 = 3_600_000;

/// 32 hex characters naming `device_id` in `network_id` during `hour` (Unix ms / 3600000).
pub fn lan_tag(network_id: &str, device_id: &str, hour: u64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(network_id.as_bytes()).expect("any key length");
    mac.update(b"vibeface:lan:v1\0");
    mac.update(device_id.as_bytes());
    mac.update(&hour.to_be_bytes());
    hex::encode(&mac.finalize().into_bytes()[..16])
}

pub fn hour(now_ms: u64) -> u64 {
    now_ms / HOUR_MS
}

/// Whether `tag` names `device_id` within an hour of `now_ms`, so clocks a little apart agree.
pub fn lan_tag_matches(tag: &str, network_id: &str, device_id: &str, now_ms: u64) -> bool {
    let hour = hour(now_ms);
    [hour.saturating_sub(1), hour, hour + 1]
        .into_iter()
        .any(|h| lan_tag(network_id, device_id, h) == tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_is_bound_to_network_device_and_hour() {
        let tag = lan_tag("net-a", "dev-1", 10);
        assert_eq!(tag.len(), 32);
        assert_ne!(tag, lan_tag("net-b", "dev-1", 10));
        assert_ne!(tag, lan_tag("net-a", "dev-2", 10));
        assert_ne!(tag, lan_tag("net-a", "dev-1", 11));
        let now = 10 * HOUR_MS + 5;
        assert!(lan_tag_matches(&tag, "net-a", "dev-1", now));
        assert!(lan_tag_matches(&tag, "net-a", "dev-1", now + HOUR_MS));
        assert!(!lan_tag_matches(&tag, "net-a", "dev-1", now + 2 * HOUR_MS));
        assert!(!lan_tag_matches(&tag, "net-b", "dev-1", now));
    }
}
