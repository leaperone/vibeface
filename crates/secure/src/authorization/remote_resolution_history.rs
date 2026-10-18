use super::{execution_handoff::*, *};
use serde_json::Value;

fn parse<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T> {
    require(
        json.len() <= MAX_EXECUTION_HANDOFF_BYTES,
        "historical proof is too large",
    )?;
    serde_json::from_str(json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid historical proof"))
}

fn clock(now: f64) -> Result<u64> {
    bootstrap::timestamp(now)
}

fn load(
    reader: &VerifiedClientAuthorization,
    policy: &remote_execution_policy::VerifiedRemoteExecutionHistoryPolicy,
    actual_json: &str,
    body_json: &str,
    invocation_json: &str,
    binding_json: &str,
    now: u64,
) -> Result<(
    VerifiedPolicySet,
    remote_execution_history::VerifiedHistoryAcceptance,
    ExecutionProofUsage,
)> {
    let baseline = policy.revalidate(reader, now)?;
    require(
        baseline.actions.iter().any(|action| {
            matches!(
                action,
                Action::OperationStatusRead | Action::OperationReconcile | Action::OperationResolve
            )
        }),
        "history requires current Project read permission",
    )?;
    let reference = remote_execution_history::derive_reference(
        actual_json,
        body_json,
        invocation_json,
        binding_json,
    )?;
    require(
        &reference == policy.reference()?,
        "history changed its original complete signed artifact",
    )?;
    let source = client_policy_import::require_confirmed_client_policy(reader)?;
    let merged = policy_import::merge_same_head_snapshots(source, policy.snapshot())?;
    let state = merged.state().clone();
    let accepted = remote_execution_history::verify_remote_execution_history_acceptance(
        &state,
        &reference,
        policy.acceptance()?,
        &reference.target_public_key,
        now,
    )?;
    match policy.acceptance()? {
        remote_execution_history::RemoteExecutionHistoryAcceptance::UnknownResolution {
            receipt,
            freshness,
        } => {
            let actual: ExecutionHandoffResolutionResult = parse(actual_json)?;
            require(
                receipt == &actual.receipt && freshness == &actual.freshness,
                "history changed its original executor resolution proof",
            )?;
        }
        remote_execution_history::RemoteExecutionHistoryAcceptance::TerminalReconciliation {
            observation_receipt,
            freshness,
        } => {
            let actual: ExecutionHandoffTerminalReconciliationResult = parse(actual_json)?;
            require(
                observation_receipt == &actual.reconciliation.resolution.observation.receipt
                    && freshness == &actual.freshness,
                "history changed its original executor observation or reconciliation proof",
            )?;
        }
        remote_execution_history::RemoteExecutionHistoryAcceptance::ExecutionResult { .. } => {}
    }
    let mut usage = accepted.proof_usage().clone();
    usage.record_budget(&baseline.budget)?;
    policy.record_metadata(&mut usage)?;
    if let Some(counts) = crate::signature_verification::SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok((state, accepted, usage))
}

pub(super) fn at_heads(
    current: &VerifiedPolicySet,
    heads: &[PolicyStamp],
) -> Result<VerifiedPolicySet> {
    let mut historical = current.clone();
    let mut domains = BTreeSet::new();
    for head in heads {
        require(
            domains.insert(head.domain.clone()),
            "duplicate historical policy domain",
        )?;
        match &head.domain {
            PolicyDomain::Account(id) => {
                let state = historical
                    .accounts
                    .get_mut(id)
                    .ok_or_else(|| AuthorizationError::NeedPolicySync(vec![head.clone()]))?;
                let mut found = None;
                for (index, document) in state.history.iter().enumerate() {
                    if account_stamp(document)? == *head {
                        found = Some(index);
                        break;
                    }
                }
                let index =
                    found.ok_or_else(|| AuthorizationError::NeedPolicySync(vec![head.clone()]))?;
                state.history.truncate(index + 1);
                state.stamp = head.clone();
            }
            PolicyDomain::Space(id) => {
                let state = historical
                    .spaces
                    .get_mut(id)
                    .ok_or_else(|| AuthorizationError::NeedPolicySync(vec![head.clone()]))?;
                let mut found = None;
                for (index, document) in state.history.iter().enumerate() {
                    if space_stamp(document)? == *head {
                        found = Some(index);
                        break;
                    }
                }
                let index =
                    found.ok_or_else(|| AuthorizationError::NeedPolicySync(vec![head.clone()]))?;
                state.history.truncate(index + 1);
                state.stamp = head.clone();
            }
        }
    }
    Ok(historical)
}

pub(super) fn admitted(
    current: &VerifiedPolicySet,
    proof: &events::AdmissionProof,
    facts: &RequestFacts,
) -> Result<VerifiedAdmission> {
    let historical = at_heads(current, &proof.challenge.policies)?;
    let transport = proof.transport.clone().into_evidence();
    let request = match prepare::remote_selection(&proof.source_body)? {
        Some((resource, key, grant)) => {
            let destination = historical.verify_execution_destination(
                &facts.resource, &facts.audience, &resource, &key, &grant,
                &facts.context, facts.action, proof.admitted_at_ms,
            )?;
            historical.verify_invocation_with_execution_destination(
                &proof.invocation, facts, &transport, &destination, proof.admitted_at_ms,
            )?
        }
        None => historical.verify_invocation(
            &proof.invocation, facts, &transport, proof.admitted_at_ms,
        )?,
    };
    let fresh = historical.verify_freshness(
        &request,
        &proof.challenge,
        &proof.receipts,
        proof.admitted_at_ms,
    )?;
    historical.admit_operation(&request, &fresh, proof.admitted_at_ms)
}

fn reader_scope(reader: &VerifiedClientAuthorization, resource: &ResourceRef) -> Result<()> {
    require(
        resource.space == reader.scope.space_id && scope_covers(&reader.grant_scope, resource),
        "historical result belongs to another Project",
    )
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyRemoteResolutionHistory))]
pub fn verify_remote_resolution_history(
    reader: &VerifiedClientAuthorization,
    policy: &remote_execution_policy::VerifiedRemoteExecutionHistoryPolicy,
    actual_json: &str,
    body_json: &str,
    invocation_json: &str,
    now_ms: f64,
) -> crate::Result<String> {
    (|| -> Result<String> {
        let _signatures = crate::signature_verification::ScopedSignatureVerification::new();
        let now = clock(now_ms)?;
        let (state, historical_proof, mut usage) = load(
            reader,
            policy,
            actual_json,
            body_json,
            invocation_json,
            "",
            now,
        )?;
        let decision_time = historical_proof.decision_at_ms();
        let body: Value = parse(body_json)?;
        let invocation: Signed<Invocation> = parse(invocation_json)?;
        invocation.verify_signature()?;
        let raw: Value = parse(actual_json)?;
        let unknown = raw.get("resolution").is_some();
        let (wire, freshness, outcome) = if unknown {
            let actual: ExecutionHandoffResolutionResult = serde_json::from_value(raw.clone())
                .map_err(|_| AuthorizationError::InvalidProof("invalid historical resolution"))?;
            verify_execution_resolution_receipt_signature(&actual)?;
            (
                actual.resolution,
                actual.freshness,
                actual.handoff_receipt.payload.state,
            )
        } else {
            let actual: ExecutionHandoffTerminalReconciliationResult =
                serde_json::from_value(raw.clone()).map_err(|_| {
                    AuthorizationError::InvalidProof("invalid historical reconciliation")
                })?;
            let outcome = actual
                .reconciliation
                .resolution
                .observation
                .receipt
                .payload
                .state;
            (actual.reconciliation, actual.freshness, outcome)
        };
        let proof = &wire.resolution.account_admission;
        let facts = execution_resolution_account_facts(&wire.resolution)?;
        reader_scope(reader, &facts.resource)?;
        require(
            decision_time <= now && proof.source_body == body && proof.invocation == invocation,
            "historical resolution changed the original Account intent",
        )?;
        let accepted = admitted(&state, proof, &facts)?;
        let decision = at_heads(&state, &freshness.challenge.policies)?;
        let resolver = decision.verify_operation_authorization(
            &accepted,
            &facts,
            &proof.transport.clone().into_evidence(),
            decision_time,
        )?;
        usage.record_admission(proof)?;
        usage.record_authorization(&resolver)?;
        if unknown {
            let actual: ExecutionHandoffResolutionResult = serde_json::from_value(raw.clone())
                .map_err(|_| AuthorizationError::InvalidProof("invalid historical resolution"))?;
            let verified =
                decision.verify_execution_resolution_ack(&actual, &resolver, decision_time)?;
            decision.verify_execution_resolution_freshness(&verified, &freshness, decision_time)?;
            usage.merge(verified.proof_usage());
            usage.record_signed(&actual.receipt)?;
            usage.record_signed(&actual.handoff_receipt)?;
        } else {
            let actual: ExecutionHandoffTerminalReconciliationResult =
                serde_json::from_value(raw.clone()).map_err(|_| {
                    AuthorizationError::InvalidProof("invalid historical reconciliation")
                })?;
            let verified = decision.verify_execution_terminal_reconciliation(
                &actual,
                &resolver,
                decision_time,
            )?;
            usage.merge(verified.proof_usage());
        }
        if let Some(counts) = crate::signature_verification::SignatureVerification::current_counts()
        {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        serde_json::to_string(&serde_json::json!({"historical":true,"state":outcome,
            "purpose":wire.resolution.purpose,"actual":raw}))
        .map_err(|_| AuthorizationError::InvalidProof("historical resolution encoding failed"))
    })()
    .map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyRemoteExecutionResultHistory))]
