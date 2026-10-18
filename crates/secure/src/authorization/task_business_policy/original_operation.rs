//! Current checks confined to an executor-accepted immutable Submit operation.

use super::super::{
    events::StoredTransport,
    execution_handoff::{
        ExecutionHandoffAdmissionReceipt, ExecutionHandoffChallenge, ExecutionHandoffFreshness,
        verify_execution_handoff_admission_checkpoint,
    },
    remote_execution_history::RemoteExecutionHistoryAcceptance,
};
use super::*;

fn submit_acceptance<'a>(
    acceptance: &'a RemoteExecutionHistoryAcceptance,
) -> Result<(
    &'a Signed<ExecutionHandoffAdmissionReceipt>,
    &'a ExecutionHandoffFreshness,
    &'a StoredTransport,
)> {
    match acceptance {
        RemoteExecutionHistoryAcceptance::ExecutionResult {
            receipt,
            freshness,
            transport,
        } => Ok((receipt, freshness, transport)),
        _ => Err(AuthorizationError::InvalidProof(
            "Task operation requires its signed executor Submit acceptance",
        )),
    }
}

fn accepted_handoff_at(
    state: &VerifiedPolicySet,
    wire: &SignedExecutionHandoff,
    acceptance: &RemoteExecutionHistoryAcceptance,
    now: u64,
) -> Result<VerifiedExecutionHandoff> {
    carrier(acceptance)?;
    let (receipt, freshness, transport) = submit_acceptance(acceptance)?;
    require(
        transport.clone().into_evidence()
            == TransportEvidence::VerifiedPeer {
                origin: wire.invocation.payload.presenter.clone(),
            },
        "Task operation changed its accepted Project transport",
    )?;
    let checkpoint = verify_execution_handoff_admission_checkpoint(
        wire,
        receipt,
        freshness,
        transport,
        receipt.payload.admitted_at_ms,
    )?;
    let event: Event = serde_json::from_str(&wire.handoff.project_dispatch.canonical_body)
        .map_err(|_| AuthorizationError::InvalidProof("invalid accepted Task dispatch"))?;
    let EventCause::Invocation { admission } = event.cause else {
        return Err(AuthorizationError::InvalidProof(
            "Task operation has no original admitted source request",
        ));
    };
    let facts = request_facts(&admission.invocation.payload);
    let admitted = remote_resolution_history::admitted(state, &admission, &facts)?;
    let original = state.verify_operation_authorization(
        &admitted,
        &facts,
        &admission.transport.clone().into_evidence(),
        now,
    )?;
    let verified = state.verify_execution_handoff(
        wire,
        &original,
        &transport.clone().into_evidence(),
        Some(&checkpoint),
        now,
    )?;
    require(
        verified.original().phase == AuthorizationPhase::Operation
            && verified.submission().phase == AuthorizationPhase::Operation,
        "Task operation cannot authorize a new admission",
    )?;
    Ok(verified)
}

pub(super) struct VerifiedTaskBusinessOperationReceipt {
    authorization: VerifiedExecutionHandoff,
    acceptance_envelope_hash: ObjectHash,
    usage: ExecutionProofUsage,
}

impl VerifiedTaskBusinessOperationReceipt {
    pub(super) fn authorization(&self) -> &VerifiedExecutionHandoff {
        &self.authorization
    }

    pub(super) fn acceptance_envelope_hash(&self) -> &ObjectHash {
        &self.acceptance_envelope_hash
    }

    pub(super) fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
}

