use super::{
    admission::{
        admission_for_operation_from, check_policy_heads_in, check_stored_manager_tool_in,
        load_admission_in, reconstruct_stored_admission,
    },
    foreign_execution_policy::{
        bind_task_execution_policy_in, check_task_policy_heads_in,
        current_execution_readonly_policy_in, current_task_execution_policy_in,
        original_task_execution_policy_in, task_policy_for_binding_in,
    },
    journal::PolicyState,
    policy::{RequestTransport, authorize_request_invocation_for_body},
};
use crate::{
    binding::SignedBinding,
    crypto::LocalIdentity,
    model::{AdmissionProof, OperationRequest},
    store::Store,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::{borrow::Cow, ops::Deref};
use vibeface_secure::authorization::{
    Action, Actor, CheckStage, DeviceId, FreshnessChallenge, Invocation, Nonce, PublicKey,
    RequestFacts, ServiceId, Signed, TransportEvidence, VerifiedAuthorization, VerifiedPolicySet,
    node_execution_policy::{VerifiedTaskCurrentPolicy, VerifiedTaskOriginalAdmissionPolicy},
    prepare::PrepareContext,
    remote_execution_policy::{RemoteExecutionPolicyPurpose, RemoteExecutionPolicySelection},
};

#[derive(Clone)]
pub(crate) struct TaskScopedAuthorization {
    authorization: VerifiedAuthorization,
    task_policy: Option<VerifiedTaskCurrentPolicy>,
}

impl Deref for TaskScopedAuthorization {
    type Target = VerifiedAuthorization;

    fn deref(&self) -> &Self::Target {
        &self.authorization
    }
}

impl TaskScopedAuthorization {
    pub(crate) fn for_admitted_resolution_in(
        connection: &Connection,
        policy: &PolicyState,
        identity: &LocalIdentity,
        proof: &AdmissionProof,
        facts: &RequestFacts,
        now_ms: u64,
    ) -> Result<Self> {
        let invocation = &proof.invocation.payload;
        ensure!(
            proof.source_body["method"] == "resolve_unknown"
                && invocation.action == Action::OperationResolve
                && matches!(&invocation.actor, Actor::Account(_))
                && invocation.context.sdk_session.is_none(),
            "resolution scope requires an independent Account resolver admission"
        );
        ensure!(
            load_admission_in(
                connection,
                invocation.resource.space.as_str(),
                &invocation.operation_id,
            )? == *proof,
            "resolution scope differs from its persisted resolver admission"
        );
        let operation_id = proof.source_body["operation_id"]
            .as_str()
            .context("resolution scope has no original operation ID")?;
        let encoded: String = connection.query_row(
            "SELECT request FROM operations WHERE space_id=? AND operation_id=?",
            params![invocation.resource.space.as_str(), operation_id],
            |row| row.get(0),
        )?;
        let request: OperationRequest = serde_json::from_str(&encoded)?;
        let operation_admission = admission_for_operation_from(
            connection,
            invocation.resource.space.as_str(),
            operation_id,
        )?;
        ensure!(
            operation_admission.source_body["method"] == "operate"
                && operation_admission.invocation.payload.resource == invocation.resource
                && operation_admission.invocation.payload.audience == invocation.audience
                && request.operation_id == operation_id
                && request.network_id == invocation.resource.space.as_str()
                && serde_json::from_value::<OperationRequest>(
                    operation_admission.source_body["request"].clone(),
                )? == request,
            "resolution scope differs from its original persisted operation"
        );
        let original = OriginalAdmissionView::load(connection, policy, proof)?;
        let admitted = reconstruct_stored_admission(connection, policy, proof, facts)?;
        let source = PublicKey::parse(&identity.public_key_hex()?)?;
        ensure!(
            invocation.audience.device == DeviceId::from_key(&source)
                && invocation.audience.service == ServiceId::from_key(&source),
            "resolution scope targets another Node"
        );
        let task_policy = if original.task_policy().is_some() {
            let encoded: String = connection.query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![
                    invocation.resource.space.as_str(),
                    format!("project-execution-handoff:{operation_id}")
                ],
                |row| row.get(0),
            )?;
            let original_handoff: vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff = serde_json::from_str(&encoded)?;
            let original_work = original_handoff.handoff.source_statement()?.work;
            let selection = RemoteExecutionPolicySelection {
                execution_resource: serde_json::from_value(
                    proof.source_body["execution_resource"].clone(),
                )
                .context("NeedPolicySync: admitted resolver has no execution resource")?,
                target_public_key: serde_json::from_value(
                    proof.source_body["target_public_key"].clone(),
                )
                .context("NeedPolicySync: admitted resolver has no execution key")?,
                execution_grant: serde_json::from_value(
                    proof.source_body["execution_grant"].clone(),
                )
                .context("NeedPolicySync: admitted resolver has no execution grant")?,
            };
            let context = PrepareContext {
                context: facts.context.clone(),
                binding_generation: original_work.binding_generation(),
                canonical_cwd: request
                    .binding
                    .as_ref()
                    .and_then(|binding| binding.binding.open_spec["cwd"].as_str())
                    .or_else(|| original_work.canonical_cwd())
                    .map(str::to_owned),
                execution_target_public_key: Some(selection.target_public_key.clone()),
            };
            Some(current_execution_readonly_policy_in(
                connection,
                policy,
                &source,
                &original_handoff,
                &selection,
                RemoteExecutionPolicyPurpose::Resolver,
                invocation,
                &proof.invocation.issuer,
                &context,
                now_ms,
            )?)
        } else {
            None
        };
        let state = task_policy
            .as_ref()
            .map_or(policy.verified(), |task| task.state());
        let authorization =
            state.verify_admitted_operation_authorization(&admitted, facts, now_ms)?;
        check_stored_manager_tool_in(connection, policy, &authorization, &proof.transport, now_ms)?;
        let scoped = Self {
            authorization,
            task_policy,
        };
        scoped.check_current_in(connection, policy.verified(), facts, now_ms)?;
        Ok(scoped)
    }

    pub(crate) fn for_admitted_operation_in(
        connection: &Connection,
        policy: &PolicyState,
        identity: &LocalIdentity,
        proof: &AdmissionProof,
        facts: &RequestFacts,
        now_ms: u64,
    ) -> Result<Self> {
        ensure!(
            proof.source_body["method"] == "operate",
            "operation scope requires an original operate admission"
        );
        let invocation = &proof.invocation.payload;
        ensure!(
            admission_for_operation_from(
                connection,
                invocation.resource.space.as_str(),
                &invocation.operation_id,
            )? == *proof,
            "operation scope differs from its persisted admission"
        );
        let request: OperationRequest =
            serde_json::from_value(proof.source_body["request"].clone())?;
        let encoded: String = connection.query_row(
            "SELECT request FROM operations WHERE space_id=? AND operation_id=?",
            params![invocation.resource.space.as_str(), invocation.operation_id],
            |row| row.get(0),
        )?;
        ensure!(
            request.operation_id == invocation.operation_id
                && request.network_id == invocation.resource.space.as_str()
                && serde_json::from_str::<OperationRequest>(&encoded)? == request,
            "operation scope differs from its persisted request"
        );
        let original = OriginalAdmissionView::load(connection, policy, proof)?;
        let admitted = reconstruct_stored_admission(connection, policy, proof, facts)?;
        let source = PublicKey::parse(&identity.public_key_hex()?)?;
        ensure!(
            invocation.audience.device == DeviceId::from_key(&source)
                && invocation.audience.service == ServiceId::from_key(&source),
            "operation scope targets another Node"
        );
        let task_policy = if original.task_policy().is_some() {
            if request.binding.is_none() {
                Some(
                    current_task_execution_policy_in(
                        connection,
                        policy,
                        identity,
                        &proof.invocation,
                        &proof.source_body,
                        now_ms,
                    )?
                    .context("NeedPolicySync: admitted execution work has no current policy")?,
                )
            } else {
                let binding = request
                    .binding
                    .as_ref()
                    .context("NeedPolicySync: admitted remote operation has no original binding")?;
                let selection = RemoteExecutionPolicySelection {
                    execution_resource: serde_json::from_value(
                        proof.source_body["execution_resource"].clone(),
                    )
                    .context(
                        "NeedPolicySync: admitted remote operation has no execution resource",
                    )?,
                    target_public_key: serde_json::from_value(
                        proof.source_body["target_public_key"].clone(),
                    )
                    .context("NeedPolicySync: admitted remote operation has no execution key")?,
                    execution_grant: serde_json::from_value(
                        proof.source_body["execution_grant"].clone(),
                    )
                    .context("NeedPolicySync: admitted remote operation has no execution grant")?,
                };
                Some(task_policy_for_binding_in(
                    connection,
                    policy,
                    &source,
                    binding,
                    &facts.resource,
                    &selection,
                    facts.action,
                    now_ms,
                )?)
            }
        } else {
            None
        };
        let state = task_policy
            .as_ref()
            .map_or(policy.verified(), |task| task.state());
        let authorization =
            state.verify_admitted_operation_authorization(&admitted, facts, now_ms)?;
        check_stored_manager_tool_in(connection, policy, &authorization, &proof.transport, now_ms)?;
        let scoped = Self {
            authorization,
            task_policy,
        };
        scoped.check_current_in(connection, policy.verified(), facts, now_ms)?;
        Ok(scoped)
    }

    #[cfg(test)]
    pub(crate) fn from_primary(authorization: VerifiedAuthorization) -> Self {
        Self {
            authorization,
            task_policy: None,
        }
    }

    pub(crate) fn authorization(&self) -> &VerifiedAuthorization {
        &self.authorization
    }

    pub(crate) fn task_policy(&self) -> Option<&VerifiedTaskCurrentPolicy> {
        self.task_policy.as_ref()
    }

    pub(crate) fn state<'a>(&'a self, primary: &'a VerifiedPolicySet) -> &'a VerifiedPolicySet {
        self.task_policy
            .as_ref()
            .map_or(primary, |task| task.state())
    }

    pub(crate) fn expires_at_ms(&self) -> u64 {
        self.task_policy
            .as_ref()
            .map_or(self.authorization.expires_at_ms(), |task| {
                task.expires_at_ms().min(self.authorization.expires_at_ms())
            })
    }

    pub(crate) fn check_current_in(
        &self,
        connection: &Connection,
        primary: &VerifiedPolicySet,
        facts: &RequestFacts,
        now_ms: u64,
    ) -> Result<()> {
        let Some(task) = &self.task_policy else {
            return check_policy_heads_in(connection, primary, &self.authorization, now_ms);
        };
        task.check_scope(facts, task.selection(), task.context(), now_ms)?;
        check_task_policy_heads_in(connection, task)?;
        task.state().check_current(&self.authorization, now_ms)?;
        ensure!(
            now_ms < self.expires_at_ms(),
            "task execution authorization expired"
        );
        Ok(())
    }

    pub(crate) fn bind_in(
        &self,
        connection: &Connection,
        body: &Value,
        binding: Option<&SignedBinding>,
        admitted_at_ms: u64,
    ) -> Result<()> {
        if let Some(task) = &self.task_policy {
            bind_task_execution_policy_in(
                connection,
                task,
                &self.authorization,
                body,
                binding,
                admitted_at_ms,
            )?;
        }
        Ok(())
    }
}

