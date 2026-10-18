//! Source policy recovery for one executor-accepted immutable business wire.

use super::super::{
    execution_handoff::{
        execution_resolution_account_facts, verify_execution_handoff_admission_checkpoint,
    },
    remote_execution_history::RemoteExecutionHistoryAcceptance,
};
use super::*;

struct AcceptedBusinessAnchor {
    accepted_at_ms: u64,
    heads: Vec<PolicyStamp>,
    usage: ExecutionProofUsage,
}

fn accepted_anchor(
    wire: &TaskBusinessWire,
    acceptance: &RemoteExecutionHistoryAcceptance,
    local_c: &PublicKey,
    now: u64,
) -> Result<AcceptedBusinessAnchor> {
    carrier(acceptance)?;
    let scope = wire.scope()?;
    require(
        &scope.target_public_key == local_c,
        "original Task business reached another executor",
    )?;
    let (accepted_at_ms, heads, mut usage) = match (wire, acceptance) {
        (
            TaskBusinessWire::Submit(wire),
            RemoteExecutionHistoryAcceptance::ExecutionResult {
                receipt,
                freshness,
                transport,
            },
        ) => {
            let accepted_at_ms = receipt.payload.admitted_at_ms;
            let checkpoint = verify_execution_handoff_admission_checkpoint(
                wire,
                receipt,
                freshness,
                transport,
                accepted_at_ms,
            )?;
            require(
                transport.existing_evidence()?
                    == TransportEvidence::VerifiedPeer {
                        origin: wire.invocation.payload.presenter.clone(),
                    },
                "original Task business changed its accepted Project transport",
            )?;
            (
                accepted_at_ms,
                freshness.challenge.policies.clone(),
                checkpoint.proof_usage().clone(),
            )
        }
        (
            TaskBusinessWire::Resolve(wire),
            RemoteExecutionHistoryAcceptance::UnknownResolution { receipt, freshness },
        ) => {
            receipt.verify_signature()?;
            let value = &receipt.payload;
            require(
                wire.resolution.purpose == ExecutionResolutionPurpose::ResolveUnknown
                    && receipt.issuer == *local_c
                    && value.version == AUTH_VERSION
                    && value.resolution_hash == wire.hash()?
                    && value.freshness_hash == ObjectHash::body(freshness)?
                    && freshness.challenge.version == AUTH_VERSION
                    && freshness.challenge.resolution_hash == wire.hash()?
                    && freshness.challenge.resolver_invocation_hash
                        == wire.resolution.account_admission.invocation.hash()?
                    && freshness.challenge.submission_invocation_hash == wire.invocation.hash()?
                    && wire.resolution.account_admission.admitted_at_ms <= value.issued_at_ms,
                "original Task resolution changed its signed executor acceptance",
            )?;
            validate_window(
                wire.invocation.payload.not_before_ms,
                wire.invocation.payload.expires_at_ms,
                value.issued_at_ms,
            )?;
            validate_window(
                freshness.challenge.issued_at_ms,
                freshness.challenge.expires_at_ms,
                value.issued_at_ms,
            )?;
            let mut usage = ExecutionProofUsage::default();
            usage.record_signed(receipt)?;
            usage.record_metadata(freshness)?;
            (
                value.issued_at_ms,
                freshness.challenge.policies.clone(),
                usage,
            )
        }
        _ => {
            return Err(AuthorizationError::InvalidProof(
                "original Task business requires its own signed executor acceptance",
            ));
        }
    };
    require(
        accepted_at_ms <= now
            && !heads.is_empty()
            && heads.len() <= MAX_POLICY_DOMAINS
            && heads.windows(2).all(|pair| pair[0].domain < pair[1].domain),
        "original Task business changed its acceptance clock or exact heads",
    )?;
    finish_usage(&mut usage)?;
    Ok(AcceptedBusinessAnchor {
        accepted_at_ms,
        heads,
        usage,
    })
}

#[derive(Clone)]
pub struct TaskBusinessOriginalPolicyImport {
    wire: TaskBusinessWire,
    acceptance: RemoteExecutionHistoryAcceptance,
    accepted_at_ms: u64,
    accepted_heads: Vec<PolicyStamp>,
    import: TaskBusinessPolicyImport,
}

