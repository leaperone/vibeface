use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use vibeface_delivery::{Order, PeerSignature, SignedKeyBinding, Word};

pub const MAX_FILE_BYTES: usize = 130_816;
pub const MAX_CONTROL_BYTES: usize = 16_384;
pub const CHUNK_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Charity,
    Paid,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Acceptance {
    Recoverable,
    Strict,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Sender,
    Receiver,
    Provider,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    Proving,
    FundingPending,
    Funded,
    Transferring,
    CipherHeld,
    KeyPublished,
    Recoverable,
    Paid,
    Partial,
    Refundable,
    Refunded,
    Reconciling,
    KeyUnavailable,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    None,
    CharityReceipt,
    CiphertextHeld,
    Recoverable,
    DecryptionProven,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentState {
    Free,
    Prepared,
    Submitted,
    Included,
    Confirmed,
    Earned,
    Withdrawn,
    Refunded,
    Unknown,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowState {
    Pending,
    Stored,
    Held,
    KeyPublished,
    Earned,
    Refunded,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payment {
    pub state: PaymentState,
    pub chain_id: Option<String>,
    pub asset: Option<String>,
    pub payer: Option<String>,
    pub provider_wallet: Option<String>,
    pub delivery_fee: String,
    pub reservation_fee: String,
    pub priority_fee: String,
    pub total: String,
    pub gas_cap: String,
    pub earned: String,
    pub withdrawn: String,
    pub refunded: String,
    pub tx_hash: Option<String>,
}
impl Payment {
    pub fn free() -> Self {
        Self {
            state: PaymentState::Free,
            chain_id: None,
            asset: None,
            payer: None,
            provider_wallet: None,
            delivery_fee: "0".into(),
            reservation_fee: "0".into(),
            priority_fee: "0".into(),
            total: "0".into(),
            gas_cap: "0".into(),
            earned: "0".into(),
            withdrawn: "0".into(),
            refunded: "0".into(),
            tx_hash: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowView {
    pub index: u16,
    pub bytes: u32,
    pub committed_bytes: u32,
    pub state: WindowState,
    pub decryption_proven: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deadlines {
    pub quote_expires_at: u64,
    pub hold_deadline: u64,
    pub key_deadline: u64,
    pub open_deadline: u64,
    pub retain_until: u64,
    pub priority_deadline: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryOrder {
    pub order_id: String,
    pub intent_id: String,
    pub role: Role,
    pub mode: Mode,
    pub acceptance: Acceptance,
    pub source_peer: String,
    pub receiver_peer: String,
    pub provider_peer: String,
    pub name: Option<String>,
    pub bytes: u64,
    pub sha256: Option<String>,
    pub phase: Phase,
    pub evidence: Evidence,
    pub payment: Payment,
    pub windows: Vec<WindowView>,
    pub deadlines: Option<Deadlines>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredOrder {
    pub network_id: Option<String>,
    pub authorized_by: Option<String>,
    pub confirmed: bool,
    pub resume_requested: bool,
    pub request_hash: String,
    pub view: DeliveryOrder,
    pub order: Order,
    pub wallet_signature: Vec<u8>,
    pub source_seals: Vec<Vec<u8>>,
    pub grants: Vec<ServiceGrant>,
    pub accepted: bool,
    pub notified: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepare {
    pub intent_id: String,
    pub receiver_peer: String,
    pub provider_peer: String,
    pub mode: Mode,
    pub acceptance: Acceptance,
    pub name: String,
    pub content_base64: String,
    pub max_service_amount: String,
    pub max_gas_amount: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantOperation {
    PutCiphertext,
    PutCapsule,
    GetCiphertext,
    GetCapsule,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantBody {
    pub provider_peer: String,
    pub client_peer: String,
    pub order_id: String,
    pub window_index: u16,
    pub object_hash: String,
    pub operation: GrantOperation,
    pub max_bytes: u32,
    pub expires_at: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceGrant {
    pub body: GrantBody,
    pub signature: String,
}
impl ServiceGrant {
    pub fn verify(&self, provider: &str, remote: &str, now: u64) -> Result<()> {
        ensure!(
            self.body.provider_peer == provider && self.body.client_peer == remote,
            "service grant belongs to another peer"
        );
        ensure!(now <= self.body.expires_at, "service grant expired");
        parse_word(&self.body.order_id)?;
        parse_word(&self.body.object_hash)?;
        vibeface_secure::verify(
            provider,
            "delivery-grant",
            &serde_json::to_vec(&self.body)?,
            &self.signature,
        )?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceRequest {
    Charity {
        request: serde_json::Value,
    },
    Announce,
    KeyBinding {
        mode: Mode,
    },
    Quote {
        mode: Mode,
        order: vibeface_delivery::UnsignedOrder,
        binding: SignedKeyBinding,
        file_bytes: u64,
        intent_id: String,
    },
    Offer {
        mode: Mode,
        order: Order,
        file_bytes: u64,
        intent_id: String,
        wallet_signature: Vec<u8>,
    },
    Grant {
        order_id: String,
        index: u16,
        operation: GrantOperation,
    },
    Offset {
        grant: ServiceGrant,
    },
    Put {
        grant: ServiceGrant,
        offset: u32,
        bytes: String,
    },
    Get {
        grant: ServiceGrant,
        offset: u32,
    },
    Hold {
        order_id: String,
        index: u16,
        consent: PeerSignature,
        seal: Vec<u8>,
    },
    Open {
        order_id: String,
        index: u16,
        seal: Vec<u8>,
    },
    Order {
        order_id: String,
    },
}

pub fn parse_word(value: &str) -> Result<Word> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "expected a lowercase 32-byte hexadecimal identifier"
    );
    hex::decode(value)?
        .try_into()
        .ok()
        .context("invalid identifier length")
}
pub fn signature(value: String) -> Result<PeerSignature> {
    let bytes: [u8; 64] = hex::decode(value)?
        .try_into()
        .ok()
        .context("invalid signature length")?;
    Ok(PeerSignature::from_bytes(bytes))
}
pub fn file_name(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && !value
                .chars()
                .any(|c| c.is_control() || matches!(c, '/' | '\\'))
            && value != "."
            && value != "..",
        "invalid delivery file name"
    );
    Ok(())
}
pub fn envelope(name: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    file_name(name)?;
    ensure!(
        bytes.len() <= MAX_FILE_BYTES,
        "file exceeds the experimental profile limit"
    );
    let mut result = b"VDP1".to_vec();
    result.extend_from_slice(&(name.len() as u16).to_be_bytes());
    result.extend_from_slice(name.as_bytes());
    result.extend_from_slice(bytes);
    Ok(result)
}
pub fn open_envelope(bytes: &[u8]) -> Result<(&str, &[u8])> {
    ensure!(
        bytes.len() >= 6 && &bytes[..4] == b"VDP1",
        "invalid private file envelope"
    );
    let length = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
    ensure!(bytes.len() >= 6 + length, "truncated private file envelope");
    let name = std::str::from_utf8(&bytes[6..6 + length])?;
    file_name(name)?;
    let data = &bytes[6 + length..];
    ensure!(
        data.len() <= MAX_FILE_BYTES,
        "recovered file exceeds profile"
    );
    Ok((name, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_file_envelope_is_bounded_and_round_trips() {
        let bytes = envelope("report.pdf", b"private").unwrap();
        assert_eq!(
            open_envelope(&bytes).unwrap(),
            ("report.pdf", b"private".as_slice())
        );
        for name in ["", "../private", "a\\b", ".", "a\n"] {
            assert!(envelope(name, b"").is_err());
        }
        assert!(open_envelope(b"VDP1\0\x08x").is_err());
        assert!(envelope("x", &vec![0; MAX_FILE_BYTES + 1]).is_err());
    }
}
