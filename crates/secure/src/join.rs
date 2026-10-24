//! Tickets, join commitments, the 6-digit short code and attestation bytes.
//!
//! Every function here is shared by the Node and the browser (through WASM), so both
//! sides compute byte-identical values.

use crate::{Result, decode32, membership::canonical_json, validate_device_id};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

const TICKET_PREFIX: &str = "vibeface1";
pub const TICKET_LIFETIME_MS: i64 = 10 * 60 * 1000;
pub const INVITE_DOMAIN: &str = "identity-invite";
pub const ATTESTATION_DOMAIN: &str = "attestation";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntryAddr {
    pub id: [u8; 32],
    pub relay_url: Option<String>,
    pub direct_addrs: Vec<SocketAddr>,
}

/// `vibeface1` ‖ lowercase base32 of the postcard encoding, like iroh's `EndpointTicket`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ticket {
    pub version: u8,
    pub entry: EntryAddr,
    pub secret: [u8; 32],
}

impl Ticket {
    pub fn new(entry_id: &str, secret: [u8; 32]) -> Result<Self> {
        validate_device_id(entry_id)?;
        Ok(Self {
            version: 1,
            entry: EntryAddr {
                id: decode32(entry_id)?,
                relay_url: None,
                direct_addrs: vec![],
            },
            secret,
        })
    }

    pub fn encode(&self) -> Result<String> {
        let bytes = postcard::to_allocvec(self).map_err(|_| "ticket encoding failed")?;
        Ok(format!(
            "{TICKET_PREFIX}{}",
            data_encoding::BASE32_NOPAD
                .encode(&bytes)
                .to_ascii_lowercase()
        ))
    }

    pub fn decode(text: &str) -> Result<Self> {
        let body = text
            .trim()
            .strip_prefix(TICKET_PREFIX)
            .filter(|body| body.len() <= 4096)
            .ok_or("not a Vibeface ticket")?;
        let bytes = data_encoding::BASE32_NOPAD
            .decode(body.to_ascii_uppercase().as_bytes())
            .map_err(|_| "ticket encoding is invalid")?;
        let (ticket, rest): (Self, _) =
            postcard::take_from_bytes(&bytes).map_err(|_| "ticket content is invalid")?;
        if !rest.is_empty() || ticket.version != 1 {
            return Err("unsupported or trailing ticket content".into());
        }
        validate_device_id(&ticket.entry_id())?;
        Ok(ticket)
    }

    pub fn entry_id(&self) -> String {
        hex::encode(self.entry.id)
    }

    pub fn invite_hash(&self) -> String {
        invite_hash(&self.secret)
    }
}

/// The entry keeps only `SHA256(secret)`; `invite_id` is its first 16 bytes.
pub fn invite_hash(secret: &[u8; 32]) -> String {
    hex::encode(Sha256::digest(secret))
}

pub fn invite_id(invite_hash: &str) -> Result<String> {
    decode32(invite_hash)?;
    Ok(invite_hash[..32].to_owned())
}

/// A management device registers this before handing out the ticket.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InviteRecord {
    pub network_id: String,
    pub invite_hash: String,
    pub issuer: String,
    pub expires_at_ms: i64,
    pub signature: String,
}

impl InviteRecord {
    pub fn signing_payload(&self) -> Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        Ok(canonical_json(&unsigned)?)
    }

    pub fn verify(&self) -> Result<()> {
        validate_device_id(&self.network_id)?;
        validate_device_id(&self.issuer)?;
        decode32(&self.invite_hash)?;
        crate::verify(
            &self.issuer,
            INVITE_DOMAIN,
            &self.signing_payload()?,
            &self.signature,
        )
    }
}

/// `SHA256(J public key ‖ nJ)`, committed before J can see the admin nonce.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = joinCommit))]
pub fn join_commit(joiner: &str, joiner_nonce: &str) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(decode32(joiner)?);
    hash.update(decode32(joiner_nonce)?);
    Ok(hex::encode(hash.finalize()))
}

/// Both screens show this; a mismatch means someone sits between them.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = shortCode))]
pub fn short_code(
    network_id: &str,
    invite_id: &str,
    joiner: &str,
    admin: &str,
    admin_nonce: &str,
    joiner_nonce: &str,
) -> Result<String> {
    let mut invite = [0u8; 16];
    hex::decode_to_slice(invite_id, &mut invite).map_err(|_| "invalid invite ID")?;
    let mut hash = Sha256::new();
    hash.update(b"vibeface:sas:v1");
    hash.update(decode32(network_id)?);
    hash.update(invite);
    for part in [joiner, admin, admin_nonce, joiner_nonce] {
        hash.update(decode32(part)?);
    }
    let digest = hash.finalize();
    let value = u32::from_be_bytes(digest[..4].try_into().expect("four bytes")) % 1_000_000;
    Ok(format!("{value:06}"))
}

