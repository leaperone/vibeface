use ed25519_dalek::{Signer, SigningKey};
use std::vec::Vec;
use vibeface_delivery::*;

pub struct Fixture {
    pub profile: ProofProfile,
    pub order: Order,
    pub encrypted: Vec<EncryptedWindow>,
    pub lengths: Vec<usize>,
}

pub fn sign(seed: u8, domain: &str, payload: &[u8]) -> PeerSignature {
    let signer = SigningKey::from_bytes(&[seed; 32]);
    PeerSignature::from_bytes(
        signer
            .sign(&signing_message(domain, payload).unwrap())
            .to_bytes(),
    )
}

fn peer(seed: u8) -> Word {
    SigningKey::from_bytes(&[seed; 32])
        .verifying_key()
        .to_bytes()
}

pub fn plaintext(length: usize, index: usize) -> Vec<u8> {
    (0..length)
        .map(|value| ((value + index * 17) % 251) as u8)
        .collect()
}

pub fn randomness(index: usize) -> WindowRandomness {
    WindowRandomness {
        salt: Secret32::new([11 + index as u8; 32]).unwrap(),
        data_key: Secret32::new([31 + index as u8; 32]).unwrap(),
        nonce: [51 + index as u8; 12],
        hpke_seed: Secret32::new([71 + index as u8; 32]).unwrap(),
    }
}

pub fn fixture(lengths: &[usize], image: Word) -> Fixture {
    let profile = ProofProfile {
        guest_image_id: image,
        statement_schema_hash: schema_hash(),
        crypto_suite: 1,
        max_windows: 16,
        max_window_bytes: MAX_WINDOW_BYTES as u32,
        max_total_bytes: (MAX_WINDOW_BYTES * MAX_WINDOWS) as u64,
        max_guest_cycles: 64_000_000,
        max_prover_memory_bytes: 16 * 1024 * 1024 * 1024,
    };
    fixture_with_profile(lengths, profile)
}

pub fn fixture_with_profile(lengths: &[usize], profile: ProofProfile) -> Fixture {
    profile.validate().unwrap();
    assert!(lengths.len() <= profile.max_windows as usize);
    assert!(
        lengths
            .iter()
            .all(|length| *length <= profile.max_window_bytes as usize)
    );
    assert!(lengths.iter().sum::<usize>() as u64 <= profile.max_total_bytes);
    let image = profile.guest_image_id;
    let mut chain_id = [0; 32];
    chain_id[30..].copy_from_slice(&31337u16.to_be_bytes());
    let binding = KeyBinding {
        chain_id,
        escrow: [0x44; 20],
        receiver_peer: peer(1),
        receiver_public: receiving_public(&Secret32::new([7; 32]).unwrap()).unwrap(),
        key_id: [0x55; 32],
        serial: 9,
        valid_from: 100,
        valid_until: 700,
        action: BindingAction::Active,
    };
    let binding_signature = sign(1, "delivery-key", &binding.signing_payload().unwrap());
    let terms = Terms {
        profile_id: profile.digest().unwrap(),
        guest_image_id: image,
        binding,
        source_peer: peer(2),
        provider_peer: peer(3),
        payer: [0x11; 20],
        provider_wallet: [0x22; 20],
        asset: [0x33; 20],
        acceptance: Acceptance::Recoverable,
        total_plaintext_bytes: lengths.iter().sum::<usize>() as u64,
        window_count: lengths.len() as u16,
        whole_order_bonus: Amount::from_u64(1),
        escrow_amount: Amount::from_u64(1 + lengths.len() as u64 * 3),
        quote_expires_at: 200,
        hold_deadline: 300,
        key_deadline: 400,
        open_deadline: 500,
        retain_until: 600,
        priority_deadline: 350,
        payer_order_nonce: [0x66; 32],
    };
    let mut offset = 0;
    let encrypted: Vec<_> = lengths
        .iter()
        .enumerate()
        .map(|(index, length)| {
            let value = encrypt_window(
                &terms,
                index as u16,
                offset,
                &plaintext(*length, index),
                &randomness(index),
                Amount::from_u64(2),
                Amount::from_u64(1),
            )
            .unwrap();
            offset += *length as u64;
            value
        })
        .collect();
    let unsigned = UnsignedOrder {
        terms,
        windows: encrypted.iter().map(|window| window.spec.clone()).collect(),
        quote_nonce: [0x77; 32],
    };
    let quote_signature = sign(
        3,
        "delivery-quote",
        &unsigned.quote_signing_payload().unwrap(),
    );
    let source_signature = sign(
        2,
        "delivery-manifest",
        &unsigned.source_signing_payload().unwrap(),
    );
    let order = Order::signed(
        unsigned,
        binding_signature,
        quote_signature,
        source_signature,
    )
    .unwrap();
    Fixture {
        profile,
        order,
        encrypted,
        lengths: lengths.to_vec(),
    }
}

impl Fixture {
    pub fn input(&self, relation: Relation, index: u16) -> ProofInput {
        let order = &self.order.unsigned;
        let statement = if relation == Relation::KeyBinding {
            Statement::key_binding(
                &self.profile,
                &SignedKeyBinding {
                    binding: order.terms.binding.clone(),
                    signature: self.order.binding_signature,
                },
            )
            .unwrap()
        } else {
            Statement::window(order, index, relation).unwrap()
        };
        let index_usize = index as usize;
        let witness = match relation {
            Relation::KeyBinding => Witness::KeyBinding {
                binding: SignedKeyBinding {
                    binding: order.terms.binding.clone(),
                    signature: self.order.binding_signature,
                },
            },
            Relation::Source => Witness::Source {
                order: Box::new(self.order.clone()),
                index,
                plaintext: SecretBytes::new(plaintext(self.lengths[index_usize], index_usize)),
                randomness: randomness(index_usize),
                ciphertext: self.encrypted[index_usize].ciphertext.clone(),
                capsule: self.encrypted[index_usize].capsule.clone(),
            },
            Relation::Hold => Witness::Hold {
                order: order.clone(),
                index,
                receiving_secret: Secret32::new([7; 32]).unwrap(),
                ciphertext: self.encrypted[index_usize].ciphertext.clone(),
                consent: sign(
                    1,
                    "delivery-accept",
                    &statement.consent_signing_payload().unwrap(),
                ),
            },
            Relation::Open => Witness::Open {
                order: order.clone(),
                index,
                receiving_secret: Secret32::new([7; 32]).unwrap(),
                ciphertext: self.encrypted[index_usize].ciphertext.clone(),
                capsule: self.encrypted[index_usize].capsule.clone(),
            },
        };
        ProofInput {
            profile: self.profile.clone(),
            statement,
            witness,
        }
    }
}