pub fn begin_task_business_original_import(
    primary: &VerifiedPolicySet,
    wire: &TaskBusinessWire,
    acceptance: &RemoteExecutionHistoryAcceptance,
    header: &Signed<TaskBusinessPolicyHeader>,
    local_c: &PublicKey,
    now: u64,
) -> Result<TaskBusinessOriginalPolicyImport> {
    let _signatures = ScopedSignatureVerification::new();
    let anchor = accepted_anchor(wire, acceptance, local_c, now)?;
    let scope = wire.scope()?;
    carrier(header)?;
    header.verify_signature()?;
    require(
        header.payload.version == AUTH_VERSION
            && header.payload.scope == scope
            && header.issuer == scope.source_node_public_key
            && !header.payload.source_heads.is_empty()
            && header.payload.source_heads.len() <= MAX_POLICY_DOMAINS
            && header
                .payload
                .source_heads
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain)
            && header
                .payload
                .source_heads
                .iter()
                .all(|head| anchor.heads.contains(head)),
        "original Task business changed its signed source header or accepted heads",
    )?;
    window(
        header.payload.issued_at_ms,
        header.payload.expires_at_ms,
        anchor.accepted_at_ms,
    )?;
    let primary_heads: Vec<_> = anchor
        .heads
        .iter()
        .filter(|head| primary.head(&head.domain).is_some())
        .cloned()
        .collect();
    require(
        primary_heads
            .iter()
            .any(|head| head.domain == PolicyDomain::Space(scope.execution_resource.space.clone())),
        "original Task business omitted its executor acceptance policy",
    )?;
    let historical = remote_resolution_history::at_heads(primary, &primary_heads)?;
    let mut usage = anchor.usage;
    let (approved, deadline) = source_grant(
        &historical,
        &scope,
        local_c,
        anchor.accepted_at_ms,
        &mut usage,
    )?;
    require(
        header.payload.expires_at_ms <= deadline
            && header
                .payload
                .source_heads
                .iter()
                .all(|head| approved.pins.iter().any(|pin| pin.domain == head.domain)),
        "original Task business introduced roots outside its accepted executor grant",
    )?;
    let pins: Vec<_> = approved
        .pins
        .iter()
        .filter(|pin| {
            header
                .payload
                .source_heads
                .iter()
                .any(|head| head.domain == pin.domain)
        })
        .cloned()
        .collect();
    let required = pins
        .iter()
        .filter_map(|pin| pin.minimum_head.clone())
        .collect();
    let import = BoundedPolicyImport::new(pins, required)?;
    let local_authority = local_authority(&historical, &scope)?;
    require(
        local_authority
            .heads()
            .iter()
            .all(|head| anchor.heads.contains(head)),
        "original Task business omitted its exact executor authority closure",
    )?;
    usage.record_signed(header)?;
    finish_usage(&mut usage)?;
    Ok(TaskBusinessOriginalPolicyImport {
        wire: wire.clone(),
        acceptance: acceptance.clone(),
        accepted_at_ms: anchor.accepted_at_ms,
        accepted_heads: anchor.heads,
        import: TaskBusinessPolicyImport {
            header: header.clone(),
            local_authority,
            import,
            original_minima: Vec::new(),
            previous_page_hash: header.hash()?,
            next_cursor: None,
            page_count: 0,
            entry_count: 0,
            complete: false,
            last_page: None,
            page_envelope_hashes: Vec::new(),
            usage,
        },
    })
}

fn original_account_authorization(
    state: &VerifiedPolicySet,
    proof: &events::AdmissionProof,
    facts: &RequestFacts,
    now: u64,
) -> Result<VerifiedAuthorization> {
    let admitted = state.verify_original_admission(proof, facts)?;
    state.verify_admitted_operation_authorization(&admitted, facts, now)
}

impl TaskBusinessOriginalPolicyImport {
    pub fn header(&self) -> &Signed<TaskBusinessPolicyHeader> {
        self.import.header()
    }

    pub fn accepted_at_ms(&self) -> u64 {
        self.accepted_at_ms
    }

    pub fn next_page_index(&self) -> u64 {
        self.import.next_page_index()
    }

