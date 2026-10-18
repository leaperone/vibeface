//! Read-only receipts for the original typed Task business challenges.

#[cfg(test)]
mod resolution_tests;

use super::super::{
    execution_handoff::{ExecutionHandoffChallenge, ExecutionHandoffResolutionChallenge},
    execution_observation::ExecutionObservationChallenge,
    remote_execution_history::RemoteExecutionHistoryAcceptance,
};
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    content = "challenge",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TaskBusinessChallenge {
    Submit(ExecutionHandoffChallenge),
    Observe(ExecutionObservationChallenge),
    Resolve(ExecutionHandoffResolutionChallenge),
}

impl TaskBusinessChallenge {
    pub fn hash(&self) -> Result<ObjectHash> {
        match self {
            Self::Submit(value) => value.hash(),
            Self::Observe(value) => value.hash(),
            Self::Resolve(value) => value.hash(),
        }
    }
    pub fn policies(&self) -> &[PolicyStamp] {
        match self {
            Self::Submit(value) => &value.policies,
            Self::Observe(value) => &value.policies,
            Self::Resolve(value) => &value.policies,
        }
    }
    pub fn issued_at_ms(&self) -> u64 {
        match self {
            Self::Submit(value) => value.issued_at_ms,
            Self::Observe(value) => value.issued_at_ms,
            Self::Resolve(value) => value.issued_at_ms,
        }
    }
    pub fn expires_at_ms(&self) -> u64 {
        match self {
            Self::Submit(value) => value.expires_at_ms,
            Self::Observe(value) => value.expires_at_ms,
            Self::Resolve(value) => value.expires_at_ms,
        }
    }

