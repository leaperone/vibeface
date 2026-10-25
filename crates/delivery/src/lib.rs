#![no_std]

extern crate alloc;

mod crypto;
mod statement;

use alloc::{
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};
use alloy_primitives::U256;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub use crypto::{encrypt_window, receiving_public, recover_window, verify_peer, verify_relation};
pub use statement::{ProofProfile, Statement, hash, schema_hash, signing_message};

pub type Word = [u8; 32];
pub type Address = [u8; 20];
pub type Result<T> = core::result::Result<T, Error>;
pub const STATEMENT_BYTES: usize = 47 * 32;
pub const MAX_WINDOW_BYTES: usize = 65_536;
pub const MAX_WINDOWS: usize = 16;
pub const CAPSULE_BYTES: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(pub &'static str);
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}
impl core::error::Error for Error {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Amount(U256);
impl Amount {
    pub const ZERO: Self = Self(U256::ZERO);
    pub const fn from_u64(value: u64) -> Self {
        Self(U256::from_limbs([value, 0, 0, 0]))
    }
    pub fn from_be_bytes(value: Word) -> Self {
        Self(U256::from_be_bytes(value))
    }
    pub fn to_be_bytes(self) -> Word {
        self.0.to_be_bytes()
    }
    pub fn from_decimal_str(value: &str) -> Result<Self> {
        if value.is_empty()
            || value.len() > 78
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || (value.len() > 1 && value.starts_with('0'))
        {
            return Err(Error("invalid canonical decimal amount"));
        }
        U256::from_str_radix(value, 10)
            .map(Self)
            .map_err(|_| Error("amount exceeds uint256"))
    }
    pub fn to_decimal_string(self) -> String {
        self.0.to_string()
    }
    pub fn checked_add(self, other: Self) -> Result<Self> {
        self.0
            .checked_add(other.0)
            .map(Self)
            .ok_or(Error("amount overflow"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Relation {
    KeyBinding = 1,
    Source = 2,
    Hold = 3,
    Open = 4,
}
impl TryFrom<u8> for Relation {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::KeyBinding),
            2 => Ok(Self::Source),
            3 => Ok(Self::Hold),
            4 => Ok(Self::Open),
            _ => Err(Error("unknown proof relation")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Acceptance {
    Recoverable = 0,
    Strict = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum BindingAction {
    Active = 1,
    Revoked = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerSignature {
    pub r: Word,
    pub s: Word,
}
impl PeerSignature {
    pub fn from_bytes(bytes: [u8; 64]) -> Self {
        Self {
            r: bytes[..32].try_into().expect("signature r"),
            s: bytes[32..].try_into().expect("signature s"),
        }
    }
    pub fn to_bytes(self) -> [u8; 64] {
        let mut bytes = [0; 64];
        bytes[..32].copy_from_slice(&self.r);
        bytes[32..].copy_from_slice(&self.s);
        bytes
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyBinding {
    pub chain_id: Word,
    pub escrow: Address,
    pub receiver_peer: Word,
    pub receiver_public: Word,
    pub key_id: Word,
    pub serial: u64,
    pub valid_from: u64,
    pub valid_until: u64,
    pub action: BindingAction,
}
impl KeyBinding {
    pub fn validate(&self) -> Result<()> {
        nonzero(&self.chain_id, "zero chain id")?;
        nonzero(&self.escrow, "zero escrow")?;
        crypto::validate_peer(&self.receiver_peer)?;
        if self.serial == 0 || self.valid_from >= self.valid_until {
            return Err(Error("invalid key serial or validity interval"));
        }
        match self.action {
            BindingAction::Active => {
                nonzero(&self.key_id, "zero receiving key id")?;
                crypto::validate_receiving_public(&self.receiver_public)?;
            }
            BindingAction::Revoked => {
                if self.receiver_public != [0; 32] || self.key_id != [0; 32] {
                    return Err(Error("revoked binding contains a receiving key"));
                }
            }
        }
        Ok(())
    }
    pub fn signing_payload(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(statement::binding_payload(self))
    }
    pub fn digest(&self) -> Result<Word> {
        Ok(hash(&self.signing_payload()?))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedKeyBinding {
    pub binding: KeyBinding,
    pub signature: PeerSignature,
}
impl SignedKeyBinding {
    pub fn verify(&self) -> Result<()> {
        verify_peer(
            &self.binding.receiver_peer,
            "delivery-key",
            &self.binding.signing_payload()?,
            &self.signature,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terms {
    pub profile_id: Word,
    pub guest_image_id: Word,
    pub binding: KeyBinding,
    pub source_peer: Word,
    pub provider_peer: Word,
    pub payer: Address,
    pub provider_wallet: Address,
    pub asset: Address,
    pub acceptance: Acceptance,
    pub total_plaintext_bytes: u64,
    pub window_count: u16,
    pub whole_order_bonus: Amount,
    pub escrow_amount: Amount,
    pub quote_expires_at: u64,
    pub hold_deadline: u64,
    pub key_deadline: u64,
    pub open_deadline: u64,
    pub retain_until: u64,
    pub priority_deadline: u64,
    pub payer_order_nonce: Word,
}
impl Terms {
    pub fn validate(&self) -> Result<()> {
        self.binding.validate()?;
        if self.binding.action != BindingAction::Active {
            return Err(Error("order requires an active receiving key"));
        }
        nonzero(&self.profile_id, "zero profile id")?;
        nonzero(&self.guest_image_id, "zero guest image")?;
        crypto::validate_peer(&self.source_peer)?;
        crypto::validate_peer(&self.provider_peer)?;
        nonzero(&self.payer, "zero payer")?;
        nonzero(&self.provider_wallet, "zero provider wallet")?;
        nonzero(&self.asset, "zero asset")?;
        if self.window_count == 0
            || self.window_count as usize > MAX_WINDOWS
            || self.total_plaintext_bytes > (MAX_WINDOWS * MAX_WINDOW_BYTES) as u64
        {
            return Err(Error("order size or window count exceeds protocol limit"));
        }
        if self.quote_expires_at < self.binding.valid_from
            || self.quote_expires_at > self.hold_deadline
            || self.hold_deadline >= self.key_deadline
            || self.key_deadline > self.open_deadline
            || self.open_deadline > self.retain_until
            || self.priority_deadline <= self.quote_expires_at
            || self.priority_deadline
                > match self.acceptance {
                    Acceptance::Recoverable => self.key_deadline,
                    Acceptance::Strict => self.open_deadline,
                }
            || self.retain_until > self.binding.valid_until
        {
            return Err(Error("invalid deadlines or receiving key retention"));
        }
        Ok(())
    }
    pub fn order_id(&self) -> Result<Word> {
        self.validate()?;
        Ok(statement::order_id(self))
    }
    pub fn digest(&self) -> Result<Word> {
        self.validate()?;
        Ok(statement::terms_hash(self))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSpec {
    pub index: u16,
    pub offset: u64,
    pub plaintext_bytes: u32,
    pub ciphertext_bytes: u32,
    pub ciphertext_hash: Word,
    pub capsule_hash: Word,
    pub plaintext_commitment: Word,
    pub delivery_fee: Amount,
    pub reservation_fee: Amount,
}
impl WindowSpec {
    pub fn digest(&self) -> Word {
        statement::window_hash(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsignedOrder {
    pub terms: Terms,
    pub windows: Vec<WindowSpec>,
    pub quote_nonce: Word,
}
impl UnsignedOrder {
    pub fn validate(&self) -> Result<()> {
        self.terms.validate()?;
        nonzero(&self.quote_nonce, "zero quote nonce")?;
        if self.windows.len() != self.terms.window_count as usize {
            return Err(Error("manifest window count"));
        }
        let mut offset = 0u64;
        let mut total = self.terms.whole_order_bonus;
        for (index, window) in self.windows.iter().enumerate() {
            if window.index as usize != index
                || window.offset != offset
                || window.plaintext_bytes as usize > MAX_WINDOW_BYTES
                || window.ciphertext_bytes != window.plaintext_bytes + 68
            {
                return Err(Error("manifest window index, offset, or length"));
            }
            if window.plaintext_bytes == 0
                && !(self.windows.len() == 1 && self.terms.total_plaintext_bytes == 0)
            {
                return Err(Error("empty manifest window"));
            }
            nonzero(&window.ciphertext_hash, "zero ciphertext hash")?;
            nonzero(&window.capsule_hash, "zero capsule hash")?;
            nonzero(&window.plaintext_commitment, "zero plaintext commitment")?;
            offset = offset
                .checked_add(u64::from(window.plaintext_bytes))
                .ok_or(Error("manifest offset overflow"))?;
            total = total
                .checked_add(window.delivery_fee)?
                .checked_add(window.reservation_fee)?;
        }
        if offset != self.terms.total_plaintext_bytes || total != self.terms.escrow_amount {
            return Err(Error(
                "manifest does not cover exact length or escrow amount",
            ));
        }
        Ok(())
    }
    pub fn manifest_root(&self) -> Result<Word> {
        self.validate()?;
        Ok(statement::manifest_root(self))
    }
    pub fn manifest_abi_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(statement::manifest_payload(self))
    }
    pub fn quote_digest(&self) -> Result<Word> {
        self.validate()?;
        Ok(statement::quote_digest(self))
    }
    pub fn quote_signing_payload(&self) -> Result<Vec<u8>> {
        Ok(self.quote_digest()?.to_vec())
    }
    pub fn source_signing_payload(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(statement::source_payload(self))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub unsigned: UnsignedOrder,
    pub binding_signature: PeerSignature,
    pub quote_signature: PeerSignature,
    pub source_signature: PeerSignature,
}
impl Order {
    pub fn verify(&self) -> Result<()> {
        self.unsigned.validate()?;
        let terms = &self.unsigned.terms;
        SignedKeyBinding {
            binding: terms.binding.clone(),
            signature: self.binding_signature,
        }
        .verify()?;
        verify_peer(
            &terms.provider_peer,
            "delivery-quote",
            &self.unsigned.quote_signing_payload()?,
            &self.quote_signature,
        )?;
        verify_peer(
            &terms.source_peer,
            "delivery-manifest",
            &self.unsigned.source_signing_payload()?,
            &self.source_signature,
        )
    }
    pub fn signed(
        unsigned: UnsignedOrder,
        binding_signature: PeerSignature,
        quote_signature: PeerSignature,
        source_signature: PeerSignature,
    ) -> Result<Self> {
        let order = Self {
            unsigned,
            binding_signature,
            quote_signature,
            source_signature,
        };
        order.verify()?;
        Ok(order)
    }
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct Secret32(Word);
impl Secret32 {
    pub fn new(value: Word) -> Result<Self> {
        nonzero(&value, "zero secret")?;
        Ok(Self(value))
    }
    pub fn as_bytes(&self) -> &Word {
        &self.0
    }
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct SecretBytes(Vec<u8>);
impl SecretBytes {
    pub fn new(value: Vec<u8>) -> Self {
        Self(value)
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Serialize, Deserialize)]
pub struct WindowRandomness {
    pub salt: Secret32,
    pub data_key: Secret32,
    pub nonce: [u8; 12],
    pub hpke_seed: Secret32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedWindow {
    pub spec: WindowSpec,
    pub ciphertext: Vec<u8>,
    pub capsule: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
pub enum Witness {
    KeyBinding {
        binding: SignedKeyBinding,
    },
    Source {
        order: Box<Order>,
        index: u16,
        plaintext: SecretBytes,
        randomness: WindowRandomness,
        ciphertext: Vec<u8>,
        capsule: Vec<u8>,
    },
    Hold {
        order: UnsignedOrder,
        index: u16,
        receiving_secret: Secret32,
        ciphertext: Vec<u8>,
        consent: PeerSignature,
    },
    Open {
        order: UnsignedOrder,
        index: u16,
        receiving_secret: Secret32,
        ciphertext: Vec<u8>,
        capsule: Vec<u8>,
    },
}

#[derive(Serialize, Deserialize)]
pub struct ProofInput {
    pub profile: ProofProfile,
    pub statement: Statement,
    pub witness: Witness,
}

pub struct RecoveredWindow {
    pub plaintext: Zeroizing<Vec<u8>>,
    pub sha256: Word,
}

fn nonzero(bytes: &[u8], message: &'static str) -> Result<()> {
    if bytes.iter().all(|byte| *byte == 0) {
        Err(Error(message))
    } else {
        Ok(())
    }
}

#[cfg(test)]
extern crate self as vibeface_delivery;
#[cfg(test)]
extern crate std;
#[cfg(test)]
#[path = "../examples/support/mod.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{fixture, sign};

    fn run(input: &ProofInput) -> Result<()> {
        verify_relation(&input.profile, &input.statement, &input.witness)
    }

    #[test]
    fn four_relations_and_two_full_windows_recover_every_byte() {
        let f = fixture(&[MAX_WINDOW_BYTES, MAX_WINDOW_BYTES], [0x91; 32]);
        run(&f.input(Relation::KeyBinding, 0)).unwrap();
        for index in 0..2 {
            for relation in [Relation::Source, Relation::Hold, Relation::Open] {
                run(&f.input(relation, index)).unwrap();
            }
            let recovered = recover_window(
                &f.order.unsigned,
                index,
                &Secret32::new([7; 32]).unwrap(),
                &f.encrypted[index as usize].ciphertext,
                &f.encrypted[index as usize].capsule,
            )
            .unwrap();
            assert_eq!(
                *recovered.plaintext,
                crate::fixture::plaintext(MAX_WINDOW_BYTES, index as usize)
            );
        }
    }

    #[test]
    fn empty_file_has_one_authenticated_empty_window() {
        let f = fixture(&[0], [0x91; 32]);
        for relation in [Relation::Source, Relation::Hold, Relation::Open] {
            run(&f.input(relation, 0)).unwrap();
        }
        assert_eq!(f.encrypted[0].ciphertext.len(), 68);
    }

    #[test]
    fn every_statement_slot_and_phase_are_bound() {
        let f = fixture(&[35], [0x91; 32]);
        let input = f.input(Relation::Source, 0);
        assert_eq!(input.statement.abi_bytes().len(), STATEMENT_BYTES);
        for slot in 0..47 {
            let mut bytes = input.statement.abi_bytes().to_vec();
            bytes[slot * 32 + 31] ^= 1;
            if let Ok(changed) = Statement::from_bytes(&bytes) {
                assert!(
                    verify_relation(&input.profile, &changed, &input.witness).is_err(),
                    "slot {slot}"
                );
            }
        }
        for relation in [Relation::Hold, Relation::Open] {
            let changed = Statement::window(&f.order.unsigned, 0, relation).unwrap();
            assert!(verify_relation(&input.profile, &changed, &input.witness).is_err());
        }
    }

    #[test]
    fn canonical_abi_rejects_padding_tail_and_unknown_enums() {
        let f = fixture(&[3], [0x91; 32]);
        let input = f.input(Relation::Source, 0);
        for slot in [
            1, 3, 13, 14, 15, 16, 19, 20, 21, 23, 24, 25, 26, 27, 28, 29, 37, 38, 39, 40, 41, 42,
            43, 46,
        ] {
            let mut bytes = input.statement.abi_bytes().to_vec();
            bytes[slot * 32] = 1;
            assert!(
                Statement::from_bytes(&bytes).is_err(),
                "padding slot {slot}"
            );
        }
        let mut bytes = input.statement.abi_bytes().to_vec();
        bytes.push(0);
        assert!(Statement::from_bytes(&bytes).is_err());
        assert!(Statement::from_bytes(&bytes[..bytes.len() - 2]).is_err());
    }

    #[test]
    fn manifest_abi_encodes_parameters_without_a_dynamic_tuple_wrapper() {
        let f = fixture(&[15, 20], [0x91; 32]);
        let order = &f.order.unsigned;
        let bytes = order.manifest_abi_bytes().unwrap();
        assert_eq!(bytes.len(), 6 * 32);
        assert_eq!(&bytes[..32], &hash(b"vibeface-delivery-manifest-v1"));
        assert_eq!(&bytes[32..64], &order.terms.digest().unwrap());
        assert_eq!(&bytes[64..96], &Amount::from_u64(96).to_be_bytes());
        assert_eq!(&bytes[96..128], &Amount::from_u64(2).to_be_bytes());
        assert_eq!(&bytes[128..160], &order.windows[0].digest());
        assert_eq!(&bytes[160..192], &order.windows[1].digest());
        assert_eq!(hash(&bytes), order.manifest_root().unwrap());
        let mut wrongly_wrapped = Amount::from_u64(32).to_be_bytes().to_vec();
        wrongly_wrapped.extend_from_slice(&bytes);
        assert_ne!(hash(&wrongly_wrapped), order.manifest_root().unwrap());
    }

    #[test]
    fn identity_signatures_are_verified_in_source_and_hold() {
        let f = fixture(&[35], [0x91; 32]);
        for which in 0..3 {
            let mut input = f.input(Relation::Source, 0);
            if let Witness::Source { order, .. } = &mut input.witness {
                match which {
                    0 => order.binding_signature.s[0] ^= 1,
                    1 => order.quote_signature.s[0] ^= 1,
                    _ => order.source_signature.s[0] ^= 1,
                }
            }
            assert!(run(&input).is_err());
        }
        let mut input = f.input(Relation::Hold, 0);
        if let Witness::Hold { consent, .. } = &mut input.witness {
            consent.s[0] ^= 1;
        }
        assert!(run(&input).is_err());
    }

    #[test]
    fn source_rejects_changed_plaintext_ciphertext_or_capsule() {
        let f = fixture(&[35], [0x91; 32]);
        for which in 0..3 {
            let mut input = f.input(Relation::Source, 0);
            if let Witness::Source {
                plaintext,
                ciphertext,
                capsule,
                ..
            } = &mut input.witness
            {
                match which {
                    0 => plaintext.0[0] ^= 1,
                    1 => ciphertext[0] ^= 1,
                    _ => capsule[0] ^= 1,
                }
            }
            assert!(run(&input).is_err());
        }
    }

    #[test]
    fn hold_requires_complete_ciphertext_and_bound_secret() {
        let f = fixture(&[35], [0x91; 32]);
        for which in 0..3 {
            let mut input = f.input(Relation::Hold, 0);
            if let Witness::Hold {
                receiving_secret,
                ciphertext,
                ..
            } = &mut input.witness
            {
                match which {
                    0 => *receiving_secret = Secret32::new([8; 32]).unwrap(),
                    1 => ciphertext.clear(),
                    _ => ciphertext[34] ^= 1,
                }
            }
            assert!(run(&input).is_err());
        }
    }

    #[test]
    fn open_authenticates_aead_after_successful_hpke_unwrap() {
        let f = fixture(&[35], [0x91; 32]);
        let mut order = f.order.unsigned.clone();
        let mut ciphertext = f.encrypted[0].ciphertext.clone();
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 1;
        order.windows[0].ciphertext_hash = hash(&ciphertext);
        let error = recover_window(
            &order,
            0,
            &Secret32::new([7; 32]).unwrap(),
            &ciphertext,
            &f.encrypted[0].capsule,
        )
        .err()
        .unwrap();
        assert_eq!(error, Error("file AEAD authentication"));
    }

    #[test]
    fn open_unwraps_the_actual_capsule_and_requires_bound_secret() {
        let f = fixture(&[35], [0x91; 32]);
        let mut order = f.order.unsigned.clone();
        let mut capsule = f.encrypted[0].capsule.clone();
        capsule[79] ^= 1;
        order.windows[0].capsule_hash = hash(&capsule);
        assert_eq!(
            recover_window(
                &order,
                0,
                &Secret32::new([7; 32]).unwrap(),
                &f.encrypted[0].ciphertext,
                &capsule
            )
            .err()
            .unwrap(),
            Error("HPKE capsule authentication")
        );
        let mut input = f.input(Relation::Open, 0);
        if let Witness::Open {
            receiving_secret, ..
        } = &mut input.witness
        {
            *receiving_secret = Secret32::new([8; 32]).unwrap();
        }
        assert!(run(&input).is_err());
    }

    #[test]
    fn manifest_rejects_missing_duplicate_gap_reorder_length_and_fee_overflow() {
        let f = fixture(&[15, 20], [0x91; 32]);
        for which in 0..7 {
            let mut order = f.order.unsigned.clone();
            match which {
                0 => {
                    order.windows.pop();
                }
                1 => order.windows[1].index = 0,
                2 => order.windows[1].offset += 1,
                3 => order.windows.swap(0, 1),
                4 => order.terms.total_plaintext_bytes -= 1,
                5 => order.windows[0].delivery_fee = Amount::from_be_bytes([0xff; 32]),
                _ => order.terms.escrow_amount = Amount::from_u64(99),
            }
            assert!(order.validate().is_err());
        }
    }

    #[test]
    fn profile_hash_schema_and_all_resource_caps_are_bound() {
        let f = fixture(&[35], [0x91; 32]);
        let input = f.input(Relation::Source, 0);
        for which in 0..8 {
            let mut p = f.profile.clone();
            match which {
                0 => p.guest_image_id[0] ^= 1,
                1 => p.statement_schema_hash[0] ^= 1,
                2 => p.crypto_suite = 2,
                3 => p.max_windows = 17,
                4 => p.max_window_bytes = 65_537,
                5 => p.max_total_bytes = 1,
                6 => p.max_guest_cycles += 1,
                _ => p.max_prover_memory_bytes += 1,
            }
            assert!(verify_relation(&p, &input.statement, &input.witness).is_err());
        }
        assert_eq!(f.profile.abi_bytes().unwrap().len(), 9 * 32);
    }

    #[test]
    fn priority_deadline_is_independent_and_in_preterms_and_signatures() {
        let f = fixture(&[35], [0x91; 32]);
        let mut changed = f.order.unsigned.clone();
        changed.terms.priority_deadline -= 1;
        assert_eq!(
            changed.terms.key_deadline,
            f.order.unsigned.terms.key_deadline
        );
        assert_ne!(
            changed.terms.digest().unwrap(),
            f.order.unsigned.terms.digest().unwrap()
        );
        assert_ne!(
            changed.quote_digest().unwrap(),
            f.order.unsigned.quote_digest().unwrap()
        );
        assert!(
            verify_peer(
                &changed.terms.provider_peer,
                "delivery-quote",
                &changed.quote_signing_payload().unwrap(),
                &f.order.quote_signature
            )
            .is_err()
        );
        changed.terms.priority_deadline = changed.terms.key_deadline + 1;
        assert!(changed.validate().is_err());
        changed.terms.priority_deadline = changed.terms.quote_expires_at;
        assert!(changed.validate().is_err());
    }

    #[test]
    fn key_binding_and_revoke_have_no_order_fields() {
        let f = fixture(&[35], [0x91; 32]);
        let input = f.input(Relation::KeyBinding, 0);
        let allowed = [0, 1, 2, 3, 4, 5, 11, 17, 18, 19, 20, 21, 22, 43, 46];
        for slot in 0..47 {
            if !allowed.contains(&slot) {
                let mut bytes = input.statement.abi_bytes().to_vec();
                bytes[slot * 32 + 31] = 1;
                assert!(Statement::from_bytes(&bytes).is_err());
            }
        }
        let mut binding = f.order.unsigned.terms.binding.clone();
        binding.serial += 1;
        binding.action = BindingAction::Revoked;
        binding.receiver_public = [0; 32];
        binding.key_id = [0; 32];
        let signed = SignedKeyBinding {
            signature: sign(1, "delivery-key", &binding.signing_payload().unwrap()),
            binding,
        };
        let statement = Statement::key_binding(&f.profile, &signed).unwrap();
        verify_relation(
            &f.profile,
            &statement,
            &Witness::KeyBinding { binding: signed },
        )
        .unwrap();
    }

    #[test]
    fn small_order_and_noncanonical_public_keys_are_rejected() {
        let f = fixture(&[35], [0x91; 32]);
        let mut one = [0; 32];
        one[0] = 1;
        let mut modulus = [0xff; 32];
        modulus[0] = 0xed;
        modulus[31] = 0x7f;
        for bad in [[0; 32], one, modulus, [0xff; 32]] {
            assert!(crypto::validate_peer(&bad).is_err());
            let mut binding = f.order.unsigned.terms.binding.clone();
            binding.receiver_public = bad;
            assert!(binding.validate().is_err());
        }
        assert!(Secret32::new([0; 32]).is_err());
        assert!(
            Amount::from_be_bytes([0xff; 32])
                .checked_add(Amount::from_u64(1))
                .is_err()
        );
        assert_eq!(
            Amount::from_decimal_str("10000000000000000000000000000000000000000")
                .unwrap()
                .to_decimal_string(),
            "10000000000000000000000000000000000000000"
        );
        for invalid in ["", "01", "-1", "+1", "1e10", "0x10"] {
            assert!(Amount::from_decimal_str(invalid).is_err());
        }
    }

    #[test]
    fn signature_envelope_matches_existing_secure_contract() {
        assert_eq!(signing_message("delivery-key", b"abc").unwrap(),
            b"vibeface:message:v1:delivery-key:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_vec());
        assert!(signing_message("delivery:key", b"abc").is_err());
        assert!(signing_message("", b"abc").is_err());
        assert!(signing_message(&"x".repeat(65), b"abc").is_err());
        let f = fixture(&[1], [0x91; 32]);
        assert!(
            verify_peer(
                &f.order.unsigned.terms.provider_peer,
                "delivery-manifest",
                &f.order.unsigned.quote_signing_payload().unwrap(),
                &f.order.quote_signature
            )
            .is_err()
        );
    }
}
