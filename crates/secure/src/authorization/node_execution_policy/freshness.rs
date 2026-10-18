use super::*;
use crate::signature_verification::{SignatureVerification, SignatureVerificationCounts};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NodeExecutionFreshnessRead {
    pub version: u16,
    pub read: NodeExecutionPolicyReadIntent,
    pub challenge: FreshnessChallenge,
}
impl sealed::Sealed for NodeExecutionFreshnessRead {}
impl SignedObject for NodeExecutionFreshnessRead {
    const DOMAIN: &'static str = "node-execution-freshness-read-v2";
}

pub struct VerifiedNodeExecutionFreshnessRead {
    request: Signed<NodeExecutionFreshnessRead>,
    local_c: PublicKey,
    heads: Vec<PolicyStamp>,
    allowed_heads: BTreeSet<PolicyStamp>,
    verified_at_ms: u64,
    expires_at_ms: u64,
    usage: ExecutionProofUsage,
}
impl VerifiedNodeExecutionFreshnessRead {
    pub fn request(&self) -> &Signed<NodeExecutionFreshnessRead> {
        &self.request
    }
    pub fn challenge(&self) -> &FreshnessChallenge {
        &self.request.payload.challenge
    }
    pub fn target_heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
    pub fn allows_head(&self, head: &PolicyStamp) -> bool {
        self.allowed_heads.contains(head)
    }
}

pub fn verify_node_execution_freshness_read(
    state: &VerifiedPolicySet,
    request: &Signed<NodeExecutionFreshnessRead>,
    local_c: &PublicKey,
    actual_noise_b: &PublicKey,
    now: u64,
) -> Result<VerifiedNodeExecutionFreshnessRead> {
    let _signatures = ScopedSignatureVerification::new();
    require(
        canonical(request)?.len() <= MAX_PROOF_BYTES,
        "node freshness read carrier exceeds 64 KiB",
    )?;
    require(
        request.payload.version == AUTH_VERSION && request.issuer == *actual_noise_b,
        "node freshness reader differs from its actual Noise key",
    )?;
    request.verify_signature()?;
    let read = &request.payload.read;
    let closure = verify_node_current_policy_claims(state, read, local_c, actual_noise_b, now)?;
    let challenge = &request.payload.challenge;
    validate_window(challenge.issued_at_ms, challenge.expires_at_ms, now)?;
    require(
        challenge.version == AUTH_VERSION
            && challenge.audience
                == (Audience {
                    service: ServiceId::from_key(actual_noise_b),
                    device: DeviceId::from_key(actual_noise_b),
                })
            && valid_operation_id(&challenge.operation_id)
            && !challenge.policies.is_empty()
            && challenge.policies.len() <= MAX_POLICY_DOMAINS
            && challenge
                .policies
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain)
            && challenge.expires_at_ms - challenge.issued_at_ms <= challenge.stage.max_age()
            && (challenge.stage != CheckStage::RemoteRead || read.expected_action.is_read()),
        "node freshness read changed its original challenge or stage",
    )?;
    let mut allowed_heads = BTreeSet::new();
    for challenged in &challenge.policies {
        let Some(expected) = closure
            .heads
            .iter()
            .find(|head| head.domain == challenged.domain)
        else {
            continue;
        };
        if challenged != expected {
            return Err(AuthorizationError::NeedPolicySync(vec![expected.clone()]));
        }
        let authority = match &expected.domain {
            PolicyDomain::Account(account) => state.account(account).map(|value| &value.authority),
            PolicyDomain::Space(space) => state.space(space).map(|value| &value.authority),
        }
        .ok_or(AuthorizationError::PolicyConflict)?;
        if authority.public_key == *local_c {
            require(
                authority.device == DeviceId::from_key(local_c)
                    && authority.service == ServiceId::from_key(local_c),
                "node freshness authority has another device or service",
            )?;
            allowed_heads.insert(expected.clone());
        }
    }
    let expires_at_ms = read
        .expires_at_ms
        .min(challenge.expires_at_ms)
        .min(closure.expires_at_ms);
    require(now < expires_at_ms, "node freshness authority has expired")?;
    let mut usage = closure.usage;
    usage.record_signed(request)?;
    finish_freshness_usage(&mut usage)?;
    Ok(VerifiedNodeExecutionFreshnessRead {
        request: request.clone(),
        local_c: local_c.clone(),
        heads: closure.heads,
        allowed_heads,
        verified_at_ms: now,
        expires_at_ms,
        usage,
    })
}