pub(crate) struct OriginalAdmissionView<'a> {
    primary: Cow<'a, VerifiedPolicySet>,
    task_policy: Option<VerifiedTaskOriginalAdmissionPolicy>,
}

impl<'a> OriginalAdmissionView<'a> {
    pub(crate) fn load(
        connection: &Connection,
        policy: &'a PolicyState,
        proof: &AdmissionProof,
    ) -> Result<Self> {
        let key = super::event_authorization::audience_key(policy.verified(), proof)?;
        let task_policy = original_task_execution_policy_in(connection, policy, &key, proof)?;
        let heads = task_policy
            .as_ref()
            .map_or(proof.challenge.policies.as_slice(), |task| {
                task.primary_heads()
            });
        let primary = if policy.current_heads_at(heads, proof.admitted_at_ms)? {
            Cow::Borrowed(policy.verified())
        } else {
            Cow::Owned(policy.historical(heads, proof.admitted_at_ms)?)
        };
        if let Some(task) = &task_policy {
            ensure!(
                task.signed_invocation_hash() == &proof.invocation.hash()?
                    && task.admitted_at_ms() == proof.admitted_at_ms
                    && heads
                        .iter()
                        .all(|head| primary.head(&head.domain) == Some(head)),
                "original task policy differs from its accepted admission"
            );
        }
        Ok(Self {
            primary,
            task_policy,
        })
    }