pub(super) fn verify_task_business_receipt_operation(
    current: &VerifiedTaskCurrentPolicy,
    header: &Signed<TaskBusinessPolicyHeader>,
    wire: &SignedExecutionHandoff,
    acceptance: &RemoteExecutionHistoryAcceptance,
    challenge: &ExecutionHandoffChallenge,
    now: u64,
) -> Result<VerifiedTaskBusinessOperationReceipt> {
    let _signatures = ScopedSignatureVerification::new();
    carrier(header)?;
    carrier(challenge)?;
    let raw = TaskBusinessWire::Submit(wire.clone());
    let scope = raw.scope()?;
    let (receipt, freshness, transport) = submit_acceptance(acceptance)?;
    verify_execution_handoff_admission_checkpoint(
        wire,
        receipt,
        freshness,
        transport,
        receipt.payload.admitted_at_ms,
    )?;
    header.verify_signature()?;
    require(
        header.payload.version == AUTH_VERSION
            && header.payload.scope == scope
            && header.issuer == scope.source_node_public_key
            && header
                .payload
                .source_heads
                .iter()
                .all(|head| freshness.challenge.policies.contains(head))
            && matches!(
                challenge.stage,
                CheckStage::Start | CheckStage::Renew | CheckStage::RemoteRead
            ),
        "Task operation receipt changed its accepted source, exact heads or stage",
    )?;
    window(
        header.payload.issued_at_ms,
        header.payload.expires_at_ms,
        receipt.payload.admitted_at_ms,
    )?;
    check_source_policy_scope(current, &raw, &scope, now)?;
    let mut usage = ExecutionProofUsage::default();
    let (approved, _) = source_grant(
        current.state(),
        &scope,
        &scope.target_public_key,
        now,
        &mut usage,
    )?;
    let project_head = current
        .state()
        .head(&PolicyDomain::Space(scope.source_project.space.clone()))
        .ok_or(AuthorizationError::PolicyConflict)?;
    let (heads, _) = remote_execution_policy::reference_closure(
        current.state(),
        std::slice::from_ref(project_head),
        &approved.pins,
    )?;
    if heads != header.payload.source_heads {
        return Err(AuthorizationError::NeedPolicySync(heads));
    }
    let verified = accepted_handoff_at(current.state(), wire, acceptance, now)?;
    let expected = ExecutionHandoffChallenge::bound_to(
        &verified,
        challenge.stage,
        challenge.nonce.clone(),
        challenge.issued_at_ms,
        challenge.expires_at_ms,
    )?;
    validate_window(challenge.issued_at_ms, challenge.expires_at_ms, now)?;
    if challenge.policies != expected.policies {
        return Err(AuthorizationError::NeedPolicySync(expected.policies));
    }
    require(
        challenge == &expected,
        "Task operation receipt changed its complete typed challenge",
    )?;
    usage.merge(verified.proof_usage());
    usage.record_signed(header)?;
    usage.record_metadata(challenge)?;
    finish_usage(&mut usage)?;
    Ok(VerifiedTaskBusinessOperationReceipt {
        authorization: verified,
        acceptance_envelope_hash: ObjectHash::body(acceptance)?,
        usage,
    })
}

#[derive(Clone)]
pub struct TaskBusinessOperationCheck {
    original: VerifiedTaskBusinessOriginalSource,
    combined: VerifiedPolicySnapshot,
    authorization: VerifiedExecutionHandoff,
    primary_heads: Vec<PolicyStamp>,
    checked_at_ms: u64,
    usage: ExecutionProofUsage,
}