/// Meters C's locally issued receipts without verifying signatures or business freshness.
pub fn record_node_freshness_receipts(
    capability: &VerifiedNodeExecutionFreshnessRead,
    receipts: &[Signed<FreshnessReceipt>],
    now: u64,
) -> Result<ExecutionProofUsage> {
    require(
        now >= capability.verified_at_ms && now < capability.expires_at_ms,
        "node freshness capability is outside its verified window",
    )?;
    require(
        canonical(&(capability.request(), receipts))?.len() <= MAX_PROOF_BYTES,
        "node freshness request and receipts exceed 64 KiB",
    )?;
    require(
        receipts.len() == capability.allowed_heads.len(),
        "node freshness receipts differ from this authority's exact subset",
    )?;
    let challenge = capability.challenge();
    let challenge_hash = challenge.hash()?;
    let mut seen = BTreeSet::new();
    let mut usage = capability.usage.clone();
    for receipt in receipts {
        let value = &receipt.payload;
        require(
            receipt.issuer == capability.local_c
                && value.version == AUTH_VERSION
                && capability.allows_head(&value.current)
                && seen.insert(value.current.domain.clone())
                && value.challenge_hash == challenge_hash
                && value.issued_at_ms >= challenge.issued_at_ms
                && value.issued_at_ms >= capability.verified_at_ms
                && value.expires_at_ms <= capability.expires_at_ms,
            "node freshness receipt changed its authority, challenge or window",
        )?;
        validate_window(value.issued_at_ms, value.expires_at_ms, now)?;
        usage.record_signed(receipt)?;
    }
    finish_freshness_usage(&mut usage)?;
    Ok(usage)
}