    pub fn next_cursor(&self) -> Option<&ObjectHash> {
        self.import.next_cursor()
    }

    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        self.import.proof_usage()
    }

    pub fn append_page(&mut self, page: &Signed<TaskBusinessPolicyPage>) -> Result<()> {
        self.import.append_page(page, self.accepted_at_ms)
    }

    pub fn finish_original(
        self,
        witness: &Signed<TaskBusinessPolicyWitness>,
    ) -> Result<VerifiedTaskBusinessOriginalSource> {
        let _signatures = ScopedSignatureVerification::new();
        let source = self.import.finish(witness, self.accepted_at_ms)?;
        source.verify_wire(&self.wire, self.accepted_at_ms)?;
        let local = policy_import::native_task_business_primary_snapshot(&source.local_authority)?;
        let combined = policy_import::merge_same_head_snapshots(&local, &source.snapshot)?;
        let state = combined.state();
        let mut usage = source.proof_usage().clone();
        match (&self.wire, &self.acceptance) {
            (
                TaskBusinessWire::Submit(wire),
                RemoteExecutionHistoryAcceptance::ExecutionResult {
                    receipt,
                    freshness,
                    transport,
                },
            ) => {
                let event: Event = serde_json::from_str(
                    &wire.handoff.project_dispatch.canonical_body,
                )
                .map_err(|_| AuthorizationError::InvalidProof("invalid original Task dispatch"))?;
                let EventCause::Invocation { admission } = event.cause else {
                    return Err(AuthorizationError::InvalidProof(
                        "original Task business has no admitted source request",
                    ));
                };
                let original = original_account_authorization(
                    state,
                    &admission,
                    &request_facts(&admission.invocation.payload),
                    self.accepted_at_ms,
                )?;
                let verified = state.verify_execution_handoff(
                    wire,
                    &original,
                    &transport.existing_evidence()?,
                    None,
                    self.accepted_at_ms,
                )?;
                let fresh = state.verify_execution_handoff_freshness(
                    &verified,
                    freshness,
                    self.accepted_at_ms,
                )?;
                let checkpoint = verify_execution_handoff_admission_checkpoint(
                    wire,
                    receipt,
                    freshness,
                    transport,
                    self.accepted_at_ms,
                )?;
                require(
                    verified.dependencies() == self.accepted_heads,
                    "original Task business changed its accepted dependency closure",
                )?;
                usage.merge(fresh.proof_usage());
                usage.merge(checkpoint.proof_usage());
            }
            (
                TaskBusinessWire::Resolve(wire),
                RemoteExecutionHistoryAcceptance::UnknownResolution { receipt, freshness },
            ) => {
                let proof = &wire.resolution.account_admission;
                let resolver = original_account_authorization(
                    state,
                    proof,
                    &execution_resolution_account_facts(&wire.resolution)?,
                    self.accepted_at_ms,
                )?;
                let verified = state.verify_execution_resolution(
                    wire,
                    &resolver,
                    &TransportEvidence::VerifiedPeer {
                        origin: wire.invocation.payload.presenter.clone(),
                    },
                    None,
                    self.accepted_at_ms,
                )?;
                state.verify_execution_resolution_freshness(
                    &verified,
                    freshness,
                    self.accepted_at_ms,
                )?;
                require(
                    verified.dependencies() == self.accepted_heads,
                    "original Task resolution changed its accepted dependency closure",
                )?;
                usage.merge(verified.proof_usage());
                usage.record_signed(receipt)?;
                usage.record_metadata(freshness)?;
                for receipt in &freshness.receipts {
                    usage.record_signed(receipt)?;
                }
            }
            _ => {
                return Err(AuthorizationError::InvalidProof(
                    "original Task business changed its accepted purpose",
                ));
            }
        }
        finish_usage(&mut usage)?;
        Ok(VerifiedTaskBusinessOriginalSource {
            wire: self.wire,
            acceptance: self.acceptance,
            accepted_at_ms: self.accepted_at_ms,
            accepted_heads: self.accepted_heads,
            source,
            combined,
            usage,
        })
    }
}

#[derive(Clone)]
pub struct VerifiedTaskBusinessOriginalSource {
    wire: TaskBusinessWire,
    acceptance: RemoteExecutionHistoryAcceptance,
    accepted_at_ms: u64,
    accepted_heads: Vec<PolicyStamp>,
    source: VerifiedTaskBusinessSource,
    combined: VerifiedPolicySnapshot,
    usage: ExecutionProofUsage,
}

impl VerifiedTaskBusinessOriginalSource {
    pub(super) fn source_snapshot(&self) -> &VerifiedPolicySnapshot {
        &self.source.snapshot
    }

    pub fn wire(&self) -> &TaskBusinessWire {
        &self.wire
    }

