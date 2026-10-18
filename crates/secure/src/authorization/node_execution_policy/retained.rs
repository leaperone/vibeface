//! Original SDK read sources for restoring accepted artifacts and policy floors.

use super::{
    provenance::{SdkPolicySourceProvenance, restore_sdk_source_provenance},
    *,
};

pub fn node_task_policy_source_from_retained(
    primary: &VerifiedPolicySet,
    primary_pins: &[PolicyPin],
    trust: &VerifiedTaskExecutionTrust,
    raw: &SdkPolicySourceProvenance,
    original_request: &NodeExecutionPolicyReadRequest,
    original_clock: u64,
) -> Result<VerifiedTaskPolicySource> {
    let _signatures = ScopedSignatureVerification::new();
    validate_pins(primary_pins)?;
    let restored_trust = verify_task_execution_trust(
        trust.original_intent(),
        trust.approval(),
        original_request.issuer(),
    )?;
    let typed = TaskExecutionPolicyRequest::Node(original_request.clone());
    check_task_read_scope(&restored_trust, &typed)?;
    require(
        original_request.issued_at_ms() >= restored_trust.approval().payload.approved_at_ms,
        "original SDK Node read predates its Task trust approval",
    )?;
    let source = restore_sdk_source_provenance(primary, raw, original_request, original_clock)?;
    let context = original_request.context()?;
    service::source_project(
        source.state(),
        &source.facts().resource,
        &source.facts().audience,
        original_request.issuer(),
    )?;
    match original_request {
        NodeExecutionPolicyReadRequest::Current(_) => {
            service::check_sdk_read_context(
                source.facts(),
                &context,
                original_request.issuer(),
                &original_request.selection()?,
            )?;
            service::check_binding(
                &restored_trust,
                &context,
                &original_request.work(),
                raw.current_binding.as_ref(),
                original_request.issuer(),
                source.facts().action,
            )?;
        }
        NodeExecutionPolicyReadRequest::History(value) => {
            let reference = &value.payload.reference;
            require(
                reference.source.issuer == *original_request.issuer()
                    && reference.old_submission.issuer == *original_request.issuer()
                    && reference.source.payload.project == source.facts().resource,
                "original SDK History source changed its signed Project artifact",
            )?;
        }
    }
    let original_pins: Vec<_> = source
        .heads()
        .iter()
        .map(|head| {
            let independent = primary_pins
                .iter()
                .find(|pin| pin.domain == head.domain)
                .ok_or_else(|| AuthorizationError::NeedPolicySync(vec![head.clone()]))?;
            Ok(PolicyPin {
                domain: independent.domain.clone(),
                genesis_root: independent.genesis_root.clone(),
                minimum_head: Some(head.clone()),
            })
        })
        .collect::<Result<_>>()?;
    let authority = service::VerifiedNodeTaskPolicyAuthority::from_sdk_provenance(&source);
    let snapshot =
        policy_import::native_node_execution_source_snapshot(&authority, &original_pins)?;
    let mut usage = source.proof_usage().clone();
    usage.record_signed(restored_trust.original_intent())?;
    usage.record_signed(restored_trust.approval())?;
    if let Some(binding) = restored_trust.original_binding() {
        usage.record_binding(binding)?;
    }
    if let Some(counts) = crate::signature_verification::SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(VerifiedTaskPolicySource {
        snapshot,
        primary_pins: original_pins,
        source_scope: Box::new(TaskPolicySourceScope::Node(
            service::NodeTaskPolicySourceScope::from_retained_sdk(&source, raw, original_request)?,
        )),
        verified_at_ms: original_clock,
        expires_at_ms: original_request.expires_at_ms().min(source.expires_at_ms()),
        usage,
        retained: true,
    })
}
