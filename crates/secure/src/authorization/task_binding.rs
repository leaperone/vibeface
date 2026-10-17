use super::{MAX_PROOF_BYTES, SpaceId};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const BINDING_DOMAIN: &str = "task-binding-v2";
pub const SEAL_DOMAIN: &str = "binding-seal-v2";
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
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(binding)?)))
}

/// Verifies the node signature; caller authorization is checked separately.
pub fn verify_binding(proof: &SignedBinding, authority_public_key: &str) -> Result<()> {
    validate_binding(&proof.binding)?;
    validate_signature(&proof.signature)?;
    ensure!(
        fingerprint(authority_public_key)? == proof.binding.authority_node,
        "binding authority fingerprint mismatch"
    );
    crate::verify(
        authority_public_key,
        BINDING_DOMAIN,
        &serde_json::to_vec(&proof.binding)?,
        &proof.signature,
    )
    .map_err(anyhow::Error::msg)
}

pub fn verify_seal(proof: &SignedSeal, target_public_key: &str) -> Result<()> {
    validate_seal(&proof.seal)?;
    validate_signature(&proof.signature)?;
    ensure!(
        fingerprint(target_public_key)? == proof.seal.target_node,
        "seal target fingerprint mismatch"
    );
    crate::verify(
        target_public_key,
        SEAL_DOMAIN,
        &serde_json::to_vec(&proof.seal)?,
        &proof.signature,
    )
    .map_err(anyhow::Error::msg)
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

fn fingerprint(public_key: &str) -> Result<String> {
    Ok(hex::encode(Sha256::digest(
        crate::decode32(public_key).map_err(anyhow::Error::msg)?,
    )))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskBindingExpectation {
    pub binding: TaskBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_target_public_key: Option<String>,
}

/// Expected intent and public keys must come from independently verified caller facts.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyTaskBinding))]
pub fn verify_task_binding(
    binding_json: &str,
    authority_public_key: &str,
    expected_json: &str,
) -> crate::Result<String> {
    (|| -> Result<String> {
        ensure!(
            binding_json.len() <= MAX_PROOF_BYTES && expected_json.len() <= MAX_PROOF_BYTES,
            "task binding proof too large"
        );
        let proof: SignedBinding = serde_json::from_str(binding_json)?;
        let expected: TaskBindingExpectation = serde_json::from_str(expected_json)?;
        ensure!(
            proof.binding == expected.binding,
            "task binding differs from expected intent"
        );
        verify_binding(&proof, authority_public_key)?;
        match (
            &proof.binding.previous_seal,
            &expected.previous_target_public_key,
        ) {
            (None, None) if proof.binding.generation == 1 => {}
            (Some(seal), Some(public_key)) if proof.binding.generation > 1 => {
                verify_seal(seal, public_key)?;
            }
            _ => anyhow::bail!("previous target evidence must match binding generation"),
        }
        Ok(serde_json::to_string(&proof)?)
    })()
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;
    use serde_json::json;

    fn authority() -> Identity {
        Identity::from_secret_hex(&"11".repeat(32)).unwrap()
    }

    fn target() -> Identity {
        Identity::from_secret_hex(&"22".repeat(32)).unwrap()
    }

    fn binding() -> TaskBinding {
        TaskBinding {
            network_id: format!("spc2_{}", "a".repeat(64)),
            conversation_id: "conversation".into(),
            task_ref: "c".repeat(64),
            task_id: "task-1".into(),
            authority_node: authority().node_id(),
            generation: 1,
            target_node: target().node_id(),
            session_id: "session-1".into(),
            open_spec: json!({"type":"open", "executor":"codex", "cwd":"/work"}),
            previous_binding_hash: None,
            previous_seal: None,
        }
    }

    fn sign(binding: TaskBinding) -> SignedBinding {
        SignedBinding {
            signature: authority()
                .sign(BINDING_DOMAIN, &serde_json::to_vec(&binding).unwrap())
                .unwrap(),
            binding,
        }
    }

    fn check(proof: &SignedBinding, expected: &TaskBindingExpectation) -> crate::Result<String> {
        verify_task_binding(
            &serde_json::to_string(proof).unwrap(),
            &authority().public_key_hex(),
            &serde_json::to_string(expected).unwrap(),
        )
    }

    #[test]
    fn signed_binding_must_match_independent_complete_intent() {
        let expected = TaskBindingExpectation {
            binding: binding(),
            previous_target_public_key: None,
        };
        let proof = sign(expected.binding.clone());
        assert_eq!(
            check(&proof, &expected).unwrap(),
            serde_json::to_string(&proof).unwrap()
        );
        for (field, value) in [
            ("space_id", json!(format!("spc2_{}", "b".repeat(64)))),
            ("conversation_id", json!("another-conversation")),
            ("task_ref", json!("d".repeat(64))),
            ("task_id", json!("another-task")),
            ("target_node", json!("e".repeat(64))),
            ("session_id", json!("another-session")),
            (
                "open_spec",
                json!({"type":"open", "executor":"codex", "cwd":"/expanded"}),
            ),
        ] {
            let mut actual = serde_json::to_value(&expected.binding).unwrap();
            actual[field] = value;
            let changed = sign(serde_json::from_value(actual).unwrap());
            assert_eq!(
                check(&changed, &expected).unwrap_err(),
                "task binding differs from expected intent"
            );
        }
        let mut unexpected_key = expected.clone();
        unexpected_key.previous_target_public_key = Some(target().public_key_hex());
        assert!(check(&proof, &unexpected_key).is_err());
    }

    #[test]
    fn binding_verification_rejects_legacy_and_invalid_proofs() {
        let expected = TaskBindingExpectation {
            binding: binding(),
            previous_target_public_key: None,
        };
        let proof = sign(expected.binding.clone());
        let mut legacy = proof.clone();
        legacy.signature = authority()
            .sign(
                "task-binding-v1",
                &serde_json::to_vec(&legacy.binding).unwrap(),
            )
            .unwrap();
        assert!(check(&legacy, &expected).is_err());
        assert!(
            verify_task_binding(
                &serde_json::to_string(&proof).unwrap(),
                &target().public_key_hex(),
                &serde_json::to_string(&expected).unwrap(),
            )
            .is_err()
        );
        for changed in [
            TaskBinding {
                network_id: "a".repeat(64),
                ..binding()
            },
            TaskBinding {
                target_node: format!("dev2_{}", "a".repeat(64)),
                ..binding()
            },
            TaskBinding {
                generation: 0,
                ..binding()
            },
            TaskBinding {
                session_id: String::new(),
                ..binding()
            },
            TaskBinding {
                open_spec: json!({"type":"open","executor":"codex","cwd":"/work","extra":true}),
                ..binding()
            },
        ] {
            let invalid_expected = TaskBindingExpectation {
                binding: changed.clone(),
                previous_target_public_key: None,
            };
            assert!(check(&sign(changed), &invalid_expected).is_err());
        }
        let mut legacy_wire = serde_json::to_value(&proof).unwrap();
        let space = legacy_wire["binding"]
            .as_object_mut()
            .unwrap()
            .remove("space_id")
            .unwrap();
        legacy_wire["binding"]["network_id"] = space;
        assert!(
            verify_task_binding(
                &legacy_wire.to_string(),
                &authority().public_key_hex(),
                &serde_json::to_string(&expected).unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn later_generations_require_the_prior_targets_authentic_seal() {
        let previous_hash = binding_hash(&binding()).unwrap();
        let seal = BindingSeal {
            network_id: binding().network_id,
            binding_hash: previous_hash.clone(),
            target_node: target().node_id(),
            final_revision: 7,
        };
        let signed_seal = SignedSeal {
            signature: target()
                .sign(SEAL_DOMAIN, &serde_json::to_vec(&seal).unwrap())
                .unwrap(),
            seal,
        };
        let expected = TaskBindingExpectation {
            binding: TaskBinding {
                generation: 2,
                target_node: authority().node_id(),
                session_id: "session-2".into(),
                previous_binding_hash: Some(previous_hash),
                previous_seal: Some(signed_seal),
                ..binding()
            },
            previous_target_public_key: Some(target().public_key_hex()),
        };
        assert!(check(&sign(expected.binding.clone()), &expected).is_ok());
        for key in [None, Some(authority().public_key_hex())] {
            let mut wrong_key = expected.clone();
            wrong_key.previous_target_public_key = key;
            assert!(check(&sign(wrong_key.binding.clone()), &wrong_key).is_err());
        }
        for legacy_domain in [false, true] {
            let mut changed = expected.clone();
            let seal = changed.binding.previous_seal.as_mut().unwrap();
            if legacy_domain {
                seal.signature = target()
                    .sign("binding-seal-v1", &serde_json::to_vec(&seal.seal).unwrap())
                    .unwrap();
            } else {
                seal.seal.final_revision += 1;
            }
            assert!(check(&sign(changed.binding.clone()), &changed).is_err());
        }
        let mut wrong_hash = expected.clone();
        wrong_hash.binding.previous_binding_hash = Some("f".repeat(64));
        assert!(check(&sign(wrong_hash.binding.clone()), &wrong_hash).is_err());
        let mut first = expected.clone();
        first.binding.generation = 1;
        assert!(check(&sign(first.binding.clone()), &first).is_err());
    }
}
