use crate::crypto::{LocalIdentity, digest, fingerprint, verify_payload};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vibeface_secure::authorization::SpaceId;

const BINDING_DOMAIN: &str = "task-binding-v2";
const SEAL_DOMAIN: &str = "binding-seal-v2";
const MAX_ID_TEXT: usize = 512;
const MAX_EXECUTOR: usize = 128;
const MAX_CWD: usize = 4_096;
const SIGNATURE_HEX_LEN: usize = 128;

/// The executor and generation pinned by a task's authority node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskBinding {
    #[serde(rename = "space_id")]
    pub network_id: String,
    pub conversation_id: String,
    pub task_ref: String,
    pub task_id: String,
    pub authority_node: String,
    pub generation: u64,
    pub target_node: String,
    pub session_id: String,
    pub open_spec: Value,
    pub previous_binding_hash: Option<String>,
    pub previous_seal: Option<SignedSeal>,
}

/// A TaskBinding signed by its fixed authority node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SignedBinding {
    pub binding: TaskBinding,
    pub signature: String,
}

/// The target executor's durable seal for a completed binding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BindingSeal {
    #[serde(rename = "space_id")]
    pub network_id: String,
    pub binding_hash: String,
    pub target_node: String,
    pub final_revision: u64,
}

/// A BindingSeal signed by the executor that owned the binding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SignedSeal {
    pub seal: BindingSeal,
    pub signature: String,
}

/// Validate the shape that is safe to sign or accept before store-level checks.
pub fn validate_binding(binding: &TaskBinding) -> Result<()> {
    SpaceId::parse(&binding.network_id)?;
    validate_text(&binding.conversation_id, "conversation_id")?;
    validate_hash(&binding.task_ref, "task_ref")?;
    validate_text(&binding.task_id, "task_id")?;
    validate_node_id(&binding.authority_node, "authority_node")?;
    ensure!(
        binding.generation > 0,
        "binding generation must be positive"
    );
    validate_node_id(&binding.target_node, "target_node")?;
    validate_text(&binding.session_id, "session_id")?;
    validate_open_spec(&binding.open_spec)?;

    match (&binding.previous_binding_hash, &binding.previous_seal) {
        (None, None) if binding.generation == 1 => {}
        (Some(previous_hash), Some(previous_seal)) if binding.generation > 1 => {
            validate_hash(previous_hash, "previous_binding_hash")?;
            validate_seal(&previous_seal.seal)?;
            ensure!(
                previous_seal.seal.network_id == binding.network_id,
                "previous seal belongs to another space"
            );
            ensure!(
                previous_seal.seal.binding_hash == *previous_hash,
                "previous seal does not match previous binding"
            );
            validate_signature(&previous_seal.signature)?;
        }
        _ => ensure!(false, "previous binding and seal must match generation"),
    }
    Ok(())
}

/// Hash the canonical unsigned binding payload.
pub fn binding_hash(binding: &TaskBinding) -> Result<String> {
    validate_binding(binding)?;
    Ok(digest(&serde_json::to_vec(binding)?))
}

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

/// Verifies the node signature; caller authorization is checked separately.
pub fn verify_binding(proof: &SignedBinding, authority_public_key: &str) -> Result<()> {
    validate_binding(&proof.binding)?;
    validate_signature(&proof.signature)?;
    ensure!(
        fingerprint(authority_public_key)? == proof.binding.authority_node,
        "binding authority fingerprint mismatch"
    );
    verify_payload(
        authority_public_key,
        BINDING_DOMAIN,
        &serde_json::to_vec(&proof.binding)?,
        &proof.signature,
    )
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

pub fn verify_seal(proof: &SignedSeal, target_public_key: &str) -> Result<()> {
    validate_seal(&proof.seal)?;
    validate_signature(&proof.signature)?;
    ensure!(
        fingerprint(target_public_key)? == proof.seal.target_node,
        "seal target fingerprint mismatch"
    );
    verify_payload(
        target_public_key,
        SEAL_DOMAIN,
        &serde_json::to_vec(&proof.seal)?,
        &proof.signature,
    )
}

pub fn validate_seal(seal: &BindingSeal) -> Result<()> {
    SpaceId::parse(&seal.network_id)?;
    validate_hash(&seal.binding_hash, "binding_hash")?;
    validate_node_id(&seal.target_node, "target_node")?;
    Ok(())
}

fn validate_open_spec(value: &Value) -> Result<()> {
    let Value::Object(object) = value else {
        anyhow::bail!("open_spec must be an object");
    };
    ensure!(object.len() == 3, "open_spec contains unknown fields");
    let Some(Value::String(kind)) = object.get("type") else {
        anyhow::bail!("open_spec.type must be a string");
    };
    ensure!(kind == "open", "open_spec.type must be open");
    let Some(Value::String(executor)) = object.get("executor") else {
        anyhow::bail!("open_spec.executor must be a string");
    };
    ensure!(
        !executor.is_empty() && executor.len() <= MAX_EXECUTOR,
        "invalid open_spec.executor"
    );
    let Some(Value::String(cwd)) = object.get("cwd") else {
        anyhow::bail!("open_spec.cwd must be a string");
    };
    ensure!(
        !cwd.is_empty() && cwd.len() <= MAX_CWD,
        "invalid open_spec.cwd"
    );
    Ok(())
}

fn validate_text(value: &str, field: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_ID_TEXT,
        "invalid {field}"
    );
    Ok(())
}

fn validate_node_id(value: &str, field: &str) -> Result<()> {
    ensure!(
        is_lower_hex_64(value),
        "{field} must be a 64-character hexadecimal id"
    );
    Ok(())
}

fn validate_hash(value: &str, field: &str) -> Result<()> {
    ensure!(
        is_lower_hex_64(value),
        "{field} must be a 64-character hexadecimal hash"
    );
    Ok(())
}

fn validate_signature(value: &str) -> Result<()> {
    ensure!(
        value.len() == SIGNATURE_HEX_LEN && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "signature must be a 64-byte hexadecimal signature"
    );
    Ok(())
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
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
