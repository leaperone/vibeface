use hpke::{
    Deserializable, Kem, OpModeR, OpModeS, Serializable, aead::ChaCha20Poly1305 as HpkeAead,
    kdf::HkdfSha256, kem::X25519HkdfSha256, setup_receiver, setup_sender,
};
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use zeroize::Zeroizing;

use crate::{CAPSULE_BYTES, Error, Result, Secret32, Word, validate_receiving_public};

type K = X25519HkdfSha256;
const SESSION_HPKE_INFO: &[u8] = b"vibeface-delivery-session-hpke-info-v1";

fn nonzero(word: &Word, error: &'static str) -> Result<()> {
    if word == &[0; 32] {
        Err(Error(error))
    } else {
        Ok(())
    }
}

pub fn seal_session_key(
    recipient: &Word,
    context: &Word,
    key: &Secret32,
    hpke_seed: &Secret32,
) -> Result<[u8; CAPSULE_BYTES]> {
    validate_receiving_public(recipient)?;
    nonzero(context, "zero session context")?;
    nonzero(key.as_bytes(), "zero session key")?;
    nonzero(hpke_seed.as_bytes(), "zero HPKE seed")?;

    let recipient = <K as Kem>::PublicKey::from_bytes(recipient)
        .map_err(|_| Error("session receiving public key"))?;
    let mut rng = ChaCha20Rng::from_seed(*hpke_seed.as_bytes());
    let (enc, mut sender) = setup_sender::<HpkeAead, HkdfSha256, K, _>(
        &OpModeS::Base,
        &recipient,
        SESSION_HPKE_INFO,
        &mut rng,
    )
    .map_err(|_| Error("session HPKE sender setup"))?;
    let sealed = sender
        .seal(key.as_bytes(), context)
        .map_err(|_| Error("session HPKE seal"))?;
    let enc_bytes: Word = enc.to_bytes().into();
    validate_receiving_public(&enc_bytes)?;
    if enc_bytes.len() + sealed.len() != CAPSULE_BYTES {
        return Err(Error("session HPKE capsule exact length"));
    }
    let mut capsule = [0; CAPSULE_BYTES];
    capsule[..enc_bytes.len()].copy_from_slice(&enc_bytes);
    capsule[enc_bytes.len()..].copy_from_slice(&sealed);
    Ok(capsule)
}

pub fn open_session_key(
    secret: &Secret32,
    context: &Word,
    capsule: &[u8; CAPSULE_BYTES],
) -> Result<Secret32> {
    nonzero(secret.as_bytes(), "zero receiving secret")?;
    nonzero(context, "zero session context")?;
    let key = <K as Kem>::PrivateKey::from_bytes(secret.as_bytes())
        .map_err(|_| Error("session receiving secret"))?;
    let enc_bytes: Word = capsule[..32]
        .try_into()
        .map_err(|_| Error("session HPKE encapsulated key length"))?;
    validate_receiving_public(&enc_bytes)?;
    let enc = <K as Kem>::EncappedKey::from_bytes(&enc_bytes)
        .map_err(|_| Error("session HPKE encapsulated key"))?;
    let mut receiver =
        setup_receiver::<HpkeAead, HkdfSha256, K>(&OpModeR::Base, &key, &enc, SESSION_HPKE_INFO)
            .map_err(|_| Error("session HPKE receiver setup"))?;
    let opened = Zeroizing::new(
        receiver
            .open(&capsule[32..], context)
            .map_err(|_| Error("session HPKE authentication"))?,
    );
    if opened.len() != 32 {
        return Err(Error("session HPKE key length"));
    }
    let key_bytes = Zeroizing::new(
        <Word>::try_from(&opened[..]).map_err(|_| Error("session HPKE key length"))?,
    );
    Secret32::new(*key_bytes)
}

#[cfg(test)]
mod tests {
    use super::{open_session_key, seal_session_key};
    use crate::{Secret32, receiving_public};

    #[test]
    fn session_key_capsule_round_trip_binds_context() {
        let secret = Secret32::new([7; 32]).unwrap();
        let recipient = receiving_public(&secret).unwrap();
        let context = [9; 32];
        let key = Secret32::new([11; 32]).unwrap();
        let seed = Secret32::new([13; 32]).unwrap();

        let capsule = seal_session_key(&recipient, &context, &key, &seed).unwrap();
        assert_eq!(
            open_session_key(&secret, &context, &capsule)
                .unwrap()
                .as_bytes(),
            key.as_bytes()
        );
        assert!(open_session_key(&secret, &[10; 32], &capsule).is_err());

        let mut tampered = capsule;
        tampered[79] ^= 1;
        assert!(open_session_key(&secret, &context, &tampered).is_err());
    }
}