pub fn begin_task_business_operation_current(
    primary: &VerifiedPolicySet,
    original: &VerifiedTaskBusinessOriginalSource,
    local_c: &PublicKey,
    saved_floors: &[PolicyPin],
    now: u64,
) -> Result<TaskBusinessOperationCheck> {
    let _signatures = ScopedSignatureVerification::new();
    let TaskBusinessWire::Submit(wire) = original.wire() else {
        return Err(AuthorizationError::InvalidProof(
            "Task execution lease requires its original accepted Submit",
        ));
    };
    let mut usage = ExecutionProofUsage::default();
    let (approved, _) = source_grant(primary, original.scope(), local_c, now, &mut usage)?;
    let selected: Vec<_> = approved
        .pins
        .iter()
        .filter(|pin| {
            original
                .foreign_heads()
                .iter()
                .any(|head| head.domain == pin.domain)
        })
        .cloned()
        .collect();
    original
        .source_snapshot()
        .with_independent_pins(&selected)?;
    let (effective, required) = effective_pins(original.pins(), saved_floors)?;
    for head in required
        .iter()
        .chain(effective.iter().filter_map(|pin| pin.minimum_head.as_ref()))
    {
        if !original.source_snapshot().state().roots.contains_key(head) {
            return Err(AuthorizationError::NeedPolicySync(vec![head.clone()]));
        }
    }
    original
        .source_snapshot()
        .with_independent_pins(&effective)?;
    let local_authority = local_authority(primary, original.scope())?;
    let local = policy_import::native_task_business_primary_snapshot(&local_authority)?;
    let combined = policy_import::merge_same_head_snapshots(&local, original.source_snapshot())?;
    let authorization = accepted_handoff_at(combined.state(), wire, original.acceptance(), now)?;
    usage.merge(authorization.proof_usage());
    usage.record_verified_object(original.header())?;
    finish_usage(&mut usage)?;
    Ok(TaskBusinessOperationCheck {
        original: original.clone(),
        combined,
        authorization,
        primary_heads: local_authority.heads,
        checked_at_ms: now,
        usage,
    })
}

impl TaskBusinessOperationCheck {
    pub fn wire(&self) -> &SignedExecutionHandoff {
        self.authorization.wire()
    }

    pub fn wire_hash(&self) -> &ObjectHash {
        self.authorization.hash()
    }

    pub fn binding(&self) -> &task_binding::SignedBinding {
        self.authorization.binding()
    }

    pub fn dependencies(&self) -> &[PolicyStamp] {
        self.authorization.dependencies()
    }

    pub fn primary_heads(&self) -> &[PolicyStamp] {
        &self.primary_heads
    }

    pub fn foreign_heads(&self) -> &[PolicyStamp] {
        self.original.foreign_heads()
    }

    pub fn checked_at_ms(&self) -> u64 {
        self.checked_at_ms
    }

    pub fn expires_at_ms(&self) -> u64 {
        self.authorization.expires_at_ms()
    }

    pub fn original_header_envelope_hash(&self) -> Result<ObjectHash> {
        self.original.header_envelope_hash()
    }

    pub fn original_witness_envelope_hash(&self) -> Result<ObjectHash> {
        self.original.witness_envelope_hash()
    }

    pub fn original_page_envelope_hashes(&self) -> &[ObjectHash] {
        self.original.original_page_envelope_hashes()
    }

    pub fn acceptance_envelope_hash(&self) -> Result<ObjectHash> {
        self.original.acceptance_envelope_hash()
    }

