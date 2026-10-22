//! `vibeface-secure` for React Native through uniffi. The surface mirrors the WASM build so
//! `@vibeface/secure-native` can satisfy the same `SecureApi` contract as the browser.

use std::sync::{Arc, Mutex, MutexGuard};

use vibeface_secure as secure;

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SecureError {
    #[error("{reason}")]
    Failed { reason: String },
}

impl From<secure::Error> for SecureError {
    fn from(error: secure::Error) -> Self {
        Self::Failed {
            reason: error.to_string(),
        }
    }
}

type Result<T> = std::result::Result<T, SecureError>;

fn lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    state.lock().expect("secure state poisoned")
}

#[derive(uniffi::Object)]
pub struct Identity(secure::Identity);

#[uniffi::export]
impl Identity {
    #[uniffi::constructor]
    pub fn generate() -> Result<Arc<Self>> {
        Ok(Arc::new(Self(secure::Identity::generate()?)))
    }

    #[uniffi::constructor]
    pub fn from_secret_hex(secret: String) -> Result<Arc<Self>> {
        Ok(Arc::new(Self(secure::Identity::from_secret_hex(&secret)?)))
    }

    pub fn secret_hex(&self) -> String {
        self.0.secret_hex()
    }

    pub fn device_id(&self) -> String {
        self.0.device_id()
    }

    pub fn sign(&self, domain: String, payload: Vec<u8>) -> Result<String> {
        Ok(self.0.sign(&domain, &payload)?)
    }

    /// Signs raw bytes, e.g. a Noise handshake's `signing_bytes`.
    pub fn sign_message(&self, message: Vec<u8>) -> String {
        self.0.sign_message(&message)
    }
}

#[derive(uniffi::Object)]
pub struct Handshake(Mutex<secure::Handshake>);

#[uniffi::export]
impl Handshake {
    #[uniffi::constructor]
    pub fn new(
        initiator: bool,
        expected_device_id: Option<String>,
        context: Vec<u8>,
    ) -> Result<Arc<Self>> {
        let inner = secure::Handshake::new(initiator, expected_device_id, &context)?;
        Ok(Arc::new(Self(Mutex::new(inner))))
    }

    pub fn signing_bytes(&self) -> Vec<u8> {
        lock(&self.0).signing_bytes()
    }

    pub fn set_proof(&self, device_id: String, signature: String) -> Result<()> {
        Ok(lock(&self.0).set_proof(&device_id, &signature)?)
    }

    pub fn write(&self, payload: Vec<u8>) -> Result<Vec<u8>> {
        Ok(lock(&self.0).write(&payload)?)
    }

    pub fn read(&self, message: Vec<u8>) -> Result<Vec<u8>> {
        Ok(lock(&self.0).read(&message)?)
    }

    pub fn remote_id(&self) -> Option<String> {
        lock(&self.0).remote_id()
    }

    pub fn is_finished(&self) -> bool {
        lock(&self.0).is_finished()
    }

    pub fn finish(&self) -> Result<Arc<Channel>> {
        let channel = lock(&self.0).finish()?;
        Ok(Arc::new(Channel(Mutex::new(channel))))
    }
}

#[derive(uniffi::Object)]
pub struct Channel(Mutex<secure::Channel>);

#[uniffi::export]
impl Channel {
    pub fn seal_message(&self, payload: Vec<u8>) -> Result<Vec<Vec<u8>>> {
        Ok(lock(&self.0).seal_message(&payload)?)
    }

    pub fn open_record(&self, record: Vec<u8>) -> Result<Option<Vec<u8>>> {
        Ok(lock(&self.0).open_record(&record)?)
    }
}

#[derive(uniffi::Object)]
pub struct Membership(secure::membership::VerifiedMembership);

#[uniffi::export]
impl Membership {
    pub fn view_json(&self) -> Result<String> {
        Ok(self.0.view_json()?)
    }

    pub fn state_json(&self) -> Result<String> {
        Ok(self.0.state_json()?)
    }

    pub fn governance_template(&self, issuer: String, action_json: String) -> Result<String> {
        Ok(self.0.governance_template(&issuer, &action_json)?)
    }

    pub fn create_recovery_checkpoint(
        &self,
        root: Arc<Identity>,
        members_json: String,
        heads_json: String,
        backup_epoch: f64,
    ) -> Result<String> {
        Ok(self
            .0
            .create_recovery_checkpoint(&root.0, &members_json, &heads_json, backup_epoch)?)
    }
}

#[uniffi::export]
pub fn verify_membership(state_json: String, expected_network: String) -> Result<Arc<Membership>> {
    let membership = secure::membership::verify_membership(&state_json, &expected_network)?;
    Ok(Arc::new(Membership(membership)))
}

#[uniffi::export]
pub fn attestation_payload(
    network_id: String,
    scope_id: String,
    body_json: String,
) -> Result<Vec<u8>> {
    Ok(secure::join::attestation_payload(
        &network_id,
        &scope_id,
        &body_json,
    )?)
}

#[uniffi::export]
pub fn create_genesis(
    root: Arc<Identity>,
    first_client_json: String,
    first_node_json: String,
) -> Result<String> {
    Ok(secure::membership::create_genesis(
        &root.0,
        &first_client_json,
        &first_node_json,
    )?)
}

#[uniffi::export]
pub fn governance_payload(event_json: String) -> Result<Vec<u8>> {
    Ok(secure::membership::governance_payload(&event_json)?)
}

#[uniffi::export]
pub fn invite_signing_payload(record_json: String) -> Result<Vec<u8>> {
    Ok(secure::join::invite_signing_payload(&record_json)?)
}

