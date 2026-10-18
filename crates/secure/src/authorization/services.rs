use super::*;

#[derive(Clone, Debug)]
pub struct VerifiedServiceDelegation {
    service: ServiceId,
    device: DeviceId,
    grant_leaf: ObjectHash,
    initiator: Actor,
    initiating_grant: Option<ObjectHash>,
    heads: Vec<PolicyStamp>,
    expires_at_ms: u64,
}

struct ServiceGrantCore {
    service: ServiceId,
    device: DeviceId,
    grant_leaf: ObjectHash,
    initiator: Actor,
    initiating_grant: Option<ObjectHash>,
    account_grant: Option<ObjectHash>,
    heads: Vec<PolicyStamp>,
    expires_at_ms: u64,
    budget: Budget,
    chain_depth: usize,
}
impl ServiceGrantCore {
    fn into_delegation(self) -> VerifiedServiceDelegation {
        VerifiedServiceDelegation {
            service: self.service,
            device: self.device,
            grant_leaf: self.grant_leaf,
            initiator: self.initiator,
            initiating_grant: self.initiating_grant,
            heads: self.heads,
            expires_at_ms: self.expires_at_ms,
        }
    }
}

/// This authorizes only a local SDK source for a foreign policy read.
pub(super) struct VerifiedSdkPolicyReadSource {
    service: ServiceId,
    device: DeviceId,
    heads: Vec<PolicyStamp>,
    expires_at_ms: u64,
    usage: execution_handoff::ExecutionProofUsage,
}
impl VerifiedSdkPolicyReadSource {
    pub(super) fn service(&self) -> &ServiceId {
        &self.service
    }
    pub(super) fn device(&self) -> &DeviceId {
        &self.device
    }
    pub(super) fn heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub(super) fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
    pub(super) fn proof_usage(&self) -> &execution_handoff::ExecutionProofUsage {
        &self.usage
    }
}

impl VerifiedServiceDelegation {
    pub fn service(&self) -> &ServiceId {
        &self.service
    }
    pub fn device(&self) -> &DeviceId {
        &self.device
    }
    pub fn grant_leaf(&self) -> &ObjectHash {
        &self.grant_leaf
    }
    pub fn initiator(&self) -> &Actor {
        &self.initiator
    }
    pub fn initiating_grant(&self) -> Option<&ObjectHash> {
        self.initiating_grant.as_ref()
    }
    pub fn heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
}

impl VerifiedPolicySet {
    pub fn verify_service_grant(
        &self,
        service: &ServiceId,
        grant_leaf: &ObjectHash,
        facts: &RequestFacts,
        now_ms: u64,
    ) -> Result<VerifiedServiceDelegation> {
        self.verify_service_grant_inner(service, grant_leaf, facts, now_ms, true)
            .map(ServiceGrantCore::into_delegation)
    }

