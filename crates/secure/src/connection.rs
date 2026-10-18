use crate::authorization::{AUTH_VERSION, AccountId, ObjectHash, PublicKey};
use serde::{Deserialize, Serialize};
use url::Url;

pub const MAX_CONNECTION_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum EnrollmentReferenceKind {
    #[serde(rename = "vibeface-device-enrollment")]
    Device,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentReference {
    pub version: u16,
    pub kind: EnrollmentReferenceKind,
    pub endpoint: String,
    pub node_public_key: PublicKey,
    pub relay: bool,
    pub account_id: AccountId,
    pub offer_hash: ObjectHash,
    pub expires_at_ms: u64,
}

impl EnrollmentReference {
    pub fn from_offer(
        offer: &crate::authorization::Signed<
            crate::authorization::enrollment::offer::EnrollmentOffer,
        >,
    ) -> crate::Result<Self> {
        Ok(Self {
            version: AUTH_VERSION,
            kind: EnrollmentReferenceKind::Device,
            endpoint: offer.payload.entry.endpoint.clone(),
            node_public_key: offer.payload.entry.node_public_key.clone(),
            relay: offer.payload.entry.relay,
            account_id: offer.payload.account.clone(),
            offer_hash: crate::authorization::enrollment::offer::enrollment_offer_hash(offer)
                .map_err(|error| error.to_string())?,
            expires_at_ms: offer.payload.expires_at_ms,
        })
    }

    pub fn route(&self) -> crate::Result<ConnectionRoute> {
        ConnectionRoute::new(&self.endpoint, self.node_public_key.as_str(), self.relay)
    }

    pub fn from_json(input: &str, now_ms: u64) -> crate::Result<Self> {
        if input.len() > MAX_CONNECTION_BYTES {
            return Err("device invitation exceeds 64 KiB".into());
        }
        let reference: Self =
            serde_json::from_str(input).map_err(|_| "invalid device invitation")?;
        if reference.version != AUTH_VERSION || reference.expires_at_ms <= now_ms {
            return Err("device invitation has expired or uses an unsupported version".into());
        }
        reference.route()?.validate()?;
        Ok(reference)
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = parseDeviceEnrollmentReference))]
pub fn parse_device_enrollment_reference(input: &str, now_ms: f64) -> crate::Result<String> {
    if !now_ms.is_finite()
        || now_ms < 0.0
        || now_ms.fract() != 0.0
        || now_ms > 9_007_199_254_740_991.0
    {
        return Err("invalid invitation timestamp".into());
    }
    serde_json::to_string(&EnrollmentReference::from_json(input, now_ms as u64)?)
        .map_err(|error| error.to_string())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConnectionRoute {
    pub endpoint: String,
    pub node_public_key: PublicKey,
    pub relay: bool,
}

impl ConnectionRoute {
    pub fn new(endpoint: &str, node_public_key: &str, relay: bool) -> crate::Result<Self> {
        let value = Self {
            endpoint: endpoint_origin(endpoint, relay)?,
            node_public_key: PublicKey::parse(node_public_key)
                .map_err(|error| error.to_string())?,
            relay,
        };
        Ok(value)
    }

    pub fn validate(&self) -> crate::Result<()> {
        if endpoint_origin(&self.endpoint, self.relay)? != self.endpoint {
            return Err("connection endpoint must be a canonical HTTP or HTTPS origin".into());
        }
        Ok(())
    }

    pub fn node_id(&self) -> crate::Result<String> {
        crate::authorization::node_fingerprint(self.node_public_key.as_str())
    }
}

fn endpoint_origin(value: &str, relay: bool) -> crate::Result<String> {
    if value.len() > 4096 {
        return Err("connection endpoint is too long".into());
    }
    let url = Url::parse(value).map_err(|_| "invalid connection endpoint")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || value.chars().any(char::is_whitespace)
    {
        return Err(
            "connection endpoint must be an HTTP or HTTPS origin without credentials".into(),
        );
    }
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if relay && url.scheme() != "https" && !local {
        return Err("a public relay requires HTTPS".into());
    }
    Ok(url.origin().ascii_serialization())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConnectionCard {
    pub url: String,
    pub node_id: String,
    pub node_public_key: PublicKey,
    pub relay: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClaimCard {
    pub url: String,
    pub node_id: String,
    pub node_public_key: PublicKey,
    pub relay: bool,
    pub capability: String,
    pub claim_nonce: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum NodeConnectionCard {
    Claim(ClaimCard),
    Connection(ConnectionCard),
}

impl NodeConnectionCard {
    pub fn new(
        endpoint: &str,
        node_public_key: &str,
        relay: bool,
        claim: Option<&str>,
    ) -> crate::Result<Self> {
        let route = ConnectionRoute::new(endpoint, node_public_key, relay)?;
        let node_id = route.node_id()?;
        let card = match claim {
            Some(value) => Self::Claim(ClaimCard {
                url: route.endpoint,
                node_id,
                node_public_key: route.node_public_key,
                relay,
                capability: value.to_owned(),
                claim_nonce: value.to_owned(),
            }),
            None => Self::Connection(ConnectionCard {
                url: route.endpoint,
                node_id,
                node_public_key: route.node_public_key,
                relay,
            }),
        };
        card.validate()?;
        Ok(card)
    }

    pub fn route(&self) -> crate::Result<ConnectionRoute> {
        let (endpoint, key, relay) = match self {
            Self::Claim(card) => (&card.url, &card.node_public_key, card.relay),
            Self::Connection(card) => (&card.url, &card.node_public_key, card.relay),
        };
        ConnectionRoute::new(endpoint, key.as_str(), relay)
    }

    pub fn validate(&self) -> crate::Result<()> {
        let route = self.route()?;
        let node_id = match self {
            Self::Claim(card) => {
                if card.capability != card.claim_nonce
                    || card.capability.len() != 64
                    || !card
                        .capability
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
                {
                    return Err("invalid or inconsistent node claim capability".into());
                }
                &card.node_id
            }
            Self::Connection(card) => &card.node_id,
        };
        if *node_id != route.node_id()? {
            return Err("node fingerprint differs from its public key".into());
        }
        Ok(())
    }

    pub fn from_json(input: &str) -> crate::Result<Self> {
        if input.len() > MAX_CONNECTION_BYTES {
            return Err("connection file exceeds 64 KiB".into());
        }
        let card: Self = serde_json::from_str(input).map_err(|_| "invalid node connection card")?;
        card.validate()?;
        Ok(card)
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = parseNodeConnectionCard))]
pub fn parse_node_connection_card(input: &str) -> crate::Result<String> {
    serde_json::to_string(&NodeConnectionCard::from_json(input)?).map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = createNodeConnectionCard))]
pub fn create_node_connection_card(
    endpoint: &str,
    node_public_key: &str,
    relay: bool,
    claim: Option<String>,
) -> crate::Result<String> {
    let card = NodeConnectionCard::new(endpoint, node_public_key, relay, claim.as_deref())?;
    serde_json::to_string(&card).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;

    #[test]
    fn connection_and_claim_cards_roundtrip_with_direct_and_relay_routes() {
        let key = Identity::generate().unwrap();
        for relay in [false, true] {
            for claim in [None, Some("ab".repeat(32))] {
                let card = NodeConnectionCard::new(
                    "https://relay.example/",
                    &key.public_key_hex(),
                    relay,
                    claim.as_deref(),
                )
                .unwrap();
                let encoded = serde_json::to_string(&card).unwrap();
                let decoded = NodeConnectionCard::from_json(&encoded).unwrap();
                assert!(card == decoded);
                assert_eq!(decoded.route().unwrap().node_id().unwrap(), key.node_id());
                let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
                assert_eq!(value["relay"], relay);
                assert_eq!(value["url"], "https://relay.example");
                assert_eq!(value.get("capability").is_some(), claim.is_some());
                assert_eq!(value.get("claim_nonce").is_some(), claim.is_some());
            }
        }
    }

    #[test]
    fn connection_cards_reject_identity_changes_null_claims_and_unknown_fields() {
        let key = Identity::generate().unwrap();
        let card =
            NodeConnectionCard::new("https://node.example", &key.public_key_hex(), false, None)
                .unwrap();
        let original = serde_json::to_value(card).unwrap();
        for (field, value) in [
            ("node_id", serde_json::json!("00".repeat(32))),
            ("capability", serde_json::Value::Null),
            ("claim_nonce", serde_json::json!("ab".repeat(32))),
            ("secret", serde_json::json!("unexpected")),
        ] {
            let mut changed = original.clone();
            changed[field] = value;
            assert!(NodeConnectionCard::from_json(&changed.to_string()).is_err());
        }
    }

    #[test]
    fn connection_routes_reject_embedded_credentials_and_nonlocal_plaintext_relays() {
        let key = Identity::generate().unwrap();
        for endpoint in [
            "https://user:secret@relay.example",
            "https://relay.example/path",
            "https://relay.example?token=secret",
            "https://relay.example#secret",
            "ftp://relay.example",
            "https:// relay.example",
        ] {
            assert!(ConnectionRoute::new(endpoint, &key.public_key_hex(), false).is_err());
        }
        assert!(ConnectionRoute::new("http://relay.example", &key.public_key_hex(), true).is_err());
        assert!(ConnectionRoute::new("http://localhost:4790", &key.public_key_hex(), true).is_ok());
        assert!(
            ConnectionRoute::new("http://192.168.1.2:4790", &key.public_key_hex(), false).is_ok()
        );
    }

    #[test]
    fn enrollment_references_preserve_routes_and_reject_expiry_and_unknown_fields() {
        let key = Identity::generate().unwrap();
        let reference = EnrollmentReference {
            version: AUTH_VERSION,
            kind: EnrollmentReferenceKind::Device,
            endpoint: "https://relay.example".into(),
            node_public_key: PublicKey::of(&key),
            relay: true,
            account_id: AccountId::from_root(&PublicKey::of(&key)),
            offer_hash: ObjectHash::body(&"offer").unwrap(),
            expires_at_ms: 200,
        };
        let encoded = serde_json::to_string(&reference).unwrap();
        assert_eq!(
            EnrollmentReference::from_json(&encoded, 100).unwrap(),
            reference
        );
        assert!(EnrollmentReference::from_json(&encoded, 200).is_err());
        let mut changed = serde_json::to_value(&reference).unwrap();
        changed["relay"] = serde_json::json!("true");
        assert!(EnrollmentReference::from_json(&changed.to_string(), 100).is_err());
        let mut changed = serde_json::to_value(&reference).unwrap();
        changed["secret"] = serde_json::json!("not an invitation field");
        assert!(EnrollmentReference::from_json(&changed.to_string(), 100).is_err());
    }
}