fn finish_freshness_usage(usage: &mut ExecutionProofUsage) -> Result<()> {
    let retained = usage.counts().actual.unwrap_or_default();
    let current = SignatureVerification::current_counts().unwrap_or_default();
    let actual = SignatureVerificationCounts {
        actual_verifications: retained
            .actual_verifications
            .max(current.actual_verifications),
        cache_hits: retained.cache_hits.max(current.cache_hits),
    };
    require(
        actual.actual_verifications <= MAX_PROOF_SIGNATURES,
        "node freshness exceeded the actual signature verification budget",
    )?;
    usage.record_actual_signature_counts(actual);
    usage.enforce_limits()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::execution_observation::tests::{
        RetainedHistoryFixture, retained_history_fixture,
    };

    fn request(fixture: &RetainedHistoryFixture, now: u64) -> Signed<NodeExecutionFreshnessRead> {
        let source = PublicKey::of(&fixture.project_node);
        let target = PublicKey::of(&fixture.executor);
        let project = fixture
            .actual
            .handoff
            .handoff
            .source_statement()
            .unwrap()
            .project;
        let leaf = fixture
            .state
            .grants
            .values()
            .find(|grant| {
                grant.payload.grantee
                    == Grantee::Actor(Actor::Service(ServiceId::from_key(&source)))
                    && grant.payload.actions.contains(&Action::ExecutionOpen)
                    && fixture
                        .state
                        .space(&grant.payload.owner_space)
                        .is_some_and(|space| space.grants.contains(&grant.hash().unwrap()))
            })
            .unwrap();
        let ResourceScope::Exact(resource) = &leaf.payload.scope else {
            panic!("fixture execution grant is not exact")
        };
        let mut context = leaf.payload.constraints.context.clone();
        context.workspace_resource = Some(project.resource.clone());
        let event: events::Event = serde_json::from_str(
            &fixture
                .actual
                .handoff
                .handoff
                .project_dispatch
                .canonical_body,
        )
        .unwrap();
        let events::EventCause::Invocation { admission } = event.cause else {
            panic!("fixture has no genuine original admission")
        };
        Signed::sign(
            NodeExecutionFreshnessRead {
                version: AUTH_VERSION,
                read: NodeExecutionPolicyReadIntent {
                    version: AUTH_VERSION,
                    source_provenance: None,
                    source_project: project,
                    selection: RemoteExecutionPolicySelection {
                        execution_resource: resource.clone(),
                        target_public_key: target.clone(),
                        execution_grant: leaf.hash().unwrap(),
                    },
                    context: prepare::PrepareContext {
                        context,
                        binding_generation: Some(1),
                        canonical_cwd: None,
                        execution_target_public_key: Some(target),
                    },
                    expected_action: Action::ExecutionOpen,
                    purpose: RemoteExecutionPolicyPurpose::Execution,
                    execution_pins: fixture.pins.clone(),
                    nonce: Nonce::random().unwrap(),
                    issued_at_ms: now,
                    expires_at_ms: now + 1_000,
                },
                challenge: admission.challenge,
            },
            &fixture.project_node,
        )
        .unwrap()
    }

    #[test]
    fn node_freshness_preserves_the_business_challenge_and_only_signs_c_authority() {
        let fixture = retained_history_fixture();
        let now = 10_020;
        let source = PublicKey::of(&fixture.project_node);
        let target = PublicKey::of(&fixture.executor);
        let request = request(&fixture, now);
        assert_ne!(
            request.payload.challenge.presenter,
            DeviceId::from_key(&source)
        );
        let capability =
            verify_node_execution_freshness_read(&fixture.state, &request, &target, &source, now)
                .unwrap();
        assert_eq!(capability.challenge(), &request.payload.challenge);
        assert!(capability.challenge().policies.len() > capability.target_heads().len());
        assert!(
            capability
                .challenge()
                .policies
                .iter()
                .any(|head| !capability.allows_head(head))
        );
        let receipts: Vec<_> = capability
            .target_heads()
            .iter()
            .filter(|head| capability.allows_head(head))
            .map(|head| {
                Signed::sign(
                    FreshnessReceipt {
                        version: AUTH_VERSION,
                        current: head.clone(),
                        challenge_hash: request.payload.challenge.hash().unwrap(),
                        issued_at_ms: now,
                        expires_at_ms: capability.expires_at_ms(),
                    },
                    &fixture.executor,
                )
                .unwrap()
            })
            .collect();
        assert!(!receipts.is_empty());
        for receipt in &receipts {
            receipt.verify_signature().unwrap();
            assert_eq!(receipt.issuer, target);
            assert_eq!(
                receipt.payload.challenge_hash,
                request.payload.challenge.hash().unwrap()
            );
        }
        let usage = record_node_freshness_receipts(&capability, &receipts, now).unwrap();
        assert_eq!(
            usage.counts().unique_signed_objects,
            capability.proof_usage().counts().unique_signed_objects + receipts.len()
        );
        assert!(usage.counts().authorization_bytes <= MAX_PROOF_BYTES);
        assert_eq!(
            usage.counts().actual.unwrap().actual_verifications,
            capability
                .proof_usage()
                .counts()
                .actual
                .unwrap()
                .actual_verifications
        );
        assert!(record_node_freshness_receipts(&capability, &[], now).is_err());
        let mut changed = receipts.clone();
        changed[0].payload.challenge_hash = ObjectHash::body(&"another challenge").unwrap();
        assert!(record_node_freshness_receipts(&capability, &changed, now).is_err());
        let mut changed = receipts.clone();
        changed[0].payload.expires_at_ms = capability.expires_at_ms() + 1;
        assert!(record_node_freshness_receipts(&capability, &changed, now).is_err());
        let mut changed = receipts.clone();
        changed[0].issuer = source.clone();
        assert!(record_node_freshness_receipts(&capability, &changed, now).is_err());
        let mut changed = receipts.clone();
        changed[0].signature = Signature("00".repeat(MAX_PROOF_BYTES));
        assert!(record_node_freshness_receipts(&capability, &changed, now).is_err());
        assert!(
            record_node_freshness_receipts(&capability, &receipts, capability.expires_at_ms())
                .is_err()
        );
    }

    #[test]
    fn node_freshness_rejects_changed_noise_scope_heads_stage_and_signature_domain() {
        let fixture = retained_history_fixture();
        let now = 10_020;
        let source = PublicKey::of(&fixture.project_node);
        let target = PublicKey::of(&fixture.executor);
        let request = request(&fixture, now);
        let verify = |value: &Signed<NodeExecutionFreshnessRead>| {
            verify_node_execution_freshness_read(&fixture.state, value, &target, &source, now)
        };
        assert!(
            verify_node_execution_freshness_read(
                &fixture.state,
                &request,
                &target,
                &PublicKey::of(&fixture.client),
                now,
            )
            .is_err()
        );
        let mut changed = request.clone();
        changed.signature = Signed::sign(request.payload.read.clone(), &fixture.project_node)
            .unwrap()
            .signature;
        assert!(verify(&changed).is_err());
        let mut value = request.payload.clone();
        value.read.context.context.task = Some(EventRef::parse(&"ad".repeat(32)).unwrap());
        assert!(verify(&Signed::sign(value, &fixture.project_node).unwrap()).is_err());
        let mut value = request.payload.clone();
        value.read.context.binding_generation = Some(0);
        assert!(verify(&Signed::sign(value, &fixture.project_node).unwrap()).is_err());
        let mut value = request.payload.clone();
        value.read.execution_pins[0].genesis_root = PublicKey::of(&fixture.client);
        assert!(verify(&Signed::sign(value, &fixture.project_node).unwrap()).is_err());
        let mut value = request.payload.clone();
        let c_domain = &fixture.pins[0].domain;
        value
            .challenge
            .policies
            .iter_mut()
            .find(|head| &head.domain == c_domain)
            .unwrap()
            .sequence += 1;
        assert!(verify(&Signed::sign(value, &fixture.project_node).unwrap()).is_err());
        let mut value = request.payload.clone();
        value.challenge.audience = Audience {
            service: ServiceId::from_key(&target),
            device: DeviceId::from_key(&target),
        };
        assert!(verify(&Signed::sign(value, &fixture.project_node).unwrap()).is_err());
        let mut value = request.payload.clone();
        value.challenge.stage = CheckStage::RemoteRead;
        assert!(verify(&Signed::sign(value, &fixture.project_node).unwrap()).is_err());
        let mut value = request.payload.clone();
        value
            .challenge
            .policies
            .push(value.challenge.policies[0].clone());
        assert!(verify(&Signed::sign(value, &fixture.project_node).unwrap()).is_err());
        let mut value = request.payload.clone();
        value.challenge.expires_at_ms = value.challenge.issued_at_ms + EXECUTION_LEASE_MS + 1;
        assert!(verify(&Signed::sign(value, &fixture.project_node).unwrap()).is_err());
        let mut value = request.payload.clone();
        value.challenge.operation_id = "x".repeat(MAX_PROOF_BYTES);
        assert!(verify(&Signed::sign(value, &fixture.project_node).unwrap()).is_err());
        assert!(
            verify_node_execution_freshness_read(
                &fixture.state,
                &request,
                &target,
                &source,
                request.payload.read.expires_at_ms,
            )
            .is_err()
        );
    }
}