/// Bytes a client signs to authorize one event body inside one conversation of one network.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = attestationPayload))]
pub fn attestation_payload(network_id: &str, scope_id: &str, body_json: &str) -> Result<Vec<u8>> {
    validate_device_id(network_id)?;
    let body: Value = serde_json::from_str(body_json).map_err(|_| "invalid event body JSON")?;
    let object = body.as_object().ok_or("event body must be an object")?;
    if object.contains_key("attestation") || !object.get("type").is_some_and(Value::is_string) {
        return Err("attested body must have a type and no attestation".into());
    }
    canonical_json(&serde_json::json!({
        "network_id": network_id,
        "scope_id": scope_id,
        "body": body,
    }))
    .map_err(Into::into)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = ticketEncode))]
pub fn ticket_encode(
    entry_id: &str,
    secret_hex: &str,
    relay_url: Option<String>,
    direct_addrs: Vec<String>,
) -> Result<String> {
    let mut ticket = Ticket::new(entry_id, decode32(secret_hex)?)?;
    ticket.entry.relay_url = relay_url.filter(|url| url.len() <= 512);
    ticket.entry.direct_addrs = direct_addrs
        .iter()
        .filter_map(|addr| addr.parse().ok())
        .take(8)
        .collect();
    ticket.encode()
}

/// `{entry_id, invite_hash, invite_id}`; the secret stays out of JSON the page logs.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = ticketDecode))]
pub fn ticket_decode(text: &str) -> Result<String> {
    let ticket = Ticket::decode(text)?;
    let hash = ticket.invite_hash();
    Ok(serde_json::json!({
        "entry_id": ticket.entry_id(),
        "invite_id": invite_id(&hash)?,
        "invite_hash": hash,
        "secret": hex::encode(ticket.secret),
    })
    .to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = inviteSigningPayload))]
pub fn invite_signing_payload(record_json: &str) -> Result<Vec<u8>> {
    let record: InviteRecord =
        serde_json::from_str(record_json).map_err(|_| "invalid invite record")?;
    record.signing_payload()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;

    #[test]
    fn tickets_round_trip_and_reject_other_text() {
        let entry = Identity::generate().unwrap().device_id();
        let ticket = Ticket::new(&entry, [7; 32]).unwrap();
        let text = ticket.encode().unwrap();
        assert!(text.starts_with("vibeface1") && text == text.to_ascii_lowercase());
        assert_eq!(Ticket::decode(&text).unwrap(), ticket);
        assert_eq!(Ticket::decode(&text).unwrap().entry_id(), entry);
        assert!(Ticket::decode(&text.replace("vibeface1", "vibeface2")).is_err());
        assert!(Ticket::decode(&format!("{text}a")).is_err());
        assert_eq!(invite_id(&ticket.invite_hash()).unwrap().len(), 32);
    }

    #[test]
    fn short_code_binds_every_input() {
        let ids: Vec<String> = (0..6)
            .map(|_| Identity::generate().unwrap().device_id())
            .collect();
        let invite = &ids[1][..32];
        let code = short_code(&ids[0], invite, &ids[2], &ids[3], &ids[4], &ids[5]).unwrap();
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|b| b.is_ascii_digit()));
        assert_ne!(
            code,
            short_code(&ids[0], invite, &ids[2], &ids[3], &ids[5], &ids[4]).unwrap()
        );
        assert_ne!(
            code,
            short_code(&ids[0], invite, &ids[3], &ids[2], &ids[4], &ids[5]).unwrap()
        );
    }

    #[test]
    fn commitments_and_invites_are_checked() {
        let joiner = Identity::generate().unwrap();
        let nonce = hex::encode([9u8; 32]);
        let commit = join_commit(&joiner.device_id(), &nonce).unwrap();
        assert_ne!(
            commit,
            join_commit(&joiner.device_id(), &hex::encode([8u8; 32])).unwrap()
        );
        let root = Identity::generate().unwrap();
        let admin = Identity::generate().unwrap();
        let mut record = InviteRecord {
            network_id: root.device_id(),
            invite_hash: invite_hash(&[1; 32]),
            issuer: admin.device_id(),
            expires_at_ms: 10,
            signature: String::new(),
        };
        record.signature = admin
            .sign(INVITE_DOMAIN, &record.signing_payload().unwrap())
            .unwrap();
        record.verify().unwrap();
        record.expires_at_ms = 11;
        assert!(record.verify().is_err());
    }

    #[test]
    fn attestation_payload_binds_network_and_conversation() {
        let network = Identity::generate().unwrap().device_id();
        let body = r#"{"type":"example","version":1,"mode":"off"}"#;
        let a = attestation_payload(&network, "c1", body).unwrap();
        assert_ne!(a, attestation_payload(&network, "c2", body).unwrap());
        assert_eq!(
            a,
            attestation_payload(
                &network,
                "c1",
                r#"{"mode":"off","type":"example","version":1}"#
            )
            .unwrap()
        );
        assert!(attestation_payload(&network, "c1", r#"{"type":"x","attestation":{}}"#).is_err());
    }
}