    pub fn wire_hash(&self) -> &ObjectHash {
        self.source.wire_hash()
    }

    pub fn acceptance(&self) -> &RemoteExecutionHistoryAcceptance {
        &self.acceptance
    }

    pub fn acceptance_envelope_hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(&self.acceptance)
    }

    pub fn header(&self) -> &Signed<TaskBusinessPolicyHeader> {
        self.source.header()
    }

    pub fn header_envelope_hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(self.source.header())
    }

    pub fn witness(&self) -> &Signed<TaskBusinessPolicyWitness> {
        self.source.witness()
    }

    pub fn witness_envelope_hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(self.source.witness())
    }

    pub fn original_page_envelope_hashes(&self) -> &[ObjectHash] {
        self.source.original_page_envelope_hashes()
    }

    pub fn scope(&self) -> &TaskBusinessScope {
        self.source.scope()
    }

    pub fn accepted_at_ms(&self) -> u64 {
        self.accepted_at_ms
    }

    pub fn accepted_heads(&self) -> &[PolicyStamp] {
        &self.accepted_heads
    }

    pub fn primary_heads(&self) -> &[PolicyStamp] {
        self.source.primary_heads()
    }

    pub fn foreign_heads(&self) -> &[PolicyStamp] {
        self.source.heads()
    }

    pub fn pins(&self) -> &[PolicyPin] {
        self.source.pins()
    }

    pub fn state(&self) -> &VerifiedPolicySet {
        self.combined.state()
    }

    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::{
        client_policy_import::require_confirmed_client_policy,
        events::StoredTransport,
        execution_handoff::{
            ExecutionHandoffAdmissionReceipt, ExecutionHandoffChallenge, ExecutionHandoffFreshness,
        },
        execution_observation::tests::retained_history_fixture_with_execution_source,
    };
    use super::super::tests::{current_policy, noise_peer, original_authorization};
    use super::*;

    #[test]
    fn accepted_task_source_recovers_original_after_current_executor_grant_revocation() {
        let mut fixture = retained_history_fixture_with_execution_source();
        let now = fixture.invocation.payload.not_before_ms;
        let current = current_policy(&fixture);
        let wire = TaskBusinessWire::Submit(fixture.actual.handoff.clone());
        let original = original_authorization(current.state(), &fixture.actual.handoff, now);
        let transport = TransportEvidence::VerifiedPeer {
            origin: DeviceId::from_key(&PublicKey::of(&fixture.project_node)),
        };
        let verified = current
            .state()
            .verify_execution_handoff(&fixture.actual.handoff, &original, &transport, None, now)
            .unwrap();
        let challenge = ExecutionHandoffChallenge::new(
            &verified,
            CheckStage::Admit,
            Nonce::random().unwrap(),
            now,
            now + 1_000,
        )
        .unwrap();
        let freshness = ExecutionHandoffFreshness {
            receipts: verified
                .dependencies()
                .iter()
                .map(|head| {
                    let authority = match &head.domain {
                        PolicyDomain::Account(id) => {
                            &current.state().account(id).unwrap().authority
                        }
                        PolicyDomain::Space(id) => &current.state().space(id).unwrap().authority,
                    };
                    let signer = [&fixture.project_node, &fixture.executor]
                        .into_iter()
                        .find(|identity| PublicKey::of(identity) == authority.public_key)
                        .unwrap();
                    Signed::sign(
                        FreshnessReceipt {
                            version: AUTH_VERSION,
                            current: head.clone(),
                            challenge_hash: challenge.hash().unwrap(),
                            issued_at_ms: now,
                            expires_at_ms: now + 1_000,
                        },
                        signer,
                    )
                    .unwrap()
                })
                .collect(),
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
        let stored_transport = StoredTransport::VerifiedPeer {
            origin: fixture.actual.handoff.invocation.payload.presenter.clone(),
        };
        fixture.acceptance = RemoteExecutionHistoryAcceptance::ExecutionResult {
            receipt: Signed::sign(
                ExecutionHandoffAdmissionReceipt::new(
                    &fixture.actual.handoff,
                    &freshness,
                    &stored_transport,
                    now,
                )
                .unwrap(),
                &fixture.executor,
            )
            .unwrap(),
            freshness,
            transport: stored_transport,
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
        let header = export.header();
        let c_key = PublicKey::of(&fixture.executor);
        let grant = wire.invocation().payload.grant_leaf.clone();
        let domain = PolicyDomain::Space(wire.scope().unwrap().execution_resource.space.clone());
        let mut document = fixture
            .state
            .space(&wire.scope().unwrap().execution_resource.space)
            .unwrap()
            .clone();
        document.previous = fixture.state.head(&domain).cloned();
        document.sequence += 1;
        document.grants.retain(|hash| hash != &grant);
        fixture
            .state
            .install_space(Signed::sign(document, &fixture.root).unwrap())
            .unwrap();
        let today = now + INVOCATION_MAX_AGE_MS + 20;
        assert!(
            begin_task_business_policy_import(
                &fixture.state,
                &noise_peer(&fixture.project_node, &fixture.executor),
                header,
                &c_key,
                &[],
                today,
            )
            .is_err()
        );
        let mut import = begin_task_business_original_import(
            &fixture.state,
            &wire,
            &fixture.acceptance,
            header,
            &c_key,
            today,
        )
        .unwrap();
        assert_eq!(import.accepted_at_ms(), now);
        assert!(import.clone().finish_original(&witness).is_err());
        for page in &pages {
            import.append_page(page).unwrap();
        }
        let original = import.finish_original(&witness).unwrap();
        assert_eq!(original.wire(), &wire);
        assert_eq!(original.accepted_at_ms(), now);
        assert_eq!(original.acceptance(), &fixture.acceptance);
        assert_eq!(original.wire_hash(), &wire.hash().unwrap());
        assert_eq!(
            original.header_envelope_hash().unwrap(),
            ObjectHash::body(header).unwrap()
        );
        assert_eq!(
            original.witness_envelope_hash().unwrap(),
            ObjectHash::body(&witness).unwrap()
        );
        assert_eq!(
            original.original_page_envelope_hashes(),
            pages
                .iter()
                .map(ObjectHash::body)
                .collect::<Result<Vec<_>>>()
                .unwrap()
        );
        assert_ne!(original.state().head(&domain), fixture.state.head(&domain));
        assert!(
            original
                .state()
                .resource(
                    &header.payload.scope.source_project.space,
                    &header.payload.scope.source_project.resource,
                )
                .is_some()
        );

        let mut changed_wire = fixture.actual.handoff.clone();
        changed_wire
            .invocation
            .payload
            .operation_id
            .push_str("-other");
        assert!(
            begin_task_business_original_import(
                &fixture.state,
                &TaskBusinessWire::Submit(changed_wire),
                &fixture.acceptance,
                header,
                &c_key,
                today,
            )
            .is_err()
        );
        let mut changed_header = header.clone();
        let ExecutionWork::TaskSession {
            binding_generation, ..
        } = &mut changed_header.payload.scope.work
        else {
            panic!("fixture requires Task work");
        };
        *binding_generation += 1;
        assert!(
            begin_task_business_original_import(
                &fixture.state,
                &wire,
                &fixture.acceptance,
                &changed_header,
                &c_key,
                today,
            )
            .is_err()
        );
        let mut changed_clock = fixture.acceptance.clone();
        let RemoteExecutionHistoryAcceptance::ExecutionResult { receipt, .. } = &mut changed_clock
        else {
            panic!("fixture requires executor admission");
        };
        receipt.payload.admitted_at_ms += 1;
        assert!(
            begin_task_business_original_import(
                &fixture.state,
                &wire,
                &changed_clock,
                header,
                &c_key,
                today,
            )
            .is_err()
        );
        let mut before_acceptance = fixture.acceptance.clone();
        let RemoteExecutionHistoryAcceptance::ExecutionResult { receipt, .. } =
            &mut before_acceptance
        else {
            unreachable!();
        };
        receipt.issuer = PublicKey::of(&fixture.project_node);
        assert!(
            begin_task_business_original_import(
                &fixture.state,
                &wire,
                &before_acceptance,
                header,
                &c_key,
                today,
            )
            .is_err()
        );

        let mut wrong_roots = fixture.state.clone();
        wrong_roots
            .grants
            .get_mut(&grant)
            .unwrap()
            .payload
            .execution_source
            .as_mut()
            .unwrap()
            .pins[0]
            .genesis_root = PublicKey::of(&fixture.client);
        assert!(
            begin_task_business_original_import(
                &wrong_roots,
                &wire,
                &fixture.acceptance,
                header,
                &c_key,
                today,
            )
            .is_err()
        );
    }
}