    fn verify_service_grant_inner(
        &self,
        service: &ServiceId,
        grant_leaf: &ObjectHash,
        facts: &RequestFacts,
        now_ms: u64,
        verify_execution_target: bool,
    ) -> Result<ServiceGrantCore> {
        validate_context(&facts.context)?;
        require(
            !facts.action.requires_account(),
            "action requires account actor",
        )?;
        let actor = Actor::Service(service.clone());
        let certificate = self
            .devices
            .values()
            .find_map(|candidate| {
                (candidate.payload.subject == actor)
                    .then(|| self.find_device(&candidate.payload.device).ok())
                    .flatten()
            })
            .ok_or(AuthorizationError::Revoked)?;
        require(
            certificate.payload.subject == actor
                && certificate
                    .payload
                    .uses
                    .contains(&DeviceUse::ServiceInvocation),
            "service has no invocation device",
        )?;
        let mut budget = Budget::default();
        let mut dependencies = BTreeMap::new();
        self.validate_certificate(
            certificate,
            now_ms,
            &mut budget,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        let record = self
            .resource(&facts.resource.space, &facts.resource.resource)
            .ok_or(AuthorizationError::InvalidProof(
                "service resource is not registered",
            ))?;
        budget.verify(record)?;
        require(
            record.payload.host == facts.audience.service,
            "service resource audience mismatch",
        )?;
        let target = self.find_device(&facts.audience.device)?;
        require(
            target.payload.subject == Actor::Service(facts.audience.service.clone()),
            "service audience device mismatch",
        )?;
        self.validate_certificate(
            target,
            now_ms,
            &mut budget,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        if verify_execution_target {
            if let Some(session) = &facts.context.execution_session {
                let target = self.find_device(&session.target)?;
                require(
                    matches!(target.payload.subject, Actor::Service(_))
                        && target.payload.uses.contains(&DeviceUse::Noise),
                    "execution target is not a service",
                )?;
                self.validate_certificate(
                    target,
                    now_ms,
                    &mut budget,
                    &mut dependencies,
                    &mut BTreeSet::new(),
                )?;
            }
        }
        let path = self.grant_chain(Some(grant_leaf))?;
        let (initiating, _) =
            self.verify_grant_path(&path, now_ms, &mut budget, &mut dependencies, true)?;
        let leaf = &path
            .last()
            .ok_or(AuthorizationError::InvalidProof("missing service grant"))?
            .payload;
        require(
            leaf.grantee == Grantee::Actor(actor.clone())
                && scope_covers(&leaf.scope, &facts.resource)
                && leaf.actions.contains(&facts.action)
                && leaf.audience == facts.audience
                && self.matches_grantee(
                    &leaf.grantee,
                    &actor,
                    now_ms,
                    &mut budget,
                    &mut dependencies,
                )?
                && context_tighter(&leaf.constraints.context, &facts.context)
                && !leaf.constraints.require_account_actor,
            "grant does not authorize the service request",
        )?;
        if leaf.constraints.require_webauthn_uv {
            return Err(AuthorizationError::UserPresenceRequired);
        }
        self.expand_authorities(now_ms, &mut budget, &mut dependencies)?;
        let (initiator, initiating_grant, account_grant) = match initiating {
            Some((account, hash)) => {
                let parent = self
                    .grants
                    .get(&hash)
                    .and_then(|grant| grant.payload.parent.clone())
                    .ok_or(AuthorizationError::InvalidProof(
                        "missing account delegation parent",
                    ))?;
                (account, Some(hash), Some(parent))
            }
            None => (actor, None, None),
        };
        Ok(ServiceGrantCore {
            service: service.clone(),
            device: certificate.payload.device.clone(),
            grant_leaf: grant_leaf.clone(),
            initiator,
            initiating_grant,
            account_grant,
            heads: dependencies.into_values().collect(),
            expires_at_ms: budget.expires_at_ms,
            chain_depth: path.len(),
            budget,
        })
    }

    pub fn verify_service_delegation(
        &self,
        parent: &VerifiedAuthorization,
        service: &ServiceId,
        grant_leaf: &ObjectHash,
        facts: &RequestFacts,
        now_ms: u64,
    ) -> Result<VerifiedServiceDelegation> {
        self.verify_service_delegation_core(parent, service, grant_leaf, facts, now_ms, true)
            .map(ServiceGrantCore::into_delegation)
    }

    pub(super) fn verify_sdk_policy_read_source(
        &self,
        parent: &VerifiedAuthorization,
        service: &ServiceId,
        grant_leaf: &ObjectHash,
        facts: &RequestFacts,
        now_ms: u64,
    ) -> Result<VerifiedSdkPolicyReadSource> {
        let _scope = crate::signature_verification::ScopedSignatureVerification::new();
        let core =
            self.verify_service_delegation_core(parent, service, grant_leaf, facts, now_ms, false)?;
        require(
            facts.context.sdk_session.as_ref().is_some_and(|session| {
                session.target == core.device && !session.session_id.is_empty()
            }),
            "SDK policy source has no matching local adapter session",
        )?;
        let mut heads: BTreeMap<_, _> = core
            .heads
            .into_iter()
            .map(|head| (head.domain.clone(), head))
            .collect();
        for head in parent.dependencies() {
            if let Some(previous) = heads.insert(head.domain.clone(), head.clone()) {
                require(
                    previous == *head,
                    "SDK policy read source has conflicting current heads",
                )?;
            }
        }
        let mut usage = execution_handoff::ExecutionProofUsage::default();
        usage.record_authorization(parent)?;
        usage.record_budget(&core.budget)?;
        usage.record_chain_depth(core.chain_depth);
        if let Some(counts) = crate::signature_verification::SignatureVerification::current_counts()
        {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        Ok(VerifiedSdkPolicyReadSource {
            service: core.service,
            device: core.device,
            heads: heads.into_values().collect(),
            expires_at_ms: core.expires_at_ms,
            usage,
        })
    }

    fn verify_service_delegation_core(
        &self,
        parent: &VerifiedAuthorization,
        service: &ServiceId,
        grant_leaf: &ObjectHash,
        facts: &RequestFacts,
        now_ms: u64,
        verify_execution_target: bool,
    ) -> Result<ServiceGrantCore> {
        self.check_current(parent, now_ms)?;
        let source = &parent.invocation.payload;
        require(
            parent.phase == AuthorizationPhase::Operation
                && matches!(source.actor, Actor::Account(_))
                && source.initiator == source.actor
                && source.initiating_grant.is_none()
                && source.action == Action::MessageAppend
                && source.resource == facts.resource
                && source.audience == facts.audience
                && source.context.conversation.is_some()
                && source.context.conversation == facts.context.conversation
                && source.context.workspace_resource == facts.context.workspace_resource,
            "service session has no matching admitted account message",
        )?;
        let mut verified = self.verify_service_grant_inner(
            service,
            grant_leaf,
            facts,
            now_ms,
            verify_execution_target,
        )?;
        require(
            verified.initiator == source.actor
                && verified.account_grant.as_ref() == Some(&source.grant_leaf)
                && verified.initiating_grant.is_some(),
            "service delegation differs from the initiating account grant",
        )?;
        verified.expires_at_ms = verified.expires_at_ms.min(parent.expires_at_ms());
        Ok(verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 10_000;

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    struct Fixture {
        state: VerifiedPolicySet,
        root: Identity,
        user: Identity,
        node: Identity,
        account: AccountId,
        space: SpaceId,
        resource: ResourceRef,
        owner_grant: ObjectHash,
        service_grant: ObjectHash,
        pins: Vec<PolicyPin>,
        journal: Vec<JournalEntry>,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_keys(0, NOW + 1_000_000)
        }

        fn with_keys(offset: u8, expires_at_ms: u64) -> Self {
            let root = key(1 + offset);
            let user = key(3 + offset);
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
                &user,
                &claim,
                expires_at_ms as f64,
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
            let resource = state
                .resources
                .values()
                .next()
                .map(|record| ResourceRef {
                    space: record.payload.space.clone(),
                    resource: record.payload.resource.clone(),
                })
                .unwrap();
            let service: Signed<Grant> =
                serde_json::from_str(&created.service_grant_json().unwrap()).unwrap();
            Self {
                state,
                root,
                user,
                node,
                account: scope.account_id,
                space: scope.space_id,
                resource,
                owner_grant: bundle.grant_leaf,
                service_grant: service.hash().unwrap(),
                pins,
                journal: bundle.journal,
            }
        }

        fn audience(&self) -> Audience {
            Audience {
                service: ServiceId::from_key(&PublicKey::of(&self.node)),
                device: DeviceId::from_key(&PublicKey::of(&self.node)),
            }
        }

        fn agent_facts(&self) -> RequestFacts {
            RequestFacts {
                resource: self.resource.clone(),
                action: Action::AgentStart,
                audience: self.audience(),
                operation_id: "agent-test".into(),
                body_hash: ObjectHash::body(&serde_json::json!({"test":"agent"})).unwrap(),
                context: InvocationContext {
                    conversation: Some("chat".into()),
                    sdk_session: Some(SessionRef {
                        target: self.audience().device,
                        session_id: "11".repeat(32),
                    }),
                    workspace_resource: Some(self.resource.resource.clone()),
                    ..Default::default()
                },
            }
        }

        fn parent(&self, grant: &ObjectHash, now_ms: u64) -> VerifiedAuthorization {
            self.parent_with_admission(grant, now_ms).0
        }

        fn parent_with_admission(
            &self,
            grant: &ObjectHash,
            now_ms: u64,
        ) -> (VerifiedAuthorization, events::AdmissionProof) {
            let context = InvocationContext {
                conversation: Some("chat".into()),
                workspace_resource: Some(self.resource.resource.clone()),
                ..Default::default()
            };
            let source_body = serde_json::json!({"method":"message","space_id":self.space,
                "resource_id":self.resource.resource,"conversation_id":"chat","authorization_id":"parent",
                "text":"run this"});
            let body_hash = ObjectHash::body(&source_body).unwrap();
            let facts = RequestFacts {
                resource: self.resource.clone(),
                action: Action::MessageAppend,
                audience: self.audience(),
                operation_id: "parent".into(),
                body_hash: body_hash.clone(),
                context: context.clone(),
            };
            let device = DeviceId::from_key(&PublicKey::of(&self.user));
            let actor = Actor::Account(self.account.clone());
            let invocation = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: actor.clone(),
                    device: device.clone(),
                    initiator: actor,
                    initiating_grant: None,
                    resource: self.resource.clone(),
                    action: Action::MessageAppend,
                    audience: self.audience(),
                    presenter: device.clone(),
                    operation_id: "parent".into(),
                    body_hash,
                    grant_leaf: grant.clone(),
                    expected_policies: vec![
                        self.state
                            .head(&PolicyDomain::Account(self.account.clone()))
                            .unwrap()
                            .clone(),
                        self.state
                            .head(&PolicyDomain::Space(self.space.clone()))
                            .unwrap()
                            .clone(),
                    ],
                    context,
                    nonce: Nonce::from_bytes([11; 32]),
                    not_before_ms: NOW,
                    expires_at_ms: NOW + 10_000,
                },
                &self.user,
            )
            .unwrap();
            let transport = TransportEvidence::DirectNoise { device };
            let auth = self
                .state
                .verify_invocation(&invocation, &facts, &transport, NOW)
                .unwrap();
            let challenge = FreshnessChallenge::new(
                &auth,
                CheckStage::Admit,
                Nonce::from_bytes([12; 32]),
                NOW,
                NOW + 5_000,
            )
            .unwrap();
            let receipts: Vec<_> = auth
                .dependencies()
                .iter()
                .map(|current| {
                    Signed::sign(
                        FreshnessReceipt {
                            version: AUTH_VERSION,
                            currents: vec![current.clone()],
                            challenge_hash: challenge.hash().unwrap(),
                            issued_at_ms: NOW,
                            expires_at_ms: NOW + 5_000,
                        },
                        &self.node,
                    )
                    .unwrap()
                })
                .collect();
            let freshness = self
                .state
                .verify_freshness(&auth, &challenge, &receipts, NOW)
                .unwrap();
            self.state.admit_operation(&auth, &freshness, NOW).unwrap();
            let proof = events::AdmissionProof {
                invocation,
                source_body,
                challenge,
                receipts,
                admitted_at_ms: NOW,
                transport: events::StoredTransport::from(&transport),
            };
            let admitted = self
                .state
                .verify_original_admission(&proof, &facts)
                .unwrap();
            let current = self
                .state
                .verify_admitted_operation_authorization(&admitted, &facts, now_ms)
                .unwrap();
            (current, proof)
        }

        fn sdk_source_provenance(
            &self,
            parent: &VerifiedAuthorization,
            child_facts: &RequestFacts,
            lease_deadline: u64,
        ) -> node_execution_policy::SdkPolicySourceProvenance {
            self.sdk_source_provenance_with_adapter(
                parent,
                child_facts,
                lease_deadline,
                &self.node,
                &self.service_grant,
            )
        }

        fn sdk_source_provenance_with_adapter(
            &self,
            parent: &VerifiedAuthorization,
            child_facts: &RequestFacts,
            lease_deadline: u64,
            adapter: &Identity,
            service_grant: &ObjectHash,
        ) -> node_execution_policy::SdkPolicySourceProvenance {
            use node_execution_policy::{
                SdkPolicyLease, SdkPolicySession, SdkPolicySourceFacts, SdkPolicySourceProvenance,
                sdk_policy_source_heads,
            };
            let service = ServiceId::from_key(&PublicKey::of(adapter));
            let (_, origin) =
                self.parent_with_admission(&parent.signed_invocation().payload.grant_leaf, NOW);
            assert_eq!(&origin.invocation, parent.signed_invocation());
            let sdk = child_facts.context.sdk_session.as_ref().unwrap().clone();
            let trigger = EventRef::parse(&sdk.session_id).unwrap();
            let body = serde_json::json!({
                "method":"agent_start","space_id":self.space,"resource_id":self.resource.resource,
                "conversation_id":"chat","authorization_id":format!("manager:{}:fixture-attempt",trigger.as_str()),
                "trigger":trigger,"attempt_id":"fixture-attempt","sdk_session":sdk,"parent_invocation":parent.invocation_hash(),
                "sdk_config":"actual-fixture-configuration",
            });
            let mut facts = self.agent_facts();
            facts.context.sdk_session = Some(sdk.clone());
            facts.operation_id = format!("manager:{}:fixture-attempt", trigger.as_str());
            facts.body_hash = ObjectHash::body(&body).unwrap();
            let delegation = self
                .state
                .verify_service_delegation(parent, &service, service_grant, &facts, NOW)
                .unwrap();
            let invocation = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Service(service.clone()),
                    device: delegation.device().clone(),
                    initiator: delegation.initiator().clone(),
                    initiating_grant: delegation.initiating_grant().cloned(),
                    resource: facts.resource.clone(),
                    action: facts.action,
                    audience: facts.audience.clone(),
                    presenter: delegation.device().clone(),
                    operation_id: facts.operation_id.clone(),
                    body_hash: facts.body_hash.clone(),
                    grant_leaf: delegation.grant_leaf().clone(),
                    expected_policies: delegation.heads().to_vec(),
                    context: facts.context.clone(),
                    nonce: Nonce::from_bytes([14; 32]),
                    not_before_ms: NOW,
                    expires_at_ms: NOW + 10_000,
                },
                adapter,
            )
            .unwrap();
            let transport = TransportEvidence::LocalAgentAdapter {
                service: service.clone(),
                session: sdk.clone(),
            };
            let manager = self
                .state
                .verify_invocation(&invocation, &facts, &transport, NOW)
                .unwrap();
            let challenge = FreshnessChallenge::new(
                &manager,
                CheckStage::Admit,
                Nonce::from_bytes([15; 32]),
                NOW,
                lease_deadline,
            )
            .unwrap();
            let receipts: Vec<_> = manager
                .dependencies()
                .iter()
                .map(|current| {
                    let authority = match &current.domain {
                        PolicyDomain::Account(id) => &self.state.account(id).unwrap().authority,
                        PolicyDomain::Space(id) => &self.state.space(id).unwrap().authority,
                    };
                    let signer = if authority.public_key == PublicKey::of(adapter) {
                        adapter
                    } else {
                        &self.node
                    };
                    assert_eq!(authority.public_key, PublicKey::of(signer));
                    Signed::sign(
                        FreshnessReceipt {
                            version: AUTH_VERSION,
                            currents: vec![current.clone()],
                            challenge_hash: challenge.hash().unwrap(),
                            issued_at_ms: NOW,
                            expires_at_ms: lease_deadline,
                        },
                        signer,
                    )
                    .unwrap()
                })
                .collect();
            let fresh = self
                .state
                .verify_freshness(&manager, &challenge, &receipts, NOW)
                .unwrap();
            self.state.admit_operation(&manager, &fresh, NOW).unwrap();
            SdkPolicySourceProvenance {
                version: AUTH_VERSION,
                origin_source_body: origin.source_body,
                origin: execution_handoff::CompactSdkOrigin {
                    message_event: trigger.clone(),
                    sdk_trigger: trigger,
                    invocation: origin.invocation,
                    challenge: origin.challenge,
                    receipts: origin.receipts,
                    admitted_at_ms: origin.admitted_at_ms,
                    transport: origin.transport,
                },
                session: SdkPolicySession {
                    space_id: self.space.clone(),
                    resource_id: self.resource.resource.clone(),
                    service: service.clone(),
                    device: delegation.device().clone(),
                    conversation_id: "chat".into(),
                    sdk_session: sdk,
                    grant_leaf: delegation.grant_leaf().clone(),
                    initiator: delegation.initiator().clone(),
                    initiating_grant: delegation.initiating_grant().cloned(),
                    parent_invocation: parent.invocation_hash().clone(),
                    expected_policies: delegation.heads().to_vec(),
                    expires_at_ms: delegation.expires_at_ms(),
                },
                service_grant: service_grant.clone(),
                manager_admission: events::AdmissionProof {
                    invocation,
                    source_body: body,
                    challenge: challenge.clone(),
                    receipts: receipts.clone(),
                    admitted_at_ms: NOW,
                    transport: events::StoredTransport::from(&transport),
                },
                lease: SdkPolicyLease {
                    challenge,
                    receipts,
                    checked_at_ms: NOW,
                    deadline: fresh.expires_at_ms(),
                },
                current_binding: None,
                child_facts: SdkPolicySourceFacts::from(child_facts),
                source_heads: sdk_policy_source_heads(
                    &self.state,
                    parent,
                    &service,
                    service_grant,
                    child_facts,
                    NOW,
                )
                .unwrap(),
            }
        }

        fn update_space(&mut self, change: impl FnOnce(&mut SpaceDocument)) {
            let mut document = self.state.space(&self.space).unwrap().clone();
            document.previous = self
                .state
                .head(&PolicyDomain::Space(self.space.clone()))
                .cloned();
            document.sequence += 1;
            change(&mut document);
            document.grants.sort();
            document.resources.sort();
            self.state
                .install_space(Signed::sign(document, &self.root).unwrap())
                .unwrap();
        }

        fn snapshot(&self) -> policy_import::VerifiedPolicySnapshot {
            let heads: Vec<_> = self
                .pins
                .iter()
                .map(|pin| self.state.head(&pin.domain).unwrap().clone())
                .collect();
            let mut import =
                policy_import::BoundedPolicyImport::new(self.pins.clone(), heads.clone()).unwrap();
            for entry in &self.journal {
                import.apply_entry(entry.clone()).unwrap();
            }
            import.finish(&heads).unwrap()
        }

        fn task_policy_source(
            &self,
            remote: &Fixture,
        ) -> (
            node_execution_policy::VerifiedTaskExecutionTrust,
            remote_execution_policy::RemoteExecutionPolicySelection,
            prepare::PrepareContext,
            RequestFacts,
        ) {
            let source = remote_execution_policy::tests::confirmed_source(
                &ClientAuthorizationBundle {
                    journal: self.journal.clone(),
                    grant_leaf: self.owner_grant.clone(),
                },
                &self.pins,
                &self.user,
                &self.node,
                &self.resource,
                NOW,
            )
            .unwrap();
            let target = PublicKey::of(&remote.node);
            let selection = remote_execution_policy::RemoteExecutionPolicySelection {
                execution_resource: remote.resource.clone(),
                target_public_key: target.clone(),
                execution_grant: remote.service_grant.clone(),
            };
            let mut facts = self.agent_facts();
            facts.action = Action::ExecutionOpen;
            facts.context.task = Some(EventRef::parse(&"aa".repeat(32)).unwrap());
            facts.context.executor_kind = Some("codex".into());
            facts.context.execution_session = Some(SessionRef {
                target: DeviceId::from_key(&target),
                session_id: "foreign-execution".into(),
            });
            facts.operation_id = "sdk-execution-open".into();
            facts.body_hash = ObjectHash::body(&serde_json::json!({
                "method":"bind_execution","space_id":self.space,"resource_id":self.resource.resource,
                "conversation_id":"chat","task_id":"task","task_ref":facts.context.task,
                "authorization_id":facts.operation_id,"target_node":remote.node.node_id(),
                "session_id":"foreign-execution","executor":"codex","cwd":"/workspace",
                "execution_resource":selection.execution_resource,"target_public_key":selection.target_public_key,
                "execution_grant":selection.execution_grant,"sdk_session":facts.context.sdk_session,
                "parent_invocation":self.parent(&self.owner_grant, NOW).invocation_hash(),
            })).unwrap();
            let mut read = facts.context.clone();
            read.sdk_session = None;
            let context = prepare::PrepareContext {
                context: read,
                binding_generation: Some(1),
                canonical_cwd: Some("/workspace".into()),
                execution_target_public_key: Some(target),
            };
            let encoded = remote_execution_policy::build_remote_execution_policy_read_intent(
                &source,
                &serde_json::to_string(&serde_json::json!({
                    "selection":selection, "context":context, "expected_action":Action::ExecutionOpen,
                    "work": ExecutionWorkIntent::TaskSession {
                        task: facts.context.task.clone().unwrap(),
                        binding_generation: 1,
                        execution_session: facts.context.execution_session.clone().unwrap(),
                    },
                    "purpose":remote_execution_policy::RemoteExecutionPolicyPurpose::Execution,
                    "execution_pins":remote.pins,
                })).unwrap(),
                &self.user, NOW as f64, (NOW + 500) as f64,
            ).unwrap();
            let intent: Signed<remote_execution_policy::RemoteExecutionPolicyReadIntent> =
                serde_json::from_str(&encoded).unwrap();
            let verified =
                remote_execution_policy::verify_remote_execution_policy_delegation_source(
                    &self.state,
                    &intent,
                    &PublicKey::of(&self.node),
                    &PublicKey::of(&self.user),
                    NOW,
                )
                .unwrap();
            let trust = node_execution_policy::approve_task_execution_policy(
                &verified, None, &self.node, NOW,
            )
            .unwrap();
            (trust, selection, context, facts)
        }
    }