    pub fn admission_receipt(&self) -> Result<&Signed<ExecutionHandoffAdmissionReceipt>> {
        submit_acceptance(self.original.acceptance()).map(|(receipt, _, _)| receipt)
    }

    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }

    pub fn issue_challenge(
        &self,
        stage: CheckStage,
        nonce: Nonce,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<ExecutionHandoffChallenge> {
        require(
            matches!(stage, CheckStage::Start | CheckStage::Renew),
            "accepted Task operation cannot issue a new admission challenge",
        )?;
        ExecutionHandoffChallenge::new(
            &self.authorization,
            stage,
            nonce,
            issued_at_ms,
            expires_at_ms,
        )
    }

    pub fn sign_receipts(
        &self,
        identity: &Identity,
        challenge: &ExecutionHandoffChallenge,
        now: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        let _signatures = ScopedSignatureVerification::new();
        carrier(challenge)?;
        let local_c = PublicKey::of(identity);
        require(
            local_c == self.original.scope().target_public_key
                && now == self.checked_at_ms
                && matches!(challenge.stage, CheckStage::Start | CheckStage::Renew),
            "Task operation receipts require their freshly checked executor authority",
        )?;
        let expected = ExecutionHandoffChallenge::bound_to(
            &self.authorization,
            challenge.stage,
            challenge.nonce.clone(),
            challenge.issued_at_ms,
            challenge.expires_at_ms,
        )?;
        require(
            challenge == &expected,
            "Task operation receipt changed its complete execution challenge",
        )?;
        validate_window(challenge.issued_at_ms, challenge.expires_at_ms, now)?;
        self.combined
            .state()
            .check_execution_handoff_current(&self.authorization, now)?;
        let expires = challenge
            .expires_at_ms
            .min(self.authorization.expires_at_ms());
        require(now < expires, "Task operation receipt authority expired")?;
        let mut receipts = Vec::new();
        let hash = challenge.hash()?;
        for head in &self.primary_heads {
            require(
                challenge.policies.contains(head),
                "Task operation receipt omitted an executor policy head",
            )?;
            let authority = match &head.domain {
                PolicyDomain::Account(id) => {
                    self.combined.state().account(id).map(|v| &v.authority)
                }
                PolicyDomain::Space(id) => self.combined.state().space(id).map(|v| &v.authority),
            }
            .ok_or(AuthorizationError::PolicyConflict)?;
            if authority.public_key == local_c {
                require(
                    authority.device == DeviceId::from_key(&local_c)
                        && authority.service == ServiceId::from_key(&local_c),
                    "Task operation executor authority changed its service or device",
                )?;
                receipts.push(Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: head.clone(),
                        challenge_hash: hash.clone(),
                        issued_at_ms: now,
                        expires_at_ms: expires,
                    },
                    identity,
                )?);
            }
        }
        carrier(&receipts)?;
        let mut usage = self.usage.clone();
        usage.record_metadata(challenge)?;
        for receipt in &receipts {
            usage.record_signed(receipt)?;
        }
        finish_usage(&mut usage)?;
        Ok(receipts)
    }

    pub fn finish(
        self,
        proof: &ExecutionHandoffFreshness,
        now: u64,
    ) -> Result<VerifiedTaskBusinessOperationCurrent> {
        let _signatures = ScopedSignatureVerification::new();
        carrier(proof)?;
        require(
            now >= self.checked_at_ms
                && matches!(proof.challenge.stage, CheckStage::Start | CheckStage::Renew),
            "Task operation changed its checked clock or execution lease stage",
        )?;
        for head in self.original.foreign_heads() {
            if !proof.challenge.policies.contains(head) {
                return Err(AuthorizationError::NeedPolicySync(
                    self.authorization.dependencies().to_vec(),
                ));
            }
        }
        let fresh = self.combined.state().verify_execution_handoff_freshness(
            &self.authorization,
            proof,
            now,
        )?;
        let mut usage = self.usage;
        usage.merge(fresh.proof_usage());
        finish_usage(&mut usage)?;
        Ok(VerifiedTaskBusinessOperationCurrent {
            original: self.original,
            authorization: self.authorization,
            freshness: proof.clone(),
            primary_heads: self.primary_heads,
            checked_at_ms: now,
            deadline_ms: fresh.expires_at_ms(),
            usage,
        })
    }
}

#[derive(Clone)]
pub struct VerifiedTaskBusinessOperationCurrent {
    original: VerifiedTaskBusinessOriginalSource,
    authorization: VerifiedExecutionHandoff,
    freshness: ExecutionHandoffFreshness,
    primary_heads: Vec<PolicyStamp>,
    checked_at_ms: u64,
    deadline_ms: u64,
    usage: ExecutionProofUsage,
}

impl VerifiedTaskBusinessOperationCurrent {
    pub fn check_current(
        &self,
        primary: &VerifiedPolicySet,
        local_c: &PublicKey,
        saved_floors: &[PolicyPin],
        now: u64,
    ) -> Result<ExecutionProofUsage> {
        let _signatures = ScopedSignatureVerification::new();
        require(
            now >= self.checked_at_ms,
            "Task operation recheck clock moved backwards",
        )?;
        let candidate = begin_task_business_operation_current(
            primary,
            &self.original,
            local_c,
            saved_floors,
            now,
        )?;
        if candidate.primary_heads() != self.primary_heads()
            || candidate.dependencies() != self.dependencies()
        {
            return Err(AuthorizationError::NeedPolicySync(
                candidate.dependencies().to_vec(),
            ));
        }
        let verified = candidate.finish(&self.freshness, now)?;
        Ok(verified.usage)
    }

