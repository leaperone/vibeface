use alloc::vec::Vec;
use chacha20poly1305::{
    ChaCha20Poly1305, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use ed25519_dalek::{Signature, VerifyingKey};
use hpke::{
    Deserializable, Kem, OpModeR, OpModeS, Serializable, aead::ChaCha20Poly1305 as HpkeAead,
    kdf::HkdfSha256, kem::X25519HkdfSha256, setup_receiver, setup_sender,
};
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::{
    Amount, CAPSULE_BYTES, EncryptedWindow, Error, MAX_WINDOW_BYTES, PeerSignature, ProofProfile,
    RecoveredWindow, Relation, Result, Secret32, Statement, Terms, UnsignedOrder, WindowRandomness,
    WindowSpec, Witness, Word, hash, signing_message, statement,
};

type K = X25519HkdfSha256;

pub(crate) fn validate_peer(public: &Word) -> Result<()> {
    let mut y = *public;
    y[31] &= 0x7f;
    if !canonical_field(&y) || public.iter().all(|byte| *byte == 0) {
        return Err(Error("noncanonical Ed25519 public key"));
    }
    let key = VerifyingKey::from_bytes(public).map_err(|_| Error("invalid Ed25519 public key"))?;
    if key.is_weak() {
        return Err(Error("small-order Ed25519 public key"));
    }
    Ok(())
}

fn canonical_field(bytes: &Word) -> bool {
    let mut modulus = [0xff; 32];
    modulus[0] = 0xed;
    modulus[31] = 0x7f;
    bytes.iter().rev().cmp(modulus.iter().rev()).is_lt()
}

pub(crate) fn validate_receiving_public(public: &Word) -> Result<()> {
    if !canonical_field(public) || x25519_dalek::x25519([0x42; 32], *public) == [0; 32] {
        return Err(Error("noncanonical or small-order X25519 public key"));
    }
    Ok(())
}

pub fn verify_peer(
    public: &Word,
    domain: &str,
    payload: &[u8],
    signature: &PeerSignature,
) -> Result<()> {
    validate_peer(public)?;
    let key = VerifyingKey::from_bytes(public).map_err(|_| Error("invalid Ed25519 public key"))?;
    key.verify_strict(
        &signing_message(domain, payload)?,
        &Signature::from_bytes(&signature.to_bytes()),
    )
    .map_err(|_| Error("invalid PeerID signature"))
}

pub fn receiving_public(secret: &Secret32) -> Result<Word> {
    crate::nonzero(secret.as_bytes(), "zero receiving secret")?;
    let key = <K as Kem>::PrivateKey::from_bytes(secret.as_bytes())
        .map_err(|_| Error("invalid receiving secret"))?;
    let public: Word = K::sk_to_pk(&key).to_bytes().into();
    validate_receiving_public(&public)?;
    Ok(public)
}

pub fn encrypt_window(
    terms: &Terms,
    index: u16,
    offset: u64,
    plaintext: &[u8],
    randomness: &WindowRandomness,
    delivery_fee: Amount,
    reservation_fee: Amount,
) -> Result<EncryptedWindow> {
    terms.validate()?;
    if plaintext.len() > MAX_WINDOW_BYTES
        || index >= terms.window_count
        || offset
            .checked_add(plaintext.len() as u64)
            .is_none_or(|end| end > terms.total_plaintext_bytes)
    {
        return Err(Error("encryption window bounds"));
    }
    crate::nonzero(randomness.salt.as_bytes(), "zero salt")?;
    crate::nonzero(randomness.data_key.as_bytes(), "zero data key")?;
    crate::nonzero(randomness.hpke_seed.as_bytes(), "zero HPKE seed")?;
    let length = plaintext.len() as u32;
    let aad = context("vibeface-delivery-data-v1", terms, index, offset, length);
    let mut payload = Zeroizing::new(Vec::with_capacity(plaintext.len() + 40));
    payload.extend_from_slice(randomness.salt.as_bytes());
    payload.extend_from_slice(&(plaintext.len() as u64).to_be_bytes());
    payload.extend_from_slice(plaintext);
    let cipher = ChaCha20Poly1305::new_from_slice(randomness.data_key.as_bytes())
        .map_err(|_| Error("data key length"))?;
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&randomness.nonce),
            Payload {
                msg: &payload,
                aad: &aad,
            },
        )
        .map_err(|_| Error("file encryption"))?;
    let mut ciphertext = Vec::with_capacity(plaintext.len() + 68);
    ciphertext.extend_from_slice(&randomness.nonce);
    ciphertext.extend_from_slice(&sealed);
    let recipient = <K as Kem>::PublicKey::from_bytes(&terms.binding.receiver_public)
        .map_err(|_| Error("receiving public key"))?;
    let mut rng = ChaCha20Rng::from_seed(*randomness.hpke_seed.as_bytes());
    let (enc, mut sender) = setup_sender::<HpkeAead, HkdfSha256, K, _>(
        &OpModeS::Base,
        &recipient,
        &context(
            "vibeface-delivery-hpke-info-v1",
            terms,
            index,
            offset,
            length,
        ),
        &mut rng,
    )
    .map_err(|_| Error("HPKE sender setup"))?;
    let sealed_key = sender
        .seal(
            randomness.data_key.as_bytes(),
            &context(
                "vibeface-delivery-hpke-aad-v1",
                terms,
                index,
                offset,
                length,
            ),
        )
        .map_err(|_| Error("HPKE seal data key"))?;
    let mut capsule = enc.to_bytes().to_vec();
    capsule.extend_from_slice(&sealed_key);
    if capsule.len() != CAPSULE_BYTES {
        return Err(Error("HPKE capsule exact length"));
    }
    let spec = WindowSpec {
        index,
        offset,
        plaintext_bytes: length,
        ciphertext_bytes: ciphertext.len() as u32,
        ciphertext_hash: hash(&ciphertext),
        capsule_hash: hash(&capsule),
        plaintext_commitment: commitment(
            terms,
            index,
            offset,
            length,
            randomness.salt.as_bytes(),
            plaintext,
        ),
        delivery_fee,
        reservation_fee,
    };
    Ok(EncryptedWindow {
        spec,
        ciphertext,
        capsule,
    })
}

