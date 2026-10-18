use super::*;

#[derive(Clone, Debug)]
pub struct VerifiedServiceDelegation {
    service: ServiceId,
    device: DeviceId,
    grant_leaf: ObjectHash,
    initiator: Actor,
    initiating_grant: Option<ObjectHash>,
    account_grant: Option<ObjectHash>,
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
            account_grant: self.account_grant,
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
                        session_id: "trigger".into(),
                    }),
                    workspace_resource: Some(self.resource.resource.clone()),
                    ..Default::default()
                },
            }
        }

        fn parent(&self, grant: &ObjectHash, now_ms: u64) -> VerifiedAuthorization {
            let context = InvocationContext {
                conversation: Some("chat".into()),
                workspace_resource: Some(self.resource.resource.clone()),
                ..Default::default()
            };
            let body_hash = ObjectHash::body(&serde_json::json!({"message":"run this"})).unwrap();
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
                            current: current.clone(),
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
            let admitted = self.state.admit_operation(&auth, &freshness, NOW).unwrap();
            self.state
                .verify_operation_authorization(&admitted, &facts, &transport, now_ms)
                .unwrap()
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
            self.state
                .install_space(Signed::sign(document, &self.root).unwrap())
                .unwrap();
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
        assert_eq!(proof.account_grant.as_ref(), Some(&fixture.owner_grant));
        assert!(proof.expires_at_ms() > NOW + 100_000);
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
