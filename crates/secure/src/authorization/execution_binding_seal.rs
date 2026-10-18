use super::{execution_handoff::*, *};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct ExecutionBindingSealTarget {
    pub binding: task_binding::SignedBinding,
    pub receipt: Signed<ExecutionHandoffReceipt>,
    pub session: ExecutionSessionSnapshot,
    pub target_public_key: PublicKey,
    pub usage: ExecutionProofUsage,
}

pub fn execution_binding_seal_facts(
    body: &Value,
    audience: &Audience,
    target_key: &PublicKey,
) -> Result<(RequestFacts, ExecutionBindingSealTarget)> {
    require(
        canonical(body)?.len() <= MAX_PROOF_BYTES,
        "remote seal carrier is too large",
    )?;
    let action = match body["method"].as_str() {
        Some("seal_remote_execution") => Action::ExecutionRebind,
        Some("remote_execution_seal_status") => Action::OperationStatusRead,
        _ => {
            return Err(AuthorizationError::InvalidProof(
                "invalid remote seal method",
            ));
        }
    };
    let receipt: Signed<ExecutionHandoffReceipt> =
        serde_json::from_value(body["execution_receipt"].clone())
            .map_err(|_| AuthorizationError::InvalidProof("invalid remote seal receipt"))?;
    receipt.verify_signature()?;
    let session = receipt
        .payload
        .session
        .clone()
        .ok_or(AuthorizationError::InvalidProof(
            "remote seal receipt has no accepted session",
        ))?;
    let binding: task_binding::SignedBinding = serde_json::from_value(body["binding"].clone())
        .map_err(|_| AuthorizationError::InvalidProof("invalid remote seal binding"))?;
    let value = &receipt.payload;
    let executor =
        binding.binding.open_spec["executor"]
            .as_str()
            .ok_or(AuthorizationError::InvalidProof(
                "remote seal binding has no executor",
            ))?;
    let binding_hash = task_binding::binding_hash(&binding.binding)
        .map_err(|_| AuthorizationError::InvalidProof("invalid remote seal binding"))?;
    require(
        receipt.issuer == *target_key
            && value.version == AUTH_VERSION
            && audience.device == DeviceId::from_key(target_key)
            && audience.service == ServiceId::from_key(target_key)
            && value.target == audience.device
            && value.target == session.target
            && value.original_space_id == session.original_project.space
            && value.original_space_id.as_str() == binding.binding.network_id
            && value.execution_space_id == session.execution_resource.space
            && value.work.execution_session().is_some_and(|v| {
                v.session_id == session.session_id && v.session_id == binding.binding.session_id
            })
            && value.work == session.work
            && session
                .work
                .task_ref()
                .is_some_and(|task| task.as_str() == binding.binding.task_ref)
            && session.work.binding_generation() == Some(binding.binding.generation)
            && session.work.binding_hash() == Some(binding_hash.as_str())
            && session.executor == executor
            && binding.binding.open_spec["cwd"].as_str() == Some(session.cwd.as_str())
            && node_fingerprint(target_key.as_str())
                .map_err(|_| AuthorizationError::InvalidProof("invalid executor pin"))?
                == binding.binding.target_node
            && session.expires_at_ms > value.issued_at_ms
            && session.expires_at_ms - value.issued_at_ms <= EXECUTION_LEASE_MS
            && body["space_id"].as_str() == Some(session.execution_resource.space.as_str())
            && body["resource_id"].as_str() == Some(session.execution_resource.resource.as_str())
            && body["original_space_id"].as_str() == Some(value.original_space_id.as_str())
            && body["operation_id"].as_str() == Some(value.operation_id.as_str()),
        "remote seal receipt differs from its pinned executor or old binding",
    )?;
    let authorization_id =
        body["authorization_id"]
            .as_str()
            .ok_or(AuthorizationError::InvalidProof(
                "remote seal authorization ID is missing",
            ))?;
    if action == Action::OperationStatusRead {
        ObjectHash::parse(body["seal_invocation_hash"].as_str().ok_or(
            AuthorizationError::InvalidProof("seal invocation hash is missing"),
        )?)?;
    }
    let context = InvocationContext {
        conversation: Some(binding.binding.conversation_id.clone()),
        task: session.work.task_ref().cloned(),
        sdk_session: None,
        execution_session: Some(SessionRef {
            target: session.target.clone(),
            session_id: session.session_id.clone(),
        }),
        executor_kind: Some(session.executor.clone()),
        workspace_resource: Some(session.execution_resource.resource.clone()),
    };
    let facts = RequestFacts {
        resource: session.execution_resource.clone(),
        action,
        audience: audience.clone(),
        operation_id: authorization_id.into(),
        body_hash: ObjectHash::body(body)?,
        context,
    };
    let mut usage = ExecutionProofUsage::default();
    usage.record_metadata(body)?;
    usage.record_signed(&receipt)?;
    usage.record_binding(&binding)?;
    usage.enforce_limits()?;
    Ok((
        facts,
        ExecutionBindingSealTarget {
            binding,
            receipt,
            session,
            target_public_key: target_key.clone(),
            usage,
        },
    ))
}