    fn verify_current(
        &self,
        current: &VerifiedTaskCurrentPolicy,
        authorization: TaskBusinessAuthorization<'_>,
        wire: &TaskBusinessWire,
        now: u64,
        usage: &mut ExecutionProofUsage,
    ) -> Result<u64> {
        validate_window(self.issued_at_ms(), self.expires_at_ms(), now)?;
        require(
            self.policies().len() <= MAX_POLICY_DOMAINS
                && self
                    .policies()
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain),
            "Task business challenge has invalid exact policy heads",
        )?;
        let (heads, expires, expected) = match (self, authorization, wire) {
            (
                Self::Submit(challenge),
                TaskBusinessAuthorization::Submit(value),
                TaskBusinessWire::Submit(original),
            ) => {
                require(
                    value.wire() == original,
                    "Task business receipt changed its verified submission",
                )?;
                current
                    .state()
                    .check_execution_handoff_current(value, now)?;
                let expected = ExecutionHandoffChallenge::bound_to(
                    value,
                    challenge.stage,
                    challenge.nonce.clone(),
                    challenge.issued_at_ms,
                    challenge.expires_at_ms,
                )?;
                usage.merge(value.proof_usage());
                (
                    value.dependencies(),
                    value.expires_at_ms(),
                    Self::Submit(expected),
                )
            }
            (
                Self::Observe(challenge),
                TaskBusinessAuthorization::Observe(value),
                TaskBusinessWire::Observe(original),
            ) => {
                require(
                    value.hash() == &original.hash()?,
                    "Task business receipt changed its verified observation",
                )?;
                current
                    .state()
                    .check_execution_observation_current(value, now)?;
                let expected = ExecutionObservationChallenge::bound_to(
                    value,
                    challenge.nonce.clone(),
                    challenge.issued_at_ms,
                    challenge.expires_at_ms,
                )?;
                usage.merge(value.proof_usage());
                (
                    value.heads(),
                    value.expires_at_ms(),
                    Self::Observe(expected),
                )
            }
            (
                Self::Resolve(challenge),
                TaskBusinessAuthorization::Resolve(value),
                TaskBusinessWire::Resolve(original),
            ) => {
                require(
                    value.wire() == original,
                    "Task business receipt changed its verified resolution",
                )?;
                current.state().check_current(value.resolver(), now)?;
                current.state().check_current(value.submission(), now)?;
                let expected = ExecutionHandoffResolutionChallenge::bound_to(
                    value,
                    challenge.nonce.clone(),
                    challenge.issued_at_ms,
                    challenge.expires_at_ms,
                )?;
                usage.merge(value.proof_usage());
                (
                    value.dependencies(),
                    value.expires_at_ms(),
                    Self::Resolve(expected),
                )
            }
            _ => {
                return Err(AuthorizationError::InvalidProof(
                    "Task business receipt changed its typed challenge purpose",
                ));
            }
        };
        if self.policies().iter().any(|old| {
            heads
                .iter()
                .any(|head| head.domain == old.domain && head != old)
        }) {
            return Err(AuthorizationError::NeedPolicySync(heads.to_vec()));
        }
        require(
            self == &expected,
            "Task business receipt changed a complete challenge anchor",
        )?;
        Ok(expires.min(self.expires_at_ms()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskBusinessReceiptRead {
    pub version: u16,
    pub source_header_hash: ObjectHash,
    pub business_wire_hash: ObjectHash,
    pub challenge: TaskBusinessChallenge,
    pub acceptance: Option<RemoteExecutionHistoryAcceptance>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

impl sealed::Sealed for TaskBusinessReceiptRead {}
impl SignedObject for TaskBusinessReceiptRead {
    const DOMAIN: &'static str = "task-business-receipt-read-v2";
}

#[derive(Clone)]
pub struct VerifiedTaskBusinessReceiptAuthority {
    scope: TaskBusinessScope,
    challenge: TaskBusinessChallenge,
    challenge_hash: ObjectHash,
    primary_heads: Vec<PolicyStamp>,
    signing_heads: Vec<PolicyStamp>,
    local_b: PublicKey,
    verified_at_ms: u64,
    expires_at_ms: u64,
    usage: ExecutionProofUsage,
}

pub fn authorize_task_business_challenge(
    primary: &VerifiedPolicySet,
    current: &VerifiedTaskCurrentPolicy,
    authorization: TaskBusinessAuthorization<'_>,
    wire: &TaskBusinessWire,
    challenge: &TaskBusinessChallenge,
    local_b: &PublicKey,
    now: u64,
) -> Result<VerifiedTaskBusinessReceiptAuthority> {
    let _signatures = ScopedSignatureVerification::new();
    carrier(challenge)?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_metadata(challenge)?;
    authorize_inner(
        primary,
        current,
        authorization,
        wire,
        challenge,
        local_b,
        now,
        usage,
    )
}

pub fn verify_task_business_receipt_read(
    primary: &VerifiedPolicySet,
    current: &VerifiedTaskCurrentPolicy,
    authorization: TaskBusinessAuthorization<'_>,
    original_header: &Signed<TaskBusinessPolicyHeader>,
    original_wire: &TaskBusinessWire,
    actual_c_noise: &ActualPolicyNoisePeer,
    request: &Signed<TaskBusinessReceiptRead>,
    local_b: &PublicKey,
    now: u64,
) -> Result<VerifiedTaskBusinessReceiptAuthority> {
    if let TaskBusinessWire::Submit(wire) = original_wire {
        return verify_task_business_operation_receipt_read(
            primary,
            current,
            original_header,
            wire,
            actual_c_noise,
            request,
            local_b,
            now,
        );
    }
    let _signatures = ScopedSignatureVerification::new();
    require(
        request.payload.acceptance.is_none(),
        "current read-only business receipts do not accept a submission checkpoint",
    )?;
    let usage = verify_receipt_request(
        original_header,
        original_wire,
        actual_c_noise,
        request,
        local_b,
        now,
    )?;
    let mut verified = authorize_inner(
        primary,
        current,
        authorization,
        original_wire,
        &request.payload.challenge,
        local_b,
        now,
        usage,
    )?;
    verified.expires_at_ms = verified.expires_at_ms.min(request.payload.expires_at_ms);
    Ok(verified)
}

pub fn verify_task_business_operation_receipt_read(
    primary: &VerifiedPolicySet,
    current: &VerifiedTaskCurrentPolicy,
    original_header: &Signed<TaskBusinessPolicyHeader>,
    original_wire: &SignedExecutionHandoff,
    actual_c_noise: &ActualPolicyNoisePeer,
    request: &Signed<TaskBusinessReceiptRead>,
    local_b: &PublicKey,
    now: u64,
) -> Result<VerifiedTaskBusinessReceiptAuthority> {
    let _signatures = ScopedSignatureVerification::new();
    let wire = TaskBusinessWire::Submit(original_wire.clone());
    let mut usage = verify_receipt_request(
        original_header,
        &wire,
        actual_c_noise,
        request,
        local_b,
        now,
    )?;
    let TaskBusinessChallenge::Submit(challenge) = &request.payload.challenge else {
        return Err(AuthorizationError::InvalidProof(
            "accepted operation requires its original handoff challenge",
        ));
    };
    require(
        matches!(
            challenge.stage,
            CheckStage::Start | CheckStage::Renew | CheckStage::RemoteRead
        ),
        "Task business admission receipts require their original Client session",
    )?;
    let acceptance =
        request
            .payload
            .acceptance
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "operation receipt reads require the executor's signed admission checkpoint",
            ))?;
    let operation = super::original_operation::verify_task_business_receipt_operation(
        current,
        original_header,
        original_wire,
        acceptance,
        challenge,
        now,
    )?;
    require(
        operation.acceptance_envelope_hash() == &ObjectHash::body(acceptance)?,
        "operation receipt read changed its verified executor checkpoint",
    )?;
    usage.merge(operation.proof_usage());
    usage.enforce_limits()?;
    let mut verified = authorize_inner(
        primary,
        current,
        TaskBusinessAuthorization::Submit(operation.authorization()),
        &wire,
        &request.payload.challenge,
        local_b,
        now,
        usage,
    )?;
    verified.expires_at_ms = verified.expires_at_ms.min(request.payload.expires_at_ms);
    Ok(verified)
}

fn verify_receipt_request(
    original_header: &Signed<TaskBusinessPolicyHeader>,
    original_wire: &TaskBusinessWire,
    actual_c_noise: &ActualPolicyNoisePeer,
    request: &Signed<TaskBusinessReceiptRead>,
    local_b: &PublicKey,
    now: u64,
) -> Result<ExecutionProofUsage> {
    carrier(request)?;
    carrier(original_header)?;
    window(
        request.payload.issued_at_ms,
        request.payload.expires_at_ms,
        now,
    )?;
    require(
        request.payload.version == AUTH_VERSION
            && original_header.payload.version == AUTH_VERSION
            && &original_header.issuer == local_b
            && &original_header.payload.scope.source_node_public_key == local_b
            && request.payload.source_header_hash == original_header.hash()?
            && request.payload.business_wire_hash
                == original_header.payload.scope.business_wire_hash
            && original_wire.scope()? == original_header.payload.scope
            && request.issuer == original_header.payload.scope.target_public_key
            && &request.issuer == actual_c_noise.public_key()
            && request.payload.issued_at_ms >= request.payload.challenge.issued_at_ms()
            && request.payload.expires_at_ms <= request.payload.challenge.expires_at_ms(),
        "Task business receipt read changed its actual executor, original wire or challenge",
    )?;
    original_header.verify_signature()?;
    request.verify_signature()?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_signed(original_header)?;
    usage.record_signed(request)?;
    Ok(usage)
}

fn authorize_inner(
    primary: &VerifiedPolicySet,
    current: &VerifiedTaskCurrentPolicy,
    authorization: TaskBusinessAuthorization<'_>,
    wire: &TaskBusinessWire,
    challenge: &TaskBusinessChallenge,
    local_b: &PublicKey,
    now: u64,
    mut usage: ExecutionProofUsage,
) -> Result<VerifiedTaskBusinessReceiptAuthority> {
    usage.merge(current.proof_usage());
    usage.enforce_limits()?;
    let scope = wire.scope()?;
    require(
        &scope.source_node_public_key == local_b,
        "Task business receipt requires the original source Node",
    )?;
    check_source_policy_scope(current, wire, &scope, now)?;
    let expires = challenge
        .verify_current(current, authorization, wire, now, &mut usage)?
        .min(current.expires_at_ms());
    require(
        now < expires,
        "Task business receipt current read authority expired",
    )?;
    let mut primary_heads = Vec::new();
    for head in current.primary_heads() {
        if primary.head(&head.domain) != Some(head) {
            return Err(AuthorizationError::NeedPolicySync(
                current.primary_heads().to_vec(),
            ));
        }
        primary_heads.push(head.clone());
    }
    let mut signing_heads = Vec::new();
    for head in challenge.policies() {
        if !primary_heads
            .iter()
            .any(|current| current.domain == head.domain)
        {
            continue;
        }
        if primary.head(&head.domain) != Some(head) {
            return Err(AuthorizationError::NeedPolicySync(primary_heads));
        }
        let authority = match &head.domain {
            PolicyDomain::Account(account) => {
                primary.account(account).map(|value| &value.authority)
            }
            PolicyDomain::Space(space) => primary.space(space).map(|value| &value.authority),
        }
        .ok_or(AuthorizationError::PolicyConflict)?;
        if &authority.public_key == local_b {
            require(
                authority.device == DeviceId::from_key(local_b)
                    && authority.service == ServiceId::from_key(local_b),
                "Task business receipt authority changed its service or device",
            )?;
            signing_heads.push(head.clone());
        }
    }
    finish_usage(&mut usage)?;
    Ok(VerifiedTaskBusinessReceiptAuthority {
        scope,
        challenge: challenge.clone(),
        challenge_hash: challenge.hash()?,
        primary_heads,
        signing_heads,
        local_b: local_b.clone(),
        verified_at_ms: now,
        expires_at_ms: expires,
        usage,
    })
}

impl VerifiedTaskBusinessReceiptAuthority {
    pub fn scope(&self) -> &TaskBusinessScope {
        &self.scope
    }
    pub fn challenge(&self) -> &TaskBusinessChallenge {
        &self.challenge
    }
    pub fn challenge_hash(&self) -> &ObjectHash {
        &self.challenge_hash
    }
    pub fn primary_heads(&self) -> &[PolicyStamp] {
        &self.primary_heads
    }
    pub fn signing_heads(&self) -> &[PolicyStamp] {
        &self.signing_heads
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }

    pub fn sign_receipts(
        &self,
        identity: &Identity,
        now: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        let _signatures = ScopedSignatureVerification::new();
        require(
            PublicKey::of(identity) == self.local_b
                && now == self.verified_at_ms
                && now < self.expires_at_ms,
            "Task business receipts require their freshly verified source authority",
        )?;
        let receipts = self
            .signing_heads
            .iter()
            .map(|head| {
                Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: head.clone(),
                        challenge_hash: self.challenge_hash.clone(),
                        issued_at_ms: now,
                        expires_at_ms: self.expires_at_ms,
                    },
                    identity,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        carrier(&receipts)?;
        let mut usage = self.usage.clone();
        for receipt in &receipts {
            usage.record_signed(receipt)?;
        }
        finish_usage(&mut usage)?;
        Ok(receipts)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{current_policy, noise_peer, original_authorization};
    use super::*;
    use crate::authorization::{
        client_policy_import::require_confirmed_client_policy,
        events::StoredTransport,
        execution_handoff::{
            ExecutionHandoffAdmissionReceipt, ExecutionHandoffFreshness,
            verify_execution_handoff_admission_checkpoint,
        },
        execution_observation::tests::{
            RetainedHistoryFixture, retained_history_fixture_with_execution_source,
        },
        node_execution_policy::{
            PolicyCarrierOrigin, VerifiedTaskExecutionTrust, begin_task_policy_import,
            task_policy_source_from_current,
        },
        remote_execution_history::RemoteExecutionHistoryAcceptance,
        remote_execution_policy::{
            RemoteExecutionPolicyPurpose, RemoteExecutionPolicySelection,
            tests::remote_policy_stream, verify_remote_execution_policy_delegation_source,
        },
    };

    pub(super) fn readonly_policy(
        fixture: &RetainedHistoryFixture,
        trust: &VerifiedTaskExecutionTrust,
        purpose: RemoteExecutionPolicyPurpose,
        now: u64,
    ) -> VerifiedTaskCurrentPolicy {
        let source = require_confirmed_client_policy(&fixture.source).unwrap();
        let leaf = fixture
            .state
            .grants
            .values()
            .find(|grant| {
                grant.payload.actions == [Action::ExecutionResolve, Action::OperationResolve]
                    && grant.payload.grantee
                        == Grantee::Actor(Actor::Service(ServiceId::from_key(&PublicKey::of(
                            &fixture.project_node,
                        ))))
            })
            .unwrap();
        let ResourceScope::Exact(resource) = &leaf.payload.scope else {
            panic!("fixture reader grant is not exact")
        };
        let selection = RemoteExecutionPolicySelection {
            execution_resource: resource.clone(),
            target_public_key: PublicKey::of(&fixture.executor),
            execution_grant: leaf.hash().unwrap(),
        };
        let mut context = leaf.payload.constraints.context.clone();
        context.workspace_resource = Some(
            trust
                .original_intent()
                .payload
                .source_selection
                .resource
                .resource
                .clone(),
        );
        let context = prepare::PrepareContext {
            context,
            binding_generation: Some(1),
            canonical_cwd: (purpose == RemoteExecutionPolicyPurpose::Resolver).then(|| {
                fixture.binding.binding.open_spec["cwd"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            }),
            execution_target_public_key: Some(PublicKey::of(&fixture.executor)),
        };
        let stream = remote_policy_stream(
            &fixture.source,
            &fixture.client,
            &fixture.project_node,
            &fixture.execution_bundle,
            &fixture.pins,
            &fixture.executor,
            &selection,
            &context,
            Action::OperationResolve,
            purpose,
            now,
        )
        .unwrap();
        let authority = verify_remote_execution_policy_delegation_source(
            source.state(),
            &stream.request.intent,
            &PublicKey::of(&fixture.project_node),
            &PublicKey::of(&fixture.client),
            now,
        )
        .unwrap();
        let source = task_policy_source_from_current(&authority, source.pins(), now).unwrap();
        let mut import = begin_task_policy_import(
            trust,
            &source,
            &stream.request,
            &stream.header,
            &[],
            PolicyCarrierOrigin::SignedTranscript,
            now,
        )
        .unwrap();
        for page in &stream.pages {
            import.append_page(page, now).unwrap();
        }
        import.finish_current(&stream.witness, now).unwrap()
    }

    pub(super) fn revoke_writer(
        fixture: &mut RetainedHistoryFixture,
        current: &VerifiedTaskCurrentPolicy,
    ) -> VerifiedPolicySet {
        let mut state = current.state().clone();
        let resource = &fixture.actual.handoff.handoff.execution_resource;
        let mut space = state.space(&resource.space).unwrap().clone();
        space.previous = state
            .head(&PolicyDomain::Space(resource.space.clone()))
            .cloned();
        space.sequence += 1;
        space
            .grants
            .retain(|hash| hash != &fixture.actual.handoff.invocation.payload.grant_leaf);
        let document = Signed::sign(space, &fixture.root).unwrap();
        state.install_space(document.clone()).unwrap();
        fixture.state.install_space(document.clone()).unwrap();
        fixture
            .execution_bundle
            .journal
            .push(JournalEntry::Space(document));
        state
    }

    #[test]
    fn task_business_observation_receipts_survive_revoked_writer_without_renewing_it() {
        let mut fixture = retained_history_fixture_with_execution_source();
        let now = fixture.invocation.payload.not_before_ms;
        let writer = current_policy(&fixture);
        let state = revoke_writer(&mut fixture, &writer);
        let readonly = readonly_policy(
            &fixture,
            writer.trust(),
            RemoteExecutionPolicyPurpose::ObservationRead,
            now,
        );
        let pair = readonly.selection().execution_grant.clone();
        let request = state
            .build_execution_observation_request(
                &fixture.actual.handoff,
                &fixture.client,
                &fixture.source.grant_leaf,
                &pair,
                "readonly-after-revocation",
                now,
                now + 500,
            )
            .unwrap();
        let wire = state
            .sign_execution_observation(
                &request,
                &fixture.actual.handoff,
                &fixture.project_node,
                now,
                now + 500,
            )
            .unwrap();
        let transport = TransportEvidence::VerifiedPeer {
            origin: DeviceId::from_key(&PublicKey::of(&fixture.project_node)),
        };
        let business = readonly
            .state()
            .verify_execution_observation_current(&wire, &transport, now)
            .unwrap();
        let challenge =
            ExecutionObservationChallenge::new(&business, Nonce::random().unwrap(), now, now + 400)
                .unwrap();
        let typed = TaskBusinessChallenge::Observe(challenge.clone());
        assert_eq!(typed.hash().unwrap(), challenge.hash().unwrap());
        let source = require_confirmed_client_policy(&fixture.source).unwrap();
        let business_wire = TaskBusinessWire::Observe(wire.clone());
        let authority = authorize_task_business_challenge(
            source.state(),
            &readonly,
            TaskBusinessAuthorization::Observe(&business),
            &business_wire,
            &typed,
            &PublicKey::of(&fixture.project_node),
            now,
        )
        .unwrap();
        let counts = authority.proof_usage().counts();
        let mut merged = authority.proof_usage().clone();
        merged.merge(readonly.proof_usage());
        assert_eq!(
            merged.counts().authorization_bytes,
            counts.authorization_bytes
        );
        assert_eq!(
            merged.counts().unique_signed_objects,
            counts.unique_signed_objects
        );
        assert_eq!(merged.counts().unique_signatures, counts.unique_signatures);
        assert!(
            counts.unique_signed_objects > business.proof_usage().counts().unique_signed_objects
        );
        let mut receipts = authority.sign_receipts(&fixture.project_node, now).unwrap();
        assert!(!receipts.is_empty());
        for head in &challenge.policies {
            if !authority.signing_heads().contains(head) {
                receipts.push(
                    Signed::sign(
                        FreshnessReceipt {
                            version: AUTH_VERSION,
                            current: head.clone(),
                            challenge_hash: challenge.hash().unwrap(),
                            issued_at_ms: now,
                            expires_at_ms: authority.expires_at_ms(),
                        },
                        &fixture.executor,
                    )
                    .unwrap(),
                );
            }
        }
        readonly
            .state()
            .verify_execution_observation_current_freshness(
                &business,
                &super::super::super::execution_observation::ExecutionObservationFreshness {
                    challenge: challenge.clone(),
                    receipts,
                },
                now,
            )
            .unwrap();
        assert!(
            !readonly
                .state()
                .space(&fixture.actual.handoff.handoff.execution_resource.space)
                .unwrap()
                .grants
                .contains(&fixture.actual.handoff.invocation.payload.grant_leaf)
        );
        let mut wrong = challenge.clone();
        wrong.observation_hash = ObjectHash::body(&"other observation").unwrap();
        assert!(
            authorize_task_business_challenge(
                source.state(),
                &readonly,
                TaskBusinessAuthorization::Observe(&business),
                &business_wire,
                &TaskBusinessChallenge::Observe(wrong),
                &PublicKey::of(&fixture.project_node),
                now
            )
            .is_err()
        );
        let submit_challenge = TaskBusinessChallenge::Submit(ExecutionHandoffChallenge {
            version: AUTH_VERSION,
            stage: CheckStage::Renew,
            handoff_hash: fixture.actual.handoff.hash().unwrap(),
            original_invocation_hash: fixture.invocation.hash().unwrap(),
            submission_invocation_hash: fixture.actual.handoff.invocation.hash().unwrap(),
            original_body_hash: fixture.invocation.payload.body_hash.clone(),
            original_request_hash: ObjectHash::body(&fixture.body["request"]).unwrap(),
            original_resource: fixture.invocation.payload.resource.clone(),
            execution_resource: fixture.actual.handoff.handoff.execution_resource.clone(),
            execution_audience: fixture.actual.handoff.handoff.execution_audience.clone(),
            target_public_key: PublicKey::of(&fixture.executor),
            project_presenter: fixture.actual.handoff.invocation.payload.presenter.clone(),
            operation_id: fixture.invocation.payload.operation_id.clone(),
            binding_hash: task_binding::binding_hash(&fixture.binding.binding).unwrap(),
            binding_generation: 1,
            execution_session: fixture
                .actual
                .handoff
                .invocation
                .payload
                .context
                .execution_session
                .clone()
                .unwrap(),
            policies: challenge.policies.clone(),
            nonce: Nonce::random().unwrap(),
            issued_at_ms: now,
            expires_at_ms: now + 300,
        });
        assert!(
            authorize_task_business_challenge(
                source.state(),
                &readonly,
                TaskBusinessAuthorization::Observe(&business),
                &business_wire,
                &submit_challenge,
                &PublicKey::of(&fixture.project_node),
                now
            )
            .is_err()
        );
        let export = build_task_business_policy_export(
            &readonly,
            TaskBusinessAuthorization::Observe(&business),
            &business_wire,
            &fixture.project_node,
            now,
        )
        .unwrap();
        let actual_c = noise_peer(&fixture.executor, &fixture.project_node);
        let read = Signed::sign(
            TaskBusinessReceiptRead {
                version: AUTH_VERSION,
                source_header_hash: export.header().hash().unwrap(),
                business_wire_hash: business_wire.hash().unwrap(),
                challenge: typed,
                acceptance: None,
                nonce: Nonce::random().unwrap(),
                issued_at_ms: now,
                expires_at_ms: now + 300,
            },
            &fixture.executor,
        )
        .unwrap();
        verify_task_business_receipt_read(
            source.state(),
            &readonly,
            TaskBusinessAuthorization::Observe(&business),
            export.header(),
            &business_wire,
            &actual_c,
            &read,
            &PublicKey::of(&fixture.project_node),
            now,
        )
        .unwrap();
    }

    #[test]
    fn task_business_receipts_keep_typed_anchors_and_require_current_writer() {
        let fixture = retained_history_fixture_with_execution_source();
        let now = fixture.invocation.payload.not_before_ms;
        let current = current_policy(&fixture);
        let source = require_confirmed_client_policy(&fixture.source).unwrap();
        let original = original_authorization(current.state(), &fixture.actual.handoff, now);
        let transport = TransportEvidence::VerifiedPeer {
            origin: DeviceId::from_key(&PublicKey::of(&fixture.project_node)),
        };
        let business = current
            .state()
            .verify_execution_handoff(&fixture.actual.handoff, &original, &transport, None, now)
            .unwrap();
        let wire = TaskBusinessWire::Submit(fixture.actual.handoff.clone());
        let challenge = ExecutionHandoffChallenge::new(
            &business,
            CheckStage::Admit,
            Nonce::random().unwrap(),
            now,
            now + 500,
        )
        .unwrap();
        let typed = TaskBusinessChallenge::Submit(challenge.clone());
        assert_eq!(typed.hash().unwrap(), challenge.hash().unwrap());
        assert_ne!(typed.hash().unwrap(), ObjectHash::body(&typed).unwrap());
        let b_key = PublicKey::of(&fixture.project_node);
        let authority = authorize_task_business_challenge(
            source.state(),
            &current,
            TaskBusinessAuthorization::Submit(&business),
            &wire,
            &typed,
            &b_key,
            now,
        )
        .unwrap();
        let mut receipts = authority.sign_receipts(&fixture.project_node, now).unwrap();
        assert!(!receipts.is_empty());
        assert!(receipts.iter().all(|receipt| receipt.issuer == b_key
            && receipt.payload.challenge_hash == challenge.hash().unwrap()));
        assert!(authority.sign_receipts(&fixture.executor, now).is_err());
        assert!(
            authority
                .sign_receipts(&fixture.project_node, now + 1)
                .is_err()
        );
        for head in &challenge.policies {
            if !authority.signing_heads().contains(head) {
                let node = match &head.domain {
                    PolicyDomain::Account(id) => &current.state().account(id).unwrap().authority,
                    PolicyDomain::Space(id) => &current.state().space(id).unwrap().authority,
                };
                assert_eq!(node.public_key, PublicKey::of(&fixture.executor));
                receipts.push(
                    Signed::sign(
                        FreshnessReceipt {
                            version: AUTH_VERSION,
                            current: head.clone(),
                            challenge_hash: challenge.hash().unwrap(),
                            issued_at_ms: now,
                            expires_at_ms: authority.expires_at_ms(),
                        },
                        &fixture.executor,
                    )
                    .unwrap(),
                );
            }
        }
        let accepted_freshness = ExecutionHandoffFreshness {
            challenge: challenge.clone(),
            receipts,
        };
        let freshness = current
            .state()
            .verify_execution_handoff_freshness(&business, &accepted_freshness, now)
            .unwrap();
        current
            .state()
            .admit_execution_handoff(&business, &freshness, now)
            .unwrap();
        let accepted_transport = StoredTransport::from(&transport);
        let acceptance = RemoteExecutionHistoryAcceptance::ExecutionResult {
            receipt: Signed::sign(
                ExecutionHandoffAdmissionReceipt::new(
                    &fixture.actual.handoff,
                    &accepted_freshness,
                    &accepted_transport,
                    now,
                )
                .unwrap(),
                &fixture.executor,
            )
            .unwrap(),
            freshness: accepted_freshness,
            transport: accepted_transport,
        };
        let mut changed = challenge.clone();
        changed.original_body_hash = ObjectHash::body(&"another original body").unwrap();
        assert!(
            authorize_task_business_challenge(
                source.state(),
                &current,
                TaskBusinessAuthorization::Submit(&business),
                &wire,
                &TaskBusinessChallenge::Submit(changed),
                &b_key,
                now
            )
            .is_err()
        );
        let wrong_type = TaskBusinessChallenge::Observe(ExecutionObservationChallenge {
            version: AUTH_VERSION,
            stage: CheckStage::RemoteRead,
            observation_hash: challenge.handoff_hash.clone(),
            request_hash: challenge.original_request_hash.clone(),
            submission_invocation_hash: challenge.submission_invocation_hash.clone(),
            policies: challenge.policies.clone(),
            nonce: challenge.nonce.clone(),
            issued_at_ms: now,
            expires_at_ms: now + 500,
        });
        assert!(
            authorize_task_business_challenge(
                source.state(),
                &current,
                TaskBusinessAuthorization::Submit(&business),
                &wire,
                &wrong_type,
                &b_key,
                now
            )
            .is_err()
        );
        let export = build_task_business_policy_export(
            &current,
            TaskBusinessAuthorization::Submit(&business),
            &wire,
            &fixture.project_node,
            now,
        )
        .unwrap();
        let mut expired_header = export.header().payload.clone();
        expired_header.expires_at_ms = now + 1;
        let expired_header = Signed::sign(expired_header, &fixture.project_node).unwrap();
        let RemoteExecutionHistoryAcceptance::ExecutionResult {
            receipt,
            freshness,
            transport: accepted_transport,
        } = &acceptance
        else {
            panic!("fixture has no accepted handoff")
        };
        let admitted = verify_execution_handoff_admission_checkpoint(
            &fixture.actual.handoff,
            receipt,
            freshness,
            accepted_transport,
            receipt.payload.admitted_at_ms,
        )
        .unwrap();
        let operation = current
            .state()
            .verify_execution_handoff(
                &fixture.actual.handoff,
                &original,
                &transport,
                Some(&admitted),
                now,
            )
            .unwrap();
        let renew = TaskBusinessChallenge::Submit(
            ExecutionHandoffChallenge::new(
                &operation,
                CheckStage::Renew,
                Nonce::random().unwrap(),
                now + 5,
                now + 500,
            )
            .unwrap(),
        );
        let read = Signed::sign(
            TaskBusinessReceiptRead {
                version: AUTH_VERSION,
                source_header_hash: expired_header.hash().unwrap(),
                business_wire_hash: wire.hash().unwrap(),
                challenge: renew.clone(),
                acceptance: Some(acceptance.clone()),
                nonce: Nonce::random().unwrap(),
                issued_at_ms: now + 6,
                expires_at_ms: now + 400,
            },
            &fixture.executor,
        )
        .unwrap();
        let actual_c = noise_peer(&fixture.executor, &fixture.project_node);
        let renewed = verify_task_business_receipt_read(
            source.state(),
            &current,
            TaskBusinessAuthorization::Submit(&operation),
            &expired_header,
            &wire,
            &actual_c,
            &read,
            &b_key,
            now + 10,
        )
        .unwrap();
        assert!(expired_header.payload.expires_at_ms < now + 10);
        assert_eq!(renewed.challenge_hash(), &renew.hash().unwrap());
        assert!(
            !renewed
                .sign_receipts(&fixture.project_node, now + 10)
                .unwrap()
                .is_empty()
        );
        let wrong_peer = noise_peer(&fixture.client, &fixture.project_node);
        assert!(
            verify_task_business_receipt_read(
                source.state(),
                &current,
                TaskBusinessAuthorization::Submit(&operation),
                &expired_header,
                &wire,
                &wrong_peer,
                &read,
                &b_key,
                now + 10
            )
            .is_err()
        );
        let mut wrong_read = read.payload.clone();
        wrong_read.business_wire_hash = ObjectHash::body(&"other wire").unwrap();
        let wrong_read = Signed::sign(wrong_read, &fixture.executor).unwrap();
        assert!(
            verify_task_business_receipt_read(
                source.state(),
                &current,
                TaskBusinessAuthorization::Submit(&operation),
                &expired_header,
                &wire,
                &actual_c,
                &wrong_read,
                &b_key,
                now + 10
            )
            .is_err()
        );
        let mut old_heads = read.payload.clone();
        if let TaskBusinessChallenge::Submit(challenge) = &mut old_heads.challenge {
            challenge.policies[0].sequence += 1;
        }
        let old_heads = Signed::sign(old_heads, &fixture.executor).unwrap();
        assert!(matches!(
            verify_task_business_receipt_read(
                source.state(),
                &current,
                TaskBusinessAuthorization::Submit(&operation),
                &expired_header,
                &wire,
                &actual_c,
                &old_heads,
                &b_key,
                now + 10
            ),
            Err(AuthorizationError::NeedPolicySync(_))
        ));
    }
}
