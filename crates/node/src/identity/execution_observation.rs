use super::{
    HandoffPolicy, StoredHandoff, check_session_work, lease_checkpoint_key,
    public_execution_result, session_from, signed_receipt, stored_from,
    verify_local_handoff_checkpoint,
};
use crate::{crypto::LocalIdentity, model::Event, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use vibeface_secure::{
    authorization::{
        AUTH_VERSION, CheckStage, DeviceId, EXECUTION_LEASE_MS, FreshnessReceipt, Nonce,
        PolicyDomain, PolicyStamp, PublicKey, REMOTE_READ_MAX_AGE_MS, ResourceKind, ServiceId,
        Signed, TransportEvidence, VerifiedPolicySet,
        events::StoredTransport,
        execution_handoff::{
            ExecutionHandoffLeaseReceipt, ExecutionHandoffResult, ExecutionProofUsage,
            verify_execution_handoff_admission_checkpoint,
            verify_execution_handoff_lease_checkpoint,
        },
        execution_observation::{
            ExecutionObservationChallenge, ExecutionObservationFreshness,
            ExecutionObservationTarget, SignedExecutionObservation,
            VerifiedExecutionObservationRead,
        },
        execution_work::ExecutionWork,
        task_business_policy::{TaskBusinessScopedView, TaskBusinessWire},
    },
    signature_verification::ScopedSignatureVerification,
};

pub(super) fn check_heads_in(
    connection: &Connection,
    policy: &VerifiedPolicySet,
    heads: &[PolicyStamp],
) -> Result<()> {
    for expected in heads {
        let (kind, id) = match &expected.domain {
            PolicyDomain::Account(id) => ("account", id.as_str()),
            PolicyDomain::Space(id) => ("space", id.as_str()),
        };
        let actual: Option<(u64, u64, String)> = connection.query_row(
            "SELECT authority_epoch,sequence,head_hash FROM current_policy_heads WHERE domain_kind=? AND domain_id=?",
            params![kind, id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        ensure!(
            policy.head(&expected.domain) == Some(expected)
                && actual
                    == Some((
                        expected.authority_epoch,
                        expected.sequence,
                        expected.head.to_string(),
                    )),
            "observation policy changed before the local read"
        );
    }
    Ok(())
}

fn check_local_route_in(
    connection: &Connection,
    policy: &VerifiedPolicySet,
    identity: &LocalIdentity,
    target: &ExecutionObservationTarget,
) -> Result<()> {
    let key = PublicKey::parse(&identity.public_key_hex()?)?;
    ensure!(
        key == target.target_public_key
            && target.execution_audience.device == DeviceId::from_key(&key)
            && target.execution_audience.service == ServiceId::from_key(&key),
        "observation targets another executor"
    );
    let execution = &target.execution_resource;
    let local: Option<String> = connection
        .query_row(
            "SELECT public_key FROM local_devices WHERE space_id=? AND device_id=?",
            params![execution.space.as_str(), identity.node_id()?],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        local.as_deref() == Some(key.as_str()),
        "observation executor is not a local device"
    );
    let registered = policy
        .resource(&execution.space, &execution.resource)
        .context("observation has no current ExecutionService")?;
    let projected: Option<(String, String, String)> = connection.query_row(
        "SELECT kind,host_service_id,record_hash FROM resource_records WHERE space_id=? AND resource_id=?",
        params![execution.space.as_str(), execution.resource.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    ensure!(
        registered.payload.kind == ResourceKind::ExecutionService
            && registered.payload.host == target.execution_audience.service
            && projected
                == Some((
                    "execution_service".to_owned(),
                    target.execution_audience.service.to_string(),
                    registered.hash()?.to_string(),
                )),
        "observation ExecutionService route changed"
    );
    Ok(())
}

fn challenge_key(challenge: &ExecutionObservationChallenge) -> String {
    format!(
        "execution-observation:{}:{}",
        challenge.observation_hash, challenge.nonce
    )
}

fn check_local_checkpoint_in(connection: &Connection, stored: &StoredHandoff) -> Result<()> {
    verify_local_handoff_checkpoint(connection, stored)?;
    let execution = stored.wire.handoff.execution_resource.space.as_str();
    let original = &stored.request.network_id;
    let operation = &stored.request.operation_id;
    let actual = connection.prepare("SELECT target_node,resource FROM execution_handoff_reservations WHERE execution_space_id=? AND original_space_id=? AND operation_id=? ORDER BY resource")?
        .query_map(params![execution, original, operation], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut expected = if stored.state.terminal() {
        Vec::new()
    } else {
        stored
            .request
            .resources
            .iter()
            .map(|resource| (stored.request.target_node.clone(), resource.clone()))
            .collect::<Vec<_>>()
    };
    expected.sort();
    ensure!(
        actual == expected,
        "observation execution reservations changed"
    );
    let dispatch: Event =
        serde_json::from_str(&stored.wire.handoff.project_dispatch.canonical_body)?;
    let source = stored.wire.handoff.source_statement()?;
    let session = session_from(
        connection,
        execution,
        original,
        dispatch.resource_id.as_str(),
        &stored.request.target_node,
        stored.request.session_id.as_deref(),
    )?;
    let native = match session.as_ref() {
        Some((session, binding, work)) => {
            ensure!(
                *work == source.work,
                "observation changed its original work"
            );
            check_session_work(
                work,
                &stored.wire.handoff.target_public_key,
                &stored.request,
                session,
                binding,
            )?;
            session.native_session_id.as_deref()
        }
        None => {
            ensure!(
                matches!(source.work, ExecutionWork::Command { .. }),
                "observed execution session disappeared"
            );
            None
        }
    };
    ensure!(
        public_execution_result(stored.result.clone(), native) == stored.result,
        "observed execution result contains private provider identifiers"
    );
    Ok(())
}

fn retained_observation_usage_in(
    connection: &Connection,
    read: &VerifiedExecutionObservationRead,
    stored: &StoredHandoff,
) -> Result<ExecutionProofUsage> {
    check_local_checkpoint_in(connection, stored)?;
    let checkpoint = verify_execution_handoff_admission_checkpoint(
        &stored.wire,
        &stored.admission_receipt,
        &stored.admission,
        &StoredTransport::from(&stored.transport),
        stored.admitted_at_ms,
    )?;
    ensure!(
        read.target() == &ExecutionObservationTarget::from_original(&stored.wire)?,
        "observation differs from the complete locally retained execution"
    );
    let mut usage = read.proof_usage().clone();
    usage.merge(checkpoint.proof_usage());
    usage.record_signed(&stored.wire.invocation)?;
    usage.record_historical_handoff(&stored.wire)?;
    Ok(usage)
}

impl Store {
    pub fn issue_execution_observation_challenge(
        &mut self,
        identity: &LocalIdentity,
        wire: &SignedExecutionObservation,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<ExecutionObservationChallenge> {
        self.issue_execution_observation_challenge_with_policy(
            HandoffPolicy::Primary,
            identity,
            wire,
            transport,
            now_ms,
        )
    }

    pub(crate) fn issue_execution_observation_challenge_scoped(
        &mut self,
        view: &TaskBusinessScopedView,
        identity: &LocalIdentity,
        wire: &SignedExecutionObservation,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<ExecutionObservationChallenge> {
        self.issue_execution_observation_challenge_with_policy(
            HandoffPolicy::TaskBusiness(view),
            identity,
            wire,
            transport,
            now_ms,
        )
    }

    fn issue_execution_observation_challenge_with_policy(
        &mut self,
        policy: HandoffPolicy<'_>,
        identity: &LocalIdentity,
        wire: &SignedExecutionObservation,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<ExecutionObservationChallenge> {
        let signature_scope = ScopedSignatureVerification::new();
        policy.check_business_wire_in(
            &self.connection,
            &self.policy,
            &TaskBusinessWire::Observe(wire.clone()),
            now_ms,
        )?;
        let current = policy
            .state(&self.policy)
            .verify_execution_observation_current(wire, transport, now_ms)?;
        let challenge = ExecutionObservationChallenge::new(
            &current,
            Nonce::random()?,
            now_ms,
            now_ms
                .checked_add(EXECUTION_LEASE_MS)
                .context("observation challenge clock overflow")?
                .min(current.expires_at_ms()),
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        policy.check_heads_in(
            &tx,
            &self.policy,
            &TaskBusinessWire::Observe(wire.clone()),
            current.heads(),
            now_ms,
        )?;
        check_local_route_in(&tx, self.policy.verified(), identity, current.target())?;
        let mut usage = current.proof_usage().clone();
        usage.record_metadata(&challenge)?;
        usage.record_actual_signature_counts(signature_scope.counts());
        usage.enforce_limits()?;
        policy.check_usage(usage)?;
        tx.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![
                current.target().execution_resource.space.as_str(),
                challenge_key(&challenge),
                serde_json::to_string(&challenge)?,
            ],
        )?;
        tx.commit()?;
        Ok(challenge)
    }

    pub fn sign_execution_observation_receipts(
        &self,
        challenge: &ExecutionObservationChallenge,
        identity: &LocalIdentity,
        now_ms: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        self.sign_execution_observation_receipts_with_policy(
            HandoffPolicy::Primary,
            None,
            challenge,
            identity,
            now_ms,
        )
    }

    pub(crate) fn sign_execution_observation_receipts_scoped(
        &self,
        view: &TaskBusinessScopedView,
        wire: &SignedExecutionObservation,
        transport: &TransportEvidence,
        challenge: &ExecutionObservationChallenge,
        identity: &LocalIdentity,
        now_ms: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        self.sign_execution_observation_receipts_with_policy(
            HandoffPolicy::TaskBusiness(view),
            Some((wire, transport)),
            challenge,
            identity,
            now_ms,
        )
    }

    fn sign_execution_observation_receipts_with_policy(
        &self,
        policy: HandoffPolicy<'_>,
        source: Option<(&SignedExecutionObservation, &TransportEvidence)>,
        challenge: &ExecutionObservationChallenge,
        identity: &LocalIdentity,
        now_ms: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        let _signature_scope = ScopedSignatureVerification::new();
        ensure!(
            challenge.version == AUTH_VERSION
                && challenge.stage == CheckStage::RemoteRead
                && challenge.issued_at_ms <= now_ms
                && now_ms < challenge.expires_at_ms
                && challenge.expires_at_ms - challenge.issued_at_ms <= REMOTE_READ_MAX_AGE_MS
                && !challenge.policies.is_empty()
                && challenge.policies.len() <= vibeface_secure::authorization::MAX_POLICY_DOMAINS
                && challenge
                    .policies
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain),
            "invalid execution observation freshness challenge"
        );
        let key = PublicKey::parse(&identity.public_key_hex()?)?;
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let mut usage =
            vibeface_secure::authorization::execution_handoff::ExecutionProofUsage::default();
        if let HandoffPolicy::TaskBusiness(_) = policy {
            let (wire, transport) =
                source.context("scoped observation receipt has no exact source wire")?;
            let business = TaskBusinessWire::Observe(wire.clone());
            policy.check_business_wire_in(&tx, &self.policy, &business, now_ms)?;
            let current = policy
                .state(&self.policy)
                .verify_execution_observation_current(wire, transport, now_ms)?;
            policy.check_heads_in(&tx, &self.policy, &business, current.heads(), now_ms)?;
            check_local_route_in(&tx, self.policy.verified(), identity, current.target())?;
            let encoded: String = tx
                .query_row(
                    "SELECT value FROM local_values WHERE space_id=? AND key=?",
                    params![
                        current.target().execution_resource.space.as_str(),
                        challenge_key(challenge)
                    ],
                    |row| row.get(0),
                )
                .context("observation challenge was not issued by this executor")?;
            ensure!(
                serde_json::from_str::<ExecutionObservationChallenge>(&encoded)? == *challenge
                    && challenge.observation_hash == wire.hash()?
                    && challenge.request_hash
                        == vibeface_secure::authorization::ObjectHash::body(&wire.request)?
                    && challenge.submission_invocation_hash == wire.invocation.hash()?
                    && challenge.policies.as_slice() == current.heads(),
                "observation challenge changed its current exact source"
            );
            usage.merge(current.proof_usage());
            usage.record_metadata(challenge)?;
        }
        let mut currents = Vec::new();
        for stamp in &challenge.policies {
            if let HandoffPolicy::TaskBusiness(view) = policy
                && !view.primary_heads().contains(stamp)
            {
                continue;
            }
            let authority = match &stamp.domain {
                PolicyDomain::Account(id) => self
                    .policy
                    .verified()
                    .account(id)
                    .map(|document| &document.authority),
                PolicyDomain::Space(id) => self
                    .policy
                    .verified()
                    .space(id)
                    .map(|document| &document.authority),
            };
            let Some(authority) = authority.filter(|authority| authority.public_key == key) else {
                continue;
            };
            check_heads_in(&tx, self.policy.verified(), std::slice::from_ref(stamp))?;
            ensure!(
                authority.device == DeviceId::from_key(&key)
                    && authority.service == ServiceId::from_key(&key),
                "observation freshness authority mismatch"
            );
            currents.push(stamp.clone());
        }
        currents.sort_by(|left, right| left.domain.cmp(&right.domain));
        let receipts = if currents.is_empty() {
            Vec::new()
        } else {
            let receipt = FreshnessReceipt {
                version: AUTH_VERSION,
                currents,
                challenge_hash: challenge.hash()?,
                issued_at_ms: now_ms,
                expires_at_ms: challenge.expires_at_ms,
            };
            receipt.validate_currents()?;
            vec![Signed::sign(receipt, &signer)?]
        };
        for receipt in &receipts {
            usage.record_signed(receipt)?;
        }
        policy.check_usage(usage)?;
        tx.commit()?;
        Ok(receipts)
    }

    pub fn observe_execution_handoff(
        &self,
        identity: &LocalIdentity,
        wire: &SignedExecutionObservation,
        proof: &ExecutionObservationFreshness,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<Option<ExecutionHandoffResult>> {
        self.observe_execution_handoff_with_policy(
            HandoffPolicy::Primary,
            identity,
            wire,
            proof,
            transport,
            now_ms,
        )
    }

    pub(crate) fn observe_execution_handoff_scoped(
        &self,
        view: &TaskBusinessScopedView,
        identity: &LocalIdentity,
        wire: &SignedExecutionObservation,
        proof: &ExecutionObservationFreshness,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<Option<ExecutionHandoffResult>> {
        self.observe_execution_handoff_with_policy(
            HandoffPolicy::TaskBusiness(view),
            identity,
            wire,
            proof,
            transport,
            now_ms,
        )
    }

    fn observe_execution_handoff_with_policy(
        &self,
        policy: HandoffPolicy<'_>,
        identity: &LocalIdentity,
        wire: &SignedExecutionObservation,
        proof: &ExecutionObservationFreshness,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<Option<ExecutionHandoffResult>> {
        let signature_scope = ScopedSignatureVerification::new();
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        policy.check_business_wire_in(
            &tx,
            &self.policy,
            &TaskBusinessWire::Observe(wire.clone()),
            now_ms,
        )?;
        let current = policy
            .state(&self.policy)
            .verify_execution_observation_current(wire, transport, now_ms)?;
        policy.check_heads_in(
            &tx,
            &self.policy,
            &TaskBusinessWire::Observe(wire.clone()),
            current.heads(),
            now_ms,
        )?;
        check_local_route_in(&tx, self.policy.verified(), identity, current.target())?;
        let current_freshness = policy
            .state(&self.policy)
            .verify_execution_observation_current_freshness(&current, proof, now_ms)?;
        let encoded: String = tx
            .query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![
                    current.target().execution_resource.space.as_str(),
                    challenge_key(&proof.challenge),
                ],
                |row| row.get(0),
            )
            .context("observation challenge was not issued by this executor")?;
        ensure!(
            serde_json::from_str::<ExecutionObservationChallenge>(&encoded)? == proof.challenge,
            "local execution observation challenge changed"
        );
        let target = current.target();
        let Some(stored) = stored_from(
            &tx,
            target.execution_resource.space.as_str(),
            target.source.project.space.as_str(),
            &target.source.operation_id,
        )?
        else {
            policy.check_usage(current_freshness.proof_usage().clone())?;
            tx.commit()?;
            return Ok(None);
        };
        let mut usage = retained_observation_usage_in(&tx, &current_freshness, &stored)?;
        let receipt = signed_receipt(&tx, identity, &stored, now_ms)?;
        if let Some((lease, verified_at, deadline)) = &stored.lease {
            let encoded: String = tx
                .query_row(
                    "SELECT value FROM local_values WHERE space_id=? AND key=?",
                    params![
                        stored.wire.handoff.execution_resource.space.as_str(),
                        lease_checkpoint_key(&stored.wire, lease)?
                    ],
                    |row| row.get(0),
                )
                .context("observed lease has no immutable checkpoint")?;
            let receipt: Signed<ExecutionHandoffLeaseReceipt> = serde_json::from_str(&encoded)?;
            usage.merge(&verify_execution_handoff_lease_checkpoint(
                &stored.wire,
                &stored.admission_receipt,
                lease,
                *verified_at,
                *deadline,
                &receipt,
            )?);
        }
        if let Some(previous) = &stored.receipt {
            usage.record_signed(previous)?;
        }
        usage.record_signed(&receipt)?;
        usage.record_actual_signature_counts(signature_scope.counts());
        usage.enforce_limits()?;
        let _usage = policy.check_usage(usage)?;
        #[cfg(test)]
        println!(
            "OBSERVATION_PROOF_USAGE {}",
            serde_json::json!({
                "case":if stored.wire.handoff.sdk_origin.is_some() {"sdk_original"} else {"account_original"},
                "observation_wire_json_bytes":serde_json::to_vec(wire)?.len(),
                "freshness_json_bytes":serde_json::to_vec(proof)?.len(),
                "proof":_usage.counts(),
            })
        );
        let observation = ExecutionHandoffResult {
            handoff: stored.wire,
            receipt,
            result: stored.result,
        };
        tx.commit()?;
        Ok(Some(observation))
    }
}
