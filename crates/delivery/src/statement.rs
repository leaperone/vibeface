use alloc::{format, string::String, vec::Vec};
use alloy_primitives::{Address as AbiAddress, B256, U256};
use alloy_sol_types::SolValue;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::{
    Acceptance, Amount, BindingAction, Error, KeyBinding, Relation, Result, STATEMENT_BYTES,
    SignedKeyBinding, Terms, UnsignedOrder, WindowSpec, Word,
};

mod abi {
    alloy_sol_types::sol!(
        #[sol(all_derives)]
        "abi/Statement.sol"
    );
}
use abi::DeliveryStatement;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofProfile {
    pub guest_image_id: Word,
    pub statement_schema_hash: Word,
    pub crypto_suite: u8,
    pub max_windows: u16,
    pub max_window_bytes: u32,
    pub max_total_bytes: u64,
    pub max_guest_cycles: u64,
    pub max_prover_memory_bytes: u64,
}
impl ProofProfile {
    pub fn validate(&self) -> Result<()> {
        if self.guest_image_id == [0; 32]
            || self.statement_schema_hash != schema_hash()
            || self.crypto_suite != 1
            || self.max_windows == 0
            || self.max_windows as usize > crate::MAX_WINDOWS
            || self.max_window_bytes == 0
            || self.max_window_bytes as usize > crate::MAX_WINDOW_BYTES
            || self.max_total_bytes == 0
            || self.max_total_bytes > u64::from(self.max_windows) * u64::from(self.max_window_bytes)
            || self.max_guest_cycles == 0
            || self.max_guest_cycles > 64_000_000
            || self.max_prover_memory_bytes == 0
            || self.max_prover_memory_bytes > 16 * 1024 * 1024 * 1024
        {
            return Err(Error("invalid proof profile or schema"));
        }
        Ok(())
    }
    pub fn abi_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(abi::DeliveryProfile {
            domain: tag("vibeface-delivery-profile-v1").into(),
            guestImageId: self.guest_image_id.into(),
            statementSchemaHash: self.statement_schema_hash.into(),
            cryptoSuite: self.crypto_suite,
            maxWindows: self.max_windows,
            maxWindowBytes: self.max_window_bytes,
            maxTotalBytes: self.max_total_bytes,
            maxGuestCycles: self.max_guest_cycles,
            maxProverMemoryBytes: self.max_prover_memory_bytes,
        }
        .abi_encode())
    }
    pub fn digest(&self) -> Result<Word> {
        Ok(hash(&self.abi_bytes()?))
    }
    pub fn check_order(&self, order: &UnsignedOrder) -> Result<()> {
        if order.terms.profile_id != self.digest()?
            || order.terms.guest_image_id != self.guest_image_id
            || order.terms.window_count > self.max_windows
            || order.terms.total_plaintext_bytes > self.max_total_bytes
            || order
                .windows
                .iter()
                .any(|window| window.plaintext_bytes > self.max_window_bytes)
        {
            return Err(Error("order exceeds or mismatches proof profile"));
        }
        Ok(())
    }
}