pub fn verify_remote_execution_result_history(
    reader: &VerifiedClientAuthorization,
    policy: &remote_execution_policy::VerifiedRemoteExecutionHistoryPolicy,
    actual_json: &str,
    body_json: &str,
    invocation_json: &str,
    binding_json: &str,
    now_ms: f64,
) -> crate::Result<String> {
    (|| -> Result<String> {
        let _signatures = crate::signature_verification::ScopedSignatureVerification::new();
        let now = clock(now_ms)?;
        let (state, historical_proof, mut usage) = load(reader, policy, actual_json,
            body_json, invocation_json, binding_json, now)?;
        let actual: ExecutionHandoffResult = parse(actual_json)?;
        let body: Value = parse(body_json)?;
        let invocation: Signed<Invocation> = parse(invocation_json)?;
        let binding: task_binding::SignedBinding = parse(binding_json)?;
        let event: events::Event = parse(&actual.handoff.handoff.project_dispatch.canonical_body)?;
        let events::EventCause::Invocation {admission:proof} = event.cause else {
            return Err(AuthorizationError::InvalidProof("historical result has no admission"));
        };
        reader_scope(reader,&proof.invocation.payload.resource)?;
        require(proof.source_body == body && proof.invocation == invocation
            && body["request"]["binding"] == serde_json::to_value(&binding)
                .map_err(|_| AuthorizationError::InvalidProof("invalid historical binding"))?
            && invocation.payload.body_hash == ObjectHash::body(&body)?
            && actual.receipt.payload.issued_at_ms <= now
            && actual.receipt.payload.issued_at_ms >= historical_proof.decision_at_ms(),
            "historical result changed the original browser request")?;
        let facts = RequestFacts {resource:invocation.payload.resource.clone(),action:invocation.payload.action,
            audience:invocation.payload.audience.clone(),operation_id:invocation.payload.operation_id.clone(),
            body_hash:ObjectHash::body(&body)?,context:invocation.payload.context.clone()};
        let accepted = admitted(&state,&proof,&facts)?;
        let historical = at_heads(&state,historical_proof.heads())?;
        let time = historical_proof.decision_at_ms();
        let original = historical.verify_operation_authorization(&accepted,&facts,
            &proof.transport.clone().into_evidence(),time)?;
        let remote_execution_history::RemoteExecutionHistoryAcceptance::ExecutionResult {
            receipt, freshness, transport,
        } = policy.acceptance()? else {
            return Err(AuthorizationError::InvalidProof("execution history requires its original acceptance"));
        };
        let checkpoint = verify_execution_handoff_admission_checkpoint(&actual.handoff,
            receipt, freshness, transport, time)?;
        let verified = historical.verify_execution_handoff(&actual.handoff,&original,
            &TransportEvidence::VerifiedPeer {origin:actual.handoff.invocation.payload.presenter.clone()},Some(&checkpoint),time)?;
        verify_execution_handoff_receipt_signature(&actual.handoff,&actual.receipt,&actual.result)?;
        usage.record_admission(&proof)?;
        usage.record_authorization(&original)?;
        usage.merge(checkpoint.proof_usage());
        usage.merge(verified.proof_usage());
        usage.record_historical_handoff(&actual.handoff)?;
        usage.record_signed(&actual.receipt)?;
        usage.normalize_history_acceptance_metadata(freshness)?;
        if let Some(counts) = crate::signature_verification::SignatureVerification::current_counts() {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        serde_json::to_string(&serde_json::json!({"historical":true,"state":actual.receipt.payload.state,"actual":actual}))
            .map_err(|_| AuthorizationError::InvalidProof("historical execution result encoding failed"))
    })().map_err(|error|error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_acceptance_metadata_keeps_all_signed_objects_and_budget_limits() {
        let fixture = execution_observation::tests::retained_history_fixture();
        let reference = remote_execution_history::derive_reference(
            &serde_json::to_string(&fixture.actual).unwrap(),
            &serde_json::to_string(&fixture.body).unwrap(),
            &serde_json::to_string(&fixture.invocation).unwrap(),
            &serde_json::to_string(&fixture.binding).unwrap(),
        ).unwrap();
        let accepted = remote_execution_history::verify_remote_execution_history_acceptance(
            &fixture.state, &reference, &fixture.acceptance,
            &PublicKey::of(&fixture.executor), 10_020,
        ).unwrap();
        let remote_execution_history::RemoteExecutionHistoryAcceptance::ExecutionResult {
            freshness, receipt, ..
        } = &fixture.acceptance else {
            panic!("fixture requires its original execution acceptance");
        };
        let mut usage = accepted.proof_usage().clone();
        let before = usage.counts();
        usage.normalize_history_acceptance_metadata(freshness).unwrap();
        let after = usage.counts();
        assert_eq!(before.unique_signed_objects, after.unique_signed_objects);
        assert_eq!(before.unique_signatures, after.unique_signatures);
        assert!(after.authorization_bytes < before.authorization_bytes);
        usage.enforce_limits().unwrap();
        for offset in 1..=(MAX_PROOF_OBJECTS - after.unique_signed_objects) {
            let mut another = receipt.payload.clone();
            another.admitted_at_ms += offset as u64;
            usage.record_signed(&Signed::sign(another, &fixture.executor).unwrap()).unwrap();
        }
        assert_eq!(usage.counts().unique_signed_objects, MAX_PROOF_OBJECTS);
        usage.enforce_limits().unwrap();
        let mut extra = receipt.payload.clone();
        extra.admitted_at_ms += MAX_PROOF_OBJECTS as u64;
        let extra = Signed::sign(extra, &fixture.executor).unwrap();
        extra.verify_signature().unwrap();
        usage.record_verified_object(&extra).unwrap();
        usage.normalize_history_acceptance_metadata(freshness).unwrap();
        let exceeded = usage.counts();
        assert_eq!(exceeded.unique_signed_objects, MAX_PROOF_OBJECTS + 1);
        assert_eq!(exceeded.unique_signatures, MAX_PROOF_SIGNATURES);
        assert!(exceeded.authorization_bytes <= MAX_PROOF_BYTES);
        assert!(matches!(usage.enforce_limits(), Err(AuthorizationError::BudgetExceeded)));
    }
}