    pub(crate) fn state(&self) -> &VerifiedPolicySet {
        self.task_policy
            .as_ref()
            .map_or(self.primary.as_ref(), |task| task.state())
    }

    pub(crate) fn primary_state(&self) -> &VerifiedPolicySet {
        self.primary.as_ref()
    }

    pub(crate) fn task_policy(&self) -> Option<&VerifiedTaskOriginalAdmissionPolicy> {
        self.task_policy.as_ref()
    }
}

impl Store {
    pub(crate) fn authorize_scoped_request_for_body(
        &self,
        identity: &LocalIdentity,
        invocation: &Signed<Invocation>,
        facts: &RequestFacts,
        transport: &TransportEvidence,
        body: &Value,
        now_ms: u64,
    ) -> Result<TaskScopedAuthorization> {
        self.authorize_scoped_request_for_transport(
            identity,
            invocation,
            facts,
            &RequestTransport::Existing(transport.clone()),
            body,
            now_ms,
        )
    }

    pub(crate) fn authorize_scoped_request_for_transport(
        &self,
        identity: &LocalIdentity,
        invocation: &Signed<Invocation>,
        facts: &RequestFacts,
        transport: &RequestTransport,
        body: &Value,
        now_ms: u64,
    ) -> Result<TaskScopedAuthorization> {
        let snapshot = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&snapshot, &self.policy)?;
        let task_policy = current_task_execution_policy_in(
            &snapshot, &current, identity, invocation, body, now_ms,
        )?;
        let state = task_policy
            .as_ref()
            .map_or(current.verified(), |task| task.state());
        let authorization = authorize_request_invocation_for_body(
            state, invocation, facts, transport, body, now_ms,
        )?;
        let scoped = TaskScopedAuthorization {
            authorization,
            task_policy,
        };
        scoped.check_current_in(&snapshot, current.verified(), facts, now_ms)?;
        snapshot.commit()?;
        Ok(scoped)
    }

    pub(crate) fn issue_scoped_freshness(
        &mut self,
        scoped: &TaskScopedAuthorization,
        stage: CheckStage,
        now_ms: u64,
    ) -> Result<FreshnessChallenge> {
        self.issue_scoped_freshness_with_transport(scoped, None, stage, now_ms)
    }

    pub(crate) fn issue_scoped_freshness_for_transport(
        &mut self,
        scoped: &TaskScopedAuthorization,
        transport: &RequestTransport,
        stage: CheckStage,
        now_ms: u64,
    ) -> Result<FreshnessChallenge> {
        self.issue_scoped_freshness_with_transport(scoped, Some(transport), stage, now_ms)
    }

    fn issue_scoped_freshness_with_transport(
        &mut self,
        scoped: &TaskScopedAuthorization,
        transport: Option<&RequestTransport>,
        stage: CheckStage,
        now_ms: u64,
    ) -> Result<FreshnessChallenge> {
        let snapshot = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&snapshot, &self.policy)?;
        let auth = scoped.authorization();
        if let Some(transport) = transport {
            transport.check_current(auth, now_ms)?;
        }
        let invocation = &auth.signed_invocation().payload;
        let facts = RequestFacts {
            resource: invocation.resource.clone(),
            action: invocation.action,
            audience: invocation.audience.clone(),
            operation_id: invocation.operation_id.clone(),
            body_hash: invocation.body_hash.clone(),
            context: invocation.context.clone(),
        };
        scoped.check_current_in(&snapshot, current.verified(), &facts, now_ms)?;
        let lifetime = if stage == CheckStage::RemoteRead {
            vibeface_secure::authorization::REMOTE_READ_MAX_AGE_MS
        } else {
            vibeface_secure::authorization::EXECUTION_LEASE_MS
        };
        let expires_at_ms = now_ms
            .checked_add(lifetime)
            .context("challenge expiry overflow")?
            .min(scoped.expires_at_ms())
            .min(transport.map_or(u64::MAX, RequestTransport::expires_at_ms));
        let challenge =
            FreshnessChallenge::new(auth, stage, Nonce::random()?, now_ms, expires_at_ms)?;
        let encoded = serde_json::to_string(&challenge)?;
        let stage = serde_json::to_string(&stage)?;
        snapshot.execute(
            "INSERT INTO authorization_challenges(space_id,nonce,invocation_hash,stage,challenge,expires_at_ms) VALUES(?,?,?,?,?,?)",
            params![auth.resource().space.as_str(), challenge.nonce.as_str(), auth.invocation_hash().as_str(), stage.trim_matches('"'), encoded, expires_at_ms],
        )?;
        snapshot.commit()?;
        Ok(challenge)
    }
}