    #[test]
    fn explicit_service_child_preserves_the_real_admitted_account_initiator() {
        let fixture = Fixture::new();
        let parent = fixture.parent(&fixture.owner_grant, NOW + 100_000);
        let proof = fixture
            .state
            .verify_service_delegation(
                &parent,
                &fixture.audience().service,
                &fixture.service_grant,
                &fixture.agent_facts(),
                NOW + 100_000,
            )
            .unwrap();
        assert_eq!(proof.service(), &fixture.audience().service);
        assert_eq!(proof.initiator(), &Actor::Account(fixture.account.clone()));
        assert_eq!(proof.initiating_grant(), Some(&fixture.service_grant));
        let initiating = fixture
            .state
            .grant(proof.initiating_grant().unwrap())
            .unwrap();
        assert_eq!(initiating.hash().unwrap(), fixture.service_grant);
        assert_eq!(
            initiating.payload.parent.as_ref(),
            Some(&fixture.owner_grant)
        );
        assert!(proof.expires_at_ms() > NOW + 100_000);
    }

    #[test]
    fn sdk_policy_source_precedes_foreign_certificate_and_complete_business_verification() {
        use node_execution_policy::service::verify_node_service_policy_source;
        let fixture = Fixture::new();
        let remote = Fixture::with_keys(20, NOW + 1_000);
        let parent = fixture.parent(&fixture.owner_grant, NOW);
        let (trust, selection, context, facts) = fixture.task_policy_source(&remote);
        assert!(
            fixture
                .state
                .find_device(&DeviceId::from_key(&PublicKey::of(&remote.node)))
                .is_err()
        );
        let provenance = fixture.sdk_source_provenance(&parent, &facts, NOW + 400);
        let (_, request) = verify_node_service_policy_source(
            &fixture.state,
            &fixture.pins,
            &trust,
            &parent,
            &fixture.audience().service,
            &fixture.service_grant,
            &facts,
            &selection,
            &context,
            trust.work(),
            None,
            &provenance,
            &fixture.node,
            remote_execution_policy::RemoteExecutionPolicyPurpose::Execution,
            NOW,
            NOW + 200,
        )
        .unwrap();
        let node_execution_policy::NodeExecutionPolicyReadRequest::Current(intent) = request else {
            panic!("SDK source did not sign a Node Current request")
        };
        intent.verify_signature().unwrap();
        assert_eq!(intent.issuer, PublicKey::of(&fixture.node));
        assert_eq!(
            intent.payload.source_provenance.as_ref(),
            Some(&ObjectHash::body(&provenance).unwrap())
        );
        assert!(intent.payload.context.context.sdk_session.is_none());
        assert!(facts.context.sdk_session.is_some());
        assert_eq!(
            fixture
                .state
                .verify_service_delegation(
                    &parent,
                    &fixture.audience().service,
                    &fixture.service_grant,
                    &facts,
                    NOW,
                )
                .unwrap_err(),
            AuthorizationError::Revoked
        );

        // The original business gate succeeds only after genuine C policy is available.
        let combined =
            policy_import::merge_same_head_snapshots(&fixture.snapshot(), &remote.snapshot())
                .unwrap();
        let delegated = combined
            .state()
            .verify_service_delegation(
                &parent,
                &fixture.audience().service,
                &fixture.service_grant,
                &facts,
                NOW,
            )
            .unwrap();
        assert!(
            delegated.heads().contains(
                remote
                    .state
                    .head(&PolicyDomain::Account(remote.account.clone()))
                    .unwrap()
            )
        );
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Service(delegated.service().clone()),
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
                nonce: Nonce::from_bytes([98; 32]),
                not_before_ms: NOW,
                expires_at_ms: delegated.expires_at_ms(),
            },
            &fixture.node,
        )
        .unwrap();
        let verified = combined
            .state()
            .verify_invocation(
                &invocation,
                &facts,
                &TransportEvidence::LocalAgentAdapter {
                    service: fixture.audience().service,
                    session: facts.context.sdk_session.clone().unwrap(),
                },
                NOW,
            )
            .unwrap();
        assert_eq!(verified.dependencies(), delegated.heads());
    }

    #[test]
    fn sdk_node_import_preserves_real_child_facts_and_verifies_the_signed_body() {
        use node_execution_policy::*;
        use remote_execution_policy::{
            RemoteExecutionPolicyPage, RemoteExecutionPolicyPurpose, RemoteExecutionPolicySnapshot,
            RemoteExecutionPolicyWitness,
        };
        let fixture = Fixture::new();
        let mut remote = Fixture::with_keys(20, NOW + 1_000);
        let target = PublicKey::of(&remote.node);
        let resource = ResourceRef {
            space: remote.space.clone(),
            resource: ResourceId::from_bytes([91; 32]),
        };
        let issued_under = remote
            .state
            .head(&PolicyDomain::Space(remote.space.clone()))
            .unwrap()
            .clone();
        let record = Signed::sign(
            ResourceRecord {
                version: AUTH_VERSION,
                space: remote.space.clone(),
                resource: resource.resource.clone(),
                kind: ResourceKind::ExecutionService,
                host: remote.audience().service,
                issued_under: issued_under.clone(),
                nonce: Nonce::random().unwrap(),
            },
            &remote.root,
        )
        .unwrap();
        remote.state.install_resource(record.clone()).unwrap();
        let execution_context = InvocationContext {
            conversation: Some("chat".into()),
            task: Some(EventRef::parse(&"aa".repeat(32)).unwrap()),
            execution_session: Some(SessionRef {
                target: DeviceId::from_key(&target),
                session_id: "foreign-execution".into(),
            }),
            executor_kind: Some("codex".into()),
            workspace_resource: Some(resource.resource.clone()),
            ..Default::default()
        };
        let grant = Signed::sign(
            Grant {
                version: AUTH_VERSION,
                owner_space: remote.space.clone(),
                scope: ResourceScope::Exact(resource.clone()),
                grantee: Grantee::Actor(Actor::Service(fixture.audience().service)),
                actions: vec![Action::ExecutionOpen, Action::ExecutionSubmit],
                audience: remote.audience(),
                constraints: GrantConstraints {
                    context: execution_context,
                    ..Default::default()
                },
                execution_source: None,
                parent: None,
                remaining_delegations: 0,
                issued_under,
                not_before_ms: NOW,
                expires_at_ms: NOW + 300,
                nonce: Nonce::random().unwrap(),
            },
            &remote.root,
        )
        .unwrap();
        let leaf = remote.state.install_grant(grant.clone(), NOW).unwrap();
        remote.journal.extend([
            JournalEntry::Resource(record.clone()),
            JournalEntry::Grant(grant),
        ]);
        remote.update_space(|document| {
            document.resources.push(record.hash().unwrap());
            document.grants.push(leaf.clone());
        });
        remote.journal.push(JournalEntry::Space(
            remote
                .state
                .space_history(&remote.space)
                .unwrap()
                .last()
                .unwrap()
                .clone(),
        ));
        remote.resource = resource;
        remote.service_grant = leaf;
        let (trust, selection, context, mut facts) = fixture.task_policy_source(&remote);
        let binding = task_binding::TaskBinding {
            network_id: fixture.space.to_string(),
            conversation_id: "chat".into(),
            task_ref: "aa".repeat(32),
            task_id: "task".into(),
            authority_node: fixture.node.node_id(),
            generation: 1,
            target_node: remote.node.node_id(),
            session_id: "foreign-execution".into(),
            open_spec: serde_json::json!({"type":"open","executor":"codex","cwd":"/workspace"}),
            previous_binding_hash: None,
            previous_seal: None,
        };
        let binding = task_binding::SignedBinding {
            signature: fixture
                .node
                .sign(
                    task_binding::BINDING_DOMAIN,
                    &serde_json::to_vec(&binding).unwrap(),
                )
                .unwrap(),
            binding,
        };
        let body = serde_json::json!({"method":"operate","space_id":fixture.space,
            "resource_id":fixture.resource.resource,"authorization_id":facts.operation_id,
            "execution_resource":selection.execution_resource,"target_public_key":selection.target_public_key,
            "execution_grant":selection.execution_grant,"request":{"space_id":fixture.space,
                "conversation_id":"chat","operation_id":facts.operation_id,"task_id":"task",
                "target_node":remote.node.node_id(),"session_id":"foreign-execution","expected_revision":0,"binding":binding,
                "action":binding.binding.open_spec,"resources":[]}});
        facts.body_hash = ObjectHash::body(&body).unwrap();
        let parent = fixture.parent(&fixture.owner_grant, NOW);
        let provenance = fixture.sdk_source_provenance(&parent, &facts, NOW + 250);
        let (source, request) = verify_node_service_policy_source(
            &fixture.state,
            &fixture.pins,
            &trust,
            &parent,
            &fixture.audience().service,
            &fixture.service_grant,
            &facts,
            &selection,
            &context,
            trust.work(),
            None,
            &provenance,
            &fixture.node,
            RemoteExecutionPolicyPurpose::Execution,
            NOW,
            NOW + 200,
        )
        .unwrap();
        let checked = verify_node_execution_policy_read(
            &remote.state,
            &request,
            &target,
            &PublicKey::of(&fixture.node),
            None,
            NOW,
        )
        .unwrap();
        let header = Signed::sign(
            RemoteExecutionPolicySnapshot {
                version: AUTH_VERSION,
                request_hash: ObjectHash::body(&request).unwrap(),
                snapshot_id: Nonce::random().unwrap(),
                saved_pins_hash: ObjectHash::body(&remote.pins).unwrap(),
                target_heads: checked.target_heads().to_vec(),
                source_node_public_key: target,
                issued_at_ms: NOW,
                expires_at_ms: NOW + 200,
                history: None,
            },
            &remote.node,
        )
        .unwrap();
        let mut initiator = crate::Handshake::new(
            true,
            &fixture.node.secret_hex(),
            Some(remote.node.node_id()),
            b"sdk-policy-test",
        )
        .unwrap();
        let mut responder = crate::Handshake::new(
            false,
            &remote.node.secret_hex(),
            Some(fixture.node.node_id()),
            b"sdk-policy-test",
        )
        .unwrap();
        responder.read(&initiator.write(&[]).unwrap()).unwrap();
        initiator.read(&responder.write(&[]).unwrap()).unwrap();
        responder.read(&initiator.write(&[]).unwrap()).unwrap();
        let peer = ActualPolicyNoisePeer::from_finished_handshake(&initiator).unwrap();
        let mut import =
            begin_node_task_policy_import(&trust, &source, &request, &header, &[], &peer, NOW)
                .unwrap();
        let entries: Vec<_> = remote
            .journal
            .iter()
            .filter(|entry| checked.allows_entry(entry).unwrap())
            .cloned()
            .collect();
        let mut previous = header.hash().unwrap();
        let mut pages = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let page_request = import.page_request();
            let next_cursor =
                (index + 1 < entries.len()).then(|| policy_import::entry_hash(entry).unwrap());
            let page = Signed::sign(
                RemoteExecutionPolicyPage {
                    version: AUTH_VERSION,
                    request_hash: header.payload.request_hash.clone(),
                    snapshot_id: header.payload.snapshot_id.clone(),
                    target_heads_hash: ObjectHash::body(&header.payload.target_heads).unwrap(),
                    page_index: index as u64,
                    request_cursor: page_request.cursor,
                    previous_page_hash: previous,
                    entry: Some(entry.clone()),
                    complete: next_cursor.is_none(),
                    next_cursor,
                },
                &remote.node,
            )
            .unwrap();
            import.append_page(&page, NOW).unwrap();
            previous = page.hash().unwrap();
            pages.push(page);
        }
        let witness = Signed::sign(
            RemoteExecutionPolicyWitness {
                version: AUTH_VERSION,
                request_hash: header.payload.request_hash.clone(),
                snapshot_id: header.payload.snapshot_id.clone(),
                target_heads: header.payload.target_heads.clone(),
                final_page_hash: previous,
                page_count: entries.len() as u64,
                entry_count: entries.len() as u64,
                snapshot_hash: header.hash().unwrap(),
                source_node_public_key: PublicKey::of(&remote.node),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 200,
            },
            &remote.node,
        )
        .unwrap();
        let policy = import.finish_current(&witness, NOW).unwrap();
        policy
            .check_scope(&facts, &selection, &context, NOW)
            .unwrap();
        let delegated = policy
            .state()
            .verify_service_delegation(
                &parent,
                &fixture.audience().service,
                &fixture.service_grant,
                &facts,
                NOW,
            )
            .unwrap();
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Service(delegated.service().clone()),
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
                expected_policies: {
                    let mut heads: Vec<_> = policy
                        .primary_heads()
                        .iter()
                        .chain(policy.foreign_heads())
                        .cloned()
                        .collect();
                    heads.sort_by(|left, right| left.domain.cmp(&right.domain));
                    heads
                },
                context: facts.context.clone(),
                nonce: Nonce::random().unwrap(),
                not_before_ms: NOW,
                expires_at_ms: NOW + 200,
            },
            &fixture.node,
        )
        .unwrap();
        policy
            .check_body_scope(&invocation, &facts, &body, NOW)
            .unwrap();
        let prepared = Signed::sign(
            prepare::PrepareIntent {
                request: invocation.payload.clone(),
                binding_generation: context.binding_generation,
                canonical_cwd: context.canonical_cwd.clone(),
                execution_target_public_key: context.execution_target_public_key.clone(),
            },
            &fixture.node,
        )
        .unwrap();
        policy.check_prepare_scope(&prepared, &body, NOW).unwrap();
        let destination = policy
            .state()
            .verify_execution_destination(
                &facts.resource,
                &facts.audience,
                &selection.execution_resource,
                &selection.target_public_key,
                &selection.execution_grant,
                &facts.context,
                facts.action,
                NOW,
            )
            .unwrap();
        policy
            .state()
            .verify_invocation_with_execution_destination(
                &invocation,
                &facts,
                &TransportEvidence::LocalAgentAdapter {
                    service: fixture.audience().service,
                    session: facts.context.sdk_session.clone().unwrap(),
                },
                &destination,
                NOW,
            )
            .unwrap();
        let mut wrong = facts.clone();
        wrong.context.sdk_session.as_mut().unwrap().session_id = "another-sdk-session".into();
        assert!(
            policy
                .check_scope(&wrong, &selection, &context, NOW)
                .is_err()
        );
        let mut wrong = context.clone();
        wrong.binding_generation = Some(2);
        assert!(policy.check_scope(&facts, &selection, &wrong, NOW).is_err());
        assert!(
            fixture
                .state
                .find_device(&DeviceId::from_key(&PublicKey::of(&remote.node)))
                .is_err()
        );
        let retained = node_task_policy_source_from_retained(
            &fixture.state,
            &fixture.pins,
            &trust,
            &provenance,
            &request,
            NOW,
        )
        .unwrap();
        assert!(matches!(
            begin_node_task_policy_import(&trust, &retained, &request, &header, &[], &peer, NOW),
            Err(AuthorizationError::InvalidProof(
                "retained Node policy requires its original transcript importer"
            ))
        ));
        let mut replay =
            begin_retained_node_task_policy_import(&trust, &retained, &request, &header, NOW)
                .unwrap();
        for page in &pages {
            replay.append_page(page, NOW).unwrap();
        }
        assert!(matches!(
            replay.clone().finish_current(&witness, NOW),
            Err(AuthorizationError::InvalidProof(
                "retained Node policy cannot become current"
            ))
        ));
        assert!(matches!(
            replay.clone().finish_history(&witness, NOW),
            Err(AuthorizationError::InvalidProof(
                "retained Node policy cannot become a current History reader"
            ))
        ));
        let floors = replay.finish_retained_floors(&witness, NOW).unwrap();
        assert_eq!(floors.len(), checked.target_heads().len());
        for head in checked.target_heads() {
            let pin = floors.iter().find(|pin| pin.domain == head.domain).unwrap();
            let independent = remote
                .pins
                .iter()
                .find(|pin| pin.domain == head.domain)
                .unwrap();
            assert_eq!(pin.genesis_root, independent.genesis_root);
            assert_eq!(pin.minimum_head.as_ref(), Some(head));
            assert!(
                fixture
                    .pins
                    .iter()
                    .all(|primary| primary.domain != pin.domain)
            );
        }
    }

    #[test]
    fn sdk_history_rejects_a_genuine_other_adapter_on_the_local_project() {
        use node_execution_policy::{
            verify_node_history_policy_source, verify_sdk_source_provenance,
        };
        let mut fixture = Fixture::new();
        let remote = Fixture::with_keys(20, NOW + 1_000);
        let (trust, _, _, _) = fixture.task_policy_source(&remote);
        let adapter = Fixture::with_keys(40, NOW + 1_000_000);
        fixture
            .state
            .accounts
            .extend(adapter.state.accounts.clone());
        fixture.state.devices.extend(adapter.state.devices.clone());
        fixture.state.roots.extend(adapter.state.roots.clone());
        let service = adapter.audience().service;
        let mut grant = fixture
            .state
            .grant(&fixture.service_grant)
            .unwrap()
            .payload
            .clone();
        grant.grantee = Grantee::Actor(Actor::Service(service.clone()));
        grant.nonce = Nonce::from_bytes([96; 32]);
        let grant = fixture
            .state
            .install_grant(Signed::sign(grant, &fixture.user).unwrap(), NOW)
            .unwrap();
        fixture.update_space(|document| document.grants.push(grant.clone()));
        let parent = fixture.parent(&fixture.owner_grant, NOW);
        let mut facts = fixture.agent_facts();
        facts.action = Action::OperationStatusRead;
        facts.context.sdk_session.as_mut().unwrap().target = adapter.audience().device;
        facts.operation_id = "sdk-history-read".into();
        facts.body_hash = ObjectHash::body(&serde_json::json!({
            "method":"operation_status","space_id":fixture.space,"resource_id":fixture.resource.resource,
            "authorization_id":facts.operation_id,"conversation_id":"chat",
            "sdk_session":facts.context.sdk_session,"parent_invocation":parent.invocation_hash(),
        })).unwrap();
        let provenance = fixture.sdk_source_provenance_with_adapter(
            &parent,
            &facts,
            NOW + 400,
            &adapter.node,
            &grant,
        );
        let verified = verify_sdk_source_provenance(
            &fixture.state,
            &parent,
            &provenance,
            &service,
            &grant,
            &facts,
            None,
            NOW,
        )
        .unwrap();
        assert_eq!(verified.facts(), &facts);
        assert_ne!(provenance.session.service, fixture.audience().service);
        assert_ne!(provenance.session.device, fixture.audience().device);
        let history = execution_observation::tests::retained_history_fixture();
        let reference = remote_execution_history::derive_reference(
            &serde_json::to_string(&history.actual).unwrap(),
            &serde_json::to_string(&history.body).unwrap(),
            &serde_json::to_string(&history.invocation).unwrap(),
            &serde_json::to_string(&history.binding).unwrap(),
        )
        .unwrap();
        remote_execution_history::validate_remote_execution_history_reference(&reference).unwrap();
        assert!(matches!(
            verify_node_history_policy_source(
                &fixture.state,
                &fixture.pins,
                &trust,
                &parent,
                &reference,
                Some(&history.binding),
                Some(&provenance),
                &fixture.node,
                NOW,
                NOW + 200,
            ),
            Err(AuthorizationError::InvalidProof(
                "native History SDK source is not this Node's service delegation"
            ))
        ));
    }

    #[test]
    fn sdk_policy_source_rejects_wrong_adapter_expired_lease_and_revoked_grant() {
        use node_execution_policy::service::verify_node_service_policy_source;
        let mut fixture = Fixture::new();
        let remote = Fixture::with_keys(20, NOW + 1_000);
        let parent = fixture.parent(&fixture.owner_grant, NOW);
        let (trust, selection, context, facts) = fixture.task_policy_source(&remote);
        let mut wrong = facts.clone();
        wrong.context.sdk_session.as_mut().unwrap().target =
            DeviceId::from_key(&PublicKey::of(&remote.node));
        assert!(matches!(
            fixture.state.verify_sdk_policy_read_source(
                &parent,
                &fixture.audience().service,
                &fixture.service_grant,
                &wrong,
                NOW,
            ),
            Err(AuthorizationError::InvalidProof(
                "SDK policy source has no matching local adapter session"
            ))
        ));
        wrong = facts.clone();
        wrong.context.sdk_session = None;
        assert!(matches!(
            fixture.state.verify_sdk_policy_read_source(
                &parent,
                &fixture.audience().service,
                &fixture.service_grant,
                &wrong,
                NOW,
            ),
            Err(AuthorizationError::InvalidProof(
                "SDK policy source has no matching local adapter session"
            ))
        ));
        let provenance = fixture.sdk_source_provenance(&parent, &facts, NOW + 100);
        assert!(matches!(
            verify_node_service_policy_source(
                &fixture.state,
                &fixture.pins,
                &trust,
                &parent,
                &fixture.audience().service,
                &fixture.service_grant,
                &facts,
                &selection,
                &context,
                trust.work(),
                None,
                &provenance,
                &fixture.node,
                remote_execution_policy::RemoteExecutionPolicyPurpose::Execution,
                NOW + 100,
                NOW + 200,
            ),
            Err(AuthorizationError::InvalidProof(
                "SDK manager lease is outside its original interval"
            ))
        ));
        let removed = fixture.service_grant.clone();
        fixture.update_space(|document| document.grants.retain(|grant| grant != &removed));
        let parent = fixture.parent(&fixture.owner_grant, NOW);
        assert!(matches!(
            fixture.state.verify_sdk_policy_read_source(
                &parent,
                &fixture.audience().service,
                &fixture.service_grant,
                &facts,
                NOW,
            ),
            Err(AuthorizationError::Revoked)
        ));
    }

    #[test]
    fn remote_execution_target_closes_service_descriptor_dependencies() {
        let mut fixture = Fixture::new();
        let remote = Fixture::with_keys(20, NOW + 1_000);
        assert_ne!(fixture.account, remote.account);
        let remote_domain = PolicyDomain::Account(remote.account.clone());
        let remote_head = remote.state.head(&remote_domain).unwrap().clone();
        fixture.state.accounts.extend(remote.state.accounts);
        fixture.state.devices.extend(remote.state.devices);
        fixture.state.roots.extend(remote.state.roots);
        let parent = fixture.parent(&fixture.owner_grant, NOW);
        let mut facts = fixture.agent_facts();
        facts.context.execution_session = Some(SessionRef {
            target: DeviceId::from_key(&PublicKey::of(&remote.node)),
            session_id: "remote-execution".into(),
        });
        let proof = fixture
            .state
            .verify_service_grant(
                &fixture.audience().service,
                &fixture.service_grant,
                &facts,
                NOW,
            )
            .unwrap();
        assert!(proof.heads().contains(&remote_head));
        assert_eq!(proof.expires_at_ms(), NOW + 1_000);
        let delegated = fixture
            .state
            .verify_service_delegation(
                &parent,
                &fixture.audience().service,
                &fixture.service_grant,
                &facts,
                NOW,
            )
            .unwrap();
        assert_eq!(delegated.heads(), proof.heads());
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Service(proof.service().clone()),
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
                nonce: Nonce::from_bytes([33; 32]),
                not_before_ms: NOW,
                expires_at_ms: proof.expires_at_ms(),
            },
            &fixture.node,
        )
        .unwrap();
        let transport = TransportEvidence::LocalAgentAdapter {
            service: proof.service().clone(),
            session: facts.context.sdk_session.clone().unwrap(),
        };
        let verified = fixture
            .state
            .verify_invocation(&invocation, &facts, &transport, NOW)
            .unwrap();
        assert_eq!(verified.dependencies(), proof.heads());
        assert_eq!(
            fixture
                .state
                .verify_service_grant(
                    &fixture.audience().service,
                    &fixture.service_grant,
                    &facts,
                    NOW + 1_000,
                )
                .unwrap_err(),
            AuthorizationError::Expired
        );
        for (target, error) in [
            (
                DeviceId::from_key(&PublicKey::of(&remote.user)),
                AuthorizationError::InvalidProof("execution target is not a service"),
            ),
            (
                DeviceId::from_key(&PublicKey::of(&key(80))),
                AuthorizationError::Revoked,
            ),
        ] {
            facts.context.execution_session.as_mut().unwrap().target = target;
            assert_eq!(
                fixture
                    .state
                    .verify_service_grant(
                        &fixture.audience().service,
                        &fixture.service_grant,
                        &facts,
                        NOW,
                    )
                    .unwrap_err(),
                error
            );
        }
    }

    #[test]
    fn account_grant_and_default_node_role_do_not_authorize_a_service() {
        let fixture = Fixture::new();
        assert!(
            fixture
                .state
                .verify_service_grant(
                    &fixture.audience().service,
                    &fixture.owner_grant,
                    &fixture.agent_facts(),
                    NOW
                )
                .is_err()
        );
        let mut replica = fixture.agent_facts();
        replica.action = Action::ReplicaRead;
        assert!(
            fixture
                .state
                .verify_service_grant(
                    &fixture.audience().service,
                    &fixture.service_grant,
                    &replica,
                    NOW
                )
                .is_err()
        );
        let unknown = ServiceId::from_key(&PublicKey::of(&key(55)));
        assert_eq!(
            fixture
                .state
                .verify_service_grant(
                    &unknown,
                    &fixture.service_grant,
                    &fixture.agent_facts(),
                    NOW
                )
                .unwrap_err(),
            AuthorizationError::Revoked
        );
    }

    #[test]
    fn service_delegation_rejects_other_context_and_account_only_actions() {
        let fixture = Fixture::new();
        let parent = fixture.parent(&fixture.owner_grant, NOW);
        let mut facts = fixture.agent_facts();
        facts.context.conversation = Some("other-chat".into());
        assert!(
            fixture
                .state
                .verify_service_delegation(
                    &parent,
                    &fixture.audience().service,
                    &fixture.service_grant,
                    &facts,
                    NOW
                )
                .is_err()
        );
        facts = fixture.agent_facts();
        facts.resource.resource = ResourceId::from_bytes([88; 32]);
        assert!(
            fixture
                .state
                .verify_service_delegation(
                    &parent,
                    &fixture.audience().service,
                    &fixture.service_grant,
                    &facts,
                    NOW
                )
                .is_err()
        );
        facts = fixture.agent_facts();
        facts.action = Action::TaskAccept;
        assert_eq!(
            fixture
                .state
                .verify_service_grant(
                    &fixture.audience().service,
                    &fixture.service_grant,
                    &facts,
                    NOW
                )
                .unwrap_err(),
            AuthorizationError::InvalidProof("action requires account actor")
        );
    }

    #[test]
    fn parent_account_grant_must_be_the_actual_delegation_parent() {
        let mut fixture = Fixture::new();
        let mut alternate = fixture
            .state
            .grant(&fixture.owner_grant)
            .unwrap()
            .payload
            .clone();
        alternate.nonce = Nonce::from_bytes([99; 32]);
        let alternate = Signed::sign(alternate, &fixture.root).unwrap();
        let alternate = fixture.state.install_grant(alternate, NOW).unwrap();
        fixture.update_space(|document| document.grants.push(alternate.clone()));
        let parent = fixture.parent(&alternate, NOW);
        assert_eq!(
            fixture
                .state
                .verify_service_delegation(
                    &parent,
                    &fixture.audience().service,
                    &fixture.service_grant,
                    &fixture.agent_facts(),
                    NOW
                )
                .unwrap_err(),
            AuthorizationError::InvalidProof(
                "service delegation differs from the initiating account grant"
            )
        );
        let removed = fixture.service_grant.clone();
        fixture.update_space(|document| document.grants.retain(|grant| grant != &removed));
        assert_eq!(
            fixture
                .state
                .verify_service_grant(
                    &fixture.audience().service,
                    &fixture.service_grant,
                    &fixture.agent_facts(),
                    NOW
                )
                .unwrap_err(),
            AuthorizationError::Revoked
        );
    }
}