    pub fn handoff_authorization(&self) -> &VerifiedExecutionHandoff {
        &self.authorization
    }

    pub fn wire(&self) -> &SignedExecutionHandoff {
        self.authorization.wire()
    }

    pub fn wire_hash(&self) -> &ObjectHash {
        self.authorization.hash()
    }

    pub fn admission_receipt(&self) -> Result<&Signed<ExecutionHandoffAdmissionReceipt>> {
        submit_acceptance(self.original.acceptance()).map(|(receipt, _, _)| receipt)
    }

    pub fn accepted_at_ms(&self) -> u64 {
        self.original.accepted_at_ms()
    }

    pub fn binding(&self) -> &task_binding::SignedBinding {
        self.authorization.binding()
    }

    pub fn freshness(&self) -> &ExecutionHandoffFreshness {
        &self.freshness
    }

    pub fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }

    pub fn checked_at_ms(&self) -> u64 {
        self.checked_at_ms
    }

    pub fn dependencies(&self) -> &[PolicyStamp] {
        self.authorization.dependencies()
    }

    pub fn primary_heads(&self) -> &[PolicyStamp] {
        &self.primary_heads
    }

    pub fn foreign_heads(&self) -> &[PolicyStamp] {
        self.original.foreign_heads()
    }

    pub fn expires_at_ms(&self) -> u64 {
        self.authorization.expires_at_ms()
    }

    pub fn original_header_envelope_hash(&self) -> Result<ObjectHash> {
        self.original.header_envelope_hash()
    }

    pub fn original_witness_envelope_hash(&self) -> Result<ObjectHash> {
        self.original.witness_envelope_hash()
    }

    pub fn original_page_envelope_hashes(&self) -> &[ObjectHash] {
        self.original.original_page_envelope_hashes()
    }

    pub fn acceptance_envelope_hash(&self) -> Result<ObjectHash> {
        self.original.acceptance_envelope_hash()
    }

    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::{
        client_policy_import::require_confirmed_client_policy,
        execution_observation::tests::{
            RetainedHistoryFixture, retained_history_fixture_with_execution_source,
        },
    };
    use super::super::tests::{current_policy, current_policy_at, original_authorization};
    use super::*;

    fn receipts(
        state: &VerifiedPolicySet,
        heads: &[PolicyStamp],
        challenge_hash: &ObjectHash,
        fixture: &RetainedHistoryFixture,
        now: u64,
        expires: u64,
    ) -> Vec<Signed<FreshnessReceipt>> {
        heads
            .iter()
            .map(|head| {
                let authority = match &head.domain {
                    PolicyDomain::Account(id) => &state.account(id).unwrap().authority,
                    PolicyDomain::Space(id) => &state.space(id).unwrap().authority,
                };
                let signer = [&fixture.project_node, &fixture.executor]
                    .into_iter()
                    .find(|identity| PublicKey::of(identity) == authority.public_key)
                    .unwrap();
                Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: head.clone(),
                        challenge_hash: challenge_hash.clone(),
                        issued_at_ms: now,
                        expires_at_ms: expires,
                    },
                    signer,
                )
                .unwrap()
            })
            .collect()
    }

    fn operation_fixture() -> (RetainedHistoryFixture, VerifiedTaskBusinessOriginalSource) {
        let mut fixture = retained_history_fixture_with_execution_source();
        let now = fixture.invocation.payload.not_before_ms;
        let mut invocation = fixture.actual.handoff.invocation.payload.clone();
        invocation.expires_at_ms = now + 1_000;
        fixture.actual.handoff.invocation =
            Signed::sign(invocation, &fixture.project_node).unwrap();
        let current = current_policy(&fixture);
        let wire = TaskBusinessWire::Submit(fixture.actual.handoff.clone());
        let original = original_authorization(current.state(), &fixture.actual.handoff, now);
        let transport = StoredTransport::VerifiedPeer {
            origin: fixture.actual.handoff.invocation.payload.presenter.clone(),
        };
        let verified = current
            .state()
            .verify_execution_handoff(
                &fixture.actual.handoff,
                &original,
                &transport.clone().into_evidence(),
                None,
                now,
            )
            .unwrap();
        let challenge = ExecutionHandoffChallenge::new(
            &verified,
            CheckStage::Admit,
            Nonce::random().unwrap(),
            now,
            now + 500,
        )
        .unwrap();
        let freshness = ExecutionHandoffFreshness {
            receipts: receipts(
                current.state(),
                verified.dependencies(),
                &challenge.hash().unwrap(),
                &fixture,
                now,
                now + 500,
            ),
            challenge,
        };
        let fresh = current
            .state()
            .verify_execution_handoff_freshness(&verified, &freshness, now)
            .unwrap();
        current
            .state()
            .admit_execution_handoff(&verified, &fresh, now)
            .unwrap();
        fixture.acceptance = RemoteExecutionHistoryAcceptance::ExecutionResult {
            receipt: Signed::sign(
                ExecutionHandoffAdmissionReceipt::new(
                    &fixture.actual.handoff,
                    &freshness,
                    &transport,
                    now,
                )
                .unwrap(),
                &fixture.executor,
            )
            .unwrap(),
            freshness,
            transport,
        };
        let mut export = build_task_business_policy_export(
            &current,
            TaskBusinessAuthorization::Submit(&verified),
            &wire,
            &fixture.project_node,
            now,
        )
        .unwrap();
        let source = require_confirmed_client_policy(&fixture.source).unwrap();
        let mut pages = Vec::new();
        for entry in source.journal() {
            if export.allows_entry(entry).unwrap() {
                pages.push(
                    export
                        .append_entry_page(&fixture.project_node, entry.clone(), now)
                        .unwrap(),
                );
            }
        }
        let (last, witness) = export.finish_pages(&fixture.project_node, now).unwrap();
        pages.push(last);
        let mut import = begin_task_business_original_import(
            &fixture.state,
            &wire,
            &fixture.acceptance,
            export.header(),
            &PublicKey::of(&fixture.executor),
            now,
        )
        .unwrap();
        for page in &pages {
            import.append_page(page).unwrap();
        }
        let original = import.finish_original(&witness).unwrap();
        (fixture, original)
    }

    #[test]
    fn accepted_task_operation_renews_after_wire_and_header_expiry_without_new_admission() {
        let (mut fixture, original) = operation_fixture();
        let now = original
            .header()
            .payload
            .expires_at_ms
            .max(fixture.actual.handoff.invocation.payload.expires_at_ms)
            + 1;
        let c_key = PublicKey::of(&fixture.executor);
        let current_b = current_policy_at(&fixture, now);
        let check =
            begin_task_business_operation_current(&fixture.state, &original, &c_key, &[], now)
                .unwrap();
        assert!(
            check
                .issue_challenge(CheckStage::Admit, Nonce::random().unwrap(), now, now + 500)
                .is_err()
        );
        assert!(
            check
                .issue_challenge(
                    CheckStage::RemoteRead,
                    Nonce::random().unwrap(),
                    now,
                    now + 500
                )
                .is_err()
        );
        let state = check.combined.clone();
        let mut capabilities = Vec::new();
        for stage in [CheckStage::Start, CheckStage::Renew] {
            let challenge = check
                .issue_challenge(stage, Nonce::random().unwrap(), now, now + 500)
                .unwrap();
            let source = verify_task_business_receipt_operation(
                &current_b,
                original.header(),
                &fixture.actual.handoff,
                &fixture.acceptance,
                &challenge,
                now,
            )
            .unwrap();
            assert!(source.authorization().submission().phase == AuthorizationPhase::Operation);
            assert_eq!(
                source.acceptance_envelope_hash(),
                &ObjectHash::body(&fixture.acceptance).unwrap()
            );
            assert!(
                check
                    .sign_receipts(&fixture.project_node, &challenge, now)
                    .is_err()
            );
            assert!(
                check
                    .sign_receipts(&fixture.executor, &challenge, now + 1)
                    .is_err()
            );
            let mut changed = challenge.clone();
            changed.binding_generation += 1;
            assert!(
                check
                    .sign_receipts(&fixture.executor, &changed, now)
                    .is_err()
            );
            let local = check
                .sign_receipts(&fixture.executor, &challenge, now)
                .unwrap();
            assert!(!local.is_empty());
            assert!(local.iter().all(|receipt| {
                receipt.issuer == c_key && check.primary_heads().contains(&receipt.payload.current)
            }));
            let mut all = receipts(
                state.state(),
                check.dependencies(),
                &challenge.hash().unwrap(),
                &fixture,
                now,
                now + 500,
            )
            .into_iter()
            .filter(|receipt| receipt.issuer != c_key)
            .collect::<Vec<_>>();
            all.extend(local);
            let proof = ExecutionHandoffFreshness {
                receipts: all,
                challenge,
            };
            let mut missing = proof.clone();
            missing.receipts.pop();
            assert!(check.clone().finish(&missing, now).is_err());
            let cap = check.clone().finish(&proof, now).unwrap();
            let later_cap = check.clone().finish(&proof, now + 1).unwrap();
            assert!(
                later_cap
                    .check_current(&fixture.state, &c_key, &[], now)
                    .is_err()
            );
            cap.check_current(&fixture.state, &c_key, &[], now).unwrap();
            assert!(
                cap.check_current(&fixture.state, &c_key, &[], now + 500)
                    .is_err()
            );
            assert_eq!(cap.wire(), &fixture.actual.handoff);
            assert_eq!(cap.deadline_ms(), now + 500);
            assert!(
                cap.handoff_authorization().original().phase == AuthorizationPhase::Operation
                    && cap.handoff_authorization().submission().phase
                        == AuthorizationPhase::Operation
            );
            let fresh = state
                .state()
                .verify_execution_handoff_freshness(cap.handoff_authorization(), &proof, now)
                .unwrap();
            assert!(
                state
                    .state()
                    .admit_execution_handoff(cap.handoff_authorization(), &fresh, now)
                    .is_err()
            );
            let mut another_wire = fixture.actual.handoff.clone();
            another_wire
                .invocation
                .payload
                .operation_id
                .push_str("-other");
            assert!(
                verify_task_business_receipt_operation(
                    &current_b,
                    original.header(),
                    &another_wire,
                    &fixture.acceptance,
                    &proof.challenge,
                    now,
                )
                .is_err()
            );
            capabilities.push(cap);
        }
        let mut higher = original.pins().to_vec();
        higher[0].minimum_head.as_mut().unwrap().sequence += 1;
        assert!(
            begin_task_business_operation_current(&fixture.state, &original, &c_key, &higher, now,)
                .is_err()
        );
        for cap in &capabilities {
            assert!(
                cap.check_current(&fixture.state, &c_key, &higher, now)
                    .is_err()
            );
        }
        let scope = original.scope();
        let mut document = fixture
            .state
            .space(&scope.execution_resource.space)
            .unwrap()
            .clone();
        document.previous = fixture
            .state
            .head(&PolicyDomain::Space(document.space.clone()))
            .cloned();
        document.sequence += 1;
        document
            .grants
            .retain(|hash| hash != &scope.execution_grant);
        fixture
            .state
            .install_space(Signed::sign(document, &fixture.root).unwrap())
            .unwrap();
        assert!(
            begin_task_business_operation_current(&fixture.state, &original, &c_key, &[], now,)
                .is_err()
        );
        for cap in &capabilities {
            assert!(cap.check_current(&fixture.state, &c_key, &[], now).is_err());
        }
        assert_eq!(
            original.accepted_at_ms(),
            fixture.invocation.payload.not_before_ms
        );
    }
}