pub fn schema_hash() -> Word {
    hash(include_bytes!("../abi/Statement.sol"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement(Vec<u8>);

impl Statement {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let value = Self(bytes.to_vec());
        value.raw()?;
        Ok(value)
    }
    pub fn abi_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn digest(&self) -> Word {
        hash(&self.0)
    }
    pub fn relation(&self) -> Result<Relation> {
        Relation::try_from(self.raw()?.relation)
    }
    pub fn consent_signing_payload(&self) -> Result<Vec<u8>> {
        if self.relation()? != Relation::Hold {
            return Err(Error("consent requires a hold statement"));
        }
        Ok(self.digest().to_vec())
    }
    pub fn key_binding(profile: &ProofProfile, binding: &SignedKeyBinding) -> Result<Self> {
        binding.verify()?;
        Self::from_bytes(
            &binding_raw(profile.digest()?, profile.guest_image_id, &binding.binding).abi_encode(),
        )
    }
    pub fn window(order: &UnsignedOrder, index: u16, relation: Relation) -> Result<Self> {
        order.validate()?;
        if relation == Relation::KeyBinding {
            return Err(Error("window has key binding relation"));
        }
        let window = order
            .windows
            .get(index as usize)
            .ok_or(Error("window index"))?;
        let mut raw = base(&order.terms);
        raw.relation = relation as u8;
        raw.preTermsHash = terms_hash(&order.terms).into();
        raw.manifestRoot = manifest_root(order).into();
        raw.quoteDigest = quote_digest(order).into();
        raw.providerQuoteNonce = order.quote_nonce.into();
        raw.windowIndex = window.index;
        raw.windowOffset = window.offset;
        raw.windowPlaintextBytes = window.plaintext_bytes;
        raw.windowCiphertextBytes = window.ciphertext_bytes;
        raw.ciphertextHash = window.ciphertext_hash.into();
        raw.capsuleHash = window.capsule_hash.into();
        raw.plaintextCommitment = window.plaintext_commitment.into();
        raw.windowDeliveryFee = window.delivery_fee.0;
        raw.windowReservationFee = window.reservation_fee.0;
        Self::from_bytes(&raw.abi_encode())
    }
    pub(crate) fn binding_fields(&self) -> Result<(Word, Word, KeyBinding)> {
        let raw = self.raw()?;
        Ok((
            raw.profileId.into(),
            raw.guestImageId.into(),
            binding_from_raw(&raw)?,
        ))
    }
    fn raw(&self) -> Result<DeliveryStatement> {
        if self.0.len() != STATEMENT_BYTES {
            return Err(Error("statement exact ABI length"));
        }
        let raw = DeliveryStatement::abi_decode_validate(&self.0)
            .map_err(|_| Error("noncanonical statement ABI"))?;
        if raw.abi_encode() != self.0 {
            return Err(Error("noncanonical statement ABI"));
        }
        if raw.domain != B256::from(tag("vibeface-delivery-statement-v1")) {
            return Err(Error("statement domain"));
        }
        let relation = Relation::try_from(raw.relation)?;
        let binding = binding_from_raw(&raw)?;
        binding.validate()?;
        if raw.keyPurpose != 1 || raw.keyBindingHash != B256::from(hash(&binding_payload(&binding)))
        {
            return Err(Error("receiving key binding hash or purpose"));
        }
        if raw.profileId == B256::ZERO || raw.guestImageId == B256::ZERO {
            return Err(Error("zero proof profile or image"));
        }
        if relation == Relation::KeyBinding {
            let expected = binding_raw(raw.profileId.into(), raw.guestImageId.into(), &binding);
            if expected.abi_encode() != self.0 {
                return Err(Error("nonzero order fields in key binding statement"));
            }
            return Ok(raw);
        }
        let terms = terms_from_raw(&raw, binding)?;
        terms.validate()?;
        if raw.cryptoSuite != 1
            || raw.orderId != B256::from(order_id(&terms))
            || raw.preTermsHash != B256::from(terms_hash(&terms))
        {
            return Err(Error("statement suite, order id, or PreTerms hash"));
        }
        if raw.windowIndex >= raw.windowCount
            || raw.windowPlaintextBytes as usize > crate::MAX_WINDOW_BYTES
            || raw.windowCiphertextBytes != raw.windowPlaintextBytes + 68
            || raw
                .windowOffset
                .checked_add(raw.windowPlaintextBytes as u64)
                .is_none_or(|end| end > raw.totalPlaintextBytes)
            || raw.manifestRoot == B256::ZERO
            || raw.quoteDigest == B256::ZERO
            || raw.ciphertextHash == B256::ZERO
            || raw.capsuleHash == B256::ZERO
            || raw.plaintextCommitment == B256::ZERO
            || raw.providerQuoteNonce == B256::ZERO
        {
            return Err(Error("statement window bounds or commitments"));
        }
        Ok(raw)
    }
}

impl Serialize for Statement {
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Statement {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        Self::from_bytes(&bytes).map_err(serde::de::Error::custom)
    }
}

pub fn hash(bytes: &[u8]) -> Word {
    Sha256::digest(bytes).into()
}
pub(crate) fn tag(value: &str) -> Word {
    hash(value.as_bytes())
}

pub fn signing_message(domain: &str, payload: &[u8]) -> Result<Vec<u8>> {
    if domain.is_empty()
        || domain.len() > 64
        || !domain
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(Error("invalid signing domain"));
    }
    let mut hex = String::with_capacity(64);
    for byte in hash(payload) {
        use core::fmt::Write;
        write!(hex, "{byte:02x}").map_err(|_| Error("signature envelope encoding"))?;
    }
    Ok(format!("vibeface:message:v1:{domain}:{hex}").into_bytes())
}

pub(crate) fn order_id(terms: &Terms) -> Word {
    hash(
        &(
            B256::from(tag("vibeface-delivery-order-v1")),
            U256::from_be_bytes(terms.binding.chain_id),
            AbiAddress::from(terms.binding.escrow),
            AbiAddress::from(terms.payer),
            U256::from_be_bytes(terms.payer_order_nonce),
        )
            .abi_encode(),
    )
}

pub(crate) fn binding_payload(binding: &KeyBinding) -> Vec<u8> {
    (
        B256::from(tag("vibeface-delivery-key-v1")),
        U256::from_be_bytes(binding.chain_id),
        AbiAddress::from(binding.escrow),
        B256::from(binding.receiver_peer),
        B256::from(binding.receiver_public),
        B256::from(binding.key_id),
        binding.serial,
        binding.valid_from,
        binding.valid_until,
        1u32,
        U256::from(binding.action as u8),
    )
        .abi_encode()
}

fn binding_raw(profile: Word, image: Word, binding: &KeyBinding) -> DeliveryStatement {
    DeliveryStatement {
        domain: tag("vibeface-delivery-statement-v1").into(),
        relation: Relation::KeyBinding as u8,
        chainId: U256::from_be_bytes(binding.chain_id),
        escrow: binding.escrow.into(),
        profileId: profile.into(),
        guestImageId: image.into(),
        receiverPeer: binding.receiver_peer.into(),
        receiverPublic: binding.receiver_public.into(),
        receiverKeyId: binding.key_id.into(),
        receiverKeySerial: binding.serial,
        keyValidFrom: binding.valid_from,
        keyValidUntil: binding.valid_until,
        keyBindingHash: hash(&binding_payload(binding)).into(),
        keyPurpose: 1,
        bindingAction: binding.action as u8,
        ..Default::default()
    }
}

fn base(terms: &Terms) -> DeliveryStatement {
    let mut raw = binding_raw(terms.profile_id, terms.guest_image_id, &terms.binding);
    raw.relation = 0;
    raw.orderId = order_id(terms).into();
    raw.sourcePeer = terms.source_peer.into();
    raw.providerPeer = terms.provider_peer.into();
    raw.payer = terms.payer.into();
    raw.providerWallet = terms.provider_wallet.into();
    raw.asset = terms.asset.into();
    raw.acceptance = terms.acceptance as u8;
    raw.cryptoSuite = 1;
    raw.totalPlaintextBytes = terms.total_plaintext_bytes;
    raw.windowCount = terms.window_count;
    raw.wholeOrderBonus = terms.whole_order_bonus.0;
    raw.escrowAmount = terms.escrow_amount.0;
    raw.quoteExpiresAt = terms.quote_expires_at;
    raw.holdDeadline = terms.hold_deadline;
    raw.keyDeadline = terms.key_deadline;
    raw.openDeadline = terms.open_deadline;
    raw.retainUntil = terms.retain_until;
    raw.priorityDeadline = terms.priority_deadline;
    raw.payerOrderNonce = U256::from_be_bytes(terms.payer_order_nonce);
    raw
}

pub(crate) fn terms_hash(terms: &Terms) -> Word {
    let mut raw = base(terms);
    raw.domain = tag("vibeface-delivery-preterms-v1").into();
    hash(&raw.abi_encode())
}

pub(crate) fn window_hash(window: &WindowSpec) -> Word {
    hash(
        &(
            B256::from(tag("vibeface-delivery-window-v1")),
            window.index,
            window.offset,
            window.plaintext_bytes,
            window.ciphertext_bytes,
            B256::from(window.ciphertext_hash),
            B256::from(window.capsule_hash),
            B256::from(window.plaintext_commitment),
            window.delivery_fee.0,
            window.reservation_fee.0,
        )
            .abi_encode(),
    )
}

pub(crate) fn manifest_payload(order: &UnsignedOrder) -> Vec<u8> {
    let leaves: Vec<B256> = order
        .windows
        .iter()
        .map(|window| B256::from(window_hash(window)))
        .collect();
    (
        B256::from(tag("vibeface-delivery-manifest-v1")),
        B256::from(terms_hash(&order.terms)),
        leaves,
    )
        .abi_encode_params()
}

pub(crate) fn manifest_root(order: &UnsignedOrder) -> Word {
    hash(&manifest_payload(order))
}

pub(crate) fn quote_digest(order: &UnsignedOrder) -> Word {
    hash(
        &(
            B256::from(tag("vibeface-delivery-quote-v1")),
            B256::from(terms_hash(&order.terms)),
            B256::from(manifest_root(order)),
            B256::from(order.quote_nonce),
        )
            .abi_encode(),
    )
}

pub(crate) fn source_payload(order: &UnsignedOrder) -> Vec<u8> {
    (
        B256::from(order_id(&order.terms)),
        B256::from(terms_hash(&order.terms)),
        B256::from(manifest_root(order)),
        B256::from(quote_digest(order)),
    )
        .abi_encode()
}

pub(crate) fn crypto_context(
    tag_name: &str,
    terms_hash: Word,
    index: u16,
    offset: u64,
    length: u32,
) -> Vec<u8> {
    (
        B256::from(tag(tag_name)),
        B256::from(terms_hash),
        index,
        offset,
        length,
    )
        .abi_encode()
}

fn binding_from_raw(raw: &DeliveryStatement) -> Result<KeyBinding> {
    Ok(KeyBinding {
        chain_id: raw.chainId.to_be_bytes(),
        escrow: raw.escrow.into(),
        receiver_peer: raw.receiverPeer.into(),
        receiver_public: raw.receiverPublic.into(),
        key_id: raw.receiverKeyId.into(),
        serial: raw.receiverKeySerial,
        valid_from: raw.keyValidFrom,
        valid_until: raw.keyValidUntil,
        action: match raw.bindingAction {
            1 => BindingAction::Active,
            2 => BindingAction::Revoked,
            _ => return Err(Error("unknown key binding action")),
        },
    })
}

fn terms_from_raw(raw: &DeliveryStatement, binding: KeyBinding) -> Result<Terms> {
    Ok(Terms {
        profile_id: raw.profileId.into(),
        guest_image_id: raw.guestImageId.into(),
        binding,
        source_peer: raw.sourcePeer.into(),
        provider_peer: raw.providerPeer.into(),
        payer: raw.payer.into(),
        provider_wallet: raw.providerWallet.into(),
        asset: raw.asset.into(),
        acceptance: match raw.acceptance {
            0 => Acceptance::Recoverable,
            1 => Acceptance::Strict,
            _ => return Err(Error("unknown acceptance mode")),
        },
        total_plaintext_bytes: raw.totalPlaintextBytes,
        window_count: raw.windowCount,
        whole_order_bonus: Amount(raw.wholeOrderBonus),
        escrow_amount: Amount(raw.escrowAmount),
        quote_expires_at: raw.quoteExpiresAt,
        hold_deadline: raw.holdDeadline,
        key_deadline: raw.keyDeadline,
        open_deadline: raw.openDeadline,
        retain_until: raw.retainUntil,
        priority_deadline: raw.priorityDeadline,
        payer_order_nonce: raw.payerOrderNonce.to_be_bytes(),
    })
}