#[uniffi::export]
pub fn join_commit(joiner: String, joiner_nonce: String) -> Result<String> {
    Ok(secure::join::join_commit(&joiner, &joiner_nonce)?)
}

#[uniffi::export]
pub fn recovery_open(envelope: Vec<u8>, password: String) -> Result<Vec<u8>> {
    Ok(secure::recovery_open(&envelope, &password)?)
}

#[uniffi::export]
pub fn recovery_seal(payload: Vec<u8>, password: String) -> Result<Vec<u8>> {
    Ok(secure::recovery_seal(&payload, &password)?)
}

#[uniffi::export]
pub fn short_code(
    network_id: String,
    invite_id: String,
    joiner: String,
    admin: String,
    admin_nonce: String,
    joiner_nonce: String,
) -> Result<String> {
    Ok(secure::join::short_code(
        &network_id,
        &invite_id,
        &joiner,
        &admin,
        &admin_nonce,
        &joiner_nonce,
    )?)
}

#[uniffi::export]
pub fn signing_message(domain: String, payload: Vec<u8>) -> Result<Vec<u8>> {
    Ok(secure::signing_message(&domain, &payload)?)
}

#[uniffi::export]
pub fn ticket_decode(text: String) -> Result<String> {
    Ok(secure::join::ticket_decode(&text)?)
}

#[uniffi::export]
pub fn ticket_encode(
    entry_id: String,
    secret_hex: String,
    relay_url: Option<String>,
    direct_addrs: Vec<String>,
) -> Result<String> {
    Ok(secure::join::ticket_encode(
        &entry_id,
        &secret_hex,
        relay_url,
        direct_addrs,
    )?)
}

#[uniffi::export]
pub fn verify(
    device_id: String,
    domain: String,
    payload: Vec<u8>,
    signature: String,
) -> Result<()> {
    Ok(secure::verify(&device_id, &domain, &payload, &signature)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTEXT: &[u8] = b"vibeface:client:v1";

    fn reason(error: SecureError) -> String {
        let SecureError::Failed { reason } = error;
        reason
    }

    #[test]
    fn noise_handshake_and_messages_round_trip_through_the_wrapper() {
        let phone = Identity::generate().unwrap();
        let node = secure::Identity::generate().unwrap();
        let client = Handshake::new(true, Some(node.device_id()), CONTEXT.to_vec()).unwrap();
        client
            .set_proof(
                phone.device_id(),
                phone.sign_message(client.signing_bytes()),
            )
            .unwrap();
        let mut server = secure::Handshake::signed(false, &node, None, CONTEXT).unwrap();

        server.read(&client.write(Vec::new()).unwrap()).unwrap();
        assert!(client.read(server.write(&[]).unwrap()).unwrap().is_empty());
        assert!(!client.is_finished());
        server.read(&client.write(Vec::new()).unwrap()).unwrap();
        assert!(client.is_finished());
        assert_eq!(client.remote_id(), Some(node.device_id()));
        assert_eq!(server.remote_id(), Some(phone.device_id()));

        let phone_channel = client.finish().unwrap();
        let mut node_channel = server.finish().unwrap();
        let large = vec![7u8; 150 * 1024];
        let records = phone_channel.seal_message(large.clone()).unwrap();
        assert!(records.len() > 1);
        let (last, head) = records.split_last().unwrap();
        for record in head {
            assert_eq!(node_channel.open_record(record).unwrap(), None);
        }
        assert_eq!(node_channel.open_record(last).unwrap(), Some(large));

        for record in node_channel.seal_message(b"reply").unwrap() {
            assert_eq!(
                phone_channel.open_record(record).unwrap(),
                Some(b"reply".to_vec())
            );
        }
        assert_eq!(
            reason(phone_channel.open_record(vec![0; 8]).unwrap_err()),
            "invalid Noise ciphertext size"
        );
    }

    #[test]
    fn a_wrong_proof_surfaces_the_raw_rust_message() {
        let phone = Identity::generate().unwrap();
        let other = Identity::generate().unwrap();
        let client = Handshake::new(true, None, CONTEXT.to_vec()).unwrap();
        let error = client
            .set_proof(
                phone.device_id(),
                other.sign_message(client.signing_bytes()),
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            secure::verify_message(
                &phone.device_id(),
                &client.signing_bytes(),
                &other.sign_message(client.signing_bytes())
            )
            .unwrap_err()
            .to_string()
        );
    }

    #[test]
    fn byte_builders_match_the_core_crate() {
        let device = secure::Identity::generate().unwrap().device_id();
        let body = r#"{"type":"note","b":1,"a":[true,"x"]}"#;
        let payload = attestation_payload(device.clone(), "scope".into(), body.into()).unwrap();
        assert_eq!(
            payload,
            secure::join::attestation_payload(&device, "scope", body).unwrap()
        );
        assert!(payload.starts_with(br#"{"body":{"a":[true,"x"],"b":1,"type":"note"}"#));
        assert_eq!(
            signing_message("attestation".into(), b"payload".to_vec()).unwrap(),
            secure::signing_message("attestation", b"payload").unwrap()
        );
        assert_eq!(
            reason(signing_message("bad domain".into(), Vec::new()).unwrap_err()),
            "invalid signing domain"
        );
    }

    #[test]
    fn identities_restore_and_sign_like_the_core_crate() {
        let identity = Identity::generate().unwrap();
        let restored = Identity::from_secret_hex(identity.secret_hex()).unwrap();
        assert_eq!(restored.device_id(), identity.device_id());
        let signature = restored
            .sign("attestation".into(), b"hello".to_vec())
            .unwrap();
        verify(
            identity.device_id(),
            "attestation".into(),
            b"hello".to_vec(),
            signature,
        )
        .unwrap();
    }
}
