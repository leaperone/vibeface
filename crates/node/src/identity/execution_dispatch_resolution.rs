use super::{
    admission::{admission_for_operation_from, consume_execution_dispatch_in},
    execution_dispatch::{VerifiedDispatchFacts, check_destination_heads_in},
    execution_handoff::resolution_resolver_from,
    journal::PolicyState,
    policy::FreshnessProof,
};
use crate::{
    crypto::LocalIdentity,
    model::{AdmissionProof, EventCause, OperationRecord, OperationRequest, OperationState},
    store::{Store, validate_operation_binding_from},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vibeface_secure::{
    Identity,
    authorization::{
        AUTH_VERSION, Action, Actor, DeviceId, Invocation, Nonce, ObjectHash, PublicKey, Signed,
        TransportEvidence, VerifiedAuthorization,
        execution_handoff::{
            ExecutionHandoffResolution, ExecutionHandoffResolutionResult, ExecutionHandoffResult,
            ExecutionHandoffTerminalReconciliationResult, ExecutionResolutionPurpose,
            SignedExecutionHandoff, SignedExecutionHandoffResolution, VerifiedExecutionResolution,
            VerifiedExecutionTerminalReconciliation, execution_resolution_account_facts,
            execution_resolution_facts, verify_execution_handoff_receipt_signature,
        },
    },
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingResolution {
    wire: SignedExecutionHandoffResolution,
    result: Option<Value>,
}

fn key(authorization: &str) -> String {
    format!("project-execution-resolution:{authorization}")
}

fn pending_from(
    connection: &Connection,
    space: &str,
    authorization: &str,
) -> Result<Option<PendingResolution>> {
    let encoded: Option<String> = connection
        .query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![space, key(authorization)],
            |row| row.get(0),
        )
        .optional()?;
    encoded
        .map(|encoded| Ok(serde_json::from_str(&encoded)?))
        .transpose()
}

fn operation_from(
    connection: &Connection,
    space: &str,
    operation: &str,
) -> Result<OperationRecord> {
    let (request, state, result): (String, String, String) = connection.query_row(
        "SELECT request,state,result FROM operations WHERE space_id=? AND operation_id=?",
        params![space, operation],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(OperationRecord {
        request: serde_json::from_str(&request)?,
        state: serde_json::from_str(&state)?,
        result: serde_json::from_str(&result)?,
    })
}

fn check_original_in(
    connection: &Connection,
    identity: &LocalIdentity,
    observation: &ExecutionHandoffResult,
) -> Result<(OperationRecord, AdmissionProof)> {
    let receipt = &observation.receipt.payload;
    let space = receipt.original_space_id.as_str();
    let operation = &receipt.operation_id;
    verify_execution_handoff_receipt_signature(
        &observation.handoff,
        &observation.receipt,
        &observation.result,
    )?;
    let encoded: String = connection.query_row(
        "SELECT value FROM local_values WHERE space_id=? AND key=?",
        params![space, format!("project-execution-handoff:{operation}")],
        |row| row.get(0),
    )?;
    ensure!(
        serde_json::from_str::<SignedExecutionHandoff>(&encoded)? == observation.handoff,
        "resolution observation differs from the original durable handoff"
    );
    let stored = operation_from(connection, space, operation)?;
    let proof = admission_for_operation_from(connection, space, operation)?;
    ensure!(
        serde_json::from_value::<OperationRequest>(proof.source_body["request"].clone())?
            == stored.request,
        "resolution changed the original operation request"
    );
    let local = PublicKey::parse(&identity.public_key_hex()?)?;
    let source = observation
        .handoff
        .handoff
        .source_attestation
        .as_ref()
        .context("original remote operation has no Project source checkpoint")?;
    source.verify_signature()?;
    proof.invocation.verify_signature()?;
    observation.handoff.invocation.verify_signature()?;
    let submission = &observation.handoff.invocation.payload;
    ensure!(
        source.issuer == local
            && source.payload == observation.handoff.handoff.source_statement()?
            && observation.handoff.invocation.issuer == local
            && proof.invocation.payload.audience.device == DeviceId::from_key(&local)
            && proof.invocation.payload.body_hash == ObjectHash::body(&proof.source_body)?
            && proof.invocation.payload.operation_id == *operation
            && submission.operation_id == *operation
            && submission.device == DeviceId::from_key(&local)
            && submission.presenter == DeviceId::from_key(&local)
            && submission.actor
                == Actor::Service(proof.invocation.payload.audience.service.clone())
            && submission.resource == observation.handoff.handoff.execution_resource
            && submission.audience == observation.handoff.handoff.execution_audience
            && submission.action == Action::ExecutionSubmit
            && submission.body_hash == ObjectHash::body(&observation.handoff.handoff)?,
        "original remote checkpoint changed its local host, signed request or submission"
    );
    let dispatch: crate::model::Event =
        serde_json::from_str(&observation.handoff.handoff.project_dispatch.canonical_body)?;
    ensure!(
        matches!(&dispatch.cause, EventCause::Invocation { admission } if admission == &proof)
            && dispatch.network_id == space
            && dispatch.resource_id == proof.invocation.payload.resource.resource
            && dispatch.scope_id == stored.request.conversation_id
            && matches!(&dispatch.body, crate::model::EventBody::DispatchRecorded {operation_id,action,..}
                if operation_id == operation && *action == serde_json::to_value(&stored.request)?),
        "resolution changed the original Task, session or dispatch"
    );
    Ok((stored, proof))
}

pub(crate) fn persisted_resolution_facts(
    connection: &Connection,
    proof: &AdmissionProof,
) -> Result<Option<vibeface_secure::authorization::RequestFacts>> {
    if proof.source_body["method"] != "resolve_unknown"
        || proof.source_body.get("execution_observation").is_none()
    {
        return Ok(None);
    }
    let saved = pending_from(
        connection,
        proof.invocation.payload.resource.space.as_str(),
        &proof.invocation.payload.operation_id,
    )?
    .context("remote resolution admission has no original prepared request")?;
    ensure!(
        saved.wire.resolution.account_admission == *proof,
        "remote resolution admission differs from its prepared request"
    );
    saved.wire.invocation.verify_signature()?;
    let facts = execution_resolution_account_facts(&saved.wire.resolution)?;
    let submission = &saved.wire.invocation.payload;
    let project_host = &proof.invocation.payload.audience;
    let mut expected_context = facts.context.clone();
    expected_context.workspace_resource = Some(
        saved
            .wire
            .resolution
            .observation
            .handoff
            .handoff
            .execution_resource
            .resource
            .clone(),
    );
    ensure!(
        saved.wire.invocation.issuer
            == PublicKey::parse(
                saved
                    .wire
                    .resolution
                    .observation
                    .handoff
                    .handoff
                    .source_attestation
                    .as_ref()
                    .context("prepared resolution has no Project source attestation")?
                    .issuer
                    .as_str(),
            )?
            && DeviceId::from_key(&saved.wire.invocation.issuer) == project_host.device
            && submission.actor == Actor::Service(project_host.service.clone())
            && submission.device == project_host.device
            && submission.presenter == project_host.device
            && submission.resource
                == saved
                    .wire
                    .resolution
                    .observation
                    .handoff
                    .handoff
                    .execution_resource
            && submission.audience
                == saved
                    .wire
                    .resolution
                    .observation
                    .handoff
                    .handoff
                    .execution_audience
            && submission.action == Action::ExecutionResolve
            && submission.operation_id == facts.operation_id
            && submission.body_hash == ObjectHash::body(&saved.wire.resolution)?
            && submission.context == expected_context
            && submission.grant_leaf.as_str()
                == proof.source_body["execution_grant"]
                    .as_str()
                    .unwrap_or_default(),
        "prepared resolution signature or destination differs from the admitted Account intent"
    );
    Ok(Some(facts))
}

impl Store {
    pub fn execution_dispatch_resolution(
        &self,
        space: &str,
        authorization: &str,
    ) -> Result<Option<SignedExecutionHandoffResolution>> {
        Ok(pending_from(&self.connection, space, authorization)?.map(|saved| saved.wire))
    }

    pub fn prepare_execution_dispatch_resolution(
        &mut self,
        identity: &LocalIdentity,
        dispatch: &VerifiedDispatchFacts,
        auth: &VerifiedAuthorization,
        proof: &FreshnessProof,
        transport: &TransportEvidence,
        body: &Value,
        now_ms: u64,
    ) -> Result<(SignedExecutionHandoffResolution, bool)> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        ensure!(
            auth.action() == Action::OperationResolve
                && matches!(auth.actor(), Actor::Account(_))
                && auth
                    .signed_invocation()
                    .payload
                    .context
                    .sdk_session
                    .is_none(),
            "remote resolution requires an independent Account authorization"
        );
        let space = auth.resource().space.as_str();
        let authorization = auth.operation_id();
        let observation: ExecutionHandoffResult =
            serde_json::from_value(body["execution_observation"].clone())?;
        let purpose: ExecutionResolutionPurpose =
            serde_json::from_value(body["resolution_purpose"].clone())?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        if let Some(saved) = pending_from(&tx, space, authorization)? {
            ensure!(
                saved.wire.resolution.account_admission.source_body == *body
                    && saved.wire.resolution.account_admission.invocation
                        == *auth.signed_invocation(),
                "resolution authorization was already used for another request"
            );
            tx.commit()?;
            self.policy = current;
            return Ok((saved.wire, false));
        }
        let (stored, original) = check_original_in(&tx, identity, &observation)?;
        ensure!(
            stored.state == OperationState::Unknown,
            "only an unknown Project operation can be resolved"
        );
        ensure!(
            observation.receipt.payload.original_space_id == auth.resource().space
                && original.invocation.payload.resource == *auth.resource()
                && original.invocation.payload.audience == *auth.audience()
                && DeviceId::from_key(&PublicKey::of(&signer)) == auth.audience().device,
            "resolution targets another Project or local host"
        );
        validate_operation_binding_from(&tx, &stored.request)?;
        dispatch.revalidate(&tx, &current, body, auth.audience(), now_ms)?;
        let admitted = consume_execution_dispatch_in(
            &tx, &current, auth, proof, transport, body, None, now_ms, dispatch,
        )?;
        let resolution = ExecutionHandoffResolution {
            version: AUTH_VERSION,
            purpose,
            observation,
            account_admission: admitted,
        };
        let facts = execution_resolution_facts(&resolution, auth)?;
        let destination = dispatch.destination();
        let grant = current
            .verified()
            .grant(destination.grant_leaf())
            .context("executor resolution grant disappeared")?;
        let expires_at_ms = now_ms
            .checked_add(vibeface_secure::authorization::INVOCATION_MAX_AGE_MS)
            .context("resolution deadline overflow")?
            .min(auth.expires_at_ms())
            .min(grant.payload.expires_at_ms);
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Service(auth.audience().service.clone()),
                device: DeviceId::from_key(&PublicKey::of(&signer)),
                initiator: destination.initiator().clone(),
                initiating_grant: destination.initiating_grant().cloned(),
                resource: facts.resource,
                action: Action::ExecutionResolve,
                audience: facts.audience,
                presenter: auth.audience().device.clone(),
                operation_id: authorization.to_owned(),
                body_hash: facts.body_hash,
                grant_leaf: destination.grant_leaf().clone(),
                expected_policies: destination.heads().to_vec(),
                context: facts.context,
                nonce: Nonce::random()?,
                not_before_ms: now_ms,
                expires_at_ms,
            },
            &signer,
        )?;
        let wire = SignedExecutionHandoffResolution {
            resolution,
            invocation,
        };
        current.verified().verify_execution_resolution(
            &wire,
            auth,
            &TransportEvidence::VerifiedPeer {
                origin: auth.audience().device.clone(),
            },
            None,
            now_ms,
        )?;
        tx.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![
                space,
                key(authorization),
                serde_json::to_string(&PendingResolution {
                    wire: wire.clone(),
                    result: None
                })?
            ],
        )?;
        tx.commit()?;
        self.policy = current;
        Ok((wire, true))
    }

    pub fn apply_execution_dispatch_resolution(
        &mut self,
        identity: &LocalIdentity,
        result: &ExecutionHandoffResolutionResult,
        now_ms: u64,
    ) -> Result<OperationRecord> {
        self.apply_project_resolution(identity, ResolutionResult::Unknown(result), now_ms)
    }

    pub fn apply_execution_dispatch_terminal_reconciliation(
        &mut self,
        identity: &LocalIdentity,
        result: &ExecutionHandoffTerminalReconciliationResult,
        now_ms: u64,
    ) -> Result<OperationRecord> {
        self.apply_project_resolution(identity, ResolutionResult::Terminal(result), now_ms)
    }

    fn apply_project_resolution(
        &mut self,
        identity: &LocalIdentity,
        result: ResolutionResult<'_>,
        now_ms: u64,
    ) -> Result<OperationRecord> {
        let _signature_scope =
            vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        let wire = result.wire();
        let account = &wire.resolution.account_admission;
        let space = account.invocation.payload.resource.space.as_str();
        let authorization = &account.invocation.payload.operation_id;
        let operation = &wire.resolution.observation.receipt.payload.operation_id;
        let wrapper = result.value()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = PolicyState::reload_verified(&tx, &self.policy)?;
        let mut pending = pending_from(&tx, space, authorization)?
            .context("resolution acknowledgement has no prepared Account decision")?;
        ensure!(
            pending.wire == *wire,
            "resolution acknowledgement changed its prepared request"
        );
        let (stored, _) = check_original_in(&tx, identity, &wire.resolution.observation)?;
        if let Some(previous) = &pending.result {
            ensure!(
                previous == &wrapper,
                "resolution retry changed the signed acknowledgement"
            );
            ensure!(
                stored.state.terminal() && stored.result == *previous,
                "completed resolution differs from the immutable Project result"
            );
            let previous_wire = resolution_wire_from(previous)?;
            ensure!(
                previous_wire == *wire,
                "completed resolution belongs to another Account decision"
            );
            tx.commit()?;
            self.policy = current;
            return Ok(stored);
        }
        ensure!(
            stored.state == OperationState::Unknown,
            "Project operation is no longer unknown"
        );
        validate_operation_binding_from(&tx, &stored.request)?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        ensure!(
            DeviceId::from_key(&local) == account.invocation.payload.audience.device,
            "resolution acknowledgement targets another Project host"
        );
        let resolver = resolution_resolver_from(&current, current.verified(), wire, now_ms)?;
        let verified = match result {
            ResolutionResult::Unknown(result) => {
                let verified = current
                    .verified()
                    .verify_execution_resolution_ack(result, &resolver, now_ms)?;
                current.verified().verify_execution_resolution_freshness(
                    &verified,
                    &result.freshness,
                    now_ms,
                )?;
                VerifiedProjectResolution::Unknown(result, verified)
            }
            ResolutionResult::Terminal(result) => {
                let verified = current
                    .verified()
                    .verify_execution_terminal_reconciliation(result, &resolver, now_ms)?;
                VerifiedProjectResolution::Terminal(result, verified)
            }
        };
        let dependencies = match &verified {
            VerifiedProjectResolution::Unknown(_, verified) => verified.dependencies(),
            VerifiedProjectResolution::Terminal(_, verified) => verified.dependencies(),
        };
        check_destination_heads_in(&tx, dependencies)?;
        let state: OperationState = serde_json::from_value(account.source_body["outcome"].clone())?;
        ensure!(state.terminal(), "resolution requires a terminal state");
        ensure!(tx.execute(
            "UPDATE operations SET state=?,result=? WHERE space_id=? AND operation_id=? AND state='\"unknown\"'",
            params![serde_json::to_string(&state)?, serde_json::to_string(&wrapper)?, space, operation],
        )? == 1, "Project operation changed before resolution");
        match verified {
            VerifiedProjectResolution::Unknown(result, verified) => {
                super::event_authorization::append_execution_resolution_observation_in(
                    &tx,
                    &current,
                    identity,
                    &wire.resolution.observation.handoff.handoff.project_dispatch,
                    result,
                    &verified,
                    now_ms,
                )?;
            }
            VerifiedProjectResolution::Terminal(result, verified) => {
                super::event_authorization::append_execution_terminal_reconciliation_observation_in(
                    &tx, &current, identity,
                    &wire.resolution.observation.handoff.handoff.project_dispatch,
                    result, &verified, now_ms,
                )?;
            }
        }
        tx.execute(
            "DELETE FROM resource_reservations WHERE space_id=? AND operation_id=?",
            params![space, operation],
        )?;
        pending.result = Some(wrapper.clone());
        ensure!(
            tx.execute(
                "UPDATE local_values SET value=? WHERE space_id=? AND key=?",
                params![serde_json::to_string(&pending)?, space, key(authorization)],
            )? == 1,
            "prepared resolution disappeared"
        );
        tx.commit()?;
        self.policy = current;
        Ok(OperationRecord {
            request: stored.request,
            state,
            result: wrapper,
        })
    }
}

