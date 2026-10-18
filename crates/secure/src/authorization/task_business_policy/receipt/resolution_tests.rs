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
        "evidence": "executor signed the terminal outcome",
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
    let resolution = resolution_authorization(&fixture, &readonly, now);
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
