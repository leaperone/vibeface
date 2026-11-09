//! Tickets, join commitments, the 6-digit short code and attestation bytes.
//!
//! Every function here is shared by the Node and the browser (through WASM), so both
//! sides compute byte-identical values.

use crate::{
    Result, decode32,
    link::{JOIN_LINK, NodeLink},
    membership::canonical_json,
    validate_device_id,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

const TICKET_PREFIX: &str = "vibeface1";
/// Base32 characters after the prefix: a node link with every hint at its bound fits.
const MAX_TICKET: usize = 2400;
pub const TICKET_LIFETIME_MS: i64 = 10 * 60 * 1000;
pub const INVITE_DOMAIN: &str = "identity-invite";
pub const ATTESTATION_DOMAIN: &str = "attestation";

/// `vibeface1` ‖ lowercase base32 of the postcard encoding, like iroh's `EndpointTicket`. It
/// carries the entry Node's node link, so a joiner reaches the entry through its addresses and
/// relay even off the mesh, and the secret the entry knows only by hash.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ticket {
    pub version: u8,
    pub entry: NodeLink,
    pub secret: [u8; 32],
}

impl Ticket {
    pub fn new(entry: NodeLink, secret: [u8; 32]) -> Result<Self> {
        entry.check()?;
        Ok(Self {
            version: 1,
            entry,
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
            .filter(|body| body.len() <= MAX_TICKET)
            .ok_or("not a Vibeface ticket")?;
        let bytes = data_encoding::BASE32_NOPAD
            .decode(body.to_ascii_uppercase().as_bytes())
            .map_err(|_| "ticket encoding is invalid")?;
        let (ticket, rest): (Self, _) =
            postcard::take_from_bytes(&bytes).map_err(|_| "ticket content is invalid")?;
        if !rest.is_empty() || ticket.version != 1 {
            return Err("unsupported or trailing ticket content".into());
        }
        ticket.entry.check()?;
        Ok(ticket)
    }

    /// The ticket in an invite: `vibeface://join/vibeface1…` or `vibeface1…` alone.
    pub fn from_link(text: &str) -> Result<Self> {
        let text = text.trim();
        let ticket = text.strip_prefix(JOIN_LINK).unwrap_or(text);
        if !ticket.starts_with(TICKET_PREFIX) {
            return Err("an invite link is vibeface://join/vibeface1…".into());
        }
        Self::decode(ticket)
    }

    /// `vibeface://join/vibeface1…`
    pub fn link(&self) -> Result<String> {
        Ok(format!("{JOIN_LINK}{}", self.encode()?))
    }

    pub fn entry_id(&self) -> String {
        self.entry.device_id()
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

/// A ticket for the entry Node `entry`, its node link.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = ticketEncode))]
pub fn ticket_encode(entry: &str, secret_hex: &str) -> Result<String> {
    Ticket::new(NodeLink::parse(entry)?, decode32(secret_hex)?)?.encode()
}

/// `{entry_id, entry, invite_id, invite_hash, secret}`, `entry` being the entry's node link.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = ticketDecode))]
pub fn ticket_decode(text: &str) -> Result<String> {
    let ticket = Ticket::decode(text)?;
    let hash = ticket.invite_hash();
    Ok(serde_json::json!({
        "entry_id": ticket.entry_id(),
        "entry": ticket.entry.to_string(),
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

    fn full_link() -> NodeLink {
        let entry = Identity::generate().unwrap().device_id();
        let addr = |text: &str| crate::link::Addr::parse(text).unwrap();
        NodeLink::new(
            &entry,
            vec![
                addr("[2001:db8:1234:5678:9abc:def0:1234:5678]:65535"),
                addr("203.0.113.5:4790"),
                addr(&format!("{}.example:65535", "n".repeat(240))),
                addr("192.168.1.20:4790"),
            ],
            Some(format!("https://{}.example:8443", "r".repeat(230))),
        )
        .unwrap()
    }

    #[test]
    fn tickets_round_trip_and_reject_other_text() {
        let entry = Identity::generate().unwrap().device_id();
        let link = NodeLink::parse(&format!(
            "vibeface://node/{entry}?addr=203.0.113.5:4790&relay=https://relay.example"
        ))
        .unwrap();
        let ticket = Ticket::new(link, [7; 32]).unwrap();
        let text = ticket.encode().unwrap();
        assert!(text.starts_with("vibeface1") && text == text.to_ascii_lowercase());
        assert_eq!(Ticket::decode(&text).unwrap(), ticket);
        assert_eq!(Ticket::decode(&text).unwrap().entry_id(), entry);
        assert!(Ticket::decode(&text.replace("vibeface1", "vibeface2")).is_err());
        assert!(Ticket::decode(&format!("{text}a")).is_err());
        assert_eq!(invite_id(&ticket.invite_hash()).unwrap().len(), 32);
        for link in [format!("vibeface://join/{text}"), format!(" {text} ")] {
            assert_eq!(Ticket::from_link(&link).unwrap(), ticket, "{link}");
        }
        assert_eq!(ticket.link().unwrap(), format!("vibeface://join/{text}"));
        for link in [
            format!("http://192.168.1.10:4790/join#{text}"),
            format!("vibeface://join?link={text}"),
            format!("vibeface://join/{text}?x=1"),
            format!("vibeface://other/{text}"),
            format!("vibeface://node/{entry}"),
        ] {
            assert!(Ticket::from_link(&link).is_err(), "{link}");
        }
    }

    #[test]
    fn tickets_stay_bounded_and_refuse_oversized_hints() {
        let full = Ticket::new(full_link(), [7; 32]).unwrap();
        let text = full.encode().unwrap();
        assert!(
            text.len() - "vibeface1".len() <= MAX_TICKET,
            "{}",
            text.len()
        );
        assert_eq!(Ticket::decode(&text).unwrap(), full);

        let mut five = full_link();
        five.addrs.push(five.addrs[1].clone());
        assert!(Ticket::new(five.clone(), [7; 32]).is_err());
        let mut forged = full.clone();
        forged.entry = five;
        let mut long = full.clone();
        long.entry.relay = Some(format!("https://{}.example", "r".repeat(300)));
        let mut odd = full;
        odd.entry.relay = Some("ftp://relay.example".into());
        for ticket in [forged, long, odd] {
            let bytes = postcard::to_allocvec(&ticket).unwrap();
            let text = format!(
                "vibeface1{}",
                data_encoding::BASE32_NOPAD
                    .encode(&bytes)
                    .to_ascii_lowercase()
            );
            assert!(Ticket::decode(&text).is_err(), "{ticket:?}");
        }
        assert!(Ticket::decode(&format!("vibeface1{}", "a".repeat(MAX_TICKET + 1))).is_err());
    }

    #[test]
    fn decoded_tickets_carry_the_entry_link() {
        let entry = Identity::generate().unwrap().device_id();
        let link = format!("vibeface://node/{entry}?addr=203.0.113.5:4790");
        let text = ticket_encode(&link, &hex::encode([7; 32])).unwrap();
        let bytes = data_encoding::BASE32_NOPAD
            .decode(text["vibeface1".len()..].to_ascii_uppercase().as_bytes())
            .unwrap();
        assert_eq!(
            bytes.len(),
            1 + 32 + 1 + (1 + 1 + 4 + 2) + 1 + 32,
            "version, entry ID, one IPv4 address, no relay and the secret"
        );
        let decoded: Value = serde_json::from_str(&ticket_decode(&text).unwrap()).unwrap();
        let hash = invite_hash(&[7; 32]);
        assert_eq!(
            decoded,
            serde_json::json!({
                "entry_id": entry,
                "entry": link,
                "invite_id": &hash[..32],
                "invite_hash": hash,
                "secret": hex::encode([7; 32]),
            })
        );
        assert!(ticket_encode(&entry, &hex::encode([7; 32])).is_err());
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