enum VerifiedProjectResolution<'a> {
    Unknown(
        &'a ExecutionHandoffResolutionResult,
        VerifiedExecutionResolution,
    ),
    Terminal(
        &'a ExecutionHandoffTerminalReconciliationResult,
        VerifiedExecutionTerminalReconciliation,
    ),
}

#[derive(Clone, Copy)]
enum ResolutionResult<'a> {
    Unknown(&'a ExecutionHandoffResolutionResult),
    Terminal(&'a ExecutionHandoffTerminalReconciliationResult),
}

impl ResolutionResult<'_> {
    fn wire(&self) -> &SignedExecutionHandoffResolution {
        match self {
            Self::Unknown(value) => &value.resolution,
            Self::Terminal(value) => &value.reconciliation,
        }
    }
    fn value(&self) -> Result<Value> {
        Ok(match self {
            Self::Unknown(value) => serde_json::to_value(value)?,
            Self::Terminal(value) => serde_json::to_value(value)?,
        })
    }
}

fn resolution_wire_from(value: &Value) -> Result<SignedExecutionHandoffResolution> {
    if value.get("reconciliation").is_some() {
        Ok(
            serde_json::from_value::<ExecutionHandoffTerminalReconciliationResult>(value.clone())?
                .reconciliation,
        )
    } else {
        Ok(serde_json::from_value::<ExecutionHandoffResolutionResult>(value.clone())?.resolution)
    }
}
