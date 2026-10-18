use super::tests::{readonly_policy, revoke_writer};
use super::*;
use crate::authorization::{
    client_policy_import::require_confirmed_client_policy,
    events::{AdmissionProof, StoredTransport},
    execution_handoff::{
        ExecutionHandoffResolution, ExecutionHandoffResolutionFreshness,
        ExecutionResolutionPurpose, execution_resolution_account_facts, execution_resolution_facts,
        verify_execution_handoff_admission_checkpoint,
    },
    execution_observation::tests::{
        RetainedHistoryFixture, retained_history_fixture_with_execution_source,
    },
    remote_execution_history::RemoteExecutionHistoryAcceptance,
    remote_execution_policy::RemoteExecutionPolicyPurpose,
    task_business_policy::tests::{current_policy, noise_peer, original_authorization},
};

fn signed_receipts(
    state: &VerifiedPolicySet,
    fixture: &RetainedHistoryFixture,
    heads: &[PolicyStamp],
    challenge_hash: &ObjectHash,
    now: u64,
    expires: u64,
) -> Vec<Signed<FreshnessReceipt>> {
    heads
        .iter()
        .map(|head| {
            let authority = match &head.domain {
                PolicyDomain::Account(account) => &state.account(account).unwrap().authority,
                PolicyDomain::Space(space) => &state.space(space).unwrap().authority,
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

fn resolution_authorization(
    fixture: &RetainedHistoryFixture,
    current: &VerifiedTaskCurrentPolicy,
    evidence: &str,
    now: u64,
) -> VerifiedExecutionResolution {
    let state = current.state();
    let selection = current.selection();
    let binding = &fixture.binding.binding;
    let body = serde_json::json!({
        "method": "resolve_unknown",
        "space_id": fixture.invocation.payload.resource.space,
        "resource_id": fixture.invocation.payload.resource.resource,
        "authorization_id": "resolve-after-revocation",
        "operation_id": fixture.invocation.payload.operation_id,
        "conversation_id": binding.conversation_id,
        "task_id": fixture.body["request"]["task_id"],
        "task_ref": binding.task_ref,
        "target_node": binding.target_node,
        "session_id": binding.session_id,
        "outcome": "succeeded",
        "evidence": evidence,
        "execution_resource": selection.execution_resource,
        "target_public_key": selection.target_public_key,
        "execution_grant": selection.execution_grant,
        "original_handoff_hash": fixture.actual.handoff.hash().unwrap(),
        "original_body_hash": fixture.invocation.payload.body_hash,
        "unknown_receipt_hash": fixture.actual.receipt.hash().unwrap(),
        "resolution_purpose": ExecutionResolutionPurpose::ReconcileTerminal,
        "execution_observation": fixture.actual,
    });
    let facts = RequestFacts {
        resource: fixture.invocation.payload.resource.clone(),
        action: Action::OperationResolve,
        audience: fixture.invocation.payload.audience.clone(),
        operation_id: "resolve-after-revocation".into(),
        body_hash: ObjectHash::body(&body).unwrap(),
        context: fixture.invocation.payload.context.clone(),
    };
    let destination = state
        .verify_execution_destination(
            &facts.resource,
            &facts.audience,
            &selection.execution_resource,
            &selection.target_public_key,
            &selection.execution_grant,
            &facts.context,
            facts.action,
            now,
        )
        .unwrap();
    let mut invocation = fixture.invocation.payload.clone();
    invocation.action = facts.action;
    invocation.operation_id = facts.operation_id.clone();
    invocation.body_hash = facts.body_hash.clone();
    invocation.grant_leaf = fixture.source.grant_leaf.clone();
    invocation.expected_policies.clear();
    invocation.nonce = Nonce::random().unwrap();
    invocation.not_before_ms = now;
    invocation.expires_at_ms = now + 900;
    let account_transport = TransportEvidence::DirectNoise {
        device: invocation.device.clone(),
    };
    let authorize = |signed: &Signed<Invocation>| {
        state.verify_invocation_with_execution_destination(
            signed,
            &facts,
            &account_transport,
            &destination,
            now,
        )
    };
    let initial = Signed::sign(invocation.clone(), &fixture.client).unwrap();
    let AuthorizationError::NeedPolicySync(heads) = authorize(&initial).unwrap_err() else {
        panic!("resolver must discover its genuine current policy closure")
    };
    invocation.expected_policies = heads;
    let invocation = Signed::sign(invocation, &fixture.client).unwrap();
    let account = authorize(&invocation).unwrap();
    let challenge = FreshnessChallenge::new(
        &account,
        CheckStage::Admit,
        Nonce::random().unwrap(),
        now,
        now + 500,
    )
    .unwrap();
    let receipts = signed_receipts(
        state,
        fixture,
        account.dependencies(),
        &challenge.hash().unwrap(),
        now,
        challenge.expires_at_ms,
    );
    let fresh = state
        .verify_freshness(&account, &challenge, &receipts, now)
        .unwrap();
    let admitted = state.admit_operation(&account, &fresh, now).unwrap();
    let resolver = state
        .verify_operation_authorization(&admitted, &facts, &account_transport, now)
        .unwrap();
    let resolution = ExecutionHandoffResolution {
        version: AUTH_VERSION,
        purpose: ExecutionResolutionPurpose::ReconcileTerminal,
        observation: fixture.actual.clone(),
        account_admission: AdmissionProof {
            invocation,
            source_body: body,
            challenge,
            receipts,
            admitted_at_ms: now,
            transport: StoredTransport::from(&account_transport),
        },
    };
    assert_eq!(
        execution_resolution_account_facts(&resolution).unwrap(),
        facts
    );
    let facts = execution_resolution_facts(&resolution, &resolver).unwrap();
    let service = ServiceId::from_key(&PublicKey::of(&fixture.project_node));
    let descriptor = state
        .verify_service_grant(&service, &selection.execution_grant, &facts, now)
        .unwrap();
    let invocation = Signed::sign(
        Invocation {
            version: AUTH_VERSION,
            actor: Actor::Service(service),
            device: descriptor.device().clone(),
            initiator: descriptor.initiator().clone(),
            initiating_grant: descriptor.initiating_grant().cloned(),
            resource: facts.resource,
            action: facts.action,
            audience: facts.audience,
            presenter: descriptor.device().clone(),
            operation_id: facts.operation_id,
            body_hash: facts.body_hash,
            grant_leaf: selection.execution_grant.clone(),
            expected_policies: descriptor.heads().to_vec(),
            context: facts.context,
            nonce: Nonce::random().unwrap(),
            not_before_ms: now,
            expires_at_ms: now + 900,
        },
        &fixture.project_node,
    )
    .unwrap();
    state
        .verify_execution_resolution(
            &SignedExecutionHandoffResolution {
                resolution,
                invocation,
            },
            &resolver,
            &TransportEvidence::VerifiedPeer {
                origin: DeviceId::from_key(&PublicKey::of(&fixture.project_node)),
            },
            None,
            now,
        )
        .unwrap()
}

#[test]
fn task_business_resolution_receipts_keep_typed_hash_after_writer_revocation() {
    let mut fixture = retained_history_fixture_with_execution_source();
    let admitted_at = fixture.invocation.payload.not_before_ms;
    let now = admitted_at + 20;
    let writer = current_policy(&fixture);
    let original = original_authorization(writer.state(), &fixture.actual.handoff, admitted_at);
    let RemoteExecutionHistoryAcceptance::ExecutionResult {
        receipt,
        freshness,
        transport,
    } = &fixture.acceptance
    else {
        panic!("fixture has no accepted submission")
    };
    let admitted = verify_execution_handoff_admission_checkpoint(
        &fixture.actual.handoff,
        receipt,
        freshness,
        transport,
        admitted_at,
    )
    .unwrap();
    let submission = writer
        .state()
        .verify_execution_handoff(
            &fixture.actual.handoff,
            &original,
            &transport.clone().into_evidence(),
            Some(&admitted),
            admitted_at,
        )
        .unwrap();
    revoke_writer(&mut fixture, &writer);
    let readonly = readonly_policy(
        &fixture,
        writer.trust(),
        RemoteExecutionPolicyPurpose::Resolver,
        now,
    );
    assert!(
        !readonly
            .state()
            .space(&fixture.actual.handoff.handoff.execution_resource.space)
            .unwrap()
            .grants
            .contains(&fixture.actual.handoff.invocation.payload.grant_leaf)
    );
    assert!(
        readonly
            .state()
            .check_execution_handoff_current(&submission, now)
            .is_err()
    );
    let resolution = resolution_authorization(
        &fixture,
        &readonly,
        "executor signed the terminal outcome",
        now,
    );
    let wire = TaskBusinessWire::Resolve(resolution.wire().clone());
    let challenge = ExecutionHandoffResolutionChallenge::new(
        &resolution,
        Nonce::random().unwrap(),
        now,
        now + 300,
    )
    .unwrap();
    let typed = TaskBusinessChallenge::Resolve(challenge.clone());
    assert_eq!(typed.hash().unwrap(), challenge.hash().unwrap());
    assert_ne!(typed.hash().unwrap(), ObjectHash::body(&typed).unwrap());
    let source = require_confirmed_client_policy(&fixture.source).unwrap();
    let b_key = PublicKey::of(&fixture.project_node);
    let authority = authorize_task_business_challenge(
        source.state(),
        &readonly,
        TaskBusinessAuthorization::Resolve(&resolution),
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
    let foreign_heads = challenge
        .policies
        .iter()
        .filter(|head| !authority.signing_heads().contains(head))
        .cloned()
        .collect::<Vec<_>>();
    assert!(!foreign_heads.is_empty());
    let foreign = signed_receipts(
        readonly.state(),
        &fixture,
        &foreign_heads,
        &challenge.hash().unwrap(),
        now,
        authority.expires_at_ms(),
    );
    assert!(
        foreign
            .iter()
            .all(|receipt| receipt.issuer == PublicKey::of(&fixture.executor))
    );
    receipts.extend(foreign);
    readonly
        .state()
        .verify_execution_resolution_freshness(
            &resolution,
            &ExecutionHandoffResolutionFreshness {
                challenge: challenge.clone(),
                receipts,
            },
            now,
        )
        .unwrap();

    for anchor in ["resolution", "resolver", "submission"] {
        let mut changed = challenge.clone();
        let other = ObjectHash::body(&"another signed business anchor").unwrap();
        match anchor {
            "resolution" => changed.resolution_hash = other,
            "resolver" => changed.resolver_invocation_hash = other,
            _ => changed.submission_invocation_hash = other,
        }
        assert!(
            authorize_task_business_challenge(
                source.state(),
                &readonly,
                TaskBusinessAuthorization::Resolve(&resolution),
                &wire,
                &TaskBusinessChallenge::Resolve(changed),
                &b_key,
                now,
            )
            .is_err()
        );
    }
    for stage in [CheckStage::Start, CheckStage::Renew] {
        let challenge = TaskBusinessChallenge::Submit(
            ExecutionHandoffChallenge::new(
                &submission,
                stage,
                Nonce::random().unwrap(),
                now,
                now + 300,
            )
            .unwrap(),
        );
        assert!(
            authorize_task_business_challenge(
                source.state(),
                &readonly,
                TaskBusinessAuthorization::Submit(&submission),
                &TaskBusinessWire::Submit(fixture.actual.handoff.clone()),
                &challenge,
                &b_key,
                now,
            )
            .is_err()
        );
    }
    let export = build_task_business_policy_export(
        &readonly,
        TaskBusinessAuthorization::Resolve(&resolution),
        &wire,
        &fixture.project_node,
        now,
    )
    .unwrap();
    let read = Signed::sign(
        TaskBusinessReceiptRead {
            version: AUTH_VERSION,
            source_header_hash: export.header().hash().unwrap(),
            business_wire_hash: wire.hash().unwrap(),
            challenge: typed,
            acceptance: None,
            nonce: Nonce::random().unwrap(),
            issued_at_ms: now,
            expires_at_ms: now + 200,
        },
        &fixture.executor,
    )
    .unwrap();
    let actual_c = noise_peer(&fixture.executor, &fixture.project_node);
    verify_task_business_receipt_read(
        source.state(),
        &readonly,
        TaskBusinessAuthorization::Resolve(&resolution),
        export.header(),
        &wire,
        &actual_c,
        &read,
        &b_key,
        now,
    )
    .unwrap();
    let wrong_peer = noise_peer(&fixture.project_node, &fixture.executor);
    assert!(
        verify_task_business_receipt_read(
            source.state(),
            &readonly,
            TaskBusinessAuthorization::Resolve(&resolution),
            export.header(),
            &wire,
            &wrong_peer,
            &read,
            &b_key,
            now,
        )
        .is_err()
    );
}

#[test]
fn task_business_resolution_receipts_reject_combined_current_policy_budget() {
    const BASELINE_EVIDENCE: &str = "executor signed the terminal outcome";
    let mut fixture = retained_history_fixture_with_execution_source();
    let now = fixture.invocation.payload.not_before_ms + 20;
    let writer = current_policy(&fixture);
    revoke_writer(&mut fixture, &writer);
    let readonly = readonly_policy(
        &fixture,
        writer.trust(),
        RemoteExecutionPolicyPurpose::Resolver,
        now,
    );
    let baseline = resolution_authorization(&fixture, &readonly, BASELINE_EVIDENCE, now);
    let request_for = |resolution: &VerifiedExecutionResolution| {
        let wire = TaskBusinessWire::Resolve(resolution.wire().clone());
        let challenge = TaskBusinessChallenge::Resolve(
            ExecutionHandoffResolutionChallenge::new(
                resolution,
                Nonce::random().unwrap(),
                now,
                now + 300,
            )
            .unwrap(),
        );
        let export = build_task_business_policy_export(
            &readonly,
            TaskBusinessAuthorization::Resolve(resolution),
            &wire,
            &fixture.project_node,
            now,
        )
        .unwrap();
        let read = Signed::sign(
            TaskBusinessReceiptRead {
                version: AUTH_VERSION,
                source_header_hash: export.header().hash().unwrap(),
                business_wire_hash: wire.hash().unwrap(),
                challenge,
                acceptance: None,
                nonce: Nonce::random().unwrap(),
                issued_at_ms: now,
                expires_at_ms: now + 200,
            },
            &fixture.executor,
        )
        .unwrap();
        (wire, export.header().clone(), read)
    };
    let (baseline_wire, baseline_header, baseline_read) = request_for(&baseline);
    let source = require_confirmed_client_policy(&fixture.source).unwrap();
    let b_key = PublicKey::of(&fixture.project_node);
    let actual_c = noise_peer(&fixture.executor, &fixture.project_node);
    authorize_task_business_challenge(
        source.state(),
        &readonly,
        TaskBusinessAuthorization::Resolve(&baseline),
        &baseline_wire,
        &baseline_read.payload.challenge,
        &b_key,
        now,
    )
    .unwrap();
    verify_task_business_receipt_read(
        source.state(),
        &readonly,
        TaskBusinessAuthorization::Resolve(&baseline),
        &baseline_header,
        &baseline_wire,
        &actual_c,
        &baseline_read,
        &b_key,
        now,
    )
    .unwrap();

    baseline.proof_usage().enforce_limits().unwrap();
    readonly.proof_usage().enforce_limits().unwrap();
    let baseline_bytes = baseline.proof_usage().counts().authorization_bytes;
    let mut combined = baseline.proof_usage().clone();
    combined.merge(readonly.proof_usage());
    let current_unique_bytes = combined.counts().authorization_bytes - baseline_bytes;
    let mut direct_usage = baseline.proof_usage().clone();
    direct_usage
        .record_metadata(&baseline_read.payload.challenge)
        .unwrap();
    let mut read_usage = baseline.proof_usage().clone();
    read_usage.record_signed(&baseline_header).unwrap();
    read_usage.record_signed(&baseline_read).unwrap();
    let entry_overhead = direct_usage
        .counts()
        .authorization_bytes
        .max(read_usage.counts().authorization_bytes)
        - baseline_bytes;
    assert!(
        current_unique_bytes > entry_overhead + 1,
        "current unique bytes {current_unique_bytes} must exceed receipt entry overhead {entry_overhead}"
    );
    let headroom = (current_unique_bytes + entry_overhead) / 2;
    let added_evidence_bytes = MAX_PROOF_BYTES
        .checked_sub(baseline_bytes + headroom)
        .expect("baseline and measured receipt headroom must leave room for evidence");
    let evidence = "e".repeat(BASELINE_EVIDENCE.len() + added_evidence_bytes);
    assert!(evidence.len() <= 65_536);
    let resolution = resolution_authorization(&fixture, &readonly, &evidence, now);
    let (wire, header, read) = request_for(&resolution);
    resolution.proof_usage().enforce_limits().unwrap();
    assert_eq!(
        resolution.proof_usage().counts().authorization_bytes,
        baseline_bytes + added_evidence_bytes
    );
    let mut direct_usage = resolution.proof_usage().clone();
    direct_usage
        .record_metadata(&read.payload.challenge)
        .unwrap();
    direct_usage.enforce_limits().unwrap();
    let mut read_usage = resolution.proof_usage().clone();
    read_usage.record_signed(&header).unwrap();
    read_usage.record_signed(&read).unwrap();
    read_usage.enforce_limits().unwrap();
    let mut combined = resolution.proof_usage().clone();
    combined.merge(readonly.proof_usage());
    assert!(combined.counts().authorization_bytes > MAX_PROOF_BYTES);
    assert!(matches!(
        combined.enforce_limits(),
        Err(AuthorizationError::BudgetExceeded)
    ));
    println!(
        "resolution_receipt_budget current={} business={} direct_without_current={} read_without_current={} combined={} current_unique={} evidence={}",
        readonly.proof_usage().counts().authorization_bytes,
        resolution.proof_usage().counts().authorization_bytes,
        direct_usage.counts().authorization_bytes,
        read_usage.counts().authorization_bytes,
        combined.counts().authorization_bytes,
        current_unique_bytes,
        evidence.len(),
    );
    assert!(matches!(
        authorize_task_business_challenge(
            source.state(),
            &readonly,
            TaskBusinessAuthorization::Resolve(&resolution),
            &wire,
            &read.payload.challenge,
            &b_key,
            now,
        ),
        Err(AuthorizationError::BudgetExceeded)
    ));
    assert!(matches!(
        verify_task_business_receipt_read(
            source.state(),
            &readonly,
            TaskBusinessAuthorization::Resolve(&resolution),
            &header,
            &wire,
            &actual_c,
            &read,
            &b_key,
            now,
        ),
        Err(AuthorizationError::BudgetExceeded)
    ));
}