pub fn recover_window(
    order: &UnsignedOrder,
    index: u16,
    secret: &Secret32,
    ciphertext: &[u8],
    capsule: &[u8],
) -> Result<RecoveredWindow> {
    order.validate()?;
    let window = order
        .windows
        .get(index as usize)
        .ok_or(Error("window index"))?;
    check_ciphertext(window, ciphertext)?;
    if receiving_public(secret)? != order.terms.binding.receiver_public {
        return Err(Error("receiving private key does not match binding"));
    }
    if capsule.len() != CAPSULE_BYTES || hash(capsule) != window.capsule_hash {
        return Err(Error("complete capsule hash or length"));
    }
    let terms = &order.terms;
    let key = <K as Kem>::PrivateKey::from_bytes(secret.as_bytes())
        .map_err(|_| Error("receiving private key"))?;
    validate_receiving_public(
        &capsule[..32]
            .try_into()
            .map_err(|_| Error("HPKE encapsulated key length"))?,
    )?;
    let enc = <K as Kem>::EncappedKey::from_bytes(&capsule[..32])
        .map_err(|_| Error("HPKE encapsulated key"))?;
    let mut receiver = setup_receiver::<HpkeAead, HkdfSha256, K>(
        &OpModeR::Base,
        &key,
        &enc,
        &context(
            "vibeface-delivery-hpke-info-v1",
            terms,
            index,
            window.offset,
            window.plaintext_bytes,
        ),
    )
    .map_err(|_| Error("HPKE receiver setup"))?;
    let data_key = Zeroizing::new(
        receiver
            .open(
                &capsule[32..],
                &context(
                    "vibeface-delivery-hpke-aad-v1",
                    terms,
                    index,
                    window.offset,
                    window.plaintext_bytes,
                ),
            )
            .map_err(|_| Error("HPKE capsule authentication"))?,
    );
    if data_key.len() != 32 {
        return Err(Error("unwrapped data key length"));
    }
    let cipher =
        ChaCha20Poly1305::new_from_slice(&data_key).map_err(|_| Error("unwrapped data key"))?;
    let payload = Zeroizing::new(
        cipher
            .decrypt(
                Nonce::from_slice(&ciphertext[..12]),
                Payload {
                    msg: &ciphertext[12..],
                    aad: &context(
                        "vibeface-delivery-data-v1",
                        terms,
                        index,
                        window.offset,
                        window.plaintext_bytes,
                    ),
                },
            )
            .map_err(|_| Error("file AEAD authentication"))?,
    );
    if payload.len() != window.plaintext_bytes as usize + 40 {
        return Err(Error("recovered payload exact length"));
    }
    let salt: Word = payload[..32]
        .try_into()
        .map_err(|_| Error("recovered salt"))?;
    let length = u64::from_be_bytes(
        payload[32..40]
            .try_into()
            .map_err(|_| Error("recovered length encoding"))?,
    );
    if length != u64::from(window.plaintext_bytes)
        || commitment(
            terms,
            index,
            window.offset,
            window.plaintext_bytes,
            &salt,
            &payload[40..],
        ) != window.plaintext_commitment
    {
        return Err(Error("recovered length or plaintext commitment"));
    }
    Ok(RecoveredWindow {
        plaintext: Zeroizing::new(payload[40..].to_vec()),
        sha256: hash(&payload[40..]),
    })
}

