//! Read-only reconciliation of one locally retained execution handoff.
use super::{events::*, execution_handoff::*, task_binding::SignedBinding, *};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionObservationPurpose {
    Observe,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionObservationTarget {
    pub source: ProjectExecutionSource,
    pub handoff_hash: ObjectHash,
    pub original_body_hash: ObjectHash,
    pub original_request_hash: ObjectHash,
    pub conversation_id: String,
    pub task_id: String,
    pub executor_kind: String,
    pub execution_resource: ResourceRef,
    pub execution_audience: Audience,
    pub target_public_key: PublicKey,
}

fn shape_error(_: impl std::fmt::Display) -> AuthorizationError {
    AuthorizationError::InvalidProof("invalid execution observation source")
}

impl ExecutionObservationTarget {
    pub fn from_original(original: &SignedExecutionHandoff) -> Result<Self> {
        let handoff = &original.handoff;
        let source = handoff.source_statement()?;
        let event: Event =
            serde_json::from_str(&handoff.project_dispatch.canonical_body).map_err(shape_error)?;
        let EventCause::Invocation { admission } = &event.cause else {
            return Err(AuthorizationError::InvalidProof(
                "observation has no admitted source",
            ));
        };
        let request = &admission.source_body["request"];
        let binding: SignedBinding =
            serde_json::from_value(request["binding"].clone()).map_err(shape_error)?;
        let original_invocation = &admission.invocation.payload;
        let expected_action = match request["action"]["type"].as_str() {
            Some("open") => Action::ExecutionOpen,
            Some("read") => Action::OperationStatusRead,
            Some("send") => Action::ExecutionSend,
            Some("steer") => Action::ExecutionSteer,
            Some("interrupt") => Action::ExecutionInterrupt,
            Some("respond_request") => Action::ExecutionRespondApproval,
            _ => {
                return Err(AuthorizationError::InvalidProof(
                    "unsupported observed execution action",
                ));
            }
        };
        let executor_kind = binding.binding.open_spec["executor"]
            .as_str()
            .ok_or(AuthorizationError::InvalidProof(
                "missing observed executor",
            ))?
            .to_owned();
        let context = InvocationContext {
            conversation: Some(event.scope_id.clone()),
            task: Some(source.task.clone()),
            sdk_session: original_invocation.context.sdk_session.clone(),
            execution_session: Some(source.execution_session.clone()),
            executor_kind: Some(executor_kind.clone()),
            workspace_resource: Some(source.project.resource.clone()),
        };
        let source_signature =
            handoff
                .source_attestation
                .as_ref()
                .ok_or(AuthorizationError::InvalidProof(
                    "missing observed Project source signature",
                ))?;
        let source_device = DeviceId::from_key(&source_signature.issuer);
        let source_service = ServiceId::from_key(&source_signature.issuer);
        let mut carrier_context = context.clone();
        carrier_context.sdk_session = None;
        carrier_context.workspace_resource = Some(handoff.execution_resource.resource.clone());
        let carrier = &original.invocation.payload;
        require(
            original_invocation.version == AUTH_VERSION
                && carrier.version == AUTH_VERSION
                && original_invocation.resource == source.project
                && original_invocation.audience.service == source_service
                && original_invocation.audience.device == source_device
                && original_invocation.operation_id == source.operation_id
                && request["operation_id"].as_str()
                    == Some(original_invocation.operation_id.as_str())
                && original_invocation.body_hash == ObjectHash::body(&admission.source_body)?
                && original_invocation.action == expected_action
                && original_invocation.context == context
                && binding.binding.target_node
                    == node_fingerprint(handoff.target_public_key.as_str()).map_err(shape_error)?
                && handoff.execution_audience.device
                    == DeviceId::from_key(&handoff.target_public_key)
                && handoff.execution_audience.service
                    == ServiceId::from_key(&handoff.target_public_key)
                && admission.source_body["execution_resource"]
                    == serde_json::to_value(&handoff.execution_resource).map_err(shape_error)?
                && admission.source_body["target_public_key"]
                    == serde_json::to_value(&handoff.target_public_key).map_err(shape_error)?
                && admission.source_body["execution_grant"].as_str()
                    == Some(carrier.grant_leaf.as_str())
                && original.invocation.issuer == source_signature.issuer
                && carrier.actor == Actor::Service(source_service)
                && carrier.device == source_device
                && carrier.presenter == source_device
                && carrier.resource == handoff.execution_resource
                && carrier.audience == handoff.execution_audience
                && carrier.operation_id == source.operation_id
                && carrier.action == Action::ExecutionSubmit
                && carrier.body_hash == ObjectHash::body(handoff)?
                && carrier.context == carrier_context,
            "observation source differs from the original accepted operation",
        )?;
        original.invocation.verify_signature()?;
        Ok(Self {
            source,
            handoff_hash: original.hash()?,
            original_body_hash: original_invocation.body_hash.clone(),
            original_request_hash: ObjectHash::body(request)?,
            conversation_id: event.scope_id,
            task_id: binding.binding.task_id,
            executor_kind,
            execution_resource: handoff.execution_resource.clone(),
            execution_audience: handoff.execution_audience.clone(),
            target_public_key: handoff.target_public_key.clone(),
        })
    }

    pub(super) fn context(&self, workspace: &ResourceId) -> InvocationContext {
        InvocationContext {
            conversation: Some(self.conversation_id.clone()),
            task: Some(self.source.task.clone()),
            sdk_session: None,
            execution_session: Some(self.source.execution_session.clone()),
            executor_kind: Some(self.executor_kind.clone()),
            workspace_resource: Some(workspace.clone()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionObservationIntent {
    pub version: u16,
    pub purpose: ExecutionObservationPurpose,
    pub target: ExecutionObservationTarget,
    pub execution_grant: ObjectHash,
    pub authorization_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionObservationRequest {
    pub intent: ExecutionObservationIntent,
    pub request: Invocation,
}
impl sealed::Sealed for ExecutionObservationRequest {}
impl SignedObject for ExecutionObservationRequest {
    const DOMAIN: &'static str = "execution-observation-request-v2";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SignedExecutionObservation {
    pub request: Signed<ExecutionObservationRequest>,
    pub invocation: Signed<Invocation>,
}
impl SignedExecutionObservation {
    pub fn hash(&self) -> Result<ObjectHash> {
        require(
            canonical(self)?.len() <= MAX_PROOF_BYTES,
            "observation request is too large",
        )?;
        ObjectHash::body(self)
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedExecutionObservationRequest {
    target: ExecutionObservationTarget,
    request_hash: ObjectHash,
    account_facts: RequestFacts,
    submission_facts: RequestFacts,
    resolver: VerifiedAuthorization,
    usage: ExecutionProofUsage,
}
impl VerifiedExecutionObservationRequest {
    pub fn target(&self) -> &ExecutionObservationTarget {
        &self.target
    }
    pub fn account_facts(&self) -> &RequestFacts {
        &self.account_facts
    }
    pub fn submission_facts(&self) -> &RequestFacts {
        &self.submission_facts
    }
    pub fn heads(&self) -> &[PolicyStamp] {
        self.resolver.dependencies()
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.resolver.expires_at_ms()
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedExecutionObservationCurrent {
    hash: ObjectHash,
    request: VerifiedExecutionObservationRequest,
    submission: VerifiedAuthorization,
    heads: Vec<PolicyStamp>,
    usage: ExecutionProofUsage,
    verified_at_ms: u64,
}
impl VerifiedExecutionObservationCurrent {
    pub fn target(&self) -> &ExecutionObservationTarget {
        self.request.target()
    }
    pub fn hash(&self) -> &ObjectHash {
        &self.hash
    }
    pub fn request_hash(&self) -> &ObjectHash {
        &self.request.request_hash
    }
    pub fn heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.request
            .expires_at_ms()
            .min(self.submission.expires_at_ms())
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedExecutionObservation {
    current: VerifiedExecutionObservationCurrent,
    usage: ExecutionProofUsage,
}
impl VerifiedExecutionObservation {
    pub fn current(&self) -> &VerifiedExecutionObservationCurrent {
        &self.current
    }
    pub fn target(&self) -> &ExecutionObservationTarget {
        self.current.target()
    }
    pub fn hash(&self) -> &ObjectHash {
        self.current.hash()
    }
    pub fn request_hash(&self) -> &ObjectHash {
        self.current.request_hash()
    }
    pub fn heads(&self) -> &[PolicyStamp] {
        self.current.heads()
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.current.expires_at_ms()
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionObservationChallenge {
    pub version: u16,
    pub stage: CheckStage,
    pub observation_hash: ObjectHash,
    pub request_hash: ObjectHash,
    pub submission_invocation_hash: ObjectHash,
    pub policies: Vec<PolicyStamp>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for ExecutionObservationChallenge {}
impl SignedObject for ExecutionObservationChallenge {
    const DOMAIN: &'static str = "execution-observation-challenge-v2";
}
impl ExecutionObservationChallenge {
    pub fn new(
        value: &VerifiedExecutionObservationCurrent,
        nonce: Nonce,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Self> {
        require(
            issued_at_ms >= value.verified_at_ms,
            "observation challenge clock moved backwards",
        )?;
        Self::bound_to(value, nonce, issued_at_ms, expires_at_ms)
    }

    fn bound_to(
        value: &VerifiedExecutionObservationCurrent,
        nonce: Nonce,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Self> {
        validate_window(issued_at_ms, expires_at_ms, issued_at_ms)?;
        require(
            expires_at_ms - issued_at_ms <= REMOTE_READ_MAX_AGE_MS,
            "observation challenge is too long",
        )?;
        Ok(Self {
            version: AUTH_VERSION,
            stage: CheckStage::RemoteRead,
            observation_hash: value.hash.clone(),
            request_hash: value.request.request_hash.clone(),
            submission_invocation_hash: value.submission.invocation_hash().clone(),
            policies: value.heads.clone(),
            nonce,
            issued_at_ms,
            expires_at_ms,
        })
    }
    pub fn hash(&self) -> Result<ObjectHash> {
        Ok(ObjectHash(format!(
            "h2_{}",
            digest(Self::DOMAIN, &canonical(self)?)
        )))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionObservationFreshness {
    pub challenge: ExecutionObservationChallenge,
    pub receipts: Vec<Signed<FreshnessReceipt>>,
}

#[derive(Clone, Debug)]
pub struct VerifiedExecutionObservationRead {
    target: ExecutionObservationTarget,
    observation_hash: ObjectHash,
    request_hash: ObjectHash,
    expires_at_ms: u64,
    usage: ExecutionProofUsage,
}
impl VerifiedExecutionObservationRead {
    pub fn target(&self) -> &ExecutionObservationTarget {
        &self.target
    }
    pub fn observation_hash(&self) -> &ObjectHash {
        &self.observation_hash
    }
    pub fn request_hash(&self) -> &ObjectHash {
        &self.request_hash
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
}

impl VerifiedPolicySet {
    pub fn build_forwarded_execution_observation_request(
        &self,
        original: &SignedExecutionHandoff,
        signer: &Identity,
        account_grant: &ObjectHash,
        execution_grant: &ObjectHash,
        authorization_id: &str,
        gateway: &PublicKey,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Signed<ExecutionObservationRequest>> {
        require(
            gateway != &PublicKey::of(signer),
            "gateway cannot be the Account signer",
        )?;
        let direct = self.build_execution_observation_request(
            original,
            signer,
            account_grant,
            execution_grant,
            authorization_id,
            now_ms,
            expires_at_ms,
        )?;
        let presenter = DeviceId::from_key(gateway);
        if presenter == direct.payload.request.audience.device {
            return Ok(direct);
        }
        let mut budget = Budget::default();
        let mut dependencies = direct
            .payload
            .request
            .expected_policies
            .iter()
            .map(|stamp| (stamp.domain.clone(), stamp.clone()))
            .collect();
        self.validate_presenter(&presenter, now_ms, &mut budget, &mut dependencies)?;
        self.expand_authorities(now_ms, &mut budget, &mut dependencies)?;
        require(
            dependencies.len() <= MAX_POLICY_DOMAINS,
            "observation has too many policy domains",
        )?;
        let mut payload = direct.payload;
        payload.request.presenter = presenter;
        payload.request.expected_policies = dependencies.into_values().collect();
        payload.request.expires_at_ms = payload.request.expires_at_ms.min(budget.expires_at_ms);
        validate_window(now_ms, payload.request.expires_at_ms, now_ms)?;
        let signed = Signed::sign(payload, signer)?;
        self.verify_execution_observation_request(&signed, original, now_ms)?;
        Ok(signed)
    }

    pub fn build_execution_observation_request(
        &self,
        original: &SignedExecutionHandoff,
        signer: &Identity,
        account_grant: &ObjectHash,
        execution_grant: &ObjectHash,
        authorization_id: &str,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Signed<ExecutionObservationRequest>> {
        let intent = ExecutionObservationIntent {
            version: AUTH_VERSION,
            purpose: ExecutionObservationPurpose::Observe,
            target: ExecutionObservationTarget::from_original(original)?,
            execution_grant: execution_grant.clone(),
            authorization_id: authorization_id.to_owned(),
        };
        let source = original.handoff.source_attestation.as_ref().ok_or(
            AuthorizationError::InvalidProof("missing observation Project source"),
        )?;
        let facts = self.observation_account_facts(
            &intent,
            &Audience {
                service: ServiceId::from_key(&source.issuer),
                device: DeviceId::from_key(&source.issuer),
            },
        )?;
        let mut budget = Budget::default();
        let mut dependencies = BTreeMap::new();
        let actor = self.actor_for_signer(
            &PublicKey::of(signer),
            now_ms,
            &mut budget,
            &mut dependencies,
        )?;
        require(
            matches!(actor, Actor::Account(_)),
            "observation signer must be an Account device",
        )?;
        for device in [
            &facts.audience.device,
            &intent.target.source.execution_session.target,
        ] {
            let certificate = self.find_device(device)?;
            require(
                matches!(certificate.payload.subject, Actor::Service(_))
                    && certificate.payload.uses.contains(&DeviceUse::Noise),
                "observation target is not a Noise service",
            )?;
            self.validate_certificate(
                certificate,
                now_ms,
                &mut budget,
                &mut dependencies,
                &mut BTreeSet::new(),
            )?;
        }
        let resource = self
            .resource(&facts.resource.space, &facts.resource.resource)
            .ok_or(AuthorizationError::InvalidProof(
                "missing observation Project",
            ))?;
        budget.verify(resource)?;
        let audience = self.find_device(&facts.audience.device)?;
        self.validate_service_reference(&facts.resource.space, audience)?;
        let path = self.grant_chain(Some(account_grant))?;
        self.verify_grant_path(&path, now_ms, &mut budget, &mut dependencies, true)?;
        let leaf = &path
            .last()
            .ok_or(AuthorizationError::InvalidProof(
                "missing Account observation grant",
            ))?
            .payload;
        require(
            scope_covers(&leaf.scope, &facts.resource)
                && leaf.actions.contains(&Action::OperationResolve)
                && leaf.audience == facts.audience
                && self.matches_grantee(
                    &leaf.grantee,
                    &actor,
                    now_ms,
                    &mut budget,
                    &mut dependencies,
                )?
                && context_tighter(&leaf.constraints.context, &facts.context),
            "Account grant does not authorize this observation",
        )?;
        if leaf.constraints.require_webauthn_uv {
            return Err(AuthorizationError::UserPresenceRequired);
        }
        self.expand_authorities(now_ms, &mut budget, &mut dependencies)?;
        let maximum =
            now_ms
                .checked_add(INVOCATION_MAX_AGE_MS)
                .ok_or(AuthorizationError::InvalidProof(
                    "observation expiry overflow",
                ))?;
        let expires_at_ms = expires_at_ms.min(maximum).min(budget.expires_at_ms);
        validate_window(now_ms, expires_at_ms, now_ms)?;
        let device = DeviceId::from_key(&PublicKey::of(signer));
        let signed = Signed::sign(
            ExecutionObservationRequest {
                intent,
                request: Invocation {
                    version: AUTH_VERSION,
                    actor: actor.clone(),
                    device: device.clone(),
                    initiator: actor,
                    initiating_grant: None,
                    resource: facts.resource,
                    action: facts.action,
                    audience: facts.audience,
                    presenter: device,
                    operation_id: facts.operation_id,
                    body_hash: facts.body_hash,
                    grant_leaf: account_grant.clone(),
                    expected_policies: dependencies.into_values().collect(),
                    context: facts.context,
                    nonce: Nonce::random()?,
                    not_before_ms: now_ms,
                    expires_at_ms,
                },
            },
            signer,
        )?;
        self.verify_execution_observation_request(&signed, original, now_ms)?;
        Ok(signed)
    }

    pub fn sign_execution_observation(
        &self,
        request: &Signed<ExecutionObservationRequest>,
        original: &SignedExecutionHandoff,
        signer: &Identity,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<SignedExecutionObservation> {
        let verified = self.verify_execution_observation_request(request, original, now_ms)?;
        let facts = verified.submission_facts();
        let service = &verified.account_facts().audience.service;
        let proof = self.verify_service_grant(
            service,
            &request.payload.intent.execution_grant,
            facts,
            now_ms,
        )?;
        require(
            proof.device() == &DeviceId::from_key(&PublicKey::of(signer)),
            "observation signer is not the Project host",
        )?;
        let maximum =
            now_ms
                .checked_add(INVOCATION_MAX_AGE_MS)
                .ok_or(AuthorizationError::InvalidProof(
                    "observation expiry overflow",
                ))?;
        let expires_at_ms = expires_at_ms
            .min(maximum)
            .min(verified.expires_at_ms())
            .min(proof.expires_at_ms());
        validate_window(now_ms, expires_at_ms, now_ms)?;
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Service(service.clone()),
                device: proof.device().clone(),
                initiator: proof.initiator().clone(),
                initiating_grant: proof.initiating_grant().cloned(),
                resource: facts.resource.clone(),
                action: facts.action,
                audience: facts.audience.clone(),
                presenter: proof.device().clone(),
                operation_id: facts.operation_id.clone(),
                body_hash: facts.body_hash.clone(),
                grant_leaf: proof.grant_leaf().clone(),
                expected_policies: proof.heads().to_vec(),
                context: facts.context.clone(),
                nonce: Nonce::random()?,
                not_before_ms: now_ms,
                expires_at_ms,
            },
            signer,
        )?;
        let wire = SignedExecutionObservation {
            request: request.clone(),
            invocation,
        };
        self.verify_execution_observation(
            &wire,
            original,
            &TransportEvidence::VerifiedPeer {
                origin: proof.device().clone(),
            },
            now_ms,
        )?;
        Ok(wire)
    }

    fn observation_account_facts(
        &self,
        intent: &ExecutionObservationIntent,
        audience: &Audience,
    ) -> Result<RequestFacts> {
        require(
            intent.version == AUTH_VERSION
                && intent.purpose == ExecutionObservationPurpose::Observe
                && valid_operation_id(&intent.authorization_id)
                && intent.target.source.version == AUTH_VERSION
                && valid_operation_id(&intent.target.source.operation_id)
                && intent.target.source.binding_generation > 0
                && lower_hex(&intent.target.source.binding_hash, 32)
                && !intent.target.task_id.is_empty()
                && intent.target.task_id.len() <= 512
                && intent.target.source.execution_session.target
                    == intent.target.execution_audience.device
                && intent.target.execution_audience.device
                    == DeviceId::from_key(&intent.target.target_public_key)
                && intent.target.execution_audience.service
                    == ServiceId::from_key(&intent.target.target_public_key)
                && intent.authorization_id != intent.target.source.operation_id,
            "invalid exact execution observation intent",
        )?;
        let target = &intent.target;
        let project = self
            .resource(
                &target.source.project.space,
                &target.source.project.resource,
            )
            .ok_or(AuthorizationError::InvalidProof(
                "missing observation Project",
            ))?;
        let certificate = self.find_device(&audience.device)?;
        require(
            project.payload.kind == ResourceKind::Project
                && project.payload.host == audience.service
                && certificate.payload.subject == Actor::Service(project.payload.host.clone()),
            "observation Project host changed",
        )?;
        Ok(RequestFacts {
            resource: target.source.project.clone(),
            action: Action::OperationResolve,
            audience: audience.clone(),
            operation_id: intent.authorization_id.clone(),
            body_hash: ObjectHash::body(intent)?,
            context: target.context(&target.source.project.resource),
        })
    }

    pub fn verify_execution_observation_request(
        &self,
        request: &Signed<ExecutionObservationRequest>,
        original: &SignedExecutionHandoff,
        now_ms: u64,
    ) -> Result<VerifiedExecutionObservationRequest> {
        let mut verified = self.verify_execution_observation_current_request(request, now_ms)?;
        let source =
            self.observation_source_usage(&request.payload.intent.target, original, now_ms)?;
        verified.usage.merge(&source);
        verified.usage.enforce_limits()?;
        Ok(verified)
    }

    fn observation_source_usage(
        &self,
        target: &ExecutionObservationTarget,
        original: &SignedExecutionHandoff,
        now_ms: u64,
    ) -> Result<ExecutionProofUsage> {
        require(
            *target == ExecutionObservationTarget::from_original(original)?,
            "observation intent differs from the local original handoff",
        )?;
        let source = self.verify_execution_observation_source(&original.handoff, now_ms)?;
        let mut usage = source.proof_usage().clone();
        usage.record_signed(&original.invocation)?;
        usage.record_historical_handoff(original)?;
        usage.enforce_limits()?;
        Ok(usage)
    }

    fn verify_execution_observation_current_request(
        &self,
        request: &Signed<ExecutionObservationRequest>,
        now_ms: u64,
    ) -> Result<VerifiedExecutionObservationRequest> {
        let mut budget = Budget::default();
        let intent_hash = budget.verify(request)?;
        let account_facts = self.observation_account_facts(
            &request.payload.intent,
            &request.payload.request.audience,
        )?;
        let shape = Signed {
            payload: request.payload.request.clone(),
            issuer: request.issuer.clone(),
            signature: request.signature.clone(),
        };
        let resolver = self.verify_portable_signed_intent_body(
            &shape,
            intent_hash,
            budget,
            &account_facts,
            now_ms,
        )?;
        let target = &request.payload.intent.target;
        let request_hash = ObjectHash::body(request)?;
        let submission_facts = RequestFacts {
            resource: target.execution_resource.clone(),
            action: Action::ExecutionResolve,
            audience: target.execution_audience.clone(),
            operation_id: account_facts.operation_id.clone(),
            body_hash: request_hash.clone(),
            context: target.context(&target.execution_resource.resource),
        };
        let mut usage = ExecutionProofUsage::default();
        usage.record_metadata(target)?;
        usage.record_authorization(&resolver)?;
        usage.enforce_limits()?;
        Ok(VerifiedExecutionObservationRequest {
            target: target.clone(),
            request_hash,
            account_facts,
            submission_facts,
            resolver,
            usage,
        })
    }

    pub fn verify_execution_observation(
        &self,
        wire: &SignedExecutionObservation,
        original: &SignedExecutionHandoff,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<VerifiedExecutionObservation> {
        let current = self.verify_execution_observation_current(wire, transport, now_ms)?;
        let mut usage = current.usage.clone();
        usage.merge(&self.observation_source_usage(current.target(), original, now_ms)?);
        usage.enforce_limits()?;
        Ok(VerifiedExecutionObservation { current, usage })
    }

    pub fn verify_execution_observation_current(
        &self,
        wire: &SignedExecutionObservation,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<VerifiedExecutionObservationCurrent> {
        let request = self.verify_execution_observation_current_request(&wire.request, now_ms)?;
        let facts = request.submission_facts();
        let project = self
            .resource(
                &request.account_facts.resource.space,
                &request.account_facts.resource.resource,
            )
            .ok_or(AuthorizationError::InvalidProof(
                "missing observation Project",
            ))?;
        let execution = self
            .resource(&facts.resource.space, &facts.resource.resource)
            .ok_or(AuthorizationError::InvalidProof(
                "missing observation ExecutionService",
            ))?;
        let project_device = self.find_device(&request.account_facts.audience.device)?;
        let target = self.find_device(&facts.audience.device)?;
        require(
            execution.payload.kind == ResourceKind::ExecutionService
                && execution.payload.host == facts.audience.service
                && project.payload.host != execution.payload.host
                && wire.invocation.payload.actor == Actor::Service(project.payload.host.clone())
                && wire.invocation.payload.device == request.account_facts.audience.device
                && wire.invocation.payload.presenter == request.account_facts.audience.device
                && wire.invocation.issuer == project_device.payload.public_key
                && project_device
                    .payload
                    .uses
                    .contains(&DeviceUse::ServiceInvocation)
                && target.payload.public_key == request.target.target_public_key
                && target.payload.subject == Actor::Service(facts.audience.service.clone())
                && matches!(transport, TransportEvidence::VerifiedPeer { origin } if origin == &request.account_facts.audience.device),
            "observation requires the current Project host and actual peer origin",
        )?;
        require(
            wire.invocation.payload.grant_leaf == wire.request.payload.intent.execution_grant,
            "observation selected a different executor grant",
        )?;
        let submission = self.verify_invocation(&wire.invocation, facts, transport, now_ms)?;
        let leaf = &self
            .grant(&wire.invocation.payload.grant_leaf)
            .ok_or(AuthorizationError::InvalidProof(
                "missing executor observation grant",
            ))?
            .payload;
        require(
            leaf.scope == ResourceScope::Exact(facts.resource.clone())
                && leaf.grantee == Grantee::Actor(wire.invocation.payload.actor.clone())
                && leaf.remaining_delegations == 0
                && leaf.actions == [Action::ExecutionResolve, Action::OperationResolve]
                && leaf.not_before_ms < leaf.expires_at_ms
                && leaf.expires_at_ms < u64::MAX
                && leaf.constraints.context == facts.context,
            "executor observation requires only the exact finite resolve pair",
        )?;
        let mut heads = BTreeMap::new();
        for stamp in request.heads().iter().chain(submission.dependencies()) {
            require(
                heads.get(&stamp.domain).is_none_or(|old| old == stamp),
                "observation combines inconsistent policy heads",
            )?;
            heads.insert(stamp.domain.clone(), stamp.clone());
        }
        require(
            heads.len() <= MAX_POLICY_DOMAINS,
            "observation has too many policy domains",
        )?;
        let mut usage = request.usage.clone();
        usage.record_authorization(&submission)?;
        usage.enforce_limits()?;
        Ok(VerifiedExecutionObservationCurrent {
            hash: wire.hash()?,
            request,
            submission,
            heads: heads.into_values().collect(),
            usage,
            verified_at_ms: now_ms,
        })
    }

    pub fn verify_execution_observation_freshness(
        &self,
        value: &VerifiedExecutionObservation,
        proof: &ExecutionObservationFreshness,
        now_ms: u64,
    ) -> Result<VerifiedExecutionObservationRead> {
        self.verify_observation_freshness(&value.current, &value.usage, proof, now_ms)
    }

    pub fn verify_execution_observation_current_freshness(
        &self,
        value: &VerifiedExecutionObservationCurrent,
        proof: &ExecutionObservationFreshness,
        now_ms: u64,
    ) -> Result<VerifiedExecutionObservationRead> {
        self.verify_observation_freshness(value, &value.usage, proof, now_ms)
    }

    fn verify_observation_freshness(
        &self,
        value: &VerifiedExecutionObservationCurrent,
        base_usage: &ExecutionProofUsage,
        proof: &ExecutionObservationFreshness,
        now_ms: u64,
    ) -> Result<VerifiedExecutionObservationRead> {
        self.check_current(&value.request.resolver, now_ms)?;
        self.check_current(&value.submission, now_ms)?;
        let challenge = &proof.challenge;
        require(
            *challenge
                == ExecutionObservationChallenge::bound_to(
                    value,
                    challenge.nonce.clone(),
                    challenge.issued_at_ms,
                    challenge.expires_at_ms,
                )?,
            "observation challenge mismatch",
        )?;
        validate_window(challenge.issued_at_ms, challenge.expires_at_ms, now_ms)?;
        require(
            proof.receipts.len() == value.heads.len(),
            "observation requires both current authority closures",
        )?;
        let challenge_hash = challenge.hash()?;
        let mut seen = BTreeSet::new();
        let mut budget = Budget::default();
        let mut usage = base_usage.clone();
        usage.record_metadata(challenge)?;
        let mut expiry = challenge.expires_at_ms.min(value.expires_at_ms());
        for signed in &proof.receipts {
            budget.verify(signed)?;
            usage.record_signed(signed)?;
            let receipt = &signed.payload;
            require(
                receipt.version == AUTH_VERSION
                    && receipt.challenge_hash == challenge_hash
                    && value.heads.contains(&receipt.current)
                    && self.head(&receipt.current.domain) == Some(&receipt.current)
                    && seen.insert(receipt.current.domain.clone()),
                "observation freshness receipt mismatch",
            )?;
            let authority = match &receipt.current.domain {
                PolicyDomain::Account(id) => self.account(id).map(|d| &d.authority),
                PolicyDomain::Space(id) => self.space(id).map(|d| &d.authority),
            }
            .ok_or(AuthorizationError::InvalidProof(
                "missing observation freshness authority",
            ))?;
            if self
                .devices
                .values()
                .any(|certificate| certificate.payload.device == authority.device)
            {
                self.find_device(&authority.device)?;
            }
            require(
                signed.issuer == authority.public_key
                    && receipt.issued_at_ms >= challenge.issued_at_ms
                    && receipt.expires_at_ms <= challenge.expires_at_ms,
                "observation receipt is not from its current authority",
            )?;
            validate_window(receipt.issued_at_ms, receipt.expires_at_ms, now_ms)?;
            expiry = expiry.min(receipt.expires_at_ms);
        }
        if let Some(counts) = crate::signature_verification::SignatureVerification::current_counts()
        {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        Ok(VerifiedExecutionObservationRead {
            target: value.target().clone(),
            observation_hash: value.hash.clone(),
            request_hash: value.request.request_hash.clone(),
            expires_at_ms: expiry,
            usage,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::Signer;
    use serde_json::{Value, json};

    const NOW: u64 = 10_000;

    fn produce_observation_request(
        original_json: &str,
        execution_grant: &str,
        authorization_id: &str,
        signer: &Identity,
        project_pins_json: &str,
        project_bundle_json: &str,
        execution_pins_json: &str,
        execution_bundle_json: &str,
        source_node: &Identity,
        project_node: &Identity,
        execution_node: &Identity,
        now_ms: f64,
    ) -> crate::Result<String> {
        let original: SignedExecutionHandoff = serde_json::from_str(original_json).unwrap();
        let target = ExecutionObservationTarget::from_original(&original).unwrap();
        let bundle: ClientAuthorizationBundle = serde_json::from_str(project_bundle_json).unwrap();
        let project_pins: Vec<PolicyPin> = serde_json::from_str(project_pins_json).unwrap();
        let execution_pins: Vec<PolicyPin> = serde_json::from_str(execution_pins_json).unwrap();
        let execution_bundle: ClientAuthorizationBundle =
            serde_json::from_str(execution_bundle_json).unwrap();
        let source = remote_execution_policy::tests::confirmed_source(
            &bundle,
            &project_pins,
            signer,
            source_node,
            &target.source.project,
            now_ms as u64,
        )?;
        let selection = remote_execution_policy::RemoteExecutionPolicySelection {
            execution_resource: target.execution_resource.clone(),
            target_public_key: target.target_public_key.clone(),
            execution_grant: ObjectHash::parse(execution_grant).unwrap(),
        };
        let context = prepare::PrepareContext {
            context: target.context(&target.source.project.resource),
            binding_generation: Some(target.source.binding_generation),
            canonical_cwd: None,
            execution_target_public_key: Some(target.target_public_key.clone()),
        };
        let stream = remote_execution_policy::tests::remote_policy_stream(
            &source,
            signer,
            project_node,
            &execution_bundle,
            &execution_pins,
            execution_node,
            &selection,
            &context,
            Action::OperationResolve,
            remote_execution_policy::RemoteExecutionPolicyPurpose::ObservationRead,
            now_ms as u64,
        )?;
        let policy = stream.import(&source, now_ms as u64)?;
        let import_meter = crate::signature_verification::SignatureVerification::new();
        let handle =
            import_meter.run(|| prepare::verify_remote_execution(&source, &policy, now_ms))?;
        let sign_meter = crate::signature_verification::SignatureVerification::new();
        let result = sign_meter.run(|| {
            handle.sign_observation_request(
                original_json,
                authorization_id,
                PublicKey::of(source_node).as_str(),
                signer,
                now_ms,
            )
        })?;
        for meter in [&import_meter, &sign_meter] {
            assert!(meter.counts().actual_verifications > 0);
            assert!(meter.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        }
        println!("observation_api_meters={}", serde_json::to_string(&json!({
            "source":serde_json::from_str::<Value>(&source.client_policy_proof_usage_json().unwrap()).unwrap(),
            "import":import_meter.counts(),"sign":sign_meter.counts(),
        })).unwrap());
        Ok(result)
    }

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    fn write_test_vector(path: &str, vector: &Value) {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        file.write_all(serde_json::to_string(vector).unwrap().as_bytes())
            .unwrap();
    }

    struct Network {
        state: VerifiedPolicySet,
        root: Identity,
        client: Identity,
        node: Identity,
        account: AccountId,
        project: ResourceRef,
        bundle: ClientAuthorizationBundle,
        pins: Vec<PolicyPin>,
    }
    impl Network {
        fn new(offset: u8) -> Self {
            let root = key(1 + offset);
            let client = key(3 + offset);
            let node = key(4 + offset);
            let nonce = Nonce::from_bytes([5 + offset; 32]);
            let claim = bootstrap::create_node_claim(
                &node,
                nonce.as_str(),
                NOW as f64,
                (NOW + 10_000) as f64,
            )
            .unwrap();
            let claim = bootstrap::verify_node_claim(
                &claim,
                &node.public_key_hex(),
                nonce.as_str(),
                NOW as f64,
            )
            .unwrap();
            let prepared = bootstrap::prepare_account_creation(
                &root,
                &key(2 + offset),
                &client,
                &claim,
                (NOW + 1_000_000) as f64,
                NOW as f64,
            )
            .unwrap();
            let possession = bootstrap::sign_node_possession(
                &prepared.node_request_json().unwrap(),
                &node,
                nonce.as_str(),
                NOW as f64,
            )
            .unwrap();
            let created =
                bootstrap::finish_account_creation(&prepared, &root, &possession, NOW as f64)
                    .unwrap();
            let bundle: ClientAuthorizationBundle =
                serde_json::from_str(&created.bundle_json().unwrap()).unwrap();
            let pins: Vec<PolicyPin> = serde_json::from_str(&created.pins_json().unwrap()).unwrap();
            let scope: ClientScope = serde_json::from_str(&created.scope_json().unwrap()).unwrap();
            let state = replay_policies(&pins, &bundle.journal).unwrap();
            let record = state.resources.values().next().unwrap();
            let project = ResourceRef {
                space: record.payload.space.clone(),
                resource: record.payload.resource.clone(),
            };
            Self {
                state,
                root,
                client,
                node,
                account: scope.account_id,
                project,
                bundle,
                pins,
            }
        }
        fn audience(&self) -> Audience {
            Audience {
                service: ServiceId::from_key(&PublicKey::of(&self.node)),
                device: DeviceId::from_key(&PublicKey::of(&self.node)),
            }
        }
    }

    struct Fixture {
        state: VerifiedPolicySet,
        project: Network,
        executor: Network,
        original: SignedExecutionHandoff,
        resolver_grant: ObjectHash,
        execution_grant: ObjectHash,
        old_execution_grant: ObjectHash,
        original_usage: ExecutionProofUsage,
    }

    pub(crate) struct RetainedHistoryFixture {
        pub(crate) state: VerifiedPolicySet,
        pub(crate) actual: ExecutionHandoffResult,
        pub(crate) acceptance: remote_execution_history::RemoteExecutionHistoryAcceptance,
        pub(crate) body: Value,
        pub(crate) invocation: Signed<Invocation>,
        pub(crate) binding: SignedBinding,
        pub(crate) pins: Vec<PolicyPin>,
        pub(crate) executor: Identity,
        pub(crate) root: Identity,
        pub(crate) source: VerifiedClientAuthorization,
        pub(crate) client: Identity,
        pub(crate) project_node: Identity,
        pub(crate) execution_bundle: ClientAuthorizationBundle,
    }

    pub(crate) fn retained_history_fixture() -> RetainedHistoryFixture {
        retained_history_fixture_from(Fixture::new())
    }

    pub(crate) fn retained_history_fixture_with_execution_source() -> RetainedHistoryFixture {
        retained_history_fixture_from(Fixture::with_authorization_resources_and_execution_source(
            "task",
            &[],
            true,
        ))
    }

    fn retained_history_fixture_from(fixture: Fixture) -> RetainedHistoryFixture {
        let event: Event =
            serde_json::from_str(&fixture.original.handoff.project_dispatch.canonical_body)
                .unwrap();
        let EventCause::Invocation { admission: proof } = event.cause else {
            panic!("fixture has no admission")
        };
        let mut heads: BTreeMap<_, _> = proof
            .challenge
            .policies
            .iter()
            .cloned()
            .map(|head| (head.domain.clone(), head))
            .collect();
        for head in &fixture.original.invocation.payload.expected_policies {
            heads.insert(head.domain.clone(), head.clone());
        }
        let historical = remote_resolution_history::at_heads(
            &fixture.state,
            &heads.into_values().collect::<Vec<_>>(),
        )
        .unwrap();
        let facts = RequestFacts {
            resource: proof.invocation.payload.resource.clone(),
            action: proof.invocation.payload.action,
            audience: proof.invocation.payload.audience.clone(),
            operation_id: proof.invocation.payload.operation_id.clone(),
            body_hash: ObjectHash::body(&proof.source_body).unwrap(),
            context: proof.invocation.payload.context.clone(),
        };
        let admitted = remote_resolution_history::admitted(&historical, &proof, &facts).unwrap();
        let original = historical
            .verify_operation_authorization(
                &admitted,
                &facts,
                &proof.transport.clone().into_evidence(),
                NOW,
            )
            .unwrap();
        let transport = StoredTransport::VerifiedPeer {
            origin: fixture.original.invocation.payload.presenter.clone(),
        };
        let handoff = historical
            .verify_execution_handoff(
                &fixture.original,
                &original,
                &transport.clone().into_evidence(),
                None,
                NOW,
            )
            .unwrap();
        let challenge = ExecutionHandoffChallenge::new(
            &handoff,
            CheckStage::Admit,
            Nonce::random().unwrap(),
            NOW,
            NOW + 1_000,
        )
        .unwrap();
        let freshness = ExecutionHandoffFreshness {
            receipts: receipts(
                &historical,
                handoff.dependencies(),
                &challenge.hash().unwrap(),
                NOW,
                &[&fixture.project.node, &fixture.executor.node],
            ),
            challenge,
        };
        let fresh = historical
            .verify_execution_handoff_freshness(&handoff, &freshness, NOW)
            .unwrap();
        historical
            .admit_execution_handoff(&handoff, &fresh, NOW)
            .unwrap();
        let checkpoint = Signed::sign(
            ExecutionHandoffAdmissionReceipt::new(&fixture.original, &freshness, &transport, NOW)
                .unwrap(),
            &fixture.executor.node,
        )
        .unwrap();
        let accepted = verify_execution_handoff_admission_checkpoint(
            &fixture.original,
            &checkpoint,
            &freshness,
            &transport,
            NOW,
        )
        .unwrap();
        historical
            .verify_execution_handoff(
                &fixture.original,
                &original,
                &transport.clone().into_evidence(),
                Some(&accepted),
                NOW,
            )
            .unwrap();
        let result = json!({"state":"completed"});
        let source = fixture
            .original
            .handoff
            .source_attestation
            .as_ref()
            .unwrap();
        let receipt = Signed::sign(
            ExecutionHandoffReceipt {
                version: AUTH_VERSION,
                operation_id: source.payload.operation_id.clone(),
                original_space_id: source.payload.project.space.clone(),
                execution_space_id: fixture.original.handoff.execution_resource.space.clone(),
                handoff_hash: fixture.original.hash().unwrap(),
                target: fixture.executor.audience().device,
                session_id: source.payload.execution_session.session_id.clone(),
                binding_generation: source.payload.binding_generation,
                state: ExecutionHandoffState::Succeeded,
                result_hash: ObjectHash::body(&result).unwrap(),
                session: None,
                issued_at_ms: NOW + 10,
            },
            &fixture.executor.node,
        )
        .unwrap();
        let actual = ExecutionHandoffResult {
            handoff: fixture.original,
            receipt,
            result,
        };
        verify_execution_handoff_receipt_signature(
            &actual.handoff,
            &actual.receipt,
            &actual.result,
        )
        .unwrap();
        let binding =
            serde_json::from_value(proof.source_body["request"]["binding"].clone()).unwrap();
        let state =
            replay_policies(&fixture.executor.pins, &fixture.executor.bundle.journal).unwrap();
        let mut source_bundle = fixture.project.bundle.clone();
        source_bundle.grant_leaf = fixture.resolver_grant.clone();
        let source = remote_execution_policy::tests::confirmed_source(
            &source_bundle, &fixture.project.pins, &fixture.project.client,
            &fixture.project.node, &fixture.project.project, NOW,
        ).unwrap();
        RetainedHistoryFixture {
            state,
            actual,
            acceptance:
                remote_execution_history::RemoteExecutionHistoryAcceptance::ExecutionResult {
                    receipt: checkpoint,
                    freshness,
                    transport,
                },
            body: proof.source_body,
            invocation: proof.invocation,
            binding,
            pins: fixture.executor.pins,
            executor: fixture.executor.node,
            root: fixture.executor.root,
            source,
            client: fixture.project.client,
            project_node: fixture.project.node,
            execution_bundle: fixture.executor.bundle,
        }
    }

    fn receipts(
        state: &VerifiedPolicySet,
        heads: &[PolicyStamp],
        challenge_hash: &ObjectHash,
        at: u64,
        nodes: &[&Identity],
    ) -> Vec<Signed<FreshnessReceipt>> {
        heads
            .iter()
            .map(|current| {
                let authority = match &current.domain {
                    PolicyDomain::Account(id) => &state.account(id).unwrap().authority,
                    PolicyDomain::Space(id) => &state.space(id).unwrap().authority,
                };
                let signer = nodes
                    .iter()
                    .find(|identity| PublicKey::of(identity) == authority.public_key)
                    .unwrap();
                Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: current.clone(),
                        challenge_hash: challenge_hash.clone(),
                        issued_at_ms: at,
                        expires_at_ms: at + 1_000,
                    },
                    signer,
                )
                .unwrap()
            })
            .collect()
    }

    fn admission(
        state: &VerifiedPolicySet,
        network: &Network,
        body: Value,
        action: Action,
        context: InvocationContext,
        nodes: &[&Identity],
    ) -> (AdmissionProof, VerifiedAuthorization) {
        let device = DeviceId::from_key(&PublicKey::of(&network.client));
        let facts = RequestFacts {
            resource: network.project.clone(),
            action,
            audience: network.audience(),
            operation_id: body["authorization_id"].as_str().unwrap().into(),
            body_hash: ObjectHash::body(&body).unwrap(),
            context,
        };
        let mut invocation = Invocation {
            version: AUTH_VERSION,
            actor: Actor::Account(network.account.clone()),
            device: device.clone(),
            initiator: Actor::Account(network.account.clone()),
            initiating_grant: None,
            resource: facts.resource.clone(),
            action,
            audience: facts.audience.clone(),
            presenter: device.clone(),
            operation_id: facts.operation_id.clone(),
            body_hash: facts.body_hash.clone(),
            grant_leaf: network.bundle.grant_leaf.clone(),
            expected_policies: vec![],
            context: facts.context.clone(),
            nonce: Nonce::random().unwrap(),
            not_before_ms: NOW,
            expires_at_ms: NOW + 100_000,
        };
        let transport = TransportEvidence::DirectNoise { device };
        let destination = prepare::remote_selection(&body).unwrap().map(|(resource, key, grant)| {
            state.verify_execution_destination(&facts.resource, &facts.audience,
                &resource, &key, &grant, &facts.context, facts.action, NOW).unwrap()
        });
        let authorize = |signed: &Signed<Invocation>| match &destination {
            Some(destination) => state.verify_invocation_with_execution_destination(
                signed, &facts, &transport, destination, NOW,
            ),
            None => state.verify_invocation(signed, &facts, &transport, NOW),
        };
        let initial = Signed::sign(invocation.clone(), &network.client).unwrap();
        let AuthorizationError::NeedPolicySync(heads) = authorize(&initial).unwrap_err() else {
            panic!("fixture must discover a real policy closure");
        };
        invocation.expected_policies = heads;
        let invocation = Signed::sign(invocation, &network.client).unwrap();
        let auth = authorize(&invocation).unwrap();
        let challenge = FreshnessChallenge::new(
            &auth,
            CheckStage::Admit,
            Nonce::random().unwrap(),
            NOW,
            NOW + 1_000,
        )
        .unwrap();
        let receipts = receipts(
            state,
            auth.dependencies(),
            &challenge.hash().unwrap(),
            NOW,
            nodes,
        );
        let fresh = state
            .verify_freshness(&auth, &challenge, &receipts, NOW)
            .unwrap();
        let admitted = state.admit_operation(&auth, &fresh, NOW).unwrap();
        let auth = state
            .verify_operation_authorization(&admitted, &facts, &transport, NOW)
            .unwrap();
        (
            AdmissionProof {
                invocation,
                source_body: body,
                challenge,
                receipts,
                admitted_at_ms: NOW,
                transport: StoredTransport::from(&transport),
            },
            auth,
        )
    }

    fn event(
        network: &Network,
        admission: AdmissionProof,
        body: EventBody,
        previous: Option<&SignedEvent>,
    ) -> SignedEvent {
        let event = Event {
            version: EVENT_VERSION,
            network_id: network.project.space.to_string(),
            resource_id: network.project.resource.clone(),
            cause: EventCause::Invocation { admission },
            scope_id: "chat".into(),
            author: network.node.node_id(),
            sequence: if previous.is_some() { 2 } else { 1 },
            previous: previous.map(|event| event.hash.clone()),
            parents: previous
                .into_iter()
                .map(|event| event.hash.clone())
                .collect(),
            created_at_ms: NOW as i64,
            body,
            blobs: vec![],
        };
        let canonical_body = serde_json::to_string(&event).unwrap();
        let hash = hex::encode(Sha256::digest(canonical_body.as_bytes()));
        let mut message = b"vibeface:event:v2:".to_vec();
        message.extend_from_slice(hash.as_bytes());
        SignedEvent {
            hash,
            canonical_body,
            signature: hex::encode(network.node.key.sign(&message).to_bytes()),
        }
    }

    fn update_space(
        state: &mut VerifiedPolicySet,
        network: &mut Network,
        change: impl FnOnce(&mut SpaceDocument),
    ) {
        let mut document = state.space(&network.project.space).unwrap().clone();
        document.previous = state
            .head(&PolicyDomain::Space(network.project.space.clone()))
            .cloned();
        document.sequence += 1;
        change(&mut document);
        document.grants.sort();
        document.resources.sort();
        let signed = Signed::sign(document, &network.root).unwrap();
        state.install_space(signed.clone()).unwrap();
        network.bundle.journal.push(JournalEntry::Space(signed));
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_task_title("task")
        }

        fn with_task_title(task_title: &str) -> Self {
            Self::with_authorization_resources(task_title, &[])
        }

        fn with_authorization_resources(task_title: &str, resources: &[String]) -> Self {
            Self::with_authorization_resources_and_execution_source(task_title, resources, false)
        }

        fn with_authorization_resources_and_execution_source(
            task_title: &str,
            resources: &[String],
            execution_source: bool,
        ) -> Self {
            let mut project = Network::new(0);
            let mut executor = Network::new(20);
            let mut state = project.state.clone();
            state.accounts.extend(executor.state.accounts.clone());
            state.spaces.extend(executor.state.spaces.clone());
            state.devices.extend(executor.state.devices.clone());
            state.resources.extend(executor.state.resources.clone());
            state.grants.extend(executor.state.grants.clone());
            state.roots.extend(executor.state.roots.clone());
            let task_body = json!({"method":"create_task","space_id":project.project.space,"resource_id":project.project.resource,"authorization_id":"create-task","conversation_id":"chat","task_id":"task","title":task_title,"goal":"goal","acceptance":"acceptance"});
            let (proof, _) = admission(
                &state,
                &project,
                task_body,
                Action::TaskCreate,
                InvocationContext {
                    conversation: Some("chat".into()),
                    workspace_resource: Some(project.project.resource.clone()),
                    ..Default::default()
                },
                &[&project.node, &executor.node],
            );
            let task = event(
                &project,
                proof,
                EventBody::TaskCreated {
                    task_id: "task".into(),
                    title: task_title.into(),
                    goal: "goal".into(),
                    acceptance: "acceptance".into(),
                },
                None,
            );
            let execution_resource = ResourceRef {
                space: executor.project.space.clone(),
                resource: ResourceId::from_bytes([80; 32]),
            };
            let execution_context = InvocationContext {
                conversation: Some("chat".into()),
                task: Some(EventRef::parse(&task.hash).unwrap()),
                execution_session: Some(SessionRef {
                    target: executor.audience().device,
                    session_id: "execution-session".into(),
                }),
                executor_kind: Some("shell".into()),
                workspace_resource: Some(execution_resource.resource.clone()),
                ..Default::default()
            };
            let stamp = state
                .head(&PolicyDomain::Space(executor.project.space.clone()))
                .unwrap()
                .clone();
            let record = Signed::sign(
                ResourceRecord {
                    version: AUTH_VERSION,
                    space: execution_resource.space.clone(),
                    resource: execution_resource.resource.clone(),
                    kind: ResourceKind::ExecutionService,
                    host: executor.audience().service,
                    issued_under: stamp.clone(),
                    nonce: Nonce::random().unwrap(),
                },
                &executor.root,
            )
            .unwrap();
            state.install_resource(record.clone()).unwrap();
            executor
                .bundle
                .journal
                .push(JournalEntry::Resource(record.clone()));
            let pair = Signed::sign(
                Grant {
                    execution_source: execution_source.then(|| ExecutionSourcePolicy {
                        project: project.project.clone(),
                        node_public_key: PublicKey::of(&project.node),
                        pins: project
                            .pins
                            .iter()
                            .cloned()
                            .map(|mut pin| {
                                pin.minimum_head = Some(state.head(&pin.domain).unwrap().clone());
                                pin
                            })
                            .collect(),
                    }),
                    version: AUTH_VERSION,
                    owner_space: execution_resource.space.clone(),
                    scope: ResourceScope::Exact(execution_resource.clone()),
                    grantee: Grantee::Actor(Actor::Service(project.audience().service)),
                    actions: vec![Action::ExecutionResolve, Action::OperationResolve],
                    audience: executor.audience(),
                    constraints: GrantConstraints {
                        context: execution_context.clone(),
                        ..Default::default()
                    },
                    parent: None,
                    remaining_delegations: 0,
                    issued_under: stamp,
                    not_before_ms: NOW,
                    expires_at_ms: NOW + 1_000_000,
                    nonce: Nonce::random().unwrap(),
                },
                &executor.root,
            )
            .unwrap();
            let execution_grant = state.install_grant(pair.clone(), NOW).unwrap();
            executor
                .bundle
                .journal
                .push(JournalEntry::Grant(pair.clone()));
            let mut old = pair.payload;
            old.actions = vec![Action::ExecutionOpen, Action::ExecutionSubmit];
            old.expires_at_ms = NOW + 100_000;
            old.nonce = Nonce::random().unwrap();
            let old = Signed::sign(old, &executor.root).unwrap();
            let old_execution_grant = state.install_grant(old.clone(), NOW).unwrap();
            executor.bundle.journal.push(JournalEntry::Grant(old));
            update_space(&mut state, &mut executor, |doc| {
                doc.resources.push(record.hash().unwrap());
                doc.grants
                    .extend([execution_grant.clone(), old_execution_grant.clone()]);
            });
            let binding = task_binding::TaskBinding {
                network_id: project.project.space.to_string(),
                conversation_id: "chat".into(),
                task_ref: task.hash.clone(),
                task_id: "task".into(),
                authority_node: project.node.node_id(),
                generation: 1,
                target_node: executor.node.node_id(),
                session_id: "execution-session".into(),
                open_spec: json!({"type":"open","executor":"shell","cwd":"/tmp"}),
                previous_binding_hash: None,
                previous_seal: None,
            };
            let binding = SignedBinding {
                signature: project
                    .node
                    .sign(
                        task_binding::BINDING_DOMAIN,
                        &serde_json::to_vec(&binding).unwrap(),
                    )
                    .unwrap(),
                binding,
            };
            let request = json!({"space_id":project.project.space,"conversation_id":"chat","operation_id":"original-open","task_id":"task","target_node":executor.node.node_id(),"session_id":"execution-session","binding":binding,"action":{"type":"open"},"resources":resources});
            let body = json!({"method":"operate","space_id":project.project.space,"resource_id":project.project.resource,"authorization_id":"original-open","request":request,"execution_resource":execution_resource,"target_public_key":PublicKey::of(&executor.node),"execution_grant":old_execution_grant});
            let mut context = execution_context;
            context.workspace_resource = Some(project.project.resource.clone());
            let (proof, auth) = admission(
                &state,
                &project,
                body,
                Action::ExecutionOpen,
                context,
                &[&project.node, &executor.node],
            );
            let dispatch = event(
                &project,
                proof,
                EventBody::DispatchRecorded {
                    operation_id: "original-open".into(),
                    task_id: Some("task".into()),
                    target_node: executor.node.node_id(),
                    session_id: "execution-session".into(),
                    action: request,
                },
                Some(&task),
            );
            let mut handoff = ExecutionHandoff {
                version: AUTH_VERSION,
                project_dispatch: dispatch,
                task_creation: task,
                sdk_origin: None,
                task_sdk_origin: None,
                execution_resource,
                execution_audience: executor.audience(),
                target_public_key: PublicKey::of(&executor.node),
                source_attestation: None,
            };
            handoff.attest_source(&project.node).unwrap();
            let facts = execution_submission_facts(&handoff, &auth).unwrap();
            let descriptor = state
                .verify_service_grant(
                    &project.audience().service,
                    &old_execution_grant,
                    &facts,
                    NOW,
                )
                .unwrap();
            let invocation = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Service(project.audience().service),
                    device: descriptor.device().clone(),
                    initiator: descriptor.initiator().clone(),
                    initiating_grant: descriptor.initiating_grant().cloned(),
                    resource: facts.resource,
                    action: facts.action,
                    audience: facts.audience,
                    presenter: descriptor.device().clone(),
                    operation_id: facts.operation_id,
                    body_hash: facts.body_hash,
                    grant_leaf: old_execution_grant.clone(),
                    expected_policies: descriptor.heads().to_vec(),
                    context: facts.context,
                    nonce: Nonce::random().unwrap(),
                    not_before_ms: NOW,
                    expires_at_ms: NOW + 100_000,
                },
                &project.node,
            )
            .unwrap();
            let original = SignedExecutionHandoff {
                handoff,
                invocation,
            };
            let original_usage = state
                .verify_execution_handoff(
                    &original,
                    &auth,
                    &TransportEvidence::VerifiedPeer {
                        origin: project.audience().device,
                    },
                    None,
                    NOW,
                )
                .unwrap()
                .proof_usage()
                .clone();
            let mut resolver = state
                .grant(&project.bundle.grant_leaf)
                .unwrap()
                .payload
                .clone();
            resolver.scope = ResourceScope::Exact(project.project.clone());
            resolver.actions = vec![Action::OperationResolve];
            resolver.remaining_delegations = 0;
            resolver.nonce = Nonce::random().unwrap();
            resolver.issued_under = state
                .head(&PolicyDomain::Space(project.project.space.clone()))
                .unwrap()
                .clone();
            let resolver = Signed::sign(resolver, &project.root).unwrap();
            let resolver_grant = state.install_grant(resolver.clone(), NOW).unwrap();
            project.bundle.journal.push(JournalEntry::Grant(resolver));
            update_space(&mut state, &mut project, |doc| {
                doc.grants.push(resolver_grant.clone())
            });
            Self {
                state,
                project,
                executor,
                original,
                resolver_grant,
                execution_grant,
                old_execution_grant,
                original_usage,
            }
        }

        fn observe_current(&self, at: u64) -> SignedExecutionObservation {
            let intent = ExecutionObservationIntent {
                version: AUTH_VERSION,
                purpose: ExecutionObservationPurpose::Observe,
                target: ExecutionObservationTarget::from_original(&self.original).unwrap(),
                execution_grant: self.execution_grant.clone(),
                authorization_id: "observe-current".into(),
            };
            let facts = self
                .state
                .observation_account_facts(&intent, &self.project.audience())
                .unwrap();
            let device = DeviceId::from_key(&PublicKey::of(&self.project.client));
            let mut payload = ExecutionObservationRequest {
                request: Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Account(self.project.account.clone()),
                    device: device.clone(),
                    initiator: Actor::Account(self.project.account.clone()),
                    initiating_grant: None,
                    resource: facts.resource,
                    action: facts.action,
                    audience: facts.audience,
                    presenter: device,
                    operation_id: facts.operation_id,
                    body_hash: facts.body_hash,
                    grant_leaf: self.resolver_grant.clone(),
                    expected_policies: vec![],
                    context: facts.context,
                    nonce: Nonce::random().unwrap(),
                    not_before_ms: at,
                    expires_at_ms: at + 1_000,
                },
                intent,
            };
            let probe = Signed::sign(payload.clone(), &self.project.client).unwrap();
            let AuthorizationError::NeedPolicySync(heads) = self
                .state
                .verify_execution_observation_current_request(&probe, at)
                .unwrap_err()
            else {
                panic!("current observation must discover its real policy closure");
            };
            payload.request.expected_policies = heads;
            let request = Signed::sign(payload, &self.project.client).unwrap();
            let current = self
                .state
                .verify_execution_observation_current_request(&request, at)
                .unwrap();
            let facts = current.submission_facts();
            let delegated = self
                .state
                .verify_service_grant(
                    &self.project.audience().service,
                    &self.execution_grant,
                    facts,
                    at,
                )
                .unwrap();
            let invocation = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Service(self.project.audience().service),
                    device: delegated.device().clone(),
                    initiator: delegated.initiator().clone(),
                    initiating_grant: delegated.initiating_grant().cloned(),
                    resource: facts.resource.clone(),
                    action: facts.action,
                    audience: facts.audience.clone(),
                    presenter: delegated.device().clone(),
                    operation_id: facts.operation_id.clone(),
                    body_hash: facts.body_hash.clone(),
                    grant_leaf: delegated.grant_leaf().clone(),
                    expected_policies: delegated.heads().to_vec(),
                    context: facts.context.clone(),
                    nonce: Nonce::random().unwrap(),
                    not_before_ms: at,
                    expires_at_ms: at + 1_000,
                },
                &self.project.node,
            )
            .unwrap();
            SignedExecutionObservation {
                request,
                invocation,
            }
        }

        fn observe(&self, at: u64) -> SignedExecutionObservation {
            let request = self
                .state
                .build_execution_observation_request(
                    &self.original,
                    &self.project.client,
                    &self.resolver_grant,
                    &self.execution_grant,
                    "observe-original",
                    at,
                    at + 100_000,
                )
                .unwrap();
            self.state
                .sign_execution_observation(
                    &request,
                    &self.original,
                    &self.project.node,
                    at,
                    at + 100_000,
                )
                .unwrap()
        }
        fn transport(&self) -> TransportEvidence {
            TransportEvidence::VerifiedPeer {
                origin: self.project.audience().device,
            }
        }
        fn freshness(
            &self,
            current: &VerifiedExecutionObservationCurrent,
            at: u64,
        ) -> ExecutionObservationFreshness {
            let challenge = ExecutionObservationChallenge::new(
                current,
                Nonce::random().unwrap(),
                at,
                at + 1_000,
            )
            .unwrap();
            let receipts = receipts(
                &self.state,
                current.heads(),
                &challenge.hash().unwrap(),
                at,
                &[&self.project.node, &self.executor.node],
            );
            ExecutionObservationFreshness {
                challenge,
                receipts,
            }
        }
    }

    fn add_gateway(fixture: &mut Fixture) -> (Network, Identity) {
        let mut gateway = Network::new(40);
        let device = key(48);
        let key = PublicKey::of(&device);
        let domain = PolicyDomain::Account(gateway.account.clone());
        let mut certificate = DeviceCertificate {
            version: AUTH_VERSION,
            owner: domain.clone(),
            subject: Actor::Service(ServiceId::from_key(&key)),
            device: DeviceId::from_key(&key),
            public_key: key,
            uses: vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
            possession: None,
            enrollment_grant: None,
            issued_under: gateway.state.head(&domain).unwrap().clone(),
            nonce: Nonce::random().unwrap(),
            not_before_ms: NOW,
            expires_at_ms: NOW + 1_000_000,
        };
        certificate.prove_possession(&device).unwrap();
        let certificate = Signed::sign(certificate, &gateway.root).unwrap();
        let mut document = gateway.state.account(&gateway.account).unwrap().clone();
        document.previous = gateway.state.head(&domain).cloned();
        document.sequence += 1;
        document.devices.push(certificate.hash().unwrap());
        document.devices.sort();
        let document = Signed::sign(document, &gateway.root).unwrap();
        gateway.state.install_account(document.clone()).unwrap();
        gateway
            .state
            .install_device(certificate.clone(), NOW)
            .unwrap();
        gateway
            .bundle
            .journal
            .push(JournalEntry::Device(certificate));
        gateway.bundle.journal.push(JournalEntry::Account(document));
        fixture
            .state
            .accounts
            .extend(gateway.state.accounts.clone());
        fixture.state.spaces.extend(gateway.state.spaces.clone());
        fixture.state.devices.extend(gateway.state.devices.clone());
        fixture
            .state
            .resources
            .extend(gateway.state.resources.clone());
        fixture.state.grants.extend(gateway.state.grants.clone());
        fixture.state.roots.extend(gateway.state.roots.clone());
        fixture
            .project
            .bundle
            .journal
            .extend(gateway.bundle.journal.clone());
        fixture.project.pins.extend(gateway.pins.clone());
        fixture
            .project
            .pins
            .sort_by(|left, right| left.domain.cmp(&right.domain));
        fixture.project.bundle.grant_leaf = fixture.resolver_grant.clone();
        fixture.executor.bundle.grant_leaf = fixture.execution_grant.clone();
        (gateway, device)
    }

    #[test]
    fn forwarded_observation_signs_gateway_heads_and_requires_its_current_authority() {
        let mut fixture = Fixture::new();
        let (gateway, gateway_device) = add_gateway(&mut fixture);
        let encoded = produce_observation_request(
            &serde_json::to_string(&fixture.original).unwrap(),
            fixture.execution_grant.as_str(),
            "observe-via-gateway",
            &fixture.project.client,
            &serde_json::to_string(&fixture.project.pins).unwrap(),
            &serde_json::to_string(&fixture.project.bundle).unwrap(),
            &serde_json::to_string(&fixture.executor.pins).unwrap(),
            &serde_json::to_string(&fixture.executor.bundle).unwrap(),
            &gateway_device,
            &fixture.project.node,
            &fixture.executor.node,
            NOW as f64,
        )
        .unwrap();
        let request: Signed<ExecutionObservationRequest> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            request.payload.request.presenter,
            DeviceId::from_key(&PublicKey::of(&gateway_device))
        );
        let gateway_domain = PolicyDomain::Account(gateway.account.clone());
        assert!(
            request
                .payload
                .request
                .expected_policies
                .iter()
                .any(|head| head.domain == gateway_domain)
        );
        let wire = fixture
            .state
            .sign_execution_observation(
                &request,
                &fixture.original,
                &fixture.project.node,
                NOW,
                NOW + 1_000,
            )
            .unwrap();
        let current = fixture
            .state
            .verify_execution_observation_current(&wire, &fixture.transport(), NOW)
            .unwrap();
        assert!(
            current
                .heads()
                .iter()
                .any(|head| head.domain == gateway_domain)
        );
        let challenge = ExecutionObservationChallenge::new(
            &current,
            Nonce::random().unwrap(),
            NOW,
            NOW + 1_000,
        )
        .unwrap();
        let all = receipts(
            &fixture.state,
            current.heads(),
            &challenge.hash().unwrap(),
            NOW,
            &[&fixture.project.node, &fixture.executor.node, &gateway.node],
        );
        let proof = ExecutionObservationFreshness {
            challenge,
            receipts: all,
        };
        let verification = crate::signature_verification::SignatureVerification::new();
        let read = verification.run(|| {
            let full = fixture
                .state
                .verify_execution_observation(&wire, &fixture.original, &fixture.transport(), NOW)
                .unwrap();
            fixture
                .state
                .verify_execution_observation_freshness(&full, &proof, NOW)
                .unwrap()
        });
        assert_eq!(read.target().source.operation_id, "original-open");
        assert!(verification.counts().actual_verifications <= MAX_PROOF_SIGNATURES);
        let mut missing = proof.clone();
        missing
            .receipts
            .retain(|receipt| receipt.payload.current.domain != gateway_domain);
        let full = fixture
            .state
            .verify_execution_observation(&wire, &fixture.original, &fixture.transport(), NOW)
            .unwrap();
        assert!(
            fixture
                .state
                .verify_execution_observation_freshness(&full, &missing, NOW)
                .is_err()
        );

        let mut changed = request.payload.clone();
        changed
            .request
            .expected_policies
            .retain(|head| head.domain != gateway_domain);
        let changed = Signed::sign(changed, &fixture.project.client).unwrap();
        assert!(matches!(
            fixture
                .state
                .verify_execution_observation_request(&changed, &fixture.original, NOW),
            Err(AuthorizationError::NeedPolicySync(_))
        ));
        let mut forged = request.clone();
        forged.payload.request.presenter = fixture.executor.audience().device;
        assert!(
            fixture
                .state
                .verify_execution_observation_request(&forged, &fixture.original, NOW)
                .is_err()
        );
        println!(
            "forwarded_observation_usage={}",
            serde_json::to_string(&read.proof_usage().counts()).unwrap()
        );
        if let Ok(path) = std::env::var("VIBEFACE_EXECUTION_OBSERVATION_GATEWAY_VECTOR") {
            write_test_vector(
                &path,
                &json!({
                    "original_json": serde_json::to_string(&fixture.original).unwrap(),
                    "execution_grant": fixture.execution_grant,
                    "authorization_id": "observe-via-gateway", "signer_fixture_key_byte": 3,
                    "account_id": fixture.project.account, "project_space_id": fixture.project.project.space,
                    "project_pins_json": serde_json::to_string(&fixture.project.pins).unwrap(),
                    "project_bundle_json": serde_json::to_string(&fixture.project.bundle).unwrap(),
                    "execution_pins_json": serde_json::to_string(&fixture.executor.pins).unwrap(),
                    "execution_bundle_json": serde_json::to_string(&fixture.executor.bundle).unwrap(),
                    "expected_gateway_public_key": gateway_device.public_key_hex(), "now": NOW,
                    "reference_request": request, "reference_wire": wire, "reference_freshness": proof,
                    "expected_target": current.target(), "expected_heads": current.heads(),
                }),
            );
        }
    }

    #[test]
    fn forwarded_observation_rejects_non_service_and_revoked_gateway() {
        let mut fixture = Fixture::new();
        let (gateway, gateway_device) = add_gateway(&mut fixture);
        assert!(
            remote_execution_policy::tests::confirmed_source(
                &fixture.project.bundle,
                &fixture.project.pins,
                &fixture.project.client,
                &gateway.client,
                &fixture.project.project,
                NOW,
            )
            .is_err()
        );
        let mut wrong_pins = fixture.project.pins.clone();
        wrong_pins
            .iter_mut()
            .find(|pin| pin.domain == PolicyDomain::Account(gateway.account.clone()))
            .unwrap()
            .genesis_root = PublicKey::of(&fixture.project.root);
        let selection = client_policy_import::AuthorizationSelection {
            resource: fixture.project.project.clone(),
            audience: fixture.project.audience(),
            grant_leaf: fixture.resolver_grant.clone(),
        };
        let wrong_root_read = client_policy_import::build_client_policy_read_request(
            fixture.project.account.as_str(),
            fixture.project.project.space.as_str(),
            &fixture.project.client,
            PublicKey::of(&gateway_device).as_str(),
            &serde_json::to_string(&wrong_pins).unwrap(),
            &serde_json::to_string(&Some(selection)).unwrap(),
            NOW as f64,
            (NOW + 1_000) as f64,
        )
        .unwrap();
        assert!(
            client_policy_import::verify_client_policy_read(
                &fixture.state,
                &serde_json::from_str(&wrong_root_read).unwrap(),
                &PublicKey::of(&gateway_device),
                &PublicKey::of(&fixture.project.client),
                NOW,
            )
            .is_err()
        );
        assert!(
            fixture
                .state
                .build_forwarded_execution_observation_request(
                    &fixture.original,
                    &fixture.project.client,
                    &fixture.resolver_grant,
                    &fixture.execution_grant,
                    "observe-bad-gateway",
                    &PublicKey::of(&gateway.client),
                    NOW,
                    NOW + 1_000,
                )
                .is_err()
        );
        let request = fixture
            .state
            .build_forwarded_execution_observation_request(
                &fixture.original,
                &fixture.project.client,
                &fixture.resolver_grant,
                &fixture.execution_grant,
                "observe-revoked-gateway",
                &PublicKey::of(&gateway_device),
                NOW,
                NOW + 1_000,
            )
            .unwrap();
        let mut document = fixture.state.account(&gateway.account).unwrap().clone();
        document.previous = fixture
            .state
            .head(&PolicyDomain::Account(gateway.account.clone()))
            .cloned();
        document.sequence += 1;
        let revoked = DeviceId::from_key(&PublicKey::of(&gateway_device));
        document.revoked_devices.push(revoked.clone());
        document.devices.retain(|hash| {
            fixture
                .state
                .devices
                .get(hash)
                .is_none_or(|certificate| certificate.payload.device != revoked)
        });
        document.revoked_devices.sort();
        let document = Signed::sign(document, &gateway.root).unwrap();
        fixture.state.install_account(document.clone()).unwrap();
        fixture
            .project
            .bundle
            .journal
            .push(JournalEntry::Account(document));
        assert!(
            remote_execution_policy::tests::confirmed_source(
                &fixture.project.bundle,
                &fixture.project.pins,
                &fixture.project.client,
                &gateway_device,
                &fixture.project.project,
                NOW,
            )
            .is_err()
        );
        assert!(
            fixture
                .state
                .verify_execution_observation_request(&request, &fixture.original, NOW)
                .is_err()
        );
    }
    #[test]
    fn observation_uses_current_resolver_and_pair_after_old_execution_grants_are_gone() {
        let mut fixture = Fixture::new();
        let old = fixture.old_execution_grant.clone();
        update_space(&mut fixture.state, &mut fixture.executor, |doc| {
            doc.grants.retain(|hash| hash != &old)
        });
        let resolver = fixture.resolver_grant.clone();
        update_space(&mut fixture.state, &mut fixture.project, |doc| {
            doc.grants.retain(|hash| hash == &resolver)
        });
        let at = NOW + 100_001;
        let wire = fixture.observe(at);
        let prepared = fixture
            .state
            .verify_execution_observation(&wire, &fixture.original, &fixture.transport(), at)
            .unwrap();
        let freshness = fixture.freshness(prepared.current(), at);
        let signatures = crate::signature_verification::SignatureVerification::new();
        let (full, read) = signatures.run(|| {
            let full = fixture
                .state
                .verify_execution_observation(&wire, &fixture.original, &fixture.transport(), at)
                .unwrap();
            let read = fixture
                .state
                .verify_execution_observation_freshness(&full, &freshness, at)
                .unwrap();
            (full, read)
        });
        assert_eq!(read.target().handoff_hash, fixture.original.hash().unwrap());
        assert_eq!(read.target().source.operation_id, "original-open");
        assert_eq!(read.expires_at_ms(), at + 1_000);
        assert_eq!(freshness.challenge.stage, CheckStage::RemoteRead);
        assert!(read.proof_usage().counts().unique_signatures <= MAX_PROOF_SIGNATURES);
        assert!(read.proof_usage().counts().unique_signed_objects <= MAX_PROOF_OBJECTS);
        assert!(read.proof_usage().counts().authorization_bytes <= MAX_PROOF_BYTES);
        let actual = read.proof_usage().counts().actual.unwrap();
        assert!(
            actual.actual_verifications > 0 && actual.actual_verifications <= MAX_PROOF_SIGNATURES
        );
        assert!(actual.cache_hits > 0);
        println!(
            "observation_usage={}",
            serde_json::to_string(&read.proof_usage().counts()).unwrap()
        );
        let shape = Signed {
            payload: wire.request.payload.request.clone(),
            issuer: wire.request.issuer.clone(),
            signature: wire.request.signature.clone(),
        };
        assert!(
            fixture
                .state
                .verify_invocation(
                    &shape,
                    full.current.request.account_facts(),
                    &TransportEvidence::DirectNoise {
                        device: shape.payload.device.clone()
                    },
                    at
                )
                .is_err()
        );
    }

    #[test]
    fn observation_challenge_remains_valid_when_the_freshness_rpc_reverifies_later() {
        let fixture = Fixture::new();
        let wire = fixture.observe(NOW);
        let first = fixture
            .state
            .verify_execution_observation(&wire, &fixture.original, &fixture.transport(), NOW)
            .unwrap();
        let proof = fixture.freshness(first.current(), NOW);
        let later = NOW + 1;
        let current = fixture
            .state
            .verify_execution_observation_current(&wire, &fixture.transport(), later)
            .unwrap();
        fixture
            .state
            .verify_execution_observation_current_freshness(&current, &proof, later)
            .unwrap();
        let full = fixture
            .state
            .verify_execution_observation(&wire, &fixture.original, &fixture.transport(), later)
            .unwrap();
        fixture
            .state
            .verify_execution_observation_freshness(&full, &proof, later)
            .unwrap();
        assert!(
            ExecutionObservationChallenge::new(
                &current,
                proof.challenge.nonce.clone(),
                NOW,
                proof.challenge.expires_at_ms,
            )
            .is_err()
        );
        let mut changed = proof.clone();
        changed.challenge.request_hash = ObjectHash::body(&"another request").unwrap();
        assert!(
            fixture
                .state
                .verify_execution_observation_current_freshness(&current, &changed, later)
                .is_err()
        );
        assert!(
            fixture
                .state
                .verify_execution_observation_current_freshness(
                    &current,
                    &proof,
                    proof.challenge.expires_at_ms,
                )
                .is_err()
        );
    }

    #[test]
    fn observation_keeps_large_task_payload_separate_from_authorization_metadata() {
        let ordinary = Fixture::new();
        let ordinary_wire = ordinary.observe(NOW);
        let ordinary_read = ordinary
            .state
            .verify_execution_observation(
                &ordinary_wire,
                &ordinary.original,
                &ordinary.transport(),
                NOW,
            )
            .unwrap();
        let title = "t".repeat(12_000);
        let fixture = Fixture::with_task_title(&title);
        let wire = fixture.observe(NOW);
        let read = fixture
            .state
            .verify_execution_observation(&wire, &fixture.original, &fixture.transport(), NOW)
            .unwrap();
        let task: Event =
            serde_json::from_str(&fixture.original.handoff.task_creation.canonical_body).unwrap();
        assert!(
            matches!(task.body, EventBody::TaskCreated { title: actual, .. } if actual == title)
        );
        let counts = read.proof_usage().counts();
        assert_eq!(
            counts.task_payload_bytes,
            title.len() + "goal".len() + "acceptance".len()
        );
        assert!(counts.authorization_bytes < MAX_PROOF_BYTES);
        assert!(
            counts.authorization_bytes
                < ordinary_read.proof_usage().counts().authorization_bytes + 64
        );
        println!(
            "large_task_payload_observation_usage={}",
            serde_json::to_string(&counts).unwrap()
        );
    }

    #[test]
    fn observation_rejects_the_union_of_bounded_current_and_historical_proofs_over_64kib() {
        let ordinary = Fixture::new();
        let mut remaining =
            MAX_PROOF_BYTES - ordinary.original_usage.counts().authorization_bytes - 1_024;
        assert!(remaining > 4_000);
        let mut resources = Vec::new();
        while remaining >= 32 {
            let length = remaining.min(4_000);
            let mut path = format!("/tmp/resource-{:03}/", resources.len());
            let mut suffix = length - path.len();
            while suffix > 200 {
                path.push_str(&"p".repeat(199));
                path.push('/');
                suffix -= 200;
            }
            path.push_str(&"p".repeat(suffix));
            assert_eq!(path.len(), length);
            assert!(path.len() <= 4_096);
            resources.push(path);
            remaining -= length;
        }
        assert!(resources.len() <= 128);
        let fixture = Fixture::with_authorization_resources("task", &resources);
        assert!(fixture.original_usage.counts().authorization_bytes < MAX_PROOF_BYTES);
        let target = ExecutionObservationTarget::from_original(&fixture.original).unwrap();
        let history = fixture
            .state
            .observation_source_usage(&target, &fixture.original, NOW)
            .unwrap();
        assert!(history.counts().authorization_bytes < MAX_PROOF_BYTES);
        let wire = fixture.observe_current(NOW);
        let current = fixture
            .state
            .verify_execution_observation_current(&wire, &fixture.transport(), NOW)
            .unwrap();
        assert!(current.proof_usage().counts().authorization_bytes < MAX_PROOF_BYTES);
        let mut union = current.proof_usage().clone();
        union.merge(&history);
        println!(
            "observation_metadata_union_usage={}",
            json!({"historical":history.counts(),"current":current.proof_usage().counts(),"union":union.counts()})
        );
        assert!(union.counts().authorization_bytes > MAX_PROOF_BYTES);
        assert!(union.counts().unique_signed_objects <= MAX_PROOF_OBJECTS);
        assert!(union.counts().unique_signatures <= MAX_PROOF_SIGNATURES);
        assert!(union.counts().max_chain_depth <= MAX_DELEGATION_DEPTH);
        assert_eq!(
            fixture
                .state
                .verify_execution_observation(&wire, &fixture.original, &fixture.transport(), NOW)
                .unwrap_err(),
            AuthorizationError::BudgetExceeded
        );
        let attempt = fixture.state.build_execution_observation_request(
            &fixture.original,
            &fixture.project.client,
            &fixture.resolver_grant,
            &fixture.execution_grant,
            "observe-large-history",
            NOW,
            NOW + 1_000,
        );
        let result = attempt.and_then(|request| {
            fixture.state.sign_execution_observation(
                &request,
                &fixture.original,
                &fixture.project.node,
                NOW,
                NOW + 1_000,
            )
        });
        assert_eq!(result.unwrap_err(), AuthorizationError::BudgetExceeded);
    }

    #[test]
    fn current_gate_does_not_claim_a_local_handoff_and_full_gate_binds_exact_original() {
        let fixture = Fixture::new();
        let mut wire = fixture.observe(NOW);
        let mut request = wire.request.payload;
        request.intent.target.handoff_hash = ObjectHash::body(&"no local acceptance").unwrap();
        request.request.body_hash = ObjectHash::body(&request.intent).unwrap();
        wire.request = Signed::sign(request, &fixture.project.client).unwrap();
        let mut invocation = wire.invocation.payload;
        invocation.body_hash = ObjectHash::body(&wire.request).unwrap();
        wire.invocation = Signed::sign(invocation, &fixture.project.node).unwrap();
        let current = fixture
            .state
            .verify_execution_observation_current(&wire, &fixture.transport(), NOW)
            .unwrap();
        let proof = fixture.freshness(&current, NOW);
        fixture
            .state
            .verify_execution_observation_current_freshness(&current, &proof, NOW)
            .unwrap();
        assert!(
            fixture
                .state
                .verify_execution_observation(&wire, &fixture.original, &fixture.transport(), NOW)
                .is_err()
        );
        assert!(
            fixture
                .state
                .verify_execution_observation_current(
                    &wire,
                    &TransportEvidence::VerifiedPeer {
                        origin: fixture.executor.audience().device
                    },
                    NOW
                )
                .is_err()
        );
    }

    #[test]
    fn observation_rejects_missing_authority_receipts_stage_changes_and_revoked_pair() {
        let mut fixture = Fixture::new();
        let wire = fixture.observe(NOW);
        let full = fixture
            .state
            .verify_execution_observation(&wire, &fixture.original, &fixture.transport(), NOW)
            .unwrap();
        let proof = fixture.freshness(full.current(), NOW);
        let mut missing = proof.clone();
        missing.receipts.pop();
        assert!(
            fixture
                .state
                .verify_execution_observation_freshness(&full, &missing, NOW)
                .is_err()
        );
        let mut wrong = proof.clone();
        wrong.challenge.stage = CheckStage::Start;
        assert!(
            fixture
                .state
                .verify_execution_observation_freshness(&full, &wrong, NOW)
                .is_err()
        );
        let mut duplicate = proof.clone();
        duplicate.receipts[1] = duplicate.receipts[0].clone();
        assert!(
            fixture
                .state
                .verify_execution_observation_freshness(&full, &duplicate, NOW)
                .is_err()
        );
        let revoked = fixture.execution_grant.clone();
        update_space(&mut fixture.state, &mut fixture.executor, |doc| {
            doc.grants.retain(|hash| hash != &revoked)
        });
        assert!(
            fixture
                .state
                .verify_execution_observation_freshness(&full, &proof, NOW)
                .is_err()
        );
        let request = fixture
            .state
            .build_execution_observation_request(
                &fixture.original,
                &fixture.project.client,
                &fixture.resolver_grant,
                &fixture.execution_grant,
                "observe-revoked-pair",
                NOW,
                NOW + 1_000,
            )
            .unwrap();
        assert!(
            fixture
                .state
                .sign_execution_observation(
                    &request,
                    &fixture.original,
                    &fixture.project.node,
                    NOW,
                    NOW + 1_000
                )
                .is_err()
        );
    }

    #[test]
    fn observation_rejects_execution_submit_in_resolver_grant_and_revoked_account_grant() {
        let mut fixture = Fixture::new();
        let mut mixed = fixture
            .state
            .grant(&fixture.execution_grant)
            .unwrap()
            .payload
            .clone();
        mixed.actions.insert(0, Action::ExecutionSubmit);
        mixed.nonce = Nonce::random().unwrap();
        let mixed = Signed::sign(mixed, &fixture.executor.root).unwrap();
        let mixed_hash = fixture.state.install_grant(mixed, NOW).unwrap();
        update_space(&mut fixture.state, &mut fixture.executor, |doc| {
            doc.grants.push(mixed_hash.clone())
        });
        let request = fixture
            .state
            .build_execution_observation_request(
                &fixture.original,
                &fixture.project.client,
                &fixture.resolver_grant,
                &mixed_hash,
                "observe-mixed-grant",
                NOW,
                NOW + 1_000,
            )
            .unwrap();
        assert!(
            fixture
                .state
                .sign_execution_observation(
                    &request,
                    &fixture.original,
                    &fixture.project.node,
                    NOW,
                    NOW + 1_000
                )
                .is_err()
        );
        let revoked = fixture.resolver_grant.clone();
        update_space(&mut fixture.state, &mut fixture.project, |doc| {
            doc.grants.retain(|hash| hash != &revoked)
        });
        assert!(
            fixture
                .state
                .build_execution_observation_request(
                    &fixture.original,
                    &fixture.project.client,
                    &fixture.resolver_grant,
                    &fixture.execution_grant,
                    "observe-revoked-account",
                    NOW,
                    NOW + 1_000
                )
                .is_err()
        );
    }

    #[test]
    fn bounded_remote_policy_imports_bind_each_purpose_and_failed_pages_do_not_advance() {
        use remote_execution_policy::{RemoteExecutionPolicyPurpose as Purpose, tests::*};
        let mut fixture = Fixture::new();
        let source_execution_grant = fixture.project.bundle.grant_leaf.clone();
        let target = ExecutionObservationTarget::from_original(&fixture.original).unwrap();
        let context = prepare::PrepareContext {
            context: target.context(&target.source.project.resource),
            binding_generation: Some(target.source.binding_generation),
            canonical_cwd: None,
            execution_target_public_key: Some(target.target_public_key.clone()),
        };
        for (purpose, action, source_grant, execution_grant, name) in [
            (
                Purpose::Execution,
                Action::ExecutionOpen,
                source_execution_grant,
                fixture.old_execution_grant.clone(),
                "execution",
            ),
            (
                Purpose::ObservationRead,
                Action::OperationResolve,
                fixture.resolver_grant.clone(),
                fixture.execution_grant.clone(),
                "observation_read",
            ),
            (
                Purpose::Resolver,
                Action::OperationResolve,
                fixture.resolver_grant.clone(),
                fixture.execution_grant.clone(),
                "resolver",
            ),
        ] {
            fixture.project.bundle.grant_leaf = source_grant;
            let source = confirmed_source(
                &fixture.project.bundle,
                &fixture.project.pins,
                &fixture.project.client,
                &fixture.project.node,
                &fixture.project.project,
                NOW,
            )
            .unwrap();
            let selection = remote_execution_policy::RemoteExecutionPolicySelection {
                execution_resource: target.execution_resource.clone(),
                target_public_key: target.target_public_key.clone(),
                execution_grant,
            };
            let stream = remote_policy_stream(
                &source,
                &fixture.project.client,
                &fixture.project.node,
                &fixture.executor.bundle,
                &fixture.executor.pins,
                &fixture.executor.node,
                &selection,
                &context,
                action,
                purpose,
                NOW,
            )
            .unwrap();
            let mut import = stream.begin(&source, NOW).unwrap();
            let before = import.page_request_json().unwrap();
            let before_usage = import.proof_usage_json().unwrap();
            let mut invalid = stream.pages[0].payload.clone();
            invalid.entry = Some(fixture.project.bundle.journal[0].clone());
            let invalid = Signed::sign(invalid, &fixture.executor.node).unwrap();
            assert!(
                import
                    .append_page(&serde_json::to_string(&invalid).unwrap(), NOW as f64)
                    .is_err()
            );
            assert_eq!(import.page_request_json().unwrap(), before);
            assert_eq!(import.proof_usage_json().unwrap(), before_usage);
            for page in &stream.pages {
                import
                    .append_page(&serde_json::to_string(page).unwrap(), NOW as f64)
                    .unwrap();
            }
            let policy = import
                .finish(&serde_json::to_string(&stream.witness).unwrap(), NOW as f64)
                .unwrap();
            let handle = prepare::verify_remote_execution(&source, &policy, NOW as f64).unwrap();
            let metadata: Value = serde_json::from_str(&handle.metadata_json().unwrap()).unwrap();
            assert_eq!(metadata["purpose"], name);
            assert!(
                prepare::verify_remote_execution(&source, &policy, (NOW + 1_000) as f64).is_err()
            );
            let original = serde_json::to_string(&fixture.original).unwrap();
            if purpose == Purpose::ObservationRead {
                let prepared = handle.prepare(
                    &source,
                    "{}",
                    "{}",
                    "operation_resolve",
                    "readonly",
                    PublicKey::of(&fixture.project.node).as_str(),
                    &fixture.project.client,
                    NOW as f64,
                );
                assert!(
                    prepared
                        .unwrap_err()
                        .contains("observation policy cannot prepare")
                );
                let signed = handle.sign(
                    &source,
                    "{}",
                    "{}",
                    "operation_resolve",
                    "readonly",
                    PublicKey::of(&fixture.project.node).as_str(),
                    &fixture.project.client,
                    NOW as f64,
                );
                assert!(
                    signed
                        .unwrap_err()
                        .contains("observation policy cannot sign")
                );
                assert!(
                    handle
                        .resolution_body_json("{}", "readonly", "unknown", "evidence", NOW as f64)
                        .unwrap_err()
                        .contains("resolution needs an independently verified resolver grant")
                );
                assert!(
                    handle
                        .resolution_result_json("{}", "{}", "{}", NOW as f64)
                        .unwrap_err()
                        .contains("invalid resolver result proof bounds")
                );
                handle
                    .sign_observation_request(
                        &original,
                        "readonly",
                        PublicKey::of(&fixture.project.node).as_str(),
                        &fixture.project.client,
                        NOW as f64,
                    )
                    .unwrap();
            } else {
                assert!(
                    handle
                        .sign_observation_request(
                            &original,
                            "wrong-purpose",
                            PublicKey::of(&fixture.project.node).as_str(),
                            &fixture.project.client,
                            NOW as f64
                        )
                        .is_err()
                );
            }
            let mut substituted = stream.request.clone();
            substituted.intent.payload.purpose = if purpose == Purpose::ObservationRead {
                Purpose::Resolver
            } else {
                Purpose::ObservationRead
            };
            substituted.intent =
                Signed::sign(substituted.intent.payload, &fixture.project.client).unwrap();
            assert!(
                remote_execution_policy::begin_remote_execution_policy_import(
                    &source,
                    &serde_json::to_string(&substituted).unwrap(),
                    &serde_json::to_string(&stream.header).unwrap(),
                    &serde_json::to_string(&fixture.executor.pins).unwrap(),
                    PublicKey::of(&fixture.executor.node).as_str(),
                    NOW as f64
                )
                .is_err()
            );
        }
    }
    #[test]
    fn wasm_entry_replays_two_independent_proofs_and_rejects_missing_executor_trust() {
        let mut fixture = Fixture::new();
        fixture.project.bundle.grant_leaf = fixture.resolver_grant.clone();
        fixture.executor.bundle.grant_leaf = fixture.execution_grant.clone();
        let original = serde_json::to_string(&fixture.original).unwrap();
        let project_pins = serde_json::to_string(&fixture.project.pins).unwrap();
        let project_bundle = serde_json::to_string(&fixture.project.bundle).unwrap();
        let execution_pins = serde_json::to_string(&fixture.executor.pins).unwrap();
        let execution_bundle = serde_json::to_string(&fixture.executor.bundle).unwrap();
        let signed = produce_observation_request(
            &original,
            fixture.execution_grant.as_str(),
            "observe-wasm",
            &fixture.project.client,
            &project_pins,
            &project_bundle,
            &execution_pins,
            &execution_bundle,
            &fixture.project.node,
            &fixture.project.node,
            &fixture.executor.node,
            NOW as f64,
        )
        .unwrap();
        let request: Signed<ExecutionObservationRequest> = serde_json::from_str(&signed).unwrap();
        fixture
            .state
            .verify_execution_observation_request(&request, &fixture.original, NOW)
            .unwrap();
        if let Ok(path) = std::env::var("VIBEFACE_EXECUTION_OBSERVATION_VECTOR") {
            let wire = fixture
                .state
                .sign_execution_observation(
                    &request,
                    &fixture.original,
                    &fixture.project.node,
                    NOW,
                    NOW + 1_000,
                )
                .unwrap();
            let verified = fixture
                .state
                .verify_execution_observation(&wire, &fixture.original, &fixture.transport(), NOW)
                .unwrap();
            let freshness = fixture.freshness(verified.current(), NOW);
            let vector = json!({
                "original_json": original,
                "execution_grant": fixture.execution_grant,
                "authorization_id": "observe-wasm",
                "signer_fixture_key_byte": 3,
                "account_id": fixture.project.account,
                "project_space_id": fixture.project.project.space,
                "expected_gateway_public_key": fixture.project.node.public_key_hex(),
                "project_pins_json": project_pins,
                "project_bundle_json": project_bundle,
                "execution_pins_json": execution_pins,
                "execution_bundle_json": execution_bundle,
                "now": NOW,
                "reference_request": request,
                "reference_wire": wire,
                "reference_freshness": freshness,
                "expected_target": verified.target(),
                "expected_heads": verified.heads(),
            });
            write_test_vector(&path, &vector);
        }
        assert!(
            produce_observation_request(
                &original,
                fixture.execution_grant.as_str(),
                "observe-no-trust",
                &fixture.project.client,
                &project_pins,
                &project_bundle,
                "[]",
                &execution_bundle,
                &fixture.project.node,
                &fixture.project.node,
                &fixture.executor.node,
                NOW as f64
            )
            .is_err()
        );
    }
}