pub(crate) fn check_client_seal_intent(
    intent: &prepare::PrepareIntent,
    body: &Value,
    audience_key: &PublicKey,
) -> Result<bool> {
    if !matches!(
        body["method"].as_str(),
        Some("seal_remote_execution" | "remote_execution_seal_status")
    ) {
        return Ok(false);
    }
    let (facts, target) =
        execution_binding_seal_facts(body, &intent.request.audience, audience_key)?;
    require(
        matches!(intent.request.actor, Actor::Account(_))
            && intent.request.context.sdk_session.is_none()
            && intent.request.resource == facts.resource
            && intent.request.action == facts.action
            && intent.request.operation_id == facts.operation_id
            && intent.request.body_hash == facts.body_hash
            && intent.request.context == facts.context
            && intent.binding_generation == Some(target.binding.binding.generation)
            && intent.canonical_cwd.is_none()
            && intent.execution_target_public_key.as_ref() == Some(audience_key),
        "remote seal intent differs from its verified Account request",
    )?;
    Ok(true)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signedInvocationHash))]
pub fn signed_invocation_hash(invocation_json: &str) -> crate::Result<String> {
    let _signatures = crate::signature_verification::ScopedSignatureVerification::new();
    (|| -> Result<String> {
        require(
            invocation_json.len() <= MAX_PROOF_BYTES,
            "invocation is too large",
        )?;
        let invocation: Signed<Invocation> = serde_json::from_str(invocation_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid invocation JSON"))?;
        invocation.verify_signature()?;
        Ok(invocation.hash()?.to_string())
    })()
    .map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyRemoteExecutionSeal))]
pub fn verify_remote_execution_seal(
    binding_json: &str,
    seal_json: &str,
    expected_project_public_key: &str,
    expected_execution_public_key: &str,
    expected_revision_json: &str,
) -> crate::Result<String> {
    let _signatures = crate::signature_verification::ScopedSignatureVerification::new();
    (|| -> Result<String> {
        require(
            binding_json.len().saturating_add(seal_json.len()) <= MAX_PROOF_BYTES,
            "remote seal proof is too large",
        )?;
        let binding: task_binding::SignedBinding = serde_json::from_str(binding_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid old binding JSON"))?;
        let seal: task_binding::SignedSeal = serde_json::from_str(seal_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid executor seal JSON"))?;
        let project_key = PublicKey::parse(expected_project_public_key)?;
        let execution_key = PublicKey::parse(expected_execution_public_key)?;
        task_binding::verify_binding(&binding, project_key.as_str()).map_err(|_| {
            AuthorizationError::InvalidProof("old binding has another Project authority")
        })?;
        task_binding::verify_seal(&seal, execution_key.as_str())
            .map_err(|_| AuthorizationError::InvalidProof("seal has another executor authority"))?;
        require(expected_revision_json.len() <= 32, "invalid final revision")?;
        let revision: u64 = serde_json::from_str(expected_revision_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid final revision"))?;
        require(revision <= 9_007_199_254_740_991, "unsafe final revision")?;
        require(
            binding.binding.target_node
                == node_fingerprint(execution_key.as_str())
                    .map_err(|_| AuthorizationError::InvalidProof("invalid old executor pin"))?
                && seal.seal.network_id == binding.binding.network_id
                && seal.seal.target_node == binding.binding.target_node
                && seal.seal.binding_hash
                    == task_binding::binding_hash(&binding.binding)
                        .map_err(|_| AuthorizationError::InvalidProof("invalid old binding"))?
                && seal.seal.final_revision == revision,
            "executor seal changed the old binding or final revision",
        )?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_binding(&binding)?;
        usage.record_metadata(&seal)?;
        usage.enforce_limits()?;
        serde_json::to_string(&seal)
            .map_err(|_| AuthorizationError::InvalidProof("executor seal encoding failed"))
    })()
    .map_err(|error| error.to_string())
}