pub fn verify_relation(
    profile: &ProofProfile,
    statement: &Statement,
    witness: &Witness,
) -> Result<()> {
    profile.validate()?;
    let (profile_id, image, _) = statement.binding_fields()?;
    if profile_id != profile.digest()? || image != profile.guest_image_id {
        return Err(Error("statement proof profile or image mismatch"));
    }
    match witness {
        Witness::KeyBinding { binding } => {
            if &Statement::key_binding(profile, binding)? != statement {
                return Err(Error("key binding statement mismatch"));
            }
        }
        Witness::Source {
            order,
            index,
            plaintext,
            randomness,
            ciphertext,
            capsule,
        } => {
            order.verify()?;
            check_statement(
                profile,
                statement,
                &order.unsigned,
                *index,
                Relation::Source,
            )?;
            let window = &order.unsigned.windows[*index as usize];
            check_ciphertext(window, ciphertext)?;
            let encrypted = encrypt_window(
                &order.unsigned.terms,
                *index,
                window.offset,
                plaintext.as_bytes(),
                randomness,
                window.delivery_fee,
                window.reservation_fee,
            )?;
            if encrypted.spec != *window
                || encrypted.ciphertext != *ciphertext
                || encrypted.capsule != *capsule
            {
                return Err(Error("complete source encryption relation"));
            }
        }
        Witness::Hold {
            order,
            index,
            receiving_secret,
            ciphertext,
            consent,
        } => {
            check_statement(profile, statement, order, *index, Relation::Hold)?;
            check_ciphertext(&order.windows[*index as usize], ciphertext)?;
            if receiving_public(receiving_secret)? != order.terms.binding.receiver_public {
                return Err(Error("hold receiving private key mismatch"));
            }
            verify_peer(
                &order.terms.binding.receiver_peer,
                "delivery-accept",
                &statement.consent_signing_payload()?,
                consent,
            )?;
        }
        Witness::Open {
            order,
            index,
            receiving_secret,
            ciphertext,
            capsule,
        } => {
            check_statement(profile, statement, order, *index, Relation::Open)?;
            recover_window(order, *index, receiving_secret, ciphertext, capsule)?;
        }
    }
    Ok(())
}

fn check_statement(
    profile: &ProofProfile,
    statement: &Statement,
    order: &UnsignedOrder,
    index: u16,
    relation: Relation,
) -> Result<()> {
    profile.check_order(order)?;
    if &Statement::window(order, index, relation)? != statement {
        return Err(Error("window statement mismatch"));
    }
    Ok(())
}

fn check_ciphertext(window: &WindowSpec, ciphertext: &[u8]) -> Result<()> {
    if ciphertext.len() != window.ciphertext_bytes as usize
        || hash(ciphertext) != window.ciphertext_hash
    {
        return Err(Error("complete ciphertext hash or length"));
    }
    Ok(())
}

fn context(label: &str, terms: &Terms, index: u16, offset: u64, length: u32) -> Vec<u8> {
    statement::crypto_context(label, terms, index, offset, length)
}

fn commitment(
    terms: &Terms,
    index: u16,
    offset: u64,
    length: u32,
    salt: &Word,
    plaintext: &[u8],
) -> Word {
    let mut digest = Sha256::new();
    digest.update(context(
        "vibeface-delivery-plain-v1",
        terms,
        index,
        offset,
        length,
    ));
    digest.update(salt);
    digest.update(plaintext);
    digest.finalize().into()
}
