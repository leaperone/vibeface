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
    project_json: &str,
    project_pins_json: &str,
    execution_json: &str,
    execution_pins_json: &str,
    now: u64,
) -> Result<VerifiedPolicySet> {
    let project: ClientAuthorizationBundle = parse(project_json)?;
    let execution: ClientAuthorizationBundle = parse(execution_json)?;
    let project_pins: Vec<PolicyPin> = parse(project_pins_json)?;
    let execution_pins: Vec<PolicyPin> = parse(execution_pins_json)?;
    let current = replay_policies(&project_pins, &project.journal)?;
    let key = &current
        .find_device(&reader.scope.device_id)?
        .payload
        .public_key;
    let baseline = verify_client_authorization(
        project_json,
        project_pins_json,
        reader.scope.account_id.as_str(),
        reader.scope.space_id.as_str(),
        key.as_str(),
        now as f64,
    )
    .map_err(|_| AuthorizationError::InvalidProof("historical reader is no longer authorized"))?;
    require(
        baseline.scope == reader.scope
            && baseline.heads == reader.heads
            && baseline.grant_leaf == reader.grant_leaf
            && baseline.audience == reader.audience
            && baseline.audience_public_key == reader.audience_public_key
            && reader.actions.iter().any(|action| {
                matches!(
                    action,
                    Action::OperationStatusRead
                        | Action::OperationReconcile
                        | Action::OperationResolve
                )
            }),
        "history requires current Project read permission",
    )?;
    let mut pins = BTreeMap::<PolicyDomain, PolicyPin>::new();
    for pin in project_pins.into_iter().chain(execution_pins) {
        if let Some(saved) = pins.get_mut(&pin.domain) {
            require(
                saved.genesis_root == pin.genesis_root,
                "historical policy roots conflict",
            )?;
            if let Some(candidate) = pin.minimum_head {
                match &saved.minimum_head {
                    Some(existing)
                        if (existing.authority_epoch, existing.sequence)
                            == (candidate.authority_epoch, candidate.sequence) =>
                    {
                        require(
                            existing == &candidate,
                            "historical policy watermarks conflict",
                        )?
                    }
                    Some(existing)
                        if (existing.authority_epoch, existing.sequence)
                            > (candidate.authority_epoch, candidate.sequence) => {}
                    _ => saved.minimum_head = Some(candidate),
                }
            }
        } else {
            pins.insert(pin.domain.clone(), pin);
        }
    }
    let mut seen = BTreeSet::new();
    let journal = project
        .journal
        .into_iter()
        .chain(execution.journal)
        .filter_map(|entry| match ObjectHash::body(&entry) {
            Ok(hash) if seen.insert(hash.clone()) => Some(Ok(entry)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>>>()?;
    replay_policies(&pins.into_values().collect::<Vec<_>>(), &journal)
}

fn at_heads(current: &VerifiedPolicySet, heads: &[PolicyStamp]) -> Result<VerifiedPolicySet> {
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

fn admitted(
    current: &VerifiedPolicySet,
    proof: &events::AdmissionProof,
    facts: &RequestFacts,
) -> Result<VerifiedAdmission> {
    let historical = at_heads(current, &proof.challenge.policies)?;
    let transport = proof.transport.clone().into_evidence();
    let request =
        historical.verify_invocation(&proof.invocation, facts, &transport, proof.admitted_at_ms)?;
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
    project_json: &str,
    project_pins_json: &str,
    execution_json: &str,
    execution_pins_json: &str,
    actual_json: &str,
    body_json: &str,
    invocation_json: &str,
    now_ms: f64,
) -> crate::Result<String> {
    (|| -> Result<String> {
        let _signatures = crate::signature_verification::ScopedSignatureVerification::new();
        let now = clock(now_ms)?;
        let state = load(
            reader,
            project_json,
            project_pins_json,
            execution_json,
            execution_pins_json,
            now,
        )?;
        let body: Value = parse(body_json)?;
        let invocation: Signed<Invocation> = parse(invocation_json)?;
        invocation.verify_signature()?;
        let raw: Value = parse(actual_json)?;
        let unknown = raw.get("resolution").is_some();
        let (wire, freshness, decision_time, outcome) = if unknown {
            let actual: ExecutionHandoffResolutionResult = serde_json::from_value(raw.clone())
                .map_err(|_| AuthorizationError::InvalidProof("invalid historical resolution"))?;
            verify_execution_resolution_receipt_signature(&actual)?;
            (
                actual.resolution,
                actual.freshness,
                actual.receipt.payload.issued_at_ms,
                actual.handoff_receipt.payload.state,
            )
        } else {
            let actual: ExecutionHandoffTerminalReconciliationResult =
                serde_json::from_value(raw.clone()).map_err(|_| {
                    AuthorizationError::InvalidProof("invalid historical reconciliation")
                })?;
            let decision = actual
                .freshness
                .receipts
                .iter()
                .map(|receipt| receipt.payload.issued_at_ms)
                .max()
                .unwrap_or(actual.freshness.challenge.issued_at_ms);
            let outcome = actual
                .reconciliation
                .resolution
                .observation
                .receipt
                .payload
                .state;
            (actual.reconciliation, actual.freshness, decision, outcome)
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
        if unknown {
            let actual: ExecutionHandoffResolutionResult = serde_json::from_value(raw.clone())
                .map_err(|_| AuthorizationError::InvalidProof("invalid historical resolution"))?;
            let verified =
                decision.verify_execution_resolution_ack(&actual, &resolver, decision_time)?;
            decision.verify_execution_resolution_freshness(&verified, &freshness, decision_time)?;
        } else {
            let actual: ExecutionHandoffTerminalReconciliationResult =
                serde_json::from_value(raw.clone()).map_err(|_| {
                    AuthorizationError::InvalidProof("invalid historical reconciliation")
                })?;
            decision.verify_execution_terminal_reconciliation(&actual, &resolver, decision_time)?;
        }
        serde_json::to_string(&serde_json::json!({"historical":true,"state":outcome,
            "purpose":wire.resolution.purpose,"actual":raw}))
        .map_err(|_| AuthorizationError::InvalidProof("historical resolution encoding failed"))
    })()
    .map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyRemoteExecutionResultHistory))]
pub fn verify_remote_execution_result_history(
    reader: &VerifiedClientAuthorization,
    project_json: &str,
    project_pins_json: &str,
    execution_json: &str,
    execution_pins_json: &str,
    actual_json: &str,
    body_json: &str,
    invocation_json: &str,
    binding_json: &str,
    now_ms: f64,
) -> crate::Result<String> {
    (|| -> Result<String> {
        let _signatures = crate::signature_verification::ScopedSignatureVerification::new();
        let now = clock(now_ms)?;
        let state = load(reader,project_json,project_pins_json,execution_json,execution_pins_json,now)?;
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
            && actual.receipt.payload.issued_at_ms >= actual.handoff.invocation.payload.not_before_ms,
            "historical result changed the original browser request")?;
        let facts = RequestFacts {resource:invocation.payload.resource.clone(),action:invocation.payload.action,
            audience:invocation.payload.audience.clone(),operation_id:invocation.payload.operation_id.clone(),
            body_hash:ObjectHash::body(&body)?,context:invocation.payload.context.clone()};
        let accepted = admitted(&state,&proof,&facts)?;
        let mut heads = actual.handoff.invocation.payload.expected_policies.clone();
        for head in &proof.challenge.policies {
            if !heads.iter().any(|current|current.domain == head.domain) { heads.push(head.clone()); }
        }
        let historical = at_heads(&state,&heads)?;
        let time = actual.handoff.invocation.payload.not_before_ms;
        let original = historical.verify_operation_authorization(&accepted,&facts,
            &proof.transport.clone().into_evidence(),time)?;
        historical.verify_execution_handoff(&actual.handoff,&original,
            &TransportEvidence::VerifiedPeer {origin:actual.handoff.invocation.payload.presenter.clone()},None,time)?;
        verify_execution_handoff_receipt_signature(&actual.handoff,&actual.receipt,&actual.result)?;
        serde_json::to_string(&serde_json::json!({"historical":true,"state":actual.receipt.payload.state,"actual":actual}))
            .map_err(|_| AuthorizationError::InvalidProof("historical execution result encoding failed"))
    })().map_err(|error|error.to_string())
}
