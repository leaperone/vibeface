#![no_std]

extern crate alloc;

mod envelope;
mod hpke;
mod settlement;

pub use envelope::{
    EncryptedSessionObject, MAX_SESSION_CHUNK_BYTES, MAX_SESSION_ID_BYTES,
    MAX_SESSION_OBJECT_BYTES, SESSION_AEAD_TAG_BYTES, SESSION_OBJECT_VERSION, SessionObjectChunk,
    SessionObjectHeader, SessionObjectKind, decrypt_session_payload, encrypt_session_payload,
};
pub use hpke::{open_session_key, seal_session_key};
pub use settlement::{
    MAX_SESSION_SETTLEMENT_ORDERS, SESSION_SETTLEMENT_RECEIPT_DOMAIN, SessionSettlementBatch,
    SessionSettlementClaim, SessionSettlementOrder, verify_session_settlement,
};
pub use vibeface_delivery::{
    Amount, CAPSULE_BYTES, Error, PeerSignature, Result, Secret32, Word, hash, receiving_public,
    signing_message, validate_receiving_public, verify_peer,
};
