use crate::crypto::LocalIdentity;
use anyhow::{Result, ensure};
use vibeface_secure::authorization::task_binding::{BINDING_DOMAIN, SEAL_DOMAIN};
pub use vibeface_secure::authorization::task_binding::{
    BindingSeal, SignedBinding, SignedSeal, TaskBinding, binding_hash, validate_binding,
    validate_seal, verify_binding, verify_seal,
};

pub fn sign_binding(identity: &LocalIdentity, binding: &TaskBinding) -> Result<SignedBinding> {
    validate_binding(binding)?;
    ensure!(
        identity.node_id()? == binding.authority_node,
        "binding authority does not match signing identity"
    );
    let payload = serde_json::to_vec(binding)?;
    Ok(SignedBinding {
        binding: binding.clone(),
        signature: identity.sign_payload(BINDING_DOMAIN, &payload)?,
    })
}

pub fn sign_seal(identity: &LocalIdentity, seal: &BindingSeal) -> Result<SignedSeal> {
    validate_seal(seal)?;
    ensure!(
        identity.node_id()? == seal.target_node,
        "seal target does not match signing identity"
    );
    let payload = serde_json::to_vec(seal)?;
    Ok(SignedSeal {
        seal: seal.clone(),
        signature: identity.sign_payload(SEAL_DOMAIN, &payload)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn binding(authority: &LocalIdentity) -> TaskBinding {
        let node = "b".repeat(64);
        TaskBinding {
            network_id: format!("spc2_{}", "a".repeat(64)),
            conversation_id: "conversation".into(),
            task_ref: "c".repeat(64),
            task_id: "task-1".into(),
            authority_node: authority.node_id().unwrap(),
            generation: 1,
            target_node: node,
            session_id: "session-1".into(),
            open_spec: json!({"type": "open", "executor": "codex", "cwd": "/tmp"}),
            previous_binding_hash: None,
            previous_seal: None,
        }
    }

    #[test]
    fn binding_tampering_is_rejected() -> Result<()> {
        let authority = LocalIdentity::generate();
        let mut proof = sign_binding(&authority, &binding(&authority))?;
        verify_binding(&proof, &authority.public_key_hex()?)?;
        let mut encoded = serde_json::to_value(&proof.binding)?;
        assert_eq!(encoded["space_id"], proof.binding.network_id);
        assert!(encoded.get("network_id").is_none());
        let space = encoded.as_object_mut().unwrap().remove("space_id").unwrap();
        encoded["network_id"] = space;
        assert!(serde_json::from_value::<TaskBinding>(encoded).is_err());
        let legacy = SignedBinding {
            binding: proof.binding.clone(),
            signature: authority
                .sign_payload("task-binding-v1", &serde_json::to_vec(&proof.binding)?)?,
        };
        assert!(verify_binding(&legacy, &authority.public_key_hex()?).is_err());
        let mut old_space = proof.binding.clone();
        old_space.network_id = "a".repeat(64);
        assert!(sign_binding(&authority, &old_space).is_err());

        proof.binding.target_node = "d".repeat(64);
        assert!(verify_binding(&proof, &authority.public_key_hex()?).is_err());

        proof.binding.target_node = "b".repeat(64);
        proof.binding.generation = 2;
        assert!(verify_binding(&proof, &authority.public_key_hex()?).is_err());
        Ok(())
    }

    #[test]
    fn seal_tampering_is_rejected() -> Result<()> {
        let target = LocalIdentity::generate();
        let seal = BindingSeal {
            network_id: format!("spc2_{}", "a".repeat(64)),
            binding_hash: "c".repeat(64),
            target_node: target.node_id()?,
            final_revision: 1,
        };
        let mut proof = sign_seal(&target, &seal)?;
        verify_seal(&proof, &target.public_key_hex()?)?;
        let mut encoded = serde_json::to_value(&seal)?;
        assert_eq!(encoded["space_id"], seal.network_id);
        assert!(encoded.get("network_id").is_none());
        let space = encoded.as_object_mut().unwrap().remove("space_id").unwrap();
        encoded["network_id"] = space;
        assert!(serde_json::from_value::<BindingSeal>(encoded).is_err());
        let legacy = SignedSeal {
            seal: seal.clone(),
            signature: target.sign_payload("binding-seal-v1", &serde_json::to_vec(&seal)?)?,
        };
        assert!(verify_seal(&legacy, &target.public_key_hex()?).is_err());
        proof.seal.final_revision = 2;
        assert!(verify_seal(&proof, &target.public_key_hex()?).is_err());
        Ok(())
    }
}
