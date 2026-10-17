use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::BTreeSet,
    marker::PhantomData,
    rc::Rc,
    sync::{Arc, Mutex},
};

pub const SIGNATURE_BUDGET_ERROR: &str = "signature verification budget exceeded";

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SignatureVerificationCounts {
    pub actual_verifications: usize,
    pub cache_hits: usize,
}

#[derive(Default)]
struct State {
    successes: BTreeSet<[u8; 32]>,
    counts: SignatureVerificationCounts,
}

#[derive(Clone, Default)]
pub struct SignatureVerification(Arc<Mutex<State>>);

thread_local! {
    static ACTIVE: RefCell<Option<SignatureVerification>> = const { RefCell::new(None) };
}

impl SignatureVerification {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enter(&self) -> ScopedSignatureVerification {
        let previous = ACTIVE.with(|active| active.replace(Some(self.clone())));
        ScopedSignatureVerification {
            previous,
            handle: self.clone(),
            _not_send: PhantomData,
        }
    }

    pub fn run<T>(&self, action: impl FnOnce() -> T) -> T {
        let _scope = self.enter();
        action()
    }

    pub fn counts(&self) -> SignatureVerificationCounts {
        self.0
            .lock()
            .expect("signature verification state poisoned")
            .counts
            .clone()
    }

    pub fn current_counts() -> Option<SignatureVerificationCounts> {
        ACTIVE.with(|active| active.borrow().as_ref().map(Self::counts))
    }
}

pub struct ScopedSignatureVerification {
    previous: Option<SignatureVerification>,
    handle: SignatureVerification,
    _not_send: PhantomData<Rc<()>>,
}

impl Default for ScopedSignatureVerification {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopedSignatureVerification {
    pub fn new() -> Self {
        let handle = ACTIVE
            .with(|active| active.borrow().clone())
            .unwrap_or_default();
        handle.enter()
    }
    pub fn counts(&self) -> SignatureVerificationCounts {
        self.handle.counts()
    }
}

impl Drop for ScopedSignatureVerification {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            active.replace(self.previous.take());
        });
    }
}

pub(crate) fn verify_cached(
    public_key: &[u8],
    domain: &str,
    signing_bytes: &[u8],
    signature: &[u8],
    verify: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let handle = ACTIVE.with(|active| active.borrow().clone());
    let Some(handle) = handle else {
        return verify();
    };
    let mut hash = Sha256::new();
    for part in [
        public_key,
        domain.as_bytes(),
        Sha256::digest(signing_bytes).as_slice(),
        signature,
    ] {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    let key: [u8; 32] = hash.finalize().into();
    let mut state = handle
        .0
        .lock()
        .map_err(|_| "signature verification state poisoned")?;
    if state.successes.contains(&key) {
        state.counts.cache_hits += 1;
        return Ok(());
    }
    if state.counts.actual_verifications >= crate::authorization::MAX_PROOF_SIGNATURES {
        return Err(SIGNATURE_BUDGET_ERROR.to_owned());
    }
    state.counts.actual_verifications += 1;
    verify()?;
    state.successes.insert(key);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;

    #[test]
    fn successful_crypto_is_reused_across_synchronous_segments_and_request_33_is_rejected() {
        let identity = Identity::generate().unwrap();
        let key = identity.public_key_hex();
        let scope = SignatureVerification::new();
        for index in 0..32u8 {
            let payload = [index];
            let signature = identity.sign("scope-test", &payload).unwrap();
            scope
                .run(|| crate::verify(&key, "scope-test", &payload, &signature))
                .unwrap();
            scope
                .run(|| crate::verify(&key, "scope-test", &payload, &signature))
                .unwrap();
        }
        let signature = identity.sign("scope-test", &[32]).unwrap();
        assert_eq!(
            scope
                .run(|| crate::verify(&key, "scope-test", &[32], &signature))
                .unwrap_err(),
            SIGNATURE_BUDGET_ERROR
        );
        assert_eq!(scope.counts().actual_verifications, 32);
        assert_eq!(scope.counts().cache_hits, 32);
        assert!(SignatureVerification::current_counts().is_none());
        SignatureVerification::new()
            .run(|| crate::verify(&key, "scope-test", &[32], &signature))
            .unwrap();
    }

    #[test]
    fn invalid_signatures_are_never_cached_and_domains_and_messages_are_distinct() {
        let identity = Identity::generate().unwrap();
        let key = identity.public_key_hex();
        let signature = identity.sign("scope-test", b"original").unwrap();
        let scope = SignatureVerification::new();
        scope.run(|| {
            crate::verify(&key, "scope-test", b"original", &signature).unwrap();
            assert!(crate::verify(&key, "other-domain", b"original", &signature).is_err());
            assert!(crate::verify(&key, "scope-test", b"altered", &signature).is_err());
            assert!(crate::verify(&key, "scope-test", b"altered", &signature).is_err());
            let nested = ScopedSignatureVerification::new();
            crate::verify(&key, "scope-test", b"original", &signature).unwrap();
            assert_eq!(nested.counts().actual_verifications, 4);
        });
        assert_eq!(scope.counts().actual_verifications, 4);
        assert_eq!(scope.counts().cache_hits, 1);
    }
}
